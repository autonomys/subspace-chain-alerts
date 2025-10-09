//! Chain alerter process-specific code.
//!
//! Initializes logging, connects to a Subspace node, and runs monitoring tasks that
//! post alerts to Slack.

#![feature(assert_matches, formatting_options)]

mod alerts;
mod chain_fork_monitor;
mod farming_monitor;
mod format;
mod slack;
mod slot_time_monitor;
mod subspace;

use crate::alerts::{Alert, BlockCheckMode, BlockGapAlertStatus};
use crate::chain_fork_monitor::{
    BlockSeen, CHAIN_FORK_BUFFER_SIZE, NewBestBlockMessage, check_for_chain_forks,
};
use crate::farming_monitor::{
    DEFAULT_FARMING_INACTIVE_BLOCK_THRESHOLD, DEFAULT_FARMING_MAX_HISTORY_BLOCK_INTERVAL,
    DEFAULT_FARMING_MIN_ALERT_BLOCK_INTERVAL, DEFAULT_HIGH_END_FARMING_ALERT_THRESHOLD,
    DEFAULT_LOW_END_FARMING_ALERT_THRESHOLD, FarmingMonitor, FarmingMonitorConfig,
    MemoryFarmingMonitor,
};
use crate::slack::{SLACK_OAUTH_SECRET_PATH, SlackClientInfo};
use crate::slot_time_monitor::{
    DEFAULT_CHECK_INTERVAL, DEFAULT_FAST_SLOTS_THRESHOLD, DEFAULT_MAX_BLOCK_BUFFER,
    DEFAULT_SLOW_SLOTS_THRESHOLD, SlotTimeMonitorConfig,
};
use crate::subspace::{
    BlockInfo, BlockLink, BlockNumber, LOCAL_SUBSPACE_NODE_URL, MAX_RECONNECTION_ATTEMPTS,
    MAX_RECONNECTION_DELAY, RawBlock, RawEvent, RawExtrinsicList, RawRpcClient, SubspaceClient,
    create_subspace_client, node_best_block_hash,
};
use clap::{ArgAction, Parser, ValueHint};
use slot_time_monitor::{MemorySlotTimeMonitor, SlotTimeMonitor};
use sp_core::crypto::{Ss58AddressFormatRegistry, set_default_ss58_version};
use std::collections::HashMap;
use std::panic;
use std::sync::Arc;
use std::time::Duration;
use subspace_process::{AsyncJoinOnDrop, init_logger, set_exit_on_panic, shutdown_signal};
use subxt::events::Phase;
use subxt::ext::futures::stream::FuturesUnordered;
use subxt::ext::futures::{FutureExt, StreamExt};
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;
use tokio::time::sleep;
use tokio::{pin, select, task};
use tracing::{debug, error, info, trace, warn};

/// The number of blocks between info-level block number logs.
/// TODO: make this configurable
const BLOCK_UPDATE_LOGGING_INTERVAL: BlockNumber = 100;

/// The number of alerts to buffer before backpressure causes the block subscriber to pause.
/// TODO: make this configurable
const ALERT_BUFFER_SIZE: usize = 100;

/// The name and emoji used by this bot instance.
#[derive(Parser, Debug)]
#[command(version, about, long_about)]
struct Args {
    /// The Slack icon used by the bot when posting.
    ///
    /// Uses Short Names (but without the ':') from:
    /// <https://projects.iamcal.com/emoji-data/table.htm>
    ///
    /// Uses the approximate country code of the instance external IP address by default.
    #[arg(long)]
    icon: Option<String>,

    /// The name used by the bot when posting alerts to Slack.
    #[arg(long, default_value = "Dev")]
    name: String,

    /// The RPC URL of the node to connect to.
    /// Uses the local node by default.
    #[arg(long, value_hint = ValueHint::Url, default_value = LOCAL_SUBSPACE_NODE_URL)]
    node_rpc_url: String,

    /// Send alerts to the production Slack channel.
    #[arg(long, alias = "prod", default_value = "false", action = ArgAction::SetTrue)]
    production: bool,

    // Integration testing options
    /// Exit after this many alerts have been posted. Mainly used for testing.
    /// Default is no limit.
    #[arg(long)]
    alert_limit: Option<usize>,

