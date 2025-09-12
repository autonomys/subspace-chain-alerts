//! Subspace chain connection and block parsing code.

pub mod decode;
#[cfg(test)]
pub mod tests;

use crate::format::{fmt_fields, fmt_timestamp};
use anyhow::Result;
use chrono::{DateTime, Utc};
use scale_value::Composite;
use std::fmt::{self, Display};
use std::ops::Sub;
use std::time::Duration;
use subspace_process::AsyncJoinOnDrop;
use subxt::backend::rpc::reconnecting_rpc_client::ExponentialBackoff;
use subxt::blocks::{Block, ExtrinsicDetails, Extrinsics};
use subxt::config::substrate::DigestItem;
use subxt::events::{EventDetails, Events, Phase};
use subxt::ext::subxt_rpcs::client::ReconnectingRpcClient;
use subxt::utils::H256;
use subxt::{OnlineClient, SubstrateConfig};
use tracing::{debug, info, trace, warn};

/// One Subspace Credit.
/// Copied from subspace-runtime-primitives.
pub const AI3: Balance = 10_u128.pow(18);

/// The target block interval, in seconds.
pub const TARGET_BLOCK_INTERVAL: u64 = 6;

/// The minumum delay between RPC reconnection attempts, in milliseconds.
pub const MIN_RECONNECTION_DELAY: u64 = 10;

/// The maximum delay between RPC reconnection attempts, in milliseconds.
pub const MAX_RECONNECTION_DELAY: u64 = 10_000;

/// The maximum number of RPC reconnection attempts before failing and exiting the process.
pub const MAX_RECONNECTION_ATTEMPTS: usize = 10;

/// The default RPC URL for a local Subspace node.
pub const LOCAL_SUBSPACE_NODE_URL: &str = "ws://127.0.0.1:9944";

/// The RPC URL for the public Subspace Foundation RPC instance.
#[allow(dead_code, reason = "only used in tests")]
pub const FOUNDATION_SUBSPACE_NODE_URL: &str = "wss://rpc.mainnet.subspace.foundation/ws";

/// The RPC URL for the public Autonomys Labs RPC instance.
#[expect(dead_code, reason = "TODO: run tests against both instances")]
pub const LABS_SUBSPACE_NODE_URL: &str = "wss://rpc-0.mainnet.autonomys.xyz/ws";

/// The Subspace block height type.
/// Copied from subspace-core-primitives.
pub type BlockNumber = u32;

/// The Subspace balance amount type.
/// Copied from subspace-runtime-primitives.
pub type Balance = u128;

/// The Subspace raw time type.
/// Copied from subspace-runtime-primitives.
pub type RawTime = u64;

/// The config for basic Subspace block and extrinsic types.
/// TODO: create a custom SubspaceConfig type
pub type SubspaceConfig = SubstrateConfig;

/// The type of Subspace client we're using.
pub type SubspaceClient = OnlineClient<SubspaceConfig>;

/// The type of raw RPC client we're using.
pub type RawRpcClient = ReconnectingRpcClient;

/// The raw block hash literal type.
#[allow(dead_code, reason = "only used in tests")]
pub type RawBlockHash = [u8; 32];

/// The type of Subspace block.
pub type RawBlock = Block<SubspaceConfig, SubspaceClient>;

/// The type of a Subspace extrinsic list.
pub type RawExtrinsicList = Extrinsics<SubspaceConfig, SubspaceClient>;

/// The type of a Subspace event list.
pub type RawExtrinsic = ExtrinsicDetails<SubspaceConfig, SubspaceClient>;

/// The Subspace/subxt extrinsic index type.
pub type ExtrinsicIndex = u32;

/// The type of a Subspace event list.
#[allow(dead_code, reason = "included for completeness")]
pub type RawEventList = Events<SubspaceConfig>;

/// The Subspace/subxt event details type.
pub type RawEvent = EventDetails<SubspaceConfig>;

/// The Subspace/subxt event index type.
pub type EventIndex = u32;

/// Create a new reconnecting Subspace client.
/// Returns the subxt client, the raw RPC client, and a task handle for the subxt metadata update
/// task.
///
/// The metadata update task is aborted when the returned handle is dropped.
pub async fn create_subspace_client(
    node_url: impl AsRef<str>,
) -> Result<
    (
        SubspaceClient,
        RawRpcClient,
        AsyncJoinOnDrop<anyhow::Result<()>>,
    ),
    anyhow::Error,
