//! Chain alerter process-specific code.

mod alerts;
mod format;
mod slack;
mod subspace;

use crate::alerts::Alert;
use crate::slack::{SLACK_OAUTH_SECRET_PATH, SlackClientInfo};
use crate::subspace::{
    BlockInfo, BlockNumber, LOCAL_SUBSPACE_NODE_URL, SubspaceConfig, create_subspace_client,
};
use clap::{Parser, ValueHint};
use std::sync::Arc;
use subspace_process::{AsyncJoinOnDrop, init_logger, set_exit_on_panic, shutdown_signal};
use subxt::OnlineClient;
use tokio::select;
use tokio::sync::watch;
use tracing::{error, info, warn};

/// The number of blocks between info-level block number logs.
/// TODO: make this configurable
const BLOCK_UPDATE_LOGGING_INTERVAL: BlockNumber = 100;

/// The name and emoji used by this bot instance.
#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
struct Args {
    /// The name used by the bot when posting alerts to Slack.
    #[arg(long, default_value = "Dev")]
    name: String,

    /// The Slack icon used by the bot when posting.
    ///
    /// Uses Short Names (but without the ':') from:
    /// <https://projects.iamcal.com/emoji-data/table.htm>
    #[arg(long)]
    icon: Option<String>,

    /// The RPC URL of the node to connect to.
    #[arg(long, value_hint = ValueHint::Url, default_value = LOCAL_SUBSPACE_NODE_URL)]
    node_rpc_url: String,
}

/// Set up the chain alerter process.
///
/// Any returned errors are fatal and require a restart.
async fn setup(args: Args) -> anyhow::Result<(Arc<SlackClientInfo>, OnlineClient<SubspaceConfig>)> {
    // Avoid a crypto provider conflict: jsonrpsee activates ring, and hyper-rustls activates
    // aws-lc, but there can only be one per process. We use the library with more formal
    // verification.
    //
    // TODO: remove ring to reduce compile time/size
    rustls::crypto::aws_lc_rs::default_provider()
        .install_default()
        .map_err(|_| anyhow::anyhow!("Selecting default TLS crypto provider failed"))?;

    // Connect to Slack and get basic info.
    let slack_client_info = SlackClientInfo::new(
        args.name,
        args.icon,
        &args.node_rpc_url,
        SLACK_OAUTH_SECRET_PATH,
    )
    .await?;

    // Create a client that subscribes to a local Substrate node.
    let chain_client = create_subspace_client(&args.node_rpc_url).await?;

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
///
/// Returns fatal errors like connection failures, but logs and ignores recoverable errors.
async fn run() -> anyhow::Result<()> {
    let args = Args::parse();

    let (slack_client_info, chain_client) = setup(args).await?;
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
        // These errors represent a connection failure or similar, and require a restart.
        let extrinsics = block.extrinsics().await?;
        let events = block.events().await?;
        let block_info = BlockInfo::new(&block, &extrinsics, &genesis_hash);

        // Notify spawned tasks that a new block has arrived.
        latest_block_tx.send_replace(Some(block_info.clone()));

        if first_block {
            // TODO:
            // - always post this to the test channel, because it's not an alert
            // - link to the prod channel from this message:
            //   <https://docs.slack.dev/messaging/formatting-message-text/#linking-channels>
            slack_client_info
                .post_message(Alert::Startup, &block_info)
                .await?;
            first_block = false;
        } else if block_info
            .block_height
            .is_multiple_of(BLOCK_UPDATE_LOGGING_INTERVAL)
        {
            // Let the user know we're still alive
            info!(
                "Processed block:\n\
                {block_info}"
            );
        }

        // Check for block stalls, and check the block itself for alerts.
        alerts::check_for_block_stall(
            slack_client_info.clone(),
            block_info.clone(),
            latest_block_tx.subscribe(),
        )
        .await;

        alerts::check_block(&slack_client_info, &block_info, &prev_block_info).await?;

        // Check each extrinsic and event for alerts.
        for extrinsic in extrinsics.iter() {
            alerts::check_extrinsic(&slack_client_info, &extrinsic, &block_info).await?;
        }

        for event in events.iter() {
            match event {
                Ok(event) => {
                    alerts::check_event(&slack_client_info, &event, &block_info).await?;
                }
                Err(e) => {
                    warn!("error parsing event, other events in this block have been skipped: {e}");
                }
            }
        }

        prev_block_info = Some(block_info);
    }

    Ok(())
}

/// The main function, which runs the chain alerter process until Ctrl-C is pressed.
///
/// Any returned errors are fatal and require a restart.
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