    /// Exit after the startup alert, even if other alerts fired during the initial context load.
    #[arg(long)]
    test_startup: bool,

    /// Enable or disable Slack message posting.
    /// Slack is enabled by default, and required a Slack OAuth secret file named `slack-secret`.
    #[arg(long, default_value = "true", action = ArgAction::Set)]
    slack: bool,
}

/// Initialize once-off setup for the chain alerter.
/// Returns the Slack client info, the Subspace client, the raw RPC client, and a task handle for
/// the metadata update task. The task is aborted when the returned handle is dropped.
///
/// Any returned errors are fatal and require a restart.
///
/// This needs to be kept in sync with `subspace::tests::test_setup()`.
///
/// TODO: make this return the same struct as `subspace::tests::test_setup()`
async fn setup(
    args: &Args,
) -> anyhow::Result<(
    Option<SlackClientInfo>,
    SubspaceClient,
    RawRpcClient,
    AsyncJoinOnDrop<anyhow::Result<()>>,
)> {
    // Display addresses in Subspace format.
    // This only applies to `sp_core::AccountId32`, not `subxt::utils::AccountId32`.
    set_default_ss58_version(Ss58AddressFormatRegistry::AutonomysAccount.into());

    // Avoid a crypto provider conflict: jsonrpsee activates ring, and hyper-rustls activates
    // aws-lc, but there can only be one per process. We use the library with more formal
    // verification.
    //
    // We expect errors here during reconnections, so we log and ignore them.
    //
    // TODO: remove ring to reduce compile time/size
    let _: Result<(), Arc<rustls::crypto::CryptoProvider>> = rustls::crypto::aws_lc_rs::default_provider()
        .install_default()
        .inspect_err(|_| {
            warn!(
                "Selecting default TLS crypto provider failed, this is expected during reconnections"
            )
        });

    // Connect to Slack and get basic info.
    let slack_client_info = if args.slack {
        Some(
            SlackClientInfo::new(
                args.production,
                &args.name,
                args.icon.clone(),
                &args.node_rpc_url,
                SLACK_OAUTH_SECRET_PATH,
            )
            .await?,
        )
    } else {
        None
    };

    let (chain_client, raw_rpc_client, metadata_update_task) =
        create_subspace_client(&args.node_rpc_url).await?;

    Ok((
        slack_client_info,
        chain_client,
        raw_rpc_client,
        metadata_update_task,
    ))
}

/// Receives alerts on a channel and posts them to Slack.
/// This task might pause if the Slack API rate limit is exceeded.
async fn slack_poster(
    slack_client: Option<SlackClientInfo>,
    alert_limit: Option<usize>,
    test_startup: bool,
    mut alert_rx: mpsc::Receiver<Alert>,
) -> anyhow::Result<()> {
    if slack_client.is_none() {
        warn!(
            ?alert_limit,
            ?test_startup,
            "slack posting is disabled, only posting alerts to the terminal",
        );
    }

    if alert_limit == Some(0) {
        info!(?test_startup, "alert limit is zero, exiting immediately");
        return Ok(());
    }

    let mut alert_count = 0;

    while let Some(alert) = alert_rx.recv().await {
        // Since we use the alert limit for testing, we always want to increment it, even if the
        // Slack alert would be duplicate or skipped. (We often disable Slack for testing.)
        alert_count += 1;

        if alert.alert.is_duplicate() {
            info!(%alert_count, ?alert_limit, ?test_startup, "skipping posting duplicate alert message:\n{alert}");
            continue;
        }

        if let Some(slack_client) = slack_client.as_ref() {
            // We have a large number of retries in the Slack poster, so it is unlikely to fail.
            let response = slack_client.post_message(&alert).await?;
            debug!(?response, %alert_count, ?alert_limit, ?test_startup, "posted alert to Slack");
        } else {
            info!(%alert_count, ?alert_limit, ?test_startup, "{alert}");
        }

        if let Some(alert_limit) = alert_limit
            && alert_count >= alert_limit
        {
            info!(%alert_count, ?alert_limit, ?test_startup, "alert limit reached, exiting");
            break;
        }

        if test_startup && alert.alert.is_test_alert() {
            info!(%alert_count, ?alert_limit, ?test_startup, "startup alert reached, exiting");
            break;
        }
    }

    Ok(())
}