> {
    info!("connecting to Subspace node at {}", node_url.as_ref());

    // Create a new client with with a reconnecting RPC client.
    let rpc = RawRpcClient::builder()
        // Reconnect with exponential backoff, take limits the number of retries.
        // The exponential series multiplies by the minimum reconnection delay each retry.
        .retry_policy(
            ExponentialBackoff::from_millis(MIN_RECONNECTION_DELAY)
                .max_delay(Duration::from_millis(MAX_RECONNECTION_DELAY))
                .take(MAX_RECONNECTION_ATTEMPTS),
        )
        .build(node_url)
        .await?;

    // TODO: decide if we want to use the chainhead backend with the reconnecting RPC client:
    // let backend = ChainHeadBackend::builder().build_with_background_task(rpc.clone());
    // let client = SubspaceClient::from_backend(Arc::new(backend)).await?;

    let client = SubspaceClient::from_rpc_client(rpc.clone()).await?;

    let update_task = spawn_metadata_update_task(&client).await;

    Ok((client, rpc, update_task))
}

/// Spawn a background task to keep the runtime metadata up to date.
/// The task is aborted when the returned handle is dropped.
pub async fn spawn_metadata_update_task(
    chain_client: &SubspaceClient,
) -> AsyncJoinOnDrop<anyhow::Result<()>> {
    info!("spawning runtime metadata update task...");
    let update_task = chain_client.updater();

    AsyncJoinOnDrop::new(
        // If a metadata update fails, we want to end the task and re-run setup.
        tokio::spawn(async move {
            update_task.perform_runtime_updates().await?;
            Ok(())
        }),
        true,
    )
}

/// Block position in the chain, including height and hash.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Ord, PartialOrd, Hash)]
pub struct BlockPosition {
    /// The block number.
    pub height: BlockNumber,

    /// The block hash.
    pub hash: H256,
}

impl Display for BlockPosition {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} ({:?})", self.height, self.hash)?;

        Ok(())
    }
}

impl BlockPosition {
    /// Create a new block position from a block height and hash.
    pub fn new(height: BlockNumber, hash: H256) -> Self {
        BlockPosition { height, hash }
    }

    /// Create a new block position from a block.
    pub fn from_block(block: &RawBlock) -> Self {
        BlockPosition {
            height: block.header().number,
            hash: block.hash(),
        }
    }

    /// Create a block position, given its hash.
    #[expect(dead_code, reason = "included for completeness")]
    pub async fn with_block_hash(
        block_hash: H256,
        chain_client: &SubspaceClient,
    ) -> anyhow::Result<Self> {
        let block = chain_client.blocks().at(block_hash).await?;

        Ok(Self::from_block(&block))
    }

    /// Returns the minimum possible block position for a block height.
    /// Use this for range queries.
    pub fn min_for_height_range(height: BlockNumber) -> Self {
        BlockPosition {
            height,
            hash: H256::zero(),
        }
    }

    /// Returns the maximum possible block position for a block height.
    /// Use this for range queries.
    #[expect(dead_code, reason = "included for completeness")]
    pub fn max_for_height_range(height: BlockNumber) -> Self {
        BlockPosition {
            height,
            hash: H256::repeat_byte(0xff),
        }
    }
}

/// Block link in the chain, including height, hash, and parent hash.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Ord, PartialOrd, Hash)]
pub struct BlockLink {
    /// The block's position.
    pub position: BlockPosition,

    /// The block's parent hash.
    pub parent_hash: H256,
}

impl Display for BlockLink {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} <- {:?}", self.position, self.parent_hash)
    }
}

impl BlockLink {
    /// Create a new block link from a block position and parent hash.
    pub fn new(position: BlockPosition, parent_hash: H256) -> Self {
        Self {
            position,
            parent_hash,
        }
    }

    /// Create a new block link from a block info.
    #[expect(dead_code, reason = "included for completeness")]
    pub fn from_block_info(block_info: &BlockInfo) -> Self {
        block_info.link
    }

    /// Create a new block link from a block.
    pub fn from_block(block: &RawBlock) -> Self {
        Self::new(BlockPosition::from_block(block), block.header().parent_hash)
    }

    /// Create a block link, given its hash.
    pub async fn with_block_hash(
        block_hash: H256,
        chain_client: &SubspaceClient,
    ) -> anyhow::Result<Self> {
        let block = chain_client.blocks().at(block_hash).await?;

        Ok(Self::from_block(&block))
    }

