//! Chain alerter process-specific code.

mod alerts;
mod format;
mod slack;
mod subspace;

use crate::slack::{SLACK_OAUTH_SECRET_PATH, SlackClientInfo};
use crate::subspace::{BlockInfo, SubspaceConfig};
use std::panic;
use std::process::exit;
use std::sync::Arc;
use subspace_process::{AsyncJoinOnDrop, init_logger, shutdown_signal};
use subxt::OnlineClient;
use tokio::select;
use tokio::sync::watch;
use tracing::{error, info};

/// Set up the chain alerter process.
async fn setup() -> anyhow::Result<(Arc<SlackClientInfo>, OnlineClient<SubspaceConfig>)> {
    // Avoid a crypto provider conflict: jsonrpsee activates ring, and hyper-rustls activates
    // aws-lc, but there can only be one per process. We use the library with more formal
    // verification.
    //
    // TODO: remove ring to reduce compile time/size
    rustls::crypto::aws_lc_rs::default_provider()
        .install_default()
        .map_err(|_| anyhow::anyhow!("Selecting default TLS crypto provider failed"))?;

    // Connect to Slack and get basic info.
    let slack_client_info = SlackClientInfo::new(SLACK_OAUTH_SECRET_PATH).await?;

    // Create a client that subscribes to a local Substrate node.
    // TODO: make URL configurable
    let chain_client = OnlineClient::<SubspaceConfig>::from_url("ws://127.0.0.1:9944").await?;

    info!("spawning runtime metadata update task...");
    // Spawn a background task to keep the runtime metadata up to date.
    // TODO: proper error handling, if an update fails we should restart the process
    // TODO: do we need to abort the process if the update task fails?
    let update_task = chain_client.updater();
    let _update_task = AsyncJoinOnDrop::new(
        tokio::spawn(async move { update_task.perform_runtime_updates().await }),
        true,
    );

    Ok((slack_client_info, chain_client))
}

/// Run the chain alerter process.
async fn run() -> anyhow::Result<()> {
    let (slack_client_info, chain_client) = setup().await?;
    // TODO: add a network name table and look up the network name by genesis hash
    let genesis_hash = chain_client.genesis_hash();

    // Tracks special actions for the first block.
    let mut first_block = true;

    // Keep the previous block's info for block to block alerts.
    let mut prev_block_info = None;
    // A channel that shares the latest block info with concurrently running tasks.
    let latest_block_tx = watch::Sender::new(None);

    // Subscribe to best blocks (before they are finalized, because finalization can take hours or days).
    // TODO: do we need to subscribe to all blocks from all forks here?
    let mut blocks_sub = chain_client.blocks().subscribe_best().await?;

    while let Some(block) = blocks_sub.next().await {
        let block = block?;
        let extrinsics = block.extrinsics().await?;
        let block_info = BlockInfo::new(&block, &extrinsics, &genesis_hash);

        // Notify spawned tasks that a new block has arrived.
        latest_block_tx.send_replace(Some(block_info.clone()));

        if first_block {
            // TODO: always post this to the test channel, because it's not an alert.
            slack_client_info
                .post_message("Launched and connected to the local node", &block_info)
                .await?;
            first_block = false;
        }

        alerts::check_for_block_stall(
            slack_client_info.clone(),
            block_info.clone(),
            latest_block_tx.subscribe(),
        )
        .await;
        alerts::check_block(&slack_client_info, &block_info, &prev_block_info).await?;

        // Extrinsic parsing should never fail, if it does, the runtime metdata is likely wrong.
        // But we don't want to panic or exit when that happens, instead we warn, and hope to
        // recover after we pick up the runtime upgrade in the next block.
        for extrinsic in extrinsics.iter() {
            alerts::check_extrinsic(&slack_client_info, &extrinsic, &block_info).await?;
        }

        prev_block_info = Some(block_info);
    }

    Ok(())
}

/// The main function, which runs the chain alerter process until Ctrl-C is pressed.
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    set_exit_on_panic();
    init_logger();
    let shutdown_fut = shutdown_signal("chain-alerter");

    select! {
        _ = shutdown_fut => {
            info!("chain-alerter exited due to user shutdown");
        }

        result = run() => {
            if let Err(e) = result {
                error!("chain-alerter exited with error: {e}");
            } else {
                info!("chain-alerter exited");
            }
        }
    }

    Ok(())
}

/// Install a panic handler which exits on panics, rather than unwinding. Unwinding can hang the
/// tokio runtime waiting for stuck tasks or threads.
///
/// TODO: move this function and its duplicates in subspace to subspace-process
pub(crate) fn set_exit_on_panic() {
    let default_panic_hook = panic::take_hook();
    panic::set_hook(Box::new(move |panic_info| {
        default_panic_hook(panic_info);
        exit(1);
    }));
}