/// Run the chain alerter process.
///
/// Returns fatal errors like connection failures, but logs and ignores recoverable errors.
async fn run(args: &Args) -> anyhow::Result<()> {
    info!(?args, "chain-alerter started");

    let (slack_client_info, chain_client, raw_rpc_client, metadata_update_task) =
        setup(args).await?;

    // Spawn a background task to post alerts to Slack.
    // We don't need to wait for the task to finish, because it will panic on failure.
    let (alert_tx, alert_rx) = mpsc::channel(ALERT_BUFFER_SIZE);
    let slack_alert_task: AsyncJoinOnDrop<anyhow::Result<()>> = AsyncJoinOnDrop::new(
        tokio::spawn(slack_poster(
            slack_client_info,
            args.alert_limit,
            args.test_startup,
            alert_rx,
        )),
        true,
    );

    // Spawn a task to check best block forks for alerts.
    let (best_fork_tx, best_fork_rx) = mpsc::channel(CHAIN_FORK_BUFFER_SIZE);
    let check_best_blocks_client = chain_client.clone();
    let check_best_blocks_fut = AsyncJoinOnDrop::new(
        tokio::spawn(check_best_blocks(
            check_best_blocks_client,
            best_fork_rx,
            alert_tx.clone(),
        )),
        true,
    );

    // Chain fork monitor is used to detect chain forks and reorgs from the best and all block
    // subscriptions, then send best block forks to be checked for alerts.
    let (new_blocks_tx, new_blocks_rx) = mpsc::channel(CHAIN_FORK_BUFFER_SIZE);
    let chain_forks_client = chain_client.clone();
    let chain_fork_monitor_task: AsyncJoinOnDrop<anyhow::Result<()>> = AsyncJoinOnDrop::new(
        tokio::spawn(check_for_chain_forks(
            chain_forks_client,
            new_blocks_rx,
            best_fork_tx,
            alert_tx,
        )),
        true,
    );

    // Spawn tasks to send blocks from the node subscriptions to the fork monitor.
    let best_chain_client = chain_client.clone();
    let best_blocks_fut = AsyncJoinOnDrop::new(
        tokio::spawn(run_on_best_blocks_subscription(
            best_chain_client,
            new_blocks_tx.clone(),
        )),
        true,
    );

    let all_blocks_fut = AsyncJoinOnDrop::new(
        tokio::spawn(run_on_all_blocks_subscription(
            chain_client,
            raw_rpc_client,
            new_blocks_tx.clone(),
        )),
        true,
    );

    // Tasks are listed in rough data flow order.
    select! {
    // Tasks that maintain internal library state, for example, subxt substrate metadata
           result = metadata_update_task => {
               match result {
                   Ok(Ok(())) => {
                       info!("runtime metadata update task finished");
                   }
                   Ok(Err(error)) => {
                       error!(%error, "runtime metadata update task failed");
                   }
                   Err(error) => {
                       error!(%error, "runtime metadata update task panicked or was cancelled");
                   }
               }
           }

           // Tasks that get new blocks from the node.
           result = best_blocks_fut => {
               match result {
                   Ok(Ok(())) => {
                       info!("best blocks subscription exited");
                   }
                   Ok(Err(error)) => {
                       error!(%error, "best blocks subscription failed");
                   }
                   Err(error) => {
                       error!(%error, "best blocks subscription panicked or was cancelled");
                   }
               }
           }
           result = all_blocks_fut => {
               match result {
                   Ok(Ok(())) => {
                       info!("all blocks subscription exited");
                   }
                   Ok(Err(error)) => {
                       error!(%error, "all blocks subscription failed");
                   }
                   Err(error) => {
                       error!(%error, "all blocks subscription panicked or was cancelled");
                   }
               }
           }

           // A task that detects missing blocks, chain forks, and reorgs.
           result = chain_fork_monitor_task => {
               match result {
                   Ok(Ok(())) => {
                       info!("chain fork monitor task finished");
                   }
                   Ok(Err(error)) => {
                       error!(%error, "chain fork monitor task failed");
                   }
                   Err(error) => {
                       error!(%error, "chain fork monitor task panicked or was cancelled");
                   }
               }
           }

           // A task that checks best blocks for alerts, after gap/reorg resolution.
           result = check_best_blocks_fut => {
               match result {
                   Ok(Ok(())) => {
                       info!("best block check task finished");
                   }
                   Ok(Err(error)) => {
                       error!(%error, "best blocks check task failed");
                   }
                   Err(error) => {
                       error!(%error, "best blocks check task panicked or was cancelled");
                   }
               }
           }

           // A task that posts alerts to Slack, if enabled.
           // If Slack is disabled, the task is spawned, but only prints alerts to the terminal.
           result = slack_alert_task => {
               match result {
                   Ok(Ok(())) => {
                       info!(slack_enabled = %args.slack, "slack alert task finished");
                   }
                   Ok(Err(error)) => {
                       error!(%error, slack_enabled = %args.slack, "slack alert task failed");
                   }
                   Err(error) => {
                       error!(%error, slack_enabled = %args.slack, "slack alert task panicked or was cancelled");
                   }
               }
           }
       }

    Ok(())
}