    /// Returns the block hash.
    pub fn hash(&self) -> H256 {
        self.position.hash
    }

    /// Returns the block height.
    pub fn height(&self) -> BlockNumber {
        self.position.height
    }

    /// Returns the parent block position.
    pub fn parent_position(&self) -> BlockPosition {
        BlockPosition::new(self.position.height.saturating_sub(1), self.parent_hash)
    }
}

/// Block info that can be formatted.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Ord, PartialOrd)]
pub struct BlockInfo {
    /// The block height, hash, and parent hash.
    pub link: BlockLink,

    /// The time extrinsic in the block, if it exists.
    pub time: Option<BlockTime>,

    /// The block slot.
    pub slot: Option<Slot>,

    /// The genesis block hash for this network.
    pub genesis_hash: H256,
}

impl Display for BlockInfo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let Self {
            link:
                BlockLink {
                    position: BlockPosition { height, hash },
                    // Skip the parent hash because it's too verbose in alerts.
                    parent_hash: _,
                },
            time,
            slot,
            genesis_hash,
        } = self;

        writeln!(f, "Block Height: {height}")?;
        // Show full block hash but truncated genesis hash.
        writeln!(f, "Hash: {hash:?}")?;
        writeln!(
            f,
            "Time: {}",
            time.as_ref()
                .map(|bt| bt.to_string())
                .unwrap_or_else(|| "unknown".to_string())
        )?;
        writeln!(
            f,
            "Slot: {}",
            slot.map(|bs| bs.to_string())
                .unwrap_or_else(|| "unknown".to_string())
        )?;
        write!(f, "Genesis: {genesis_hash}")?;

        Ok(())
    }
}

impl BlockInfo {
    /// Create a block info from a block and its extrinsics.
    pub fn new(block: &RawBlock, extrinsics: &RawExtrinsicList, genesis_hash: &H256) -> Self {
        Self {
            link: BlockLink::from_block(block),
            time: BlockTime::new(extrinsics),
            slot: Slot::new(block),
            genesis_hash: *genesis_hash,
        }
    }

    /// Create a block info from a block, after fetching its extrinsics.
    pub async fn with_block(
        block: &RawBlock,
        chain_client: &SubspaceClient,
    ) -> anyhow::Result<Self> {
        // These errors represent a connection failure or similar, and require a
        // restart.
        let extrinsics = block.extrinsics().await?;

        Ok(BlockInfo::new(
            block,
            &extrinsics,
            &chain_client.genesis_hash(),
        ))
    }

    /// Create a block info, given its hash.
    pub async fn with_block_hash(
        block_hash: H256,
        chain_client: &SubspaceClient,
    ) -> anyhow::Result<Self> {
        let block = chain_client.blocks().at(block_hash).await?;

        Self::with_block(&block, chain_client).await
    }

    /// Returns the block height.
    pub fn height(&self) -> BlockNumber {
        self.link.height()
    }

    /// Returns the block hash.
    pub fn hash(&self) -> H256 {
        self.link.hash()
    }

    /// Returns the block position.
    pub fn position(&self) -> BlockPosition {
        self.link.position
    }

    /// Returns the parent block hash.
    pub fn parent_hash(&self) -> H256 {
        self.link.parent_hash
    }
}

/// A block time formatted different ways.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Ord, PartialOrd)]
pub struct BlockTime {
    /// The block UNIX time (in milliseconds).
    pub unix_time: RawTime,
}

impl Display for BlockTime {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} ({})", self.human_time(), self.unix_time)
    }
}

impl BlockTime {
    /// Returns the block UNIX time (in milliseconds), a date time type, and a human-readable time
    /// string.
    ///
    /// If the block does not have a timestamp set extrinsic, or parsing fails, returns `None`.
    pub fn new(extrinsics: &RawExtrinsicList) -> Option<BlockTime> {
        // Find the timestamp set extrinsic (usually the first extrinsic).
        for extrinsic in extrinsics.iter() {
            let Ok(meta) = extrinsic.extrinsic_metadata() else {
                // If we can't get the extrinsic pallet and call name, there's nothing we can do.
                // We'll log it elsewhere, so just move on.
                continue;
            };

            if meta.pallet.name() != "Timestamp" || meta.variant.name != "set" {
                // Not the timestamp set extrinsic.
                continue;
            }

            // If we can't get the field value, there's only one timestamp extrinsic per block, and
            // only one field in it, so we just return None.
            let unix_time = extrinsic
                .field_values()
                .ok()?
                .into_values()
                .next()?
                .as_u128()?
                .try_into()
                .ok()?;

            return Some(BlockTime { unix_time });
        }

        None
    }

