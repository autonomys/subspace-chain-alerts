//! Test setup for subspace node connections.

use crate::ALERT_BUFFER_SIZE;
use crate::alerts::Alert;
use crate::subspace::{
    BlockInfo, BlockNumber, Event, EventIndex, EventInfo, ExtrinsicIndex, ExtrinsicInfo,
    FOUNDATION_SUBSPACE_NODE_URL, RawRpcClient, SubspaceClient, SubspaceConfig,
    create_subspace_client,
};
use std::env;
use subspace_process::{AsyncJoinOnDrop, init_logger};
use subxt::blocks::{ExtrinsicDetails, Extrinsics};
use subxt::events::Events;
use subxt::utils::H256;
use tokio::sync::mpsc;
use tracing::info;

/// The default RPC URL for a local Subspace node.
pub fn node_rpc_url() -> String {
    let node_url =
        env::var("NODE_URL").unwrap_or_else(|_| FOUNDATION_SUBSPACE_NODE_URL.to_string());
    info!("using node RPC URL: {node_url}");

    node_url
}

/// Set up a test environment for subspace node connections.
///
/// Returns the runtime metadata update task, which will be aborted on drop.
///
/// This needs to be kept in sync with `main::setup()`.
///
/// TODO: make this return the same struct as `main::setup()`
pub async fn test_setup(
    node_rpc_url: impl AsRef<str>,
) -> anyhow::Result<(
    SubspaceClient,
    RawRpcClient,
    mpsc::Sender<Alert>,
    mpsc::Receiver<Alert>,
    AsyncJoinOnDrop<anyhow::Result<()>>,
)> {
    init_logger();

    // Avoid a crypto provider conflict: see main::setup() for details.
    // Ignore any errors, because this function can only be called once.
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();

    // Create a client that subscribes to the configured Substrate node.
    let (chain_client, raw_rpc_client, update_task) =
        create_subspace_client(node_rpc_url.as_ref()).await?;

    // Create a channel to receive alerts.
    let (alert_tx, alert_rx) = mpsc::channel(ALERT_BUFFER_SIZE);

    Ok((
        chain_client,
        raw_rpc_client,
        alert_tx,
        alert_rx,
        update_task,
    ))
}

/// Get the block info, extrinsics, and events for a block.
///
/// If `block_hash` is provided, it will be used to get the block, otherwise it gets the latest
/// block. If `expected_block_height` is provided, it will be used to check that the block height is
/// correct, otherwise it will not be checked.
///
/// TODO:
/// - make this return a struct
/// - add a method that takes a raw RPC client, and uses it to fetch the block info by block height
pub async fn fetch_block_info(
    subspace_client: &SubspaceClient,
    block_hash: impl Into<Option<H256>>,
    expected_block_height: impl Into<Option<BlockNumber>>,
) -> anyhow::Result<(
    BlockInfo,
    Extrinsics<SubspaceConfig, SubspaceClient>,
    Events<SubspaceConfig>,
)> {
    let block = if let Some(block_hash) = block_hash.into() {
        subspace_client.blocks().at(block_hash).await?
    } else {
        subspace_client.blocks().at_latest().await?
    };

    let extrinsics = block.extrinsics().await?;
    let events = block.events().await?;
    let genesis_hash = subspace_client.genesis_hash();
    let block_info = BlockInfo::new(&block, &extrinsics, &genesis_hash);

    if let Some(expected_block_height) = expected_block_height.into() {
        assert_eq!(block_info.height(), expected_block_height);
    }

    Ok((block_info, extrinsics, events))
}

/// Extract and decode an extrinsic from a block.
pub async fn decode_extrinsic(
    block_info: &BlockInfo,
    extrinsics: &Extrinsics<SubspaceConfig, SubspaceClient>,
    extrinsic_index: ExtrinsicIndex,
) -> anyhow::Result<(
    ExtrinsicDetails<SubspaceConfig, SubspaceClient>,
    ExtrinsicInfo,
)> {
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
pub async fn decode_event(
    block_info: &BlockInfo,
    events: &Events<SubspaceConfig>,
    event_index: EventIndex,
) -> anyhow::Result<(Event, EventInfo)> {
    // TODO: extract this into a subspace test helper function
    let event = events
        .iter()
        .nth(event_index.try_into().expect("EventIndex fits in usize"))
        .ok_or_else(|| anyhow::anyhow!("event not found"))??;

    let event_info = EventInfo::new(&event, block_info);

    Ok((event, event_info))
}
