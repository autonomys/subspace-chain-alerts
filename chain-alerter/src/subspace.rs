//! Subspace chain connection and block parsing code.

pub mod decode;
#[cfg(test)]
pub mod tests;

use crate::alerts::account::Accounts;
use crate::alerts::subscan::{BlockUrl, EventUrl, ExtrinsicUrl};
use crate::alerts::transfer::TransferValue;
use crate::format::{fmt_amount, fmt_fields, fmt_timestamp};
use anyhow::Result;
use chrono::{DateTime, TimeDelta, Utc};
use scale_value::Composite;
use sp_core::crypto::AccountId32;
use std::fmt::{self, Display};
use std::ops::Sub;
use std::sync::Arc;
use std::time::Duration;
use subspace_process::AsyncJoinOnDrop;
use subxt::backend::rpc::reconnecting_rpc_client::ExponentialBackoff;
use subxt::blocks::{Block, ExtrinsicDetails, Extrinsics};
use subxt::config::substrate::DigestItem;
use subxt::events::{EventDetails, Events, Phase};
use subxt::ext::subxt_rpcs::client::ReconnectingRpcClient;
use subxt::{OnlineClient, SubstrateConfig};
use tracing::{debug, info, trace, warn};

/// The placeholder hash for the parent of the genesis block.
/// Well-known value.
pub const PARENT_OF_GENESIS: BlockHash = subxt::utils::H256([0; 32]);

/// One Subspace Credit.
/// Copied from subspace-runtime-primitives.
pub const AI3: Balance = 10_u128.pow(18);

/// The target block interval, in seconds.
pub const TARGET_BLOCK_INTERVAL: u64 = 6;

/// The minumum delay between RPC reconnection attempts, in milliseconds.
pub const MIN_RECONNECTION_DELAY: u64 = 10;

/// The maximum delay between RPC reconnection attempts, in milliseconds.
/// Also used as the delay between restarts.
///
/// TODO: make this configurable
pub const MAX_RECONNECTION_DELAY: u64 = 10_000;

/// The maximum number of RPC reconnection attempts before failing and exiting the process.
/// TODO: make this configurable
pub const MAX_RECONNECTION_ATTEMPTS: usize = 10;

/// The default RPC URL for a local Subspace node.
pub const LOCAL_SUBSPACE_NODE_URL: &str = "ws://127.0.0.1:9944";

/// The RPC URL for the public Subspace Foundation RPC instance.
#[allow(dead_code, reason = "only used in tests")]
pub const FOUNDATION_SUBSPACE_NODE_URL: &str = "wss://rpc.mainnet.subspace.foundation/ws";

/// The RPC URL for the public Autonomys Labs RPC instance.
#[allow(dead_code, reason = "only used in tests")]
pub const LABS_SUBSPACE_NODE_URL: &str = "wss://rpc-0.mainnet.autonomys.xyz/ws";

/// The Subspace consensus account ID type.
pub type AccountId = AccountId32;

/// The Subspace block height type.
/// Copied from subspace-core-primitives.
pub type BlockNumber = u32;

/// The Subspace block hash type.
/// TODO: turn this into a wrapper type so we don't get it confused with other hashes.
pub type BlockHash = subxt::utils::H256;