    /// Returns the block time as a date time type.
    pub fn date_time(&self) -> Option<DateTime<Utc>> {
        // If the time is out of range, return None.
        // This should never happen due to consensus rules.
        DateTime::from_timestamp_millis(i64::try_from(self.unix_time).ok()?)
    }

    /// Returns a human-readable time string.
    pub fn human_time(&self) -> String {
        fmt_timestamp(self.date_time())
    }
}

/// Extrinsic info that can be formatted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExtrinsicInfo {
    /// The extrinsic pallet name, also known as section.
    pub pallet: String,

    /// The extrinsic call name, also known as module or variant.
    pub call: String,

    /// The extrinsic index.
    pub index: ExtrinsicIndex,

    /// The extrinsic hash.
    pub hash: H256,

    /// The extrinsic fields, with the extrinsic index as a context.
    pub fields: Composite<ExtrinsicIndex>,
}

impl Display for ExtrinsicInfo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let Self {
            pallet,
            call,
            index,
            hash,
            fields,
        } = self;

        writeln!(f, "Extrinsic {pallet}::{call} (index {index})")?;
        writeln!(f, "Hash: {hash:?}")?;
        write!(f, "{}", fmt_fields(fields))?;

        Ok(())
    }
}

impl ExtrinsicInfo {
    /// Check and collect an extrinsic's info.
    pub fn new(extrinsic: &RawExtrinsic, block_info: &BlockInfo) -> Option<ExtrinsicInfo> {
        let Ok(meta) = extrinsic.extrinsic_metadata() else {
            // If we can't get the extrinsic pallet and call name, there's nothing we can do.
            // Just log it and move on.
            warn!(
                ?block_info,
                "extrinsic {} pallet/name unavailable in block",
                extrinsic.index(),
            );
            return None;
        };

        // We can usually get the extrinsic fields, but we don't need the fields for some
        // extrinsic alerts. So we just warn and substitute empty fields.
        let fields = extrinsic.field_values().unwrap_or_else(|_| {
            warn!(
                ?block_info,
                hash = ?extrinsic.hash(),
                "extrinsic {}:{} ({}) fields unavailable in block",
                meta.pallet.name(),
                meta.variant.name,
                extrinsic.index(),
            );
            Composite::unnamed(Vec::new())
        });

        Some(ExtrinsicInfo {
            pallet: meta.pallet.name().to_string(),
            call: meta.variant.name.to_string(),
            index: extrinsic.index(),
            hash: extrinsic.hash(),
            fields,
        })
    }

    /// Format the extrinsic's fields as a string, truncating it if it is too long.
    #[expect(dead_code, reason = "included for completeness")]
    pub fn fields_str(&self) -> String {
        fmt_fields(&self.fields)
    }
}

/// Event info that can be formatted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EventInfo {
    /// The event pallet name, also known as section.
    pub pallet: String,

    /// The event kind, also known as module or variant.
    pub kind: String,

    /// The event index in the block.
    pub index: EventIndex,

    /// The phase the event was emitted in.
    pub phase: Phase,

    /// The event fields, with the event index as a context.
    pub fields: Composite<EventIndex>,
}

impl Display for EventInfo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let Self {
            pallet,
            kind,
            index,
            phase,
            fields,
        } = self;

        writeln!(f, "Event {pallet}::{kind} (index {index})")?;
        writeln!(f, "Phase: {phase:?}")?;
        write!(f, "{}", fmt_fields(fields))?;

        Ok(())
    }
}

impl EventInfo {
    /// Check and collect an event's info.
    pub fn new(event: &RawEvent, block_info: &BlockInfo) -> EventInfo {
        let meta = event.event_metadata();

        // We can usually get the event fields, but we don't need the fields for some
        // event alerts. So we just warn and substitute empty fields.
        let fields = event.field_values().unwrap_or_else(|_| {
            warn!(
                ?block_info,
                "event {}:{} ({}) fields unavailable in block",
                meta.pallet.name(),
                meta.variant.name,
                event.index(),
            );
            Composite::unnamed(Vec::new())
        });

        EventInfo {
            pallet: meta.pallet.name().to_string(),
            kind: meta.variant.name.to_string(),
            phase: event.phase(),
            index: event.index(),
            fields,
        }
    }

