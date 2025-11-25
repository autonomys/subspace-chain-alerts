use crate::error::Error;
use crate::subspace::{BlockHash, BlockNumber, Slot};
use futures_util::StreamExt;
use libp2p::kad::Event as KadEvent;
use libp2p::multiaddr::Protocol as MultiAddrProtocol;
use libp2p::swarm::{NetworkBehaviour, SwarmEvent};
use libp2p::{Multiaddr, PeerId, Swarm, SwarmBuilder, noise, tcp, yamux};
use log::{debug, error, info};
use parity_scale_codec::{Decode, Encode};
use std::collections::BTreeSet;
use std::num::NonZeroU32;
use std::time::Duration;
use substrate_p2p::discovery::{Discovery, DiscoveryBuilder};
use substrate_p2p::notifications::behavior::{
    Behavior as NotificationsBehavior, Event as NotificationsEvent, Protocol,
};
use substrate_p2p::notifications::messages::ProtocolRole;
use tokio::sync::broadcast::{Receiver, Sender, channel};

const POT_PROTOCOL: &str = "/subspace/subspace-proof-of-time/1";

pub(crate) type PoTStream = Receiver<GossipProof>;
type PoTSink = Sender<GossipProof>;

#[derive(Debug, Copy, Clone, Eq, PartialEq, Hash, Encode, Decode)]
pub(crate) struct PotSeed([u8; 16]);

#[derive(Debug, Copy, Clone, Eq, PartialEq, Hash, Encode, Decode)]
pub(crate) struct PotOutput([u8; 16]);

#[derive(Debug, Copy, Clone, Eq, PartialEq, Hash, Encode, Decode)]
pub(crate) struct PotCheckpoints([PotOutput; 8]);

#[derive(Debug, Copy, Clone, Eq, PartialEq, Hash, Encode, Decode)]
pub(crate) struct GossipProof {
    /// Slot number
    pub(crate) slot: Slot,
    /// Proof of time seed
    pub(crate) seed: PotSeed,
    /// Iterations per slot
    pub(crate) slot_iterations: NonZeroU32,
    /// Proof of time checkpoints
    pub(crate) checkpoints: PotCheckpoints,
}

#[derive(NetworkBehaviour)]
struct Behavior {
    discovery: Discovery,
    pot_notifications: NotificationsBehavior<BlockNumber, BlockHash, GossipProof>,
}

/// Overarching p2p network with discovery through Kad.
pub(crate) struct Network {
    swarm: Swarm<Behavior>,
    bootnodes: Vec<Multiaddr>,
    pot_sink: PoTSink,
    pot_stream: PoTStream,
    authorities: BTreeSet<PeerId>,
    fullnodes: BTreeSet<PeerId>,
}

impl Network {
    pub(crate) async fn new(
        bootnodes: Vec<Multiaddr>,
        genesis_hash: BlockHash,
    ) -> Result<Network, Error> {
        let swarm = build_swarm(genesis_hash)?;
        let (pot_sink, pot_stream) = channel(100);
        Ok(Self {
            swarm,
            bootnodes,
            pot_sink,
            pot_stream,
            authorities: Default::default(),
            fullnodes: Default::default(),
        })
    }

    pub(crate) fn pot_stream(&self) -> PoTStream {
        self.pot_stream.resubscribe()
    }

    fn add_peer_role(&mut self, peer: PeerId, role: ProtocolRole) {
        match role {
            ProtocolRole::FullNode => {
                self.fullnodes.insert(peer);
            }
            ProtocolRole::LightNode => {}
            ProtocolRole::Authority => {
                self.authorities.insert(peer);
            }
        }
    }