/// The Subspace extrinsic hash type.
/// TODO: turn this into a wrapper type so we don't get it confused with other hashes.
pub type ExtrinsicHash = subxt::utils::H256;

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
/// Returns the subxt client, the raw RPC client (if it is the primary server), and a task handle
/// for the subxt metadata update task.
///
/// The metadata update task is aborted when the returned handle is dropped.
pub async fn create_subspace_client(
    node_url: impl AsRef<str>,
    is_primary: bool,
) -> Result<
    (
        SubspaceClient,
        Option<RawRpcClient>,
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

    if is_primary {
        let client = SubspaceClient::from_rpc_client(rpc.clone()).await?;
        let update_task = spawn_metadata_update_task(&client).await;

        Ok((client, Some(rpc), update_task))
    } else {
        let client = SubspaceClient::from_rpc_client(rpc).await?;
        let update_task = spawn_metadata_update_task(&client).await;

        Ok((client, None, update_task))
    }
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

/// Get the hash of the best block from the supplied node RPC.
pub async fn node_best_block_hash(raw_rpc_client: &RawRpcClient) -> anyhow::Result<BlockHash> {
    // Check if this is the best block.
    let best_block_hash = raw_rpc_client
        .request("chain_getBlockHash".to_string(), None)
        .await?
        .to_string();
    // JSON string values are quoted inside the JSON, and start with "0x".
    let Ok(best_block_hash) = serde_json::from_str::<String>(&best_block_hash) else {
        anyhow::bail!("failed to parse best block hash: {best_block_hash}");
    };
    let best_block_hash = best_block_hash
        .strip_prefix("0x")
        .unwrap_or(&best_block_hash);
    let best_block_hash: RawBlockHash = hex::decode(best_block_hash)?
        .try_into()
        .map_err(|e| anyhow::anyhow!("failed to parse best block hash: {}", hex::encode(e)))?;
    let best_block_hash = BlockHash::from(best_block_hash);

    Ok(best_block_hash)
}

/// Get a raw block from a hash, using the first chain client that succeeds.
/// Used for `BlockLink`, `BlockPosition`, and `Slot`.
/// Use `block_full_from_hash` if you need a `BlockInfo`, extrinsics, or events.
///
/// The returned `RawBlock` contains a reference to the node that provided the block.
/// This will work on non-archival nodes, because they keep a list of finalized block hashes.
/// But retrieving `BlockInfo`, extrinsics, or events from older blocks requires an archival node.
async fn raw_block_from_hash(
    block_hash: impl Into<Option<BlockHash>> + Copy,
    chain_clients: &[SubspaceClient],
) -> anyhow::Result<RawBlock> {
    let block_hash = block_hash.into();
    let mut result = None;

    for chain_client in chain_clients {
        let raw_block = if let Some(block_hash) = block_hash {
            chain_client.blocks().at(block_hash).await
        } else {
            chain_client.blocks().at_latest().await
        };

        match raw_block {
            Ok(block) => {
                result = Some(Ok(block));
                break;
            }
            Err(e) => {
                debug!(?e, "failed to get block from chain client");
                result = Some(Err(e));
            }
        }
    }

    let block = result.expect("always at least one RPC server")?;

    Ok(block)
}

/// Get a block, its extrinsics, and events from a block hash, using the first chain client that
/// succeeds.
///
/// This must be used if you want a `BlockInfo`, extrinsics, or events for a block.
pub async fn block_full_from_hash(
    block_hash: impl Into<Option<BlockHash>> + Copy,
    need_events: bool,
    chain_clients: &[SubspaceClient],
) -> anyhow::Result<(RawBlock, RawExtrinsicList, Option<RawEventList>)> {
    let mut result = None;

    for chain_client in chain_clients {
        match block_full_single_client(block_hash, need_events, chain_client).await {
            Ok(block_full) => {
                result = Some(Ok(block_full));
                break;
            }
            Err(e) => {
                debug!(?e, "failed to get block from chain client");
                result = Some(Err(e));
            }
        }
    }

    let block_full = result.expect("always at least one RPC server")?;

    Ok(block_full)
}

/// Get a block, its extrinsics, and events from a block hash, using the supplied chain client.
async fn block_full_single_client(
    block_hash: impl Into<Option<BlockHash>> + Copy,
    need_events: bool,
    chain_client: &SubspaceClient,
) -> anyhow::Result<(RawBlock, RawExtrinsicList, Option<RawEventList>)> {
    let block_hash = block_hash.into();

    let block = if let Some(block_hash) = block_hash {
        chain_client.blocks().at(block_hash).await?
    } else {
        chain_client.blocks().at_latest().await?
    };
    let extrinsics = block.extrinsics().await?;

    let events = if need_events {
        Some(block.events().await?)
    } else {
        None
    };

    Ok((block, extrinsics, events))
}

/// Block position in the chain, including height and hash.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Ord, PartialOrd, Hash)]
pub struct BlockPosition {
    /// The block number.
    pub height: BlockNumber,

    /// The block hash.
    pub hash: BlockHash,
}

impl Display for BlockPosition {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} ({:?})", self.height, self.hash)?;

        Ok(())
    }
}

