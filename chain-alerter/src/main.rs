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
    BlockSeen, BlockSeenMessage, CHAIN_FORK_BUFFER_SIZE, NewBestBlockMessage, check_for_chain_forks,
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
    MAX_RECONNECTION_DELAY, RawEvent, RawEventList, RawExtrinsicList, RpcClientList,
    create_subspace_client,
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

    /// The RPC URLs of the node to connect to.
    /// Uses the local node by default.
    ///
    /// The primary node determines the best block and reorgs.
    /// Other nodes provide side chains only.
    #[arg(long, value_hint = ValueHint::Url, default_value = LOCAL_SUBSPACE_NODE_URL)]
    node_rpc_url: Vec<String>,

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
///
/// Returns the Slack client info, the Subspace clients, the raw RPC client (for the primary RPC
/// server), and a task handle for each metadata update task. The tasks are aborted when the
/// returned handles are dropped.
///
/// Any returned errors are fatal and require a restart.
///
/// TODO: make this return the same struct as `subspace::tests::test_setup()`
async fn setup(
    slack: bool,
    production: bool,
    name: impl AsRef<str>,
    icon: Option<String>,
    mut node_rpc_urls: Vec<String>,
) -> anyhow::Result<(
    Option<SlackClientInfo>,
    RpcClientList,
    FuturesUnordered<AsyncJoinOnDrop<anyhow::Result<()>>>,
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
            #[cfg(not(test))]
            warn!(
                "Selecting default TLS crypto provider failed, this is expected during reconnections"
            );
            #[cfg(test)]
            info!(
                "Selecting default TLS crypto provider failed, this is expected with `cargo test`"
            );
        });

    // Connect to Slack and get basic info.
    let slack_client_info = if slack {
        Some(SlackClientInfo::new(production, name, icon, SLACK_OAUTH_SECRET_PATH).await?)
    } else {
        None
    };

    let mut chain_clients = Vec::new();
    let metadata_update_tasks = FuturesUnordered::new();

    if node_rpc_urls.is_empty() {
        node_rpc_urls.push(LOCAL_SUBSPACE_NODE_URL.to_string());
    }

    let (chain_client, raw_rpc_client, metadata_update_task) =
        create_subspace_client(node_rpc_urls[0].as_str(), true).await?;

    chain_clients.push(chain_client);
    metadata_update_tasks.push(metadata_update_task);

    for node_rpc_url in node_rpc_urls.iter().skip(1) {
        let (chain_client, raw_rpc_client, metadata_update_task) =
            create_subspace_client(node_rpc_url.as_str(), false).await?;

        assert!(
            raw_rpc_client.is_none(),
            "secondary raw RPC clients are not used",
        );
        chain_clients.push(chain_client);
        metadata_update_tasks.push(metadata_update_task);
    }

    // The RPC clients must be created once, then cloned via the inner Arc, to avoid "The background
    // task closed" RPC client errors.
    let rpc_server_list = RpcClientList::new(
        chain_clients,
        node_rpc_urls,
        raw_rpc_client.expect("primary raw RPC client is always created"),
    );

    Ok((slack_client_info, rpc_server_list, metadata_update_tasks))
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
        // Give best blocks a chance to win the subscription race.
        task::yield_now().await;

        // Since we use the alert limit for testing, we always want to increment it, even if the
        // Slack alert would be duplicate or skipped. (We often disable Slack for testing.)
        alert_count += 1;

        let node_rpc_urls = alert.node_rpc_urls();

        if alert.alert.is_duplicate() {
            info!(
                %alert_count,
                ?alert_limit,
                ?test_startup,
                ?node_rpc_urls,
                "skipping posting duplicate alert message:\n{alert}",
            );
            continue;
        }

        if let Some(slack_client) = slack_client.as_ref() {
            // We have a large number of retries in the Slack poster, so it is unlikely to fail.
            let response = slack_client.post_message(&alert).await?;
            debug!(
                ?response,
                %alert_count,
                ?alert_limit,
                ?test_startup,
                ?node_rpc_urls,
                "posted alert to Slack",
            );
            task::yield_now().await;
        } else {
            info!(
                %alert_count,
                ?alert_limit,
                ?test_startup,
                ?node_rpc_urls,
                "{alert}",
            );
        }

        if let Some(alert_limit) = alert_limit
            && alert_count >= alert_limit
        {
            info!(
                %alert_count,
                ?alert_limit,
                ?test_startup,
                ?node_rpc_urls,
                "alert limit reached, exiting",
            );
            break;
        }

        if test_startup && alert.alert.is_test_alert() {
            info!(
                %alert_count,
                ?alert_limit,
                ?test_startup,
                ?node_rpc_urls,
                "startup alert reached, exiting",
            );
            break;
        }
    }

    Ok(())
}