    pub(crate) async fn run(&mut self) -> Result<(), Error> {
        // Dial bootnodes
        for addr in &self.bootnodes {
            if let Err(err) = self.swarm.dial(addr.clone()) {
                error!("Failed to dial {addr:?}: {err:?}");
            } else {
                info!("🛰️ Connected to bootnode {addr:?}");
            }

            if let Some(peer_id) = extract_peer_id(addr) {
                self.swarm
                    .behaviour_mut()
                    .discovery
                    .add_address(&peer_id, addr.clone());
            }
        }

        // Periodic discovery every 30 seconds
        let mut discovery_interval = tokio::time::interval(Duration::from_secs(30));

        loop {
            tokio::select! {
                _ = discovery_interval.tick() => {
                    // Query discovery periodically
                    info!("🔍 Starting periodic peer discovery...");
                    // Do 50 queries randomly
                    for _ in 0..50 {
                        self.swarm.behaviour_mut().discovery.get_closest_peers(PeerId::random());
                    }

                    // Log currently connected peers
                    let connected_peers = self.swarm.connected_peers().count();
                    info!("🤝 Connected peers: {connected_peers:?}");
                    info!("🤝 Authority nodes: {:?}", self.authorities.len());
                    info!("🤝 Full nodes: {:?}", self.fullnodes.len());
                }

                event = self.swarm.select_next_some() => match event {
                    SwarmEvent::Behaviour(event) => {
                        match event {
                            BehaviorEvent::Discovery(event) => {
                                if let KadEvent::RoutablePeer { peer, address } = event {
                                    debug!("Discovered routable peer {peer:?} at address {address:?}");
                                    if let Err(err) = self.swarm.dial(address.clone()) {
                                        error!("Failed to dial discovered peer {address:?}: {err:?}");
                                    }
                                }
                            }
                            BehaviorEvent::PotNotifications(event) => match event {
                                NotificationsEvent::ProtocolOpen { peer_id, role, .. } => {
                                    info!("📡 PoT slot stream opened with peer[{peer_id}] with role: {role:?}");
                                    self.add_peer_role(peer_id, role);
                                }
                                NotificationsEvent::ProtocolClosed { peer_id } => {
                                    info!("❌ PoT slot stream closed with peer[{peer_id}]");
                                }
                                NotificationsEvent::Notification { peer_id, message,  } => {
                                    debug!("New Slot: {} from peer {peer_id:?}", message.slot);
                                    if let Err(err) = self.pot_sink.send(message){
                                        error!("❌ Failed to send new slot message: {err:?}");
                                    }
                                }
                            },
                        }
                    }
                    SwarmEvent::ConnectionClosed{ peer_id, .. } => {
                        self.authorities.remove(&peer_id);
                        self.fullnodes.remove(&peer_id);
                    }
                    _ => {}
               }
            }
        }
    }
}

fn build_swarm(genesis_hash: BlockHash) -> Result<Swarm<Behavior>, Error> {
    // Establish PoT substream using libp2p_stream’s Control handle.
    let mut swarm = SwarmBuilder::with_new_identity()
        .with_tokio()
        .with_tcp(
            tcp::Config::default(),
            noise::Config::new,
            yamux::Config::default,
        )
        .expect("infallible tcp transport")
        .with_dns()?
        .with_behaviour(|key| {
            let pot_notifications =
                NotificationsBehavior::<BlockNumber, BlockHash, GossipProof>::new(
                    Protocol::Protocol::<BlockHash>(POT_PROTOCOL.into()),
                );
            let local_peer_id = PeerId::from_public_key(&key.public());
            let discovery = DiscoveryBuilder::default()
                .record_ttl(Some(Duration::from_secs(60 * 30)))
                .provider_ttl(Some(Duration::from_secs(60 * 30)))
                .query_timeout(Duration::from_secs(5 * 60))
                .build(local_peer_id, &hex::encode(genesis_hash));

            Behavior {
                discovery,
                pot_notifications,
            }
        })
        .expect("infallible behavior construction")
        .with_swarm_config(|cfg| cfg.with_idle_connection_timeout(Duration::from_secs(u64::MAX)))
        .build();

    swarm
        .listen_on(
            "/ip4/0.0.0.0/tcp/0"
                .parse()
                .expect("Valid listen address; qed"),
        )
        .expect("Valid listen address; qed");

    Ok(swarm)
}

fn extract_peer_id(addr: &Multiaddr) -> Option<PeerId> {
    addr.iter().find_map(|p| {
        if let MultiAddrProtocol::P2p(peer_id) = p {
            Some(peer_id)
        } else {
            None
        }
    })
}