impl BlockPosition {
    /// Create a new block position from a block height and hash.
    pub fn new(height: BlockNumber, hash: BlockHash) -> Self {
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
        block_hash: BlockHash,
        chain_clients: &[SubspaceClient],
    ) -> anyhow::Result<Self> {
        let block = raw_block_from_hash(block_hash, chain_clients).await?;

        Ok(Self::from_block(&block))
    }

    /// Returns the minimum possible block position for a block height.
    /// Use this for range queries.
    pub fn min_for_height_range(height: BlockNumber) -> Self {
        BlockPosition {
            height,
            hash: BlockHash::zero(),
        }
    }

    /// Returns the maximum possible block position for a block height.
    /// Use this for range queries.
    #[expect(dead_code, reason = "included for completeness")]
    pub fn max_for_height_range(height: BlockNumber) -> Self {
        BlockPosition {
            height,
            hash: BlockHash::repeat_byte(0xff),
        }
    }
}

/// Block link in the chain, including height, hash, and parent hash.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Ord, PartialOrd, Hash)]
pub struct BlockLink {
    /// The block's position.
    pub position: BlockPosition,

    /// The block's parent hash.
    pub parent_hash: BlockHash,
}

impl Display for BlockLink {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} <- {:?}", self.position, self.parent_hash)
    }
}

impl BlockLink {
    /// Create a new block link from a block position and parent hash.
    pub fn new(position: BlockPosition, parent_hash: BlockHash) -> Self {
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
    pub fn from_raw_block(block: &RawBlock) -> Self {
        Self::new(BlockPosition::from_block(block), block.header().parent_hash)
    }

    /// Create a block link, given its hash.
    pub async fn with_block_hash(
        block_hash: BlockHash,
        chain_clients: &[SubspaceClient],
    ) -> anyhow::Result<Self> {
        let block = raw_block_from_hash(block_hash, chain_clients).await?;

        Ok(Self::from_raw_block(&block))
    }

    /// Returns the block hash.
    pub fn hash(&self) -> BlockHash {
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
    /// TODO: if we add other non-Copy fields, change this to `Arc<BlockLink>`.
    pub link: BlockLink,

    /// The time extrinsic in the block, if it exists.
    /// This time is guaranteed to be monotonic by the Subspace consensus rules.
    pub chain_time: Option<BlockTime<ChainTime>>,

    /// The local time that the block was received by the alerter.
    /// This time can be out of order, particularly for replayed or startup blocks.
    pub local_time: BlockTime<LocalTime>,

    /// The block slot.
    pub slot: Option<Slot>,

    /// The genesis block hash for this network.
    pub genesis_hash: BlockHash,
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
            chain_time,
            local_time,
            slot,
            // Available via links to Subscan.
            genesis_hash: _,
        } = self;

        // Show truncated block hash, link to Subscan with full hash.
        writeln!(f, "Block: [{hash}]({}) ({height})", hash.block_url())?;
        writeln!(
            f,
            "{}",
            chain_time
                .as_ref()
                .map(|bt| bt.to_string())
                .unwrap_or_else(|| "unknown".to_string())
        )?;
        writeln!(f, "{local_time}")?;
        writeln!(
            f,
            "Slot: {}",
            slot.map(|bs| bs.to_string())
                .unwrap_or_else(|| "unknown".to_string())
        )?;

        Ok(())
    }
}

impl BlockInfo {
    /// Create a block info from a block and its extrinsics.
    pub fn new(block: &RawBlock, extrinsics: &RawExtrinsicList, genesis_hash: &BlockHash) -> Self {
        Self {
            link: BlockLink::from_raw_block(block),
            chain_time: BlockTime::new_from_extrinsics(extrinsics),
            local_time: BlockTime::new_from_local_time(),
            slot: Slot::new(block),
            genesis_hash: *genesis_hash,
        }
    }

    /// Create a block info, given its hash.
    pub async fn with_block_hash(
        block_hash: impl Into<Option<BlockHash>> + Copy,
        chain_clients: &[SubspaceClient],
    ) -> anyhow::Result<Self> {
        let (block, extrinsics, _no_events) =
            block_full_from_hash(block_hash, false, chain_clients).await?;

        Ok(Self::new(
            &block,
            &extrinsics,
            // The genesis hash is the same for all chain clients, so we use the primary client.
            &chain_clients[0].genesis_hash(),
        ))
    }

    /// Returns the block height.
    pub fn height(&self) -> BlockNumber {
        self.link.height()
    }