/// Send blocks from the "all blocks" subscription to the fork monitor.
async fn run_on_all_blocks_subscription(
    chain_client: SubspaceClient,
    raw_rpc_client: RawRpcClient,
    new_blocks_tx: mpsc::Sender<BlockSeen>,
) -> anyhow::Result<()> {
    // Subscribe to all blocks, including side forks and best blocks.
    let mut blocks_sub = chain_client.blocks().subscribe_all().await?;

    while let Some(block) = blocks_sub.next().await {
        // These errors represent a connection failure or similar, and require a restart.
        let block = block?;
        let block = BlockLink::from_block(&block);

        let best_block_hash = node_best_block_hash(&raw_rpc_client).await?;
        let is_best_block = block.hash() == best_block_hash;
        debug!(
            %is_best_block,
            ?best_block_hash,
            ?block,
            "checking if block is the current best block",
        );

        // Let the user know we're still alive.
        if block.height().is_multiple_of(BLOCK_UPDATE_LOGGING_INTERVAL) {
            info!(%is_best_block, ?block, "Processed block from all blocks subscription");
        }

        // Notify the fork monitor that we've seen a new block.
        let block_seen = if is_best_block {
            BlockSeen::from_best_block(Arc::new(block))
        } else {
            BlockSeen::from_any_block(Arc::new(block))
        };
        new_blocks_tx.send(block_seen).await?;

        // Give tasks (that are spawned by other tasks) an opportunity to run on any new blocks.
        task::yield_now().await;
    }

    Ok(())
}

/// Send blocks from the "best blocks" subscription to the fork monitor.
async fn run_on_best_blocks_subscription(
    chain_client: SubspaceClient,
    new_blocks_tx: mpsc::Sender<BlockSeen>,
) -> anyhow::Result<()> {
    // Subscribe blocks that are the best block when they are received.
    let mut blocks_sub = chain_client.blocks().subscribe_best().await?;

    while let Some(block) = blocks_sub.next().await {
        // These errors represent a connection failure or similar, and require a restart.
        let block = block?;
        let block = BlockLink::from_block(&block);

        // Let the user know we're still alive.
        if block.height().is_multiple_of(BLOCK_UPDATE_LOGGING_INTERVAL) {
            info!(?block, "Processed block from best blocks subscription");
        }

        // Notify the fork monitor that we've seen a new block.
        let block_seen = BlockSeen::from_best_block(Arc::new(block));
        new_blocks_tx.send(block_seen).await?;

        // Give tasks (that are spawned by other tasks) an opportunity to run on any new blocks.
        task::yield_now().await;
    }

    Ok(())
}

