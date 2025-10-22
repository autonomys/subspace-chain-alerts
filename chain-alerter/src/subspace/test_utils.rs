//! Test setup and utilities for subspace node connections, blocks, extrinsics, and events.

use crate::alerts::Alert;
use crate::subspace::{
    BlockHash, BlockInfo, BlockLink, BlockNumber, BlockPosition, BlockTime, ChainTime, EventIndex,
    EventInfo, ExtrinsicIndex, ExtrinsicInfo, FOUNDATION_SUBSPACE_NODE_URL, LABS_SUBSPACE_NODE_URL,
    RawEvent, RawEventList, RawExtrinsic, RawExtrinsicList, RawTime, RpcClientList, Slot,
    block_full_from_hash,
};
use crate::{ALERT_BUFFER_SIZE, setup};
use rand::{Rng, rng};
use std::env;
use std::sync::Arc;
use subspace_process::{AsyncJoinOnDrop, init_logger};
use subxt::ext::futures::stream::FuturesUnordered;
use tokio::sync::mpsc;
use tracing::info;

/// The default RPC URL for a local Subspace node.
pub fn node_rpc_urls() -> Vec<String> {
    let mut node_urls: Vec<String> = env::var("NODE_URL").ok().into_iter().collect();

    // Randomly choose between the foundation and labs nodes.
    if rng().random_bool(0.5) {
        info!("using foundation node first");
        node_urls.extend([
            FOUNDATION_SUBSPACE_NODE_URL.to_string(),
            LABS_SUBSPACE_NODE_URL.to_string(),
        ]);
    } else {
        info!("using labs node first");
        node_urls.extend([
            LABS_SUBSPACE_NODE_URL.to_string(),
            FOUNDATION_SUBSPACE_NODE_URL.to_string(),
        ]);
    }

    info!("using node RPC URLs: {node_urls:?}");

    node_urls
}

/// Set up a test environment for subspace node connections.
///
/// Returns the runtime metadata update task, which will be aborted on drop.
///
/// This needs to be kept in sync with `main::setup()`.
///
/// TODO: make this return the same struct as `main::setup()`
pub async fn test_setup(
    node_rpc_urls: Vec<String>,
) -> anyhow::Result<(
    RpcClientList,
    mpsc::Sender<Alert>,
    mpsc::Receiver<Alert>,
    FuturesUnordered<AsyncJoinOnDrop<anyhow::Result<()>>>,
)> {
    init_logger();

    let (slack_client_info, rpc_client_list, metadata_update_tasks) =
        setup(false, false, "test".to_string(), None, node_rpc_urls).await?;

    assert!(slack_client_info.is_none(), "we didn't ask for slack");

    let (alert_tx, alert_rx) = alert_channel_only_setup();

    Ok((rpc_client_list, alert_tx, alert_rx, metadata_update_tasks))
}

/// Set up alert channels for testing.
pub fn alert_channel_only_setup() -> (mpsc::Sender<Alert>, mpsc::Receiver<Alert>) {
    let (alert_tx, alert_rx) = mpsc::channel(ALERT_BUFFER_SIZE);
    (alert_tx, alert_rx)
}

/// Fetch a block's info, extrinsics, and events, and check the expected height.
pub async fn fetch_block_info(
    block_hash: BlockHash,
    need_events: bool,
    rpc_client_list: &RpcClientList,
    expected_block_number: BlockNumber,
) -> anyhow::Result<(BlockInfo, RawExtrinsicList, Option<RawEventList>)> {
    info!(
        ?need_events,
        ?rpc_client_list,
        ?expected_block_number,
        "fetching block info for {block_hash}...",
    );
    let (raw_block, extrinsics, events) =
        block_full_from_hash(block_hash, need_events, rpc_client_list).await?;

    let block_info = BlockInfo::new(&raw_block, &extrinsics, &rpc_client_list.genesis_hash());

    assert_eq!(
        block_info.height(),
        expected_block_number,
        "unexpected block height in tests",
    );

    info!(
        ?need_events,
        ?rpc_client_list,
        "fetched block info for {block_hash} ({expected_block_number})",
    );

    Ok((block_info, extrinsics, events))
}

/// Create a mock block info for testing with a given time and slot.
pub fn mock_block_info(
    time: impl Into<Option<RawTime>> + Copy,
    slot: impl Into<Option<Slot>> + Copy,
) -> BlockInfo {
    BlockInfo {
        link: BlockLink::new(
            BlockPosition::new(100, BlockHash::zero()),
            BlockHash::zero(),
        ),
        chain_time: time.into().map(|t| BlockTime {
            unix_time: t,
            source: ChainTime,
        }),
        local_time: BlockTime::new_from_local_time(),
        slot: slot.into(),
        genesis_hash: BlockHash::zero(),
    }
}

/// Extract and decode an extrinsic from a block.
pub fn decode_extrinsic(
    block_info: &BlockInfo,
    extrinsics: &RawExtrinsicList,
    extrinsic_index: ExtrinsicIndex,
) -> anyhow::Result<(RawExtrinsic, Arc<ExtrinsicInfo>)> {
    let extrinsic = extrinsics
        .iter()
        .nth(
            extrinsic_index
                .try_into()
                .expect("ExtrinsicIndex fits in usize"),
        )
        .ok_or_else(|| anyhow::anyhow!("extrinsic not found"))?;

    let extrinsic_info = ExtrinsicInfo::new(&extrinsic, block_info)
        .ok_or_else(|| anyhow::anyhow!("extrinsic info invalid"))?;

    Ok((extrinsic, extrinsic_info))
}

/// Extract and decode an event from a block.
pub fn decode_event(
    block_info: &BlockInfo,
    extrinsic_info: Option<Arc<ExtrinsicInfo>>,
    events: &RawEventList,
    event_index: EventIndex,
) -> anyhow::Result<(RawEvent, EventInfo)> {
    // TODO: extract this into a subspace test helper function
    let event = events
        .iter()
        .nth(event_index.try_into().expect("EventIndex fits in usize"))
        .ok_or_else(|| anyhow::anyhow!("event not found"))??;

    let event_info = EventInfo::new(&event, block_info, extrinsic_info);

    Ok((event, event_info))
}