    /// Returns the block hash.
    pub fn hash(&self) -> BlockHash {
        self.link.hash()
    }

    /// Returns the block position.
    pub fn position(&self) -> BlockPosition {
        self.link.position
    }

    /// Returns the parent block hash.
    pub fn parent_hash(&self) -> BlockHash {
        self.link.parent_hash
    }
}

/// A marker trait for block time sources.
pub trait BlockTimeSource {}

/// A monotonic block time sourced from the blockchain.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Ord, PartialOrd, Hash)]
pub struct ChainTime;

/// A potentially out-of-order block time sourced from the alerter local clock.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Ord, PartialOrd, Hash)]
pub struct LocalTime;

impl BlockTimeSource for ChainTime {}

impl BlockTimeSource for LocalTime {}

/// A block chain time, which can be formatted in different ways.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Ord, PartialOrd)]
pub struct BlockTime<S: BlockTimeSource> {
    /// The block UNIX time (in milliseconds).
    pub unix_time: RawTime,

    /// The block time source.
    pub source: S,
}

impl<S: BlockTimeSource + fmt::Debug> Display for BlockTime<S> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{:?}: {} ({})",
            self.source,
            self.human_time(),
            self.unix_time
        )
    }
}

impl BlockTime<ChainTime> {
    /// Returns the block UNIX time extrinsic, if it exists.
    ///
    /// If the block does not have a timestamp set extrinsic, or parsing fails, returns `None`.
    pub fn new_from_extrinsics(extrinsics: &RawExtrinsicList) -> Option<Self> {
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

            return Some(BlockTime {
                unix_time,
                source: ChainTime,
            });
        }

        None
    }
}

impl BlockTime<LocalTime> {
    /// Returns the current local UNIX time (in milliseconds).
    pub fn new_from_local_time() -> Self {
        Self {
            unix_time: Utc::now()
                .timestamp_millis()
                .try_into()
                .expect("local time is always after 1970"),
            source: LocalTime,
        }
    }
}

impl<S: BlockTimeSource> BlockTime<S> {
    /// Returns the block time as a date time type.
    pub fn date_time(&self) -> Option<DateTime<Utc>> {
        // If the time is out of range, return None.
        // This should never happen for chain time due to consensus rules.
        // For local times if it happens, the alert's clock is very wrong.
        DateTime::from_timestamp_millis(i64::try_from(self.unix_time).ok()?)
    }

    /// Returns a human-readable time string.
    pub fn human_time(&self) -> String {
        fmt_timestamp(self.date_time())
    }
}

/// Calculates the local time since a block was received.
///
/// Returns a negative delta if the times are out of order, which can happen if the local clock has
/// changed.
pub fn local_time_gap_since_block(block_info: BlockInfo) -> TimeDelta {
    gap_since_time(
        Utc::now(),
        block_info
            .local_time
            .date_time()
            .expect("local time is always valid, was originally from a DateTime"),
    )
}

/// Calculates the local time gap between two blocks being received.
///
/// Returns a negative delta if the times are out of order, which can happen if the local clock has
/// changed, or the blocks were received out of order.
pub fn local_time_gap_between_blocks(
    block_info: BlockInfo,
    prev_block_info: BlockInfo,
) -> TimeDelta {
    gap_since_time(
        block_info
            .local_time
            .date_time()
            .expect("local time is always valid, was originally from a DateTime"),
        prev_block_info
            .local_time
            .date_time()
            .expect("local time is always valid, was originally from a DateTime"),
    )
}

/// Calculates the chain timestamp gap between two blocks, if both are present and have timestamps.
///
/// Returns `None` if either block info is missing, or a block is missing a timestamp.
/// Returns a negative delta if the blocks are out of order.
pub fn chain_time_gap_between_blocks(
    block_info: impl Into<Option<BlockInfo>> + Copy,
    prev_block_info: impl Into<Option<BlockInfo>> + Copy,
) -> Option<TimeDelta> {
    let block_info = block_info.into()?;
    let prev_block_info = prev_block_info.into()?;

    Some(gap_since_time(
        block_info.chain_time?.date_time()?,
        prev_block_info.chain_time?.date_time()?,
    ))
}

