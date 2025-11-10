use crate::error::Error;
use futures_util::{StreamExt, TryStreamExt, stream};
use log::{debug, error, info};
use sp_blockchain::{CachedHeaderMetadata, HashAndNumber};
use sp_runtime::codec::{Decode, Encode};
use sp_runtime::traits::{BlakeTwo256, Block as BlockT, Header as HeaderT};
use sp_runtime::{OpaqueExtrinsic, generic};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use subxt::client::ClientRuntimeUpdater;
use subxt::config::substrate::SubstrateHeader;
use subxt::events::Events;
use subxt::{OnlineClient, SubstrateConfig};
use subxt_core::Config;
use subxt_core::storage::address::StorageKey;
use subxt_rpcs::{LegacyRpcMethods, RpcClient};
use tokio::sync::broadcast::{Receiver, Sender, channel};

/// Opaque block header type.
type Header = generic::Header<u32, BlakeTwo256>;
/// Opaque block type.
type Block = generic::Block<Header, OpaqueExtrinsic>;
type BlockHashFor<Block> = <Block as BlockT>::Hash;
type BlockNumberFor<Block> = <<Block as BlockT>::Header as HeaderT>::Number;
type SubspaceClient = OnlineClient<SubstrateConfig>;
type SubspaceRpcClient = LegacyRpcMethods<SubstrateConfig>;
type BlocksSink = Sender<BlocksExt>;
pub(crate) type BlocksStream = Receiver<BlocksExt>;

/// Subspace slot type.
pub(crate) type Slot = u64;
/// Subspace timestamp type.
pub(crate) type Timestamp = u64;
/// Block with extracted details.
#[derive(Debug, Clone)]
pub(crate) struct BlockExt {
    pub(crate) number: BlockNumberFor<Block>,
    pub(crate) hash: BlockHashFor<Block>,
    pub(crate) parent_hash: BlockHashFor<Block>,
    pub(crate) state_root: BlockHashFor<Block>,
    pub(crate) extrinsics_root: BlockHashFor<Block>,
    client: Arc<SubspaceClient>,
}

impl BlockExt {
    async fn read_storage<Args: StorageKey, T: Decode>(
        &self,
        pallet: &str,
        storage: &str,
        arg_data: Args,
    ) -> Result<T, Error> {
        let query = subxt::dynamic::storage(pallet, storage, arg_data);
        self.client
            .storage()
            .at(self.hash)
            .fetch(&query)
            .await?
            .map(|encoded| T::decode(&mut encoded.encoded()).map_err(Error::Scale))
            .ok_or(Error::Storage(format!("{pallet}.{storage}")))?
    }

    /// Returns block timestamp.
    pub(crate) async fn timestamp(&self) -> Result<Timestamp, Error> {
        self.read_storage("Timestamp", "Now", ()).await
    }

    /// Returns block slot.
    pub(crate) async fn slot(&self) -> Result<Slot, Error> {
        let slots = self
            .read_storage::<_, BTreeMap<BlockNumberFor<Block>, Slot>>("Subspace", "BlockSlots", ())
            .await?;
        slots
            .get(&self.number)
            .cloned()
            .ok_or(Error::Storage(format!(
                "Missing slot for block: {})",
                self.number
            )))
    }

    /// Returns block extrinsics.
    pub(crate) async fn extrinsics<Ext: Decode>(&self) -> Result<Vec<Ext>, Error> {
        let exts = self
            .client
            .backend()
            .block_body(self.hash)
            .await?
            .ok_or(Error::MissingBlockBody(self.hash))?
            .into_iter()
            .map(|ext| Ext::decode(&mut &ext[..]))
            .try_collect::<Vec<_>>()?;
        Ok(exts)
    }

    /// Returns block events
    pub(crate) async fn events(&self) -> Result<Events<SubstrateConfig>, Error> {
        let events = self.client.events().at(self.hash).await?;
        Ok(events)
    }
}

#[derive(Debug, Clone)]
pub(crate) struct ReorgData {
    pub(crate) enacted: Vec<HashAndNumber<Block>>,
    pub(crate) retracted: Vec<HashAndNumber<Block>>,
    pub(crate) common_block: HashAndNumber<Block>,
}