/// Run best block alert checks, receiving new best blocks after gap/reorg resolution from
/// `best_forks_rx`, and sending alerts to `alert_tx`.
async fn check_best_blocks(
    chain_client: SubspaceClient,
    mut best_forks_rx: mpsc::Receiver<NewBestBlockMessage>,
    alert_tx: mpsc::Sender<Alert>,
) -> anyhow::Result<()> {
    // TODO: add a network name table and look up the network name by genesis hash

    // Tracks special actions for the first block.
    let mut first_block = true;

    // Keep the previous block's info for block to block alerts.
    let mut prev_block_info: Option<BlockInfo> = None;
    // A channel that shares the latest block info with concurrently running tasks.
    let latest_block_tx = watch::Sender::new(None);

    // Tracks the previous block's gap alert status.
    let mut prev_block_gap_status = BlockGapAlertStatus::NoAlert;

    // Slot time monitor is used to check if the slot time is within the expected range.
    let mut slot_time_monitor = MemorySlotTimeMonitor::new(SlotTimeMonitorConfig::new(
        DEFAULT_CHECK_INTERVAL,
        DEFAULT_MAX_BLOCK_BUFFER,
        DEFAULT_SLOW_SLOTS_THRESHOLD,
        DEFAULT_FAST_SLOTS_THRESHOLD,
        alert_tx.clone(),
    ));

    // TODO: now that the farming monitor has a 1000 block history, it takes a long time to start
    // alerting. At startup, re-load DEFAULT_FARMING_MIN_ALERT_BLOCK_INTERVAL previous blocks into
    // its history. Disable alerts using a new `BlockCheckMode::ContextOnly`.
    let mut farming_monitor = MemoryFarmingMonitor::new(&FarmingMonitorConfig {
        alert_tx: alert_tx.clone(),
        max_block_interval: DEFAULT_FARMING_MAX_HISTORY_BLOCK_INTERVAL,
        low_end_change_threshold: DEFAULT_LOW_END_FARMING_ALERT_THRESHOLD,
        high_end_change_threshold: DEFAULT_HIGH_END_FARMING_ALERT_THRESHOLD,
        inactive_block_threshold: DEFAULT_FARMING_INACTIVE_BLOCK_THRESHOLD,
        minimum_block_interval: DEFAULT_FARMING_MIN_ALERT_BLOCK_INTERVAL,
    });

    // Tasks spawned by the block stall alert.
    let mut block_stall_join_handles: FuturesUnordered<
        JoinHandle<anyhow::Result<Option<BlockGapAlertStatus>>>,
    > = FuturesUnordered::new();

    while let Some((mode, raw_block, raw_extrinsics, block_info)) = best_forks_rx.recv().await {
        // Let the user know we're still alive.
        if block_info
            .height()
            .is_multiple_of(BLOCK_UPDATE_LOGGING_INTERVAL)
        {
            debug!(?block_info, "Processed block from fork monitor");
        }

        if first_block && mode.is_current() {
            alerts::startup_alert(mode, &alert_tx, &block_info).await?;
            first_block = false;
        } else if block_info
            .height()
            .is_multiple_of(BLOCK_UPDATE_LOGGING_INTERVAL)
        {
            // Let the user know we're still alive.
            debug!(?block_info, "Processed best block from fork monitor");
        }

        // Notify spawned tasks that a new block has arrived, and give them time to process that
        // block. This is needed even if there is a block gap.
        latest_block_tx.send_replace(Some((block_info, prev_block_gap_status)));
        task::yield_now().await;

        // If there has been a reorg, replace the previous block with the correct (fork point)
        // block.
        if let Some(prev_block_info) = prev_block_info.as_mut()
            && prev_block_info.hash() != block_info.parent_hash()
        {
            *prev_block_info =
                BlockInfo::with_block_hash(block_info.parent_hash(), &chain_client).await?;
        }

        // We only check for block stalls on current blocks.
        if mode.is_current() {
            let stall_task_join_handle = alerts::check_for_block_stall(
                mode,
                alert_tx.clone(),
                block_info,
                latest_block_tx.subscribe(),
            )
            .await;

            block_stall_join_handles.push(stall_task_join_handle);
        }

        // We check for other alerts in any mode.
        prev_block_gap_status = run_on_best_block(
            mode,
            &raw_block,
            &block_info,
            &raw_extrinsics,
            prev_block_info.as_ref(),
            prev_block_gap_status,
            &mut slot_time_monitor,
            &mut farming_monitor,
            &alert_tx,
        )
        .await?;

        // Give spawned tasks another opportunity to run.
        task::yield_now().await;

        trace!(block_stall_join_handles = %block_stall_join_handles.len(), ?prev_block_gap_status, "spawned tasks before joining");

        // Join any spawned block stall tasks that have finished.
        // When there are no more finished tasks, continue to the next block.
        while let Some(block_stall_result) =
            block_stall_join_handles.next().now_or_never().flatten()
        {
            // We only want to reset the previous block gap status if the task actually issued an
            // alert. In the absence of an alert, we can't assume there were no gaps (due to the
            // slop). The most reliable indicators of a reset are the block gap checks in
            // check_block().
            if let Some(block_gap_status) = block_stall_result?? {
                prev_block_gap_status = block_gap_status;
            }
        }

        trace!(block_stall_join_handles = %block_stall_join_handles.len(), ?prev_block_gap_status, "spawned tasks after joining");

        prev_block_info = Some(block_info);
    }

    Ok(())
}

