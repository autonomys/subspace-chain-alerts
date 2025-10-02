//! Test setup for subspace node connections.

use crate::ALERT_BUFFER_SIZE;
use crate::alerts::Alert;
use crate::subspace::{
    BlockHash, BlockInfo, BlockNumber, EventIndex, EventInfo, ExtrinsicIndex, ExtrinsicInfo,
    FOUNDATION_SUBSPACE_NODE_URL, LABS_SUBSPACE_NODE_URL, RawEvent, RawEventList, RawExtrinsic,
    RawExtrinsicList, RawRpcClient, SubspaceClient, create_subspace_client,
};
use async_once::AsyncOnce;
use lazy_static::lazy_static;
use rand::{Rng, rng};
use sp_core::crypto::{Ss58AddressFormatRegistry, set_default_ss58_version};
use std::env;
use std::sync::Arc;
use subspace_process::{AsyncJoinOnDrop, init_logger};
use tokio::sync::mpsc;
use tracing::info;

/// The type of the sharedfoundation and labs subspace clients.
type SubspaceSetup = (
    SubspaceClient,
    RawRpcClient,
    Arc<AsyncJoinOnDrop<anyhow::Result<()>>>,
);

lazy_static! {
    /// Fallback Foundation Subspace Client to avoid test failures from pruned blocks.
    /// TODO: move the Arc inside AsyncJoinOnDrop and derive Clone for it
    static ref FOUNDATION_SUBSPACE_CLIENT: AsyncOnce<SubspaceSetup> = AsyncOnce::new(async {
        create_test_subspace_client(FOUNDATION_SUBSPACE_NODE_URL).await.expect("failed to create foundation subspace client")
    });

    /// Fallback Labs Subspace Client to avoid test failures from pruned blocks.
    static ref LABS_SUBSPACE_CLIENT: AsyncOnce<SubspaceSetup> = AsyncOnce::new(async {
        create_test_subspace_client(LABS_SUBSPACE_NODE_URL)
            .await
            .expect("failed to create labs subspace client")
    });
}

/// The default RPC URL for a local Subspace node.
pub fn node_rpc_url() -> String {
    let node_url = env::var("NODE_URL").unwrap_or_else(|_| {
        // Randomly choose between the foundation and labs nodes.
        if rng().random_bool(0.5) {
            info!("using foundation node");
            FOUNDATION_SUBSPACE_NODE_URL
        } else {
            info!("using labs node");
            LABS_SUBSPACE_NODE_URL
        }
        .to_string()
    });
    info!("using node RPC URL: {node_url}");

    node_url
}

/// Create a subspace client and wrap the update task in an Arc.
/// This function is required because some rust-analyzer versions don't support complex syntax in
/// lazy_static.
pub async fn create_test_subspace_client(
    node_rpc_url: impl AsRef<str>,
) -> anyhow::Result<SubspaceSetup> {
    let (client, raw_rpc_client, update_task) = create_subspace_client(node_rpc_url).await?;
    Ok((client, raw_rpc_client, Arc::new(update_task)))
}

/// Set up a test environment for subspace node connections.
///
/// Returns the runtime metadata update task, which will be aborted on drop.
///
/// This needs to be kept in sync with `main::setup()`.
///
/// # Panics
///
/// If the test setup is called more than once.
///
/// TODO: make this return the same struct as `main::setup()`
pub async fn test_setup(
    node_rpc_url: impl AsRef<str>,
) -> anyhow::Result<(
    SubspaceClient,
    RawRpcClient,
    mpsc::Sender<Alert>,
    mpsc::Receiver<Alert>,
    Arc<AsyncJoinOnDrop<anyhow::Result<()>>>,
)> {
    init_logger();

    // Display addresses in Subspace format.
    // This only applies to `sp_core::AccountId32`, not `subxt::utils::AccountId32`.
    set_default_ss58_version(Ss58AddressFormatRegistry::AutonomysAccount.into());

    // Avoid a crypto provider conflict: see main::setup() for details.
    // An error is a bug in the test, because setup should only be called once.
    rustls::crypto::aws_lc_rs::default_provider()
        .install_default()
        .expect("failed to install crypto provider, this function should only be called once");

    // Create a client that subscribes to the configured Substrate node.
    let (chain_client, raw_rpc_client, update_task) =
        if node_rpc_url.as_ref() == FOUNDATION_SUBSPACE_NODE_URL {
            let (client, raw_rpc_client, update_task) = FOUNDATION_SUBSPACE_CLIENT.get().await;
            (client.clone(), raw_rpc_client.clone(), update_task.clone())
        } else if node_rpc_url.as_ref() == LABS_SUBSPACE_NODE_URL {
            let (client, raw_rpc_client, update_task) = LABS_SUBSPACE_CLIENT.get().await;
            (client.clone(), raw_rpc_client.clone(), update_task.clone())
        } else {
            // For custom URLs we don't share the instance between tests.
            create_test_subspace_client(node_rpc_url).await?
        };

    // Create a channel to receive alerts.
    let (alert_tx, alert_rx) = alert_channel_only_setup();

    Ok((
        chain_client,
        raw_rpc_client,
        alert_tx,
        alert_rx,
        update_task,
    ))
}

/// Set up alert channels for testing.
pub fn alert_channel_only_setup() -> (mpsc::Sender<Alert>, mpsc::Receiver<Alert>) {
    let (alert_tx, alert_rx) = mpsc::channel(ALERT_BUFFER_SIZE);
    (alert_tx, alert_rx)
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
    block_hash: impl Into<Option<BlockHash>>,
    expected_block_height: impl Into<Option<BlockNumber>>,
) -> anyhow::Result<(BlockInfo, RawExtrinsicList, RawEventList)> {
    let block_hash = block_hash.into();

    // Local RPC servers are faster, but they might not have all the blocks.
    let (block_info, extrinsics, events) = if let Ok(block_extras) =
        get_block(block_hash, subspace_client).await
    {
        info!(?block_hash, "got block from original RPC server");
        block_extras
    } else if let Ok(block_extras) =
        get_block(block_hash, &FOUNDATION_SUBSPACE_CLIENT.get().await.0).await
    {
        info!(
            ?block_hash,
            "getting block from original RPC server failed, got block from fallback foundation server",
        );
        block_extras
    } else {
        info!(
            ?block_hash,
            "getting block from other servers failed, trying fallback labs RPC server",
        );
        get_block(block_hash, &LABS_SUBSPACE_CLIENT.get().await.0).await?
    };

    if let Some(expected_block_height) = expected_block_height.into() {
        assert_eq!(block_info.height(), expected_block_height);
    }

    Ok((block_info, extrinsics, events))
}

/// Get a single block using the supplied client.
async fn get_block(
    block_hash: Option<BlockHash>,
    client: &SubspaceClient,
) -> Result<(BlockInfo, RawExtrinsicList, RawEventList), subxt::Error> {
    let genesis_hash = client.genesis_hash();

    let block = if let Some(block_hash) = block_hash {
        client.blocks().at(block_hash).await?
    } else {
        client.blocks().at_latest().await?
    };

    let extrinsics = block.extrinsics().await?;
    let events = block.events().await?;

    Ok((
        BlockInfo::new(&block, &extrinsics, &genesis_hash),
        extrinsics,
        events,
    ))
}

/// Extract and decode an extrinsic from a block.
pub async fn decode_extrinsic(
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
pub async fn decode_event(
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