/// Best blocks that have been enacted and potential re-org depth if there was a re-org.
#[derive(Debug, Clone)]
pub(crate) struct BlocksExt {
    pub(crate) blocks: Vec<BlockExt>,
    pub(crate) maybe_reorg_data: Option<ReorgData>,
}

/// Maximum number of headers to load in the cache.
const CACHE_HEADER_DEPTH: u32 = 100;

/// Blocks listen for all the blocks imported to the chain
/// handle the tree_route and broadcast the BlockExt with ReOrg depth if any.
pub(crate) struct Blocks {
    rpc_client: Arc<SubspaceRpcClient>,
    client: Arc<SubspaceClient>,
    sink: BlocksSink,
    stream: BlocksStream,
}

impl Blocks {
    pub(crate) async fn new_from_url(url: &str) -> Result<Self, Error> {
        let rpc_client = RpcClient::from_url(url).await?;
        let rpc = Arc::new(LegacyRpcMethods::<SubstrateConfig>::new(rpc_client.clone()));
        let client = Arc::new(SubspaceClient::from_url(url).await?);
        let (sink, stream) = channel(100);
        Ok(Self {
            rpc_client: rpc,
            client,
            sink,
            stream,
        })
    }

    pub(crate) fn runtime_metadata_updater(&self) -> ClientRuntimeUpdater<SubstrateConfig> {
        self.client.updater()
    }

    pub(crate) fn blocks_stream(&self) -> BlocksStream {
        self.stream.resubscribe()
    }

    /// Listens for all the blocks being imported,
    /// calculate the tree route if there is a re-org,
    /// and broadcast the best blocks.
    pub(crate) async fn listen_for_all_blocks(&self) -> Result<(), Error> {
        let blocks_client = self.client.blocks();
        let mut sub = blocks_client.subscribe_all().await?.fuse();
        let (mut header_metadata, mut current_best_block) =
            self.load_header_metadata_cache().await?;

        loop {
            let block = sub.next().await.ok_or(Error::MissingBlock)??;
            let block_hash = block.hash();
            let block_number = block.number();

            header_metadata.add_header(block_hash, block.header().clone());
            if !self.is_canonical_block(block_number, block_hash).await? {
                info!("⚠️ Imported fork block: {block_number}[{block_hash}]");
                continue;
            }

            // calculate tree route from previous best to current best
            let tree_route =
                sp_blockchain::tree_route(&header_metadata, current_best_block.hash, block_hash)?;
            let enacted = tree_route.enacted().to_vec();
            let retracted = tree_route.retracted().to_vec();
            if enacted.is_empty() && retracted.is_empty() {
                // happens when best block == imported block
                // we are on the best canonical path
                continue;
            }

            if enacted.is_empty() {
                // should not happen where enact nothing but retract blocks
                continue;
            }

            current_best_block = enacted
                .last()
                .expect("Latest enacted should exist as checked above; qed")
                .clone();

            let block_exts = stream::iter(enacted.clone())
                .map(|hash_and_number| self.get_block_ext(&header_metadata, hash_and_number.hash))
                .buffered(5)
                .try_collect::<Vec<_>>()
                .await?;

            info!(
                "✅ Imported best block: {}[{}]",
                current_best_block.number, current_best_block.hash,
            );

            let maybe_reorg_data = (!retracted.is_empty()).then(|| ReorgData {
                enacted,
                retracted,
                common_block: tree_route.common_block().clone(),
            });
            if let Err(err) = self.sink.send(BlocksExt {
                blocks: block_exts,
                maybe_reorg_data,
            }) {
                error!("Error sending blocks data: {err}");
            }
            let number_to_clean = block_number.saturating_sub(CACHE_HEADER_DEPTH);
            header_metadata.remove_header(number_to_clean);
        }
    }

    async fn is_canonical_block(
        &self,
        block_number: BlockNumberFor<Block>,
        block_hash: BlockHashFor<Block>,
    ) -> Result<bool, Error> {
        let hash = self
            .rpc_client
            .chain_get_block_hash(Some(block_number.into()))
            .await?
            .ok_or(Error::MissingBlock)?;
        Ok(block_hash == hash)
    }

    async fn get_block_ext(
        &self,
        cache: &HeadersMetadataCache,
        hash: BlockHashFor<Block>,
    ) -> Result<BlockExt, Error> {
        let header = cache.get_header(hash)?;
        let Header {
            parent_hash,
            number,
            state_root,
            extrinsics_root,
            ..
        } = header;

        Ok(BlockExt {
            number,
            hash,
            parent_hash,
            state_root,
            extrinsics_root,
            client: self.client.clone(),
        })
    }