/// Calculates the timestamp gap between two times.
fn gap_since_time(later_time: DateTime<Utc>, earlier_time: DateTime<Utc>) -> TimeDelta {
    later_time.signed_duration_since(earlier_time)
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
    pub hash: ExtrinsicHash,

    /// The extrinsic signing address, if it exists.
    pub signing_address: Option<AccountId>,

    /// The block the extrinsic is in.
    /// TODO: if we add other non-Copy fields to BlockInfo, change this to `Arc<BlockLink>`.
    pub block: BlockLink,

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
            // Already displayed in the accounts list.
            signing_address: _,
            // Already displayed by the block info.
            block: _,
            // Too detailed for an alert, can be seen on Subscan.
            fields: _,
        } = self;

        writeln!(
            f,
            "Extrinsic {pallet}::{call}({index}) [{hash}]({})",
            hash.extrinsic_url(),
        )?;

        if let Some(transfer_value) = self.transfer_value() {
            writeln!(f, "Transfer Value: {}", fmt_amount(transfer_value))?;
        }
        if let Some(accounts) = self.accounts_str() {
            writeln!(f, "Accounts: {accounts}")?;
        }

        Ok(())
    }
}

impl ExtrinsicInfo {
    /// Check and collect an extrinsic's info.
    pub fn new(extrinsic: &RawExtrinsic, block_info: &BlockInfo) -> Option<Arc<ExtrinsicInfo>> {
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

        trace!(
            "extrinsic {}::{} (index {}): signing address: {:?}, signature: {:?}",
            meta.pallet.name(),
            meta.variant.name,
            extrinsic.index(),
            extrinsic.address_bytes().map(hex::encode),
            extrinsic.signature_bytes().map(hex::encode),
        );
        // For unsigned extrinsics, this field is `None`.
        // Strip the SCALE variant of the MultiAddress enum, because we're checking the length
        // anyway.
        let signing_address = extrinsic
            .address_bytes()
            .and_then(|addr| addr.split_at_checked(1)?.1.try_into().ok())
            .map(<[u8; 32]>::into);

        Some(Arc::new(ExtrinsicInfo {
            pallet: meta.pallet.name().to_string(),
            call: meta.variant.name.to_string(),
            index: extrinsic.index(),
            hash: extrinsic.hash(),
            signing_address,
            block: block_info.link,
            fields,
        }))
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

    /// The block the event is in.
    /// TODO: if we add other non-Copy fields to BlockInfo, change this to `Arc<BlockLink>`.
    pub block: BlockLink,

    /// The extrinsic that emitted this event, if there was one.
    pub extrinsic_info: Option<Arc<ExtrinsicInfo>>,

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
            // Already displayed by the block info.
            block: _,
            // Available via links to Subscan.
            extrinsic_info,
            fields,
        } = self;

        writeln!(f, "Event {pallet}::{kind}([{index}]({}))", self.event_url(),)?;

        if let Some(extrinsic_info) = extrinsic_info {
            // This links `ApplyExtrinsic(N)` to the extrinsic.
            writeln!(f, "Phase: [{phase:?}]({})", extrinsic_info.extrinsic_url())?;
        } else {
            writeln!(f, "Phase: {phase:?}")?;
        }

        if let Some(transfer_value) = self.transfer_value() {
            writeln!(f, "Transfer Value: {}", fmt_amount(transfer_value))?;
        }
        if let Some(accounts) = self.accounts_str() {
            writeln!(f, "Accounts: {accounts}")?;
        }

        write!(f, "{}", fmt_fields(fields))?;

        Ok(())
    }
}

impl EventInfo {
    /// Check and collect an event's info.
    pub fn new(
        event: &RawEvent,
        block_info: &BlockInfo,
        extrinsic_info: Option<Arc<ExtrinsicInfo>>,
    ) -> EventInfo {
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
            index: event.index(),
            phase: event.phase(),
            block: block_info.link,
            extrinsic_info,
            fields,
        }
    }

    /// Format the event's fields as a string, truncating it if it is too long.
    #[expect(dead_code, reason = "included for completeness")]
    pub fn fields_str(&self) -> String {
        fmt_fields(&self.fields)
    }
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

        trace!(
            "Found pre runtime digest with slot number {:?}",
            hex::encode(slot_bytes),
        );
        Some(Slot(u64::from_le_bytes(slot_bytes)))
    }
}