    /// Format the event's fields as a string, truncating it if it is too long.
    #[expect(dead_code, reason = "included for completeness")]
    pub fn fields_str(&self) -> String {
        fmt_fields(&self.fields)
    }
}

/// Calculates the timestamp gap between a block and a later time, if the block is present and has a
/// timestamp. Returns `None` if the block info is missing, or the block is missing a timestamp.
pub fn gap_since_time(
    latest_time: DateTime<Utc>,
    prev_block_info: impl Into<Option<BlockInfo>>,
) -> Option<Duration> {
    let prev_block_info = prev_block_info.into()?;
    let prev_block_time = prev_block_info.time?;

    let gap = latest_time.signed_duration_since(prev_block_time.date_time()?);

    gap.to_std().ok()
}

/// Calculates the timestamp gap between two blocks, if both are present and have timestamps.
/// Returns `None` if either block info is missing, or a block is missing a timestamp.
pub fn gap_since_last_block(
    block_info: impl Into<Option<BlockInfo>>,
    prev_block_info: impl Into<Option<BlockInfo>>,
) -> Option<Duration> {
    let block_info = block_info.into()?;
    let block_time = block_info.time?;

    gap_since_time(block_time.date_time()?, prev_block_info)
}

/// A Subspace block slot.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Ord, PartialOrd)]
pub struct Slot(pub u64);

impl Display for Slot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl Sub<Slot> for Slot {
    type Output = u64;

    fn sub(self, rhs: Slot) -> Self::Output {
        self.0.saturating_sub(rhs.0)
    }
}

impl Sub<u64> for Slot {
    type Output = Slot;

    fn sub(self, rhs: u64) -> Self::Output {
        Slot(self.0.saturating_sub(rhs))
    }
}

impl Slot {
    /// The `PreDigest` variant we know how to parse.
    const PRE_DIGEST_VERSION: u8 = 0;
    /// The length of the slot number in bytes.
    /// <https://docs.rs/sp-consensus-slots/0.44.0/src/sp_consensus_slots/lib.rs.html#43>
    const SLOT_LEN: usize = (u64::BITS as usize) / 8;
    /// The offset of the slot number in the pre-runtime digest.
    /// SCALE enum variants are always one byte long:
    /// <https://github.com/autonomys/subspace/blob/7ce6f74032910338314c2c9b6e4a7833530467dc/crates/sp-consensus-subspace/src/digests.rs#L23>
    const SLOT_OFFSET: usize = 1;

    /// Create a new slot from a block.
    pub fn new(block: &RawBlock) -> Option<Slot> {
        for log in block.header().digest.logs.clone() {
            trace!("Checking log {:?}, looking for pre runtime digest", log);

            if let DigestItem::PreRuntime(_, pre_digest) = log {
                return Self::decode_slot_number(pre_digest);
            }
        }

        None
    }

    /// Decodes the slot number from a pre-runtime digest.
    /// See <https://github.com/autonomys/subspace/blob/7ce6f74032910338314c2c9b6e4a7833530467dc/crates/sp-consensus-subspace/src/digests.rs#L20>
    fn decode_slot_number(pre_digest: Vec<u8>) -> Option<Slot> {
        if pre_digest.is_empty() {
            warn!("pre-runtime digest is empty",);
            return None;
        }

        if pre_digest[0] != Self::PRE_DIGEST_VERSION {
            warn!(
                "unknown pre-runtime digest version: {:?} expected: {:?}",
                pre_digest[0],
                Self::PRE_DIGEST_VERSION,
            );
            return None;
        }

        let slot_bytes = pre_digest
            .into_iter()
            .skip(Self::SLOT_OFFSET)
            .take(Self::SLOT_LEN)
            .collect::<Vec<u8>>();

        let slot_bytes: [u8; Self::SLOT_LEN] = slot_bytes
            .try_into()
            .inspect_err(|digest_bytes| {
                warn!(
                    "not enough bytes for slot number in pre-runtime digest: {}",
                    hex::encode(digest_bytes)
                );
            })
            .ok()?;

        debug!(
            "Found pre runtime digest with slot number {:?}",
            hex::encode(slot_bytes)
        );
        Some(Slot(u64::from_le_bytes(slot_bytes)))
    }
}
