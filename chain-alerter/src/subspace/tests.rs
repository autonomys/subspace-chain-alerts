//! Test setup for subspace node connections.

use crate::ALERT_BUFFER_SIZE;
use crate::alerts::Alert;
use crate::subspace::{
    FOUNDATION_SUBSPACE_NODE_URL, SubspaceClient, create_subspace_client,
    spawn_metadata_update_task,
};
use std::env;
use subspace_process::{AsyncJoinOnDrop, init_logger};
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
pub async fn test_setup(
    node_rpc_url: impl AsRef<str>,
) -> anyhow::Result<(
    SubspaceClient,
    mpsc::Sender<Alert>,
    mpsc::Receiver<Alert>,
    AsyncJoinOnDrop<()>,
)> {
    init_logger();

    // Avoid a crypto provider conflict: see main::setup() for details.
    // Ignore any errors, because this function can only be called once.
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();

    // Create a client that subscribes to the configured Substrate node.
    let chain_client = create_subspace_client(node_rpc_url.as_ref()).await?;

    let update_task = spawn_metadata_update_task(&chain_client).await;

    // Create a channel to receive alerts.
    let (alert_tx, alert_rx) = mpsc::channel(ALERT_BUFFER_SIZE);

    Ok((chain_client, alert_tx, alert_rx, update_task))
}