#[expect(
    clippy::too_many_arguments,
    reason = "TODO: move some of these arguments into a struct"
)]
/// Run checks on a single block, against its previous block.
async fn run_on_best_block(
    mode: BlockCheckMode,
    block: &RawBlock,
    block_info: &BlockInfo,
    extrinsics: &RawExtrinsicList,
    prev_block_info: Option<&BlockInfo>,
    prev_block_gap_status: BlockGapAlertStatus,
    slot_time_monitor: &mut MemorySlotTimeMonitor,
    farming_monitor: &mut MemoryFarmingMonitor,
    alert_tx: &mpsc::Sender<Alert>,
) -> anyhow::Result<BlockGapAlertStatus> {
    let events = block.events().await?;
    let events = events
        .iter()
        .filter_map(|event_result| {
            event_result
                .inspect_err(|e| {
                    warn!(
                        ?mode,
                        "error parsing event, other events in this block have been skipped: {e}"
                    );
                })
                .ok()
        })
        .collect::<Vec<RawEvent>>();

    // Check the block itself for alerts, including stall resumes.
    let block_gap_status = alerts::check_block(
        mode,
        alert_tx,
        block_info,
        prev_block_info,
        prev_block_gap_status,
    )
    .await?;
    slot_time_monitor.process_block(mode, block_info).await?;
    farming_monitor
        .process_block(mode, block_info, &events)
        .await?;

    // Check each extrinsic and event for alerts.
    let mut extrinsic_infos = HashMap::new();
    for extrinsic in extrinsics.iter() {
        let extrinsic_info =
            alerts::check_extrinsic(mode, alert_tx, &extrinsic, block_info).await?;
        if let Some(extrinsic_info) = extrinsic_info {
            extrinsic_infos.insert(extrinsic.index(), extrinsic_info);
        }
    }

    for event in events.iter() {
        let extrinsic_info = if let Phase::ApplyExtrinsic(extrinsic_index) = event.phase() {
            extrinsic_infos.get(&extrinsic_index).cloned()
        } else {
            None
        };

        alerts::check_event(mode, alert_tx, event, block_info, extrinsic_info).await?;
    }

    Ok(block_gap_status)
}

/// The main function, which runs the chain alerter process until Ctrl-C is pressed.
///
/// Any returned errors are fatal and require a restart.
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    set_exit_on_panic();
    init_logger();

    let shutdown_handle = shutdown_signal("chain-alerter").fuse();
    pin!(shutdown_handle);

    let args = Args::parse();

    // If we have an alert limit, we don't want to restart when it is reached.
    let max_reconnection_attempts = if args.alert_limit.is_some() || args.test_startup {
        0
    } else {
        MAX_RECONNECTION_ATTEMPTS
    };

    for reconnection_attempt in 0..=max_reconnection_attempts {
        select! {
            _ = &mut shutdown_handle => {
                info!(%reconnection_attempt, "chain-alerter exited due to user shutdown");
                break;
            }

            // TODO:
            // - store the most recent block and pass it to run(), so we restart at the right place
            // - create the RPC client outside this method and re-use it (but this might be more error-prone)
            result = run(&args) => {
                let restart_message = if reconnection_attempt < max_reconnection_attempts {
                    ", restarting..."
                } else {
                    ""
                };

                if let Err(error) = result {
                    error!(
                        %error,
                        alert_limit = ?args.alert_limit,
                        %reconnection_attempt,
                        %max_reconnection_attempts,
                        "chain-alerter exited with error{restart_message}",
                    );
                } else {
                    info!(
                        alert_limit = ?args.alert_limit,
                        %reconnection_attempt,
                        %max_reconnection_attempts,
                        "chain-alerter exited{restart_message}",
                    );
                }
            }
        }

        // Wait for RPC reconnection before restarting.
        sleep(Duration::from_millis(MAX_RECONNECTION_DELAY)).await;
    }

    Ok(())
}