    async fn load_header_metadata_cache(
        &self,
    ) -> Result<(HeadersMetadataCache, HashAndNumber<Block>), Error> {
        let latest_hash = self
            .rpc_client
            .chain_get_block_hash(None)
            .await?
            .ok_or(Error::MissingBlock)?;
        let latest_head = self
            .rpc_client
            .chain_get_header(Some(latest_hash))
            .await?
            .ok_or(Error::MissingBlock)?;

        let cache_start_number = latest_head.number.saturating_sub(CACHE_HEADER_DEPTH);

        info!(
            "Loading header cache from block {} to {}",
            cache_start_number, latest_head.number
        );
        let mut header_metadata = HeadersMetadataCache::default();
        stream::iter(
            (cache_start_number..=latest_head.number).map(|number| async move {
                let block_hash = self
                    .rpc_client
                    .chain_get_block_hash(Some(number.into()))
                    .await?
                    .ok_or(Error::MissingBlock)?;
                let header = self
                    .rpc_client
                    .chain_get_header(Some(block_hash))
                    .await?
                    .ok_or(Error::MissingBlock)?;
                debug!("Block header from RPC {number} - {block_hash}");
                Ok::<_, Error>((block_hash, header))
            }),
        )
        .buffered(30)
        .try_collect::<Vec<_>>()
        .await?
        .into_iter()
        .for_each(|(hash, header)| {
            header_metadata.add_header(hash, header);
        });

        info!("Cache header metadata completed.");
        Ok((
            header_metadata,
            HashAndNumber {
                number: latest_head.number,
                hash: latest_hash,
            },
        ))
    }
}

#[derive(Debug, Default)]
struct HeadersMetadataCache {
    block_header_data: BTreeMap<BlockHashFor<Block>, Header>,
    block_number_hashes: BTreeMap<BlockNumberFor<Block>, BTreeSet<BlockHashFor<Block>>>,
}

impl HeadersMetadataCache {
    fn add_header(
        &mut self,
        hash: BlockHashFor<Block>,
        header: <SubstrateConfig as Config>::Header,
    ) -> bool {
        let SubstrateHeader {
            parent_hash,
            number,
            state_root,
            extrinsics_root,
            digest,
        } = header;
        let encoded = digest.encode();
        let decoded_digest =
            sp_runtime::generic::Digest::decode(&mut &encoded[..]).expect("Digest is always valid");
        let header = Header {
            parent_hash,
            number,
            state_root,
            extrinsics_root,
            digest: decoded_digest,
        };
        assert_eq!(hash, header.hash());
        let number = header.number;
        let replaced = self.block_header_data.insert(hash, header);
        self.block_number_hashes
            .entry(number)
            .or_default()
            .insert(hash);
        debug!("Block[{number}] {hash} cached");
        replaced.is_some()
    }

    fn remove_header(&mut self, number: BlockNumberFor<Block>) {
        debug!("Removing header from cache: {number}");
        let hashes = self.block_number_hashes.remove(&number).unwrap_or_default();
        hashes.into_iter().for_each(|hash| {
            self.block_header_data.remove(&hash);
        })
    }

    fn get_header(&self, hash: BlockHashFor<Block>) -> Result<Header, Error> {
        self.block_header_data
            .get(&hash)
            .cloned()
            .ok_or(Error::MissingBlockHashFromCache(hash))
    }
}

impl sp_blockchain::HeaderMetadata<Block> for HeadersMetadataCache {
    type Error = Error;

    fn header_metadata(
        &self,
        hash: BlockHashFor<Block>,
    ) -> Result<CachedHeaderMetadata<Block>, Self::Error> {
        debug!("Retrieving header from cache: {hash}");
        let block_header_meta = self.block_header_data.get(&hash);
        block_header_meta
            .map(CachedHeaderMetadata::<Block>::from)
            .ok_or(Error::MissingBlockHashFromCache(hash))
    }

    fn insert_header_metadata(&self, _: BlockHashFor<Block>, _: CachedHeaderMetadata<Block>) {
        // nothing to do here
    }

    fn remove_header_metadata(&self, _: BlockHashFor<Block>) {
        // nothing to do here
    }
}