/// Run the chain alerter process.
///
/// Returns fatal errors like connection failures, but logs and ignores recoverable errors.
async fn run(args: &mut Args) -> anyhow::Result<()> {
    info!(?args, "chain-alerter started");

    let (slack_client_info, rpc_client_list, mut metadata_update_tasks) = setup(
        args.slack,
        args.production,
        &args.name,
        args.icon.clone(),
        args.node_rpc_url.clone(),
    )
    .await?;

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
    let check_best_blocks_clients = rpc_client_list.clone();
    let check_best_blocks_task = AsyncJoinOnDrop::new(
        tokio::spawn(check_best_blocks(
            check_best_blocks_clients,
            best_fork_rx,
            alert_tx.clone(),
        )),
        true,
    );

    // Chain fork monitor is used to detect chain forks and reorgs from the best and all block
    // subscriptions, then send best block forks to be checked for alerts.
    let (new_blocks_tx, new_blocks_rx) = mpsc::channel(CHAIN_FORK_BUFFER_SIZE);
    let chain_forks_clients = rpc_client_list.clone();
    let chain_fork_monitor_task: AsyncJoinOnDrop<anyhow::Result<()>> = AsyncJoinOnDrop::new(
        tokio::spawn(check_for_chain_forks(
            chain_forks_clients,
            new_blocks_rx,
            best_fork_tx,
            alert_tx.clone(),
        )),
        true,
    );

    // Spawn a task to send best blocks from the primary node subscription to the fork monitor.
    let best_chain_clients = rpc_client_list.clone();
    let best_blocks_task = AsyncJoinOnDrop::new(
        tokio::spawn(run_on_best_blocks_subscription(
            best_chain_clients,
            new_blocks_tx.clone(),
        )),
        true,
    );

    // Spawn tasks to send "all blocks" from node subscriptions to the fork monitor.
    let mut all_blocks_tasks = FuturesUnordered::new();
    for client_index in 0..rpc_client_list.len() {
        let all_blocks_task = AsyncJoinOnDrop::new(
            tokio::spawn(run_on_all_blocks_subscription(
                rpc_client_list.clone(),
                client_index,
                new_blocks_tx.clone(),
                alert_tx.clone(),
            )),
            true,
        );
        all_blocks_tasks.push(all_blocks_task);
    }

    // Tasks are listed in rough data flow order.
    select! {
        // Make the best blocks win the subscription race, to save on best block check RPC calls.
        biased;

        // Tasks that maintain internal library state, for example, subxt substrate metadata
        result = metadata_update_tasks.next() => {
            match result {
                Some(Ok(Ok(()))) => {
                    info!("runtime metadata update task finished");
                }
                Some(Ok(Err(error))) => {
                    error!(%error, "runtime metadata update task failed");
                }
                Some(Err(error)) => {
                    error!(%error, "runtime metadata update task panicked or was cancelled");
                }
                None => {
                    unreachable!("no metadata update tasks");
                }
            }
        }

        // Tasks that get new blocks from the node(s).
        result = best_blocks_task => {
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
        result = all_blocks_tasks.next() => {
            match result {
                Some(Ok(Ok(()))) => {
                    info!("all blocks subscription exited");
                }
                Some(Ok(Err(error))) => {
                    error!(%error, "all blocks subscription failed");
                }
                Some(Err(error)) => {
                    error!(%error, "all blocks subscription panicked or was cancelled");
                }
                None => {
                    unreachable!("no all blocks tasks");
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
        result = check_best_blocks_task => {
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

/// Send blocks from an "all blocks" subscription to the fork monitor.
async fn run_on_all_blocks_subscription(
    // Avoid cloning individual clients by passing the RpcClientList and an index.
    rpc_client_list: RpcClientList,
    client_index: usize,
    new_blocks_tx: mpsc::Sender<BlockSeenMessage>,
    alert_tx: mpsc::Sender<Alert>,
) -> anyhow::Result<()> {
    let chain_client = &rpc_client_list.clients()[client_index];
    let node_rpc_url = &rpc_client_list.node_rpc_urls()[client_index];
    let genesis_hash = chain_client.genesis_hash();

    // A channel that shares the latest block info with concurrently running block stall tasks.
    let latest_block_tx = watch::Sender::new(None);

    // Tasks spawned for the block stall alert.
    let mut block_stall_join_handles: FuturesUnordered<JoinHandle<anyhow::Result<Option<bool>>>> =
        FuturesUnordered::new();

    // Tracks the previous block's stall alert status.
    let mut is_stalled = false;

    // Subscribe to all blocks, including side forks and best blocks.
    let mut blocks_sub = chain_client.blocks().subscribe_all().await?;

    while let Some(block) = blocks_sub.next().await {
        // Give best blocks a chance to win the subscription race.
        task::yield_now().await;

        // These errors represent a connection failure or similar, and require a restart.
        let block = block?;
        let block = BlockLink::from_raw_block(&block);

        // Let the user know we're still alive.
        if block.height().is_multiple_of(BLOCK_UPDATE_LOGGING_INTERVAL) {
            info!(
                ?block,
                ?node_rpc_url,
                "Processed block from all blocks subscription",
            );
        }

        // Notify spawned tasks that a new block has arrived, and give them time to process that
        // block. This is needed even if there is a block gap.
        latest_block_tx.send_replace(Some((block, is_stalled)));
        task::yield_now().await;

        // We only check for block stalls on current subscription blocks, and only if we're not
        // stalled already.
        if !is_stalled {
            let stall_task_join_handle = alerts::check_for_block_stall(
                BlockCheckMode::Current,
                block,
                &genesis_hash,
                alert_tx.clone(),
                latest_block_tx.subscribe(),
                node_rpc_url.to_string(),
            )
            .await;

            block_stall_join_handles.push(stall_task_join_handle);
        }

        // Notify the fork monitor that we've seen a new block.
        let block_seen = BlockSeen::from_any_block(Arc::new(block));
        new_blocks_tx
            .send((block_seen, node_rpc_url.clone()))
            .await?;

        trace!(block_stall_join_handles = %block_stall_join_handles.len(), ?is_stalled, "spawned tasks before joining");

        // Join any spawned block stall tasks that have finished.
        // When there are no more finished tasks, continue to the next block.
        while let Some(block_stall_result) =
            block_stall_join_handles.next().now_or_never().flatten()
        {
            // We only want to set or reset the stall status if the task issued an alert,
            // or came out of the stall.
            if let Some(block_gap_status) = block_stall_result?? {
                is_stalled = block_gap_status;
            }
        }

        trace!(block_stall_join_handles = %block_stall_join_handles.len(), ?is_stalled, "spawned tasks after joining");

        task::yield_now().await;
    }

    Ok(())
}

/// Send blocks from the "best blocks" subscription to the fork monitor.
///
/// This subscription is redundant, because we get all the best blocks from the "all blocks"
/// subscription, then the fork monitor checks if they are the best block. But subscribing to
/// best blocks improves our latency, because we can skip an RPC call to the server to check if
/// these blocks are the best block.
///
/// If small data volumes or server subscription load are more important than latency, this
/// subscription can be disabled.
async fn run_on_best_blocks_subscription(
    rpc_client_list: RpcClientList,
    new_blocks_tx: mpsc::Sender<BlockSeenMessage>,
) -> anyhow::Result<()> {
    // Subscribe blocks that are the best block when they are received.
    let mut blocks_sub = rpc_client_list.primary().blocks().subscribe_best().await?;
    let node_rpc_url = rpc_client_list.primary_node_rpc_url();

    while let Some(block) = blocks_sub.next().await {
        // These errors represent a connection failure or similar, and require a restart.
        let block = block?;
        let block = BlockLink::from_raw_block(&block);

        // Let the user know we're still alive.
        if block.height().is_multiple_of(BLOCK_UPDATE_LOGGING_INTERVAL) {
            info!(
                ?block,
                ?node_rpc_url,
                "Processed block from best blocks subscription",
            );
        }

        // Notify the fork monitor that we've seen a new block.
        let block_seen = BlockSeen::from_best_block(Arc::new(block));
        new_blocks_tx
            .send((block_seen, node_rpc_url.to_string()))
            .await?;

        // We don't give tasks an opportunity to run, because we want best blocks to win the
        // subscription race.
    }

    Ok(())
}

/// Run best block alert checks, receiving new best blocks after gap/reorg resolution from
/// `best_forks_rx`, and sending alerts to `alert_tx`.
async fn check_best_blocks(
    rpc_client_list: RpcClientList,
    mut best_forks_rx: mpsc::Receiver<NewBestBlockMessage>,
    alert_tx: mpsc::Sender<Alert>,
) -> anyhow::Result<()> {
    // TODO: add a network name table and look up the network name by genesis hash

    // Tracks special actions for the first block.
    let mut first_block = true;

    // Keep the previous block's info for block to block alerts.
    let mut prev_block_info: Option<BlockInfo> = None;

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

    while let Some((mode, block_info, raw_extrinsics, raw_events, node_rpc_url)) =
        best_forks_rx.recv().await
    {
        // Let the user know we're still alive.
        if block_info
            .height()
            .is_multiple_of(BLOCK_UPDATE_LOGGING_INTERVAL)
        {
            debug!(?block_info, "Processed block from fork monitor");
        }

        if first_block && mode.is_current() {
            alerts::startup_alert(
                mode,
                &block_info,
                &alert_tx,
                rpc_client_list.node_rpc_urls().to_vec(),
            )
            .await?;
            first_block = false;
        } else if block_info
            .height()
            .is_multiple_of(BLOCK_UPDATE_LOGGING_INTERVAL)
        {
            // Let the user know we're still alive.
            debug!(?block_info, "Processed best block from fork monitor");
        }
        // Give the best blocks subscription time to run.
        task::yield_now().await;

        // If there has been a reorg, replace the previous block with the correct (fork point)
        // block.
        if let Some(prev_block_info) = prev_block_info.as_mut()
            && prev_block_info.hash() != block_info.parent_hash()
        {
            *prev_block_info =
                BlockInfo::with_block_hash(block_info.parent_hash(), &rpc_client_list).await?;
        }

        // We check for most alerts in any mode.
        prev_block_gap_status = run_on_best_block(
            mode,
            &block_info,
            &raw_extrinsics,
            &raw_events,
            prev_block_info.as_ref(),
            prev_block_gap_status,
            &mut slot_time_monitor,
            &mut farming_monitor,
            &alert_tx,
            &node_rpc_url,
        )
        .await?;

        task::yield_now().await;

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
    block_info: &BlockInfo,
    extrinsics: &RawExtrinsicList,
    events: &RawEventList,
    prev_block_info: Option<&BlockInfo>,
    prev_block_gap_status: BlockGapAlertStatus,
    slot_time_monitor: &mut MemorySlotTimeMonitor,
    farming_monitor: &mut MemoryFarmingMonitor,
    alert_tx: &mpsc::Sender<Alert>,
    node_rpc_url: &str,
) -> anyhow::Result<BlockGapAlertStatus> {
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
        block_info,
        prev_block_info,
        prev_block_gap_status,
        alert_tx,
        node_rpc_url,
    )
    .await?;
    slot_time_monitor
        .process_block(mode, block_info, node_rpc_url)
        .await?;
    farming_monitor
        .process_block(mode, block_info, &events, node_rpc_url)
        .await?;

    // Check each extrinsic and event for alerts.
    let mut extrinsic_infos = HashMap::new();
    for extrinsic in extrinsics.iter() {
        let extrinsic_info =
            alerts::check_extrinsic(mode, &extrinsic, block_info, alert_tx, node_rpc_url).await?;
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

        alerts::check_event(
            mode,
            event,
            block_info,
            extrinsic_info,
            alert_tx,
            node_rpc_url,
        )
        .await?;
    }

    Ok(block_gap_status)
}

/// The main function, which runs the chain alerter process until Ctrl-C is pressed.
///
/// Any returned errors are fatal and require a restart.
#[allow(
    clippy::unwrap_in_result,
    reason = "expect is inside a macro, Rust nightly 2025-10-12 should ignore it but doesn't"
)]
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    set_exit_on_panic();
    init_logger();

    let shutdown_handle = shutdown_signal("chain-alerter").fuse();
    pin!(shutdown_handle);

    let mut args = Args::parse();

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
            result = run(&mut args) => {
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
