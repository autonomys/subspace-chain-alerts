//! Chain alerter process-specific code.
//!
//! Initializes logging, connects to a Subspace node, and runs monitoring tasks that
//! post alerts to Slack.

#![feature(assert_matches, duration_constructors_lite, formatting_options)]

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
    BlockInfo, BlockLink, BlockNumber, BlockPosition, BlockSubscription, LOCAL_SUBSPACE_NODE_URL,
    MAX_ALERTER_RELAUNCH_ATTEMPTS, MAX_RECONNECTION_DELAY, MAX_SUBSCRIPTION_RECONNECTION_ATTEMPTS,
    RawBlock, RawBlockError, RawBlockResult, RawEvent, RawEventList, RawExtrinsicList,
    RpcClientList, SubType, SubspaceClient, create_subspace_client,
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
use tokio::time::{MissedTickBehavior, interval, sleep};
use tokio::{pin, select, task};
use tracing::{debug, error, info, trace, warn};

/// The number of blocks between info-level block number logs.
/// TODO: make this configurable
const BLOCK_UPDATE_LOGGING_INTERVAL: BlockNumber = 100;

/// The number of alerts to buffer before backpressure causes the block subscriber to pause.
/// TODO: make this configurable
const ALERT_BUFFER_SIZE: usize = 100;

/// The amount of time to wait between uptime kuma status updates.
/// TODO: make this configurable
const UPTIME_KUMA_STATUS_INTERVAL: Duration = Duration::from_secs(60);

/// The amount of time to wait between block subscription watchdog checks.
///
/// As of November 2025, we see block gaps of a minute every day or so, so we set this threshold
/// much higher, to make sure it isn't triggered accidentally.
///
/// TODO: make this configurable
const BLOCK_SUBSCRIPTION_WATCHDOG_INTERVAL: Duration = Duration::from_mins(10);

/// A Slack-based alerter that runs on the Autonomys network.
// The documentation lines above are printed at the start of the `--help` message.
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

    /// The RPC URLs of the nodes to connect to, can be specified multiple times.
    /// Uses the local node by default.
    ///
    /// The primary node determines the best block and reorgs.
    /// Other nodes provide side chains only.
    #[arg(long, value_hint = ValueHint::Url, default_value = LOCAL_SUBSPACE_NODE_URL)]
    node_rpc_url: Vec<String>,

    /// The URLs of the uptime kuma servers which get uptime statuses, can be specified multiple
    /// times. Disabled by default.
    #[arg(long, value_hint = ValueHint::Url)]
    uptime_kuma_url: Vec<String>,

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
    FuturesUnordered<AsyncJoinOnDrop<(anyhow::Result<()>, String)>>,
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
    let node_rpc_url = rpc_client_list.primary_node_rpc_url().to_string();

    // A channel that shares the latest best block info with concurrently running block stall,
    // uptime, and block subscription watchdog tasks.
    let latest_best_block_tx = watch::Sender::new(None);
    let latest_best_block_rx = latest_best_block_tx.subscribe();

    let best_blocks_task = AsyncJoinOnDrop::new(
        tokio::spawn(
            run_on_best_blocks_subscription(
                best_chain_clients,
                new_blocks_tx.clone(),
                latest_best_block_tx,
                alert_tx.clone(),
            )
            .map(|result| (result, node_rpc_url)),
        ),
        true,
    );

    // Spawn tasks to send "all blocks" from node subscriptions to the fork monitor.
    let mut all_blocks_tasks = FuturesUnordered::new();
    for client_index in 0..rpc_client_list.len() {
        let node_rpc_url = rpc_client_list.node_rpc_urls()[client_index].clone();
        let all_blocks_task = AsyncJoinOnDrop::new(
            tokio::spawn(
                run_on_all_blocks_subscription(
                    rpc_client_list.clone(),
                    client_index,
                    new_blocks_tx.clone(),
                )
                .map(|result| (result, node_rpc_url)),
            ),
            true,
        );
        all_blocks_tasks.push(all_blocks_task);
    }

    // Spawn tasks to send uptime statuses to the uptime kuma servers.
    // Each task is independent, so hangs on one uptime server don't impact the others.
    let mut uptime_kuma_tasks = FuturesUnordered::new();
    let http_client = reqwest::Client::new();
    for uptime_kuma_url in args.uptime_kuma_url.iter() {
        let uptime_kuma_url = uptime_kuma_url.to_string();
        let uptime_kuma_task = AsyncJoinOnDrop::new(
            tokio::spawn(
                send_uptime_kuma_status(
                    http_client.clone(),
                    uptime_kuma_url.clone(),
                    latest_best_block_rx.clone(),
                )
                .map(|result| (result, uptime_kuma_url)),
            ),
            true,
        );
        uptime_kuma_tasks.push(uptime_kuma_task);
    }

    // Spawn a watchdog task to check the block subscriptions are working.
    let block_subscription_watchdog_task = AsyncJoinOnDrop::new(
        tokio::spawn(block_subscription_watchdog(latest_best_block_rx.clone())),
        true,
    );

    // Tasks are listed in rough data flow order.
    select! {
        // These tasks are all spawned, so tokio determines their scheduling. All this select does
        // is check the JoinHandles, to see if any tasks have finished.

        // Tasks that maintain internal library state, for example, subxt substrate metadata
        result = metadata_update_tasks.next() => {
            match result {
                Some(Ok((Ok(()), node_rpc_url))) => {
                    info!("runtime metadata update task finished for {node_rpc_url}");
                }
                Some(Ok((Err(error), node_rpc_url))) => {
                    error!(%error, "runtime metadata update task failed for {node_rpc_url}");
                }
                // TODO: if this ever happens, here or below, move the node_rpc_url outside AsyncJoinOnDrop
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
                Ok((Ok(()), node_rpc_url)) => {
                    info!("best blocks subscription exited for {node_rpc_url}");
                }
                Ok((Err(error), node_rpc_url)) => {
                    error!(%error, "best blocks subscription failed for {node_rpc_url}");
                }
                Err(error) => {
                    error!(%error, "best blocks subscription panicked or was cancelled");
                }
            }
        }
        result = all_blocks_tasks.next() => {
            match result {
                Some(Ok((Ok(()), node_rpc_url))) => {
                    info!("all blocks subscription exited for {node_rpc_url}");
                }
                Some(Ok((Err(error), node_rpc_url))) => {
                    error!(%error, "all blocks subscription failed for {node_rpc_url}");
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

        // Tasks that send uptime statuses to the uptime kuma servers, if enabled.
        // We deliberately put these tasks last, so if any other tasks starves the select! loop, the
        // alerter is considered unresponsive.
        Some(result) = uptime_kuma_tasks.next() => {
            match result {
                Ok(((), uptime_kuma_url)) => {
                    info!("uptime kuma status task finished for {uptime_kuma_url}");
                }
                Err(error) => {
                    error!(%error, "uptime kuma status task panicked or was cancelled");
                }
            }
        }

        // A task that checks the block subscriptions are working.
        result = block_subscription_watchdog_task => {
            match result {
                Ok(Ok(())) => {
                    info!("block subscription watchdog task finished");
                }
                Ok(Err(error)) => {
                    error!(%error, "block subscription watchdog task failed");
                }
                Err(error) => {
                    error!(%error, "block subscription watchdog task panicked or was cancelled");
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
) -> anyhow::Result<()> {
    let chain_client = &rpc_client_list.clients()[client_index];
    let node_rpc_url = &rpc_client_list.node_rpc_urls()[client_index];

    // Subscribe to all blocks, including side forks and best blocks.
    let mut blocks_sub = chain_client.blocks().subscribe_all().await?;
    // Tracks the number of subscription failures since we've seen any blocks.
    let mut subscription_failures = 0;

    while let Some(block) = blocks_sub.next().await {
        // Give best blocks a chance to win the subscription race.
        task::yield_now().await;

        // These errors represent a connection failure or similar. Unfortunately, this happens
        // frequently on some public servers, so we log it, and re-establish the subscription.
        let block = handle_subscription_error(
            SubType::All,
            chain_client,
            &mut blocks_sub,
            block,
            &mut subscription_failures,
            node_rpc_url,
        )
        .await?;

        let Some(block) = block else {
            // It is ok to continue when we ignore a subscription error, because the fork monitor
            // will fill in any gaps (and ignore any duplicates).
            continue;
        };
        let block = BlockLink::from_raw_block(&block);

        // Let the user know we're still alive.
        if block.height().is_multiple_of(BLOCK_UPDATE_LOGGING_INTERVAL) {
            info!(
                ?block,
                ?node_rpc_url,
                "Processed block from all blocks subscription",
            );
        }

        // Notify the fork monitor that we've seen a new block.
        let block_seen = BlockSeen::from_any_block(Arc::new(block));
        new_blocks_tx
            .send((block_seen, node_rpc_url.clone()))
            .await?;

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
    latest_best_block_tx: watch::Sender<Option<(BlockLink, bool)>>,
    alert_tx: mpsc::Sender<Alert>,
) -> anyhow::Result<()> {
    let chain_client = rpc_client_list.primary();
    let node_rpc_url = rpc_client_list.primary_node_rpc_url();
    let genesis_hash = chain_client.genesis_hash();

    // Tasks spawned for the block stall alert.
    let mut block_stall_join_handles: FuturesUnordered<JoinHandle<anyhow::Result<Option<bool>>>> =
        FuturesUnordered::new();

    // Tracks the previous block's stall alert status.
    let mut is_stalled = false;

    // Subscribe blocks that are the best block when they are received.
    let mut blocks_sub = chain_client.blocks().subscribe_best().await?;
    // Tracks the number of subscription failures since we've seen any blocks.
    let mut subscription_failures = 0;

    while let Some(block) = blocks_sub.next().await {
        // These errors represent a connection failure or similar. Unfortunately, this happens
        // frequently on some public servers, so we log it, and re-establish the subscription.
        let block = handle_subscription_error(
            SubType::Best,
            chain_client,
            &mut blocks_sub,
            block,
            &mut subscription_failures,
            node_rpc_url,
        )
        .await?;

        let Some(block) = block else {
            // It is ok to continue when we ignore a subscription error, because the fork monitor
            // will fill in any gaps (and ignore any duplicates).
            continue;
        };
        let block = BlockLink::from_raw_block(&block);

        // Let the user know we're still alive.
        if block.height().is_multiple_of(BLOCK_UPDATE_LOGGING_INTERVAL) {
            info!(
                ?block,
                ?node_rpc_url,
                "Processed block from best blocks subscription",
            );
        }

        // Notify spawned tasks that a new block has arrived, and give them time to process that
        // block. This is needed even if there is a block gap.
        latest_best_block_tx.send_replace(Some((block, is_stalled)));
        task::yield_now().await;

        // We only check for block stalls on current subscription blocks, and only if we're not
        // stalled already.
        if !is_stalled {
            let stall_task_join_handle = alerts::check_for_block_stall(
                BlockCheckMode::Current,
                block,
                &genesis_hash,
                alert_tx.clone(),
                latest_best_block_tx.subscribe(),
                node_rpc_url.to_string(),
            )
            .await;

            block_stall_join_handles.push(stall_task_join_handle);
        }

        // Notify the fork monitor that we've seen a new block.
        let block_seen = BlockSeen::from_best_block(Arc::new(block));
        new_blocks_tx
            .send((block_seen, node_rpc_url.to_string()))
            .await?;

        trace!(block_stall_join_handles = %block_stall_join_handles.len(), ?is_stalled, ?node_rpc_url, "spawned tasks before joining");

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

        trace!(block_stall_join_handles = %block_stall_join_handles.len(), ?is_stalled, ?node_rpc_url, "spawned tasks after joining");

        // We don't give tasks an opportunity to run, because we want best blocks to win the
        // subscription race.
    }

    Ok(())
}

/// Handle an error from an RPC subscription by re-establishing the subscription, unless we've
/// reached the failure limit.
async fn handle_subscription_error(
    subscription_type: SubType,
    chain_client: &SubspaceClient,
    blocks_sub: &mut BlockSubscription,
    block_result: RawBlockResult,
    subscription_failures: &mut usize,
    node_rpc_url: &str,
) -> Result<Option<RawBlock>, RawBlockError> {
    match block_result {
        Ok(block) => {
            // We've got a block without an error, so reset the subscription failure count.
            *subscription_failures = 0;

            Ok(Some(block))
        }
        Err(error) => {
            *subscription_failures += 1;

            if *subscription_failures <= MAX_SUBSCRIPTION_RECONNECTION_ATTEMPTS {
                info!(
                    %error,
                    %subscription_failures,
                    %MAX_SUBSCRIPTION_RECONNECTION_ATTEMPTS,
                    ?node_rpc_url,
                    "error receiving block from {subscription_type} subscription, waiting before re-establishing subscription...",
                );
            } else {
                error!(
                    %error,
                    %subscription_failures,
                    %MAX_SUBSCRIPTION_RECONNECTION_ATTEMPTS,
                    ?node_rpc_url,
                    "error receiving block from {subscription_type} subscription, exiting",
                );

                // Tell the caller to return a fatal error, because we've reached the failure limit.
                return Err(error);
            }

            // Wait before re-establishing the subscription for transient network issues to
            // resolve.
            tokio::time::sleep(Duration::from_millis(MAX_RECONNECTION_DELAY)).await;
            // TODO: for some errors we might have to re-create the
            // whole RPC client, in that case it's easier to just restart the entire alerter
            // task, because we also need to re-create the metadata task and
            // monitor it.
            if subscription_type.is_best() {
                *blocks_sub = chain_client.blocks().subscribe_best().await?;
            } else {
                *blocks_sub = chain_client.blocks().subscribe_all().await?;
            }

            debug!(
                %error,
                %subscription_failures,
                %MAX_SUBSCRIPTION_RECONNECTION_ATTEMPTS,
                ?node_rpc_url,
                "{subscription_type} subscription successfully re-established",
            );

            // Tell the caller to continue, because we've re-established the subscription.
            Ok(None)
        }
    }
}

/// Sends an uptime status to `uptime_kuma_url`, using the latest best block height from
/// `latest_best_block_rx` as the "ping" parameter.
///
/// Ignores any HTTPS errors, because a failing uptime server should not bring down the alerter.
async fn send_uptime_kuma_status(
    http_client: reqwest::Client,
    uptime_kuma_url: String,
    latest_best_block_rx: watch::Receiver<Option<(BlockLink, bool)>>,
) {
    let mut timer = interval(UPTIME_KUMA_STATUS_INTERVAL);
    // If there is a timer delay, after the delayed tick, wait for the complete period before
    // ticking again.
    timer.set_missed_tick_behavior(MissedTickBehavior::Delay);
    // Only issue an uptime status after the first interval (the default is immediately).
    timer.reset();

    loop {
        timer.tick().await;

        // Only borrow the watch channel contents temporarily, to avoid blocking channel updates.
        let (latest_height, is_stalled) = {
            let latest_block = latest_best_block_rx.borrow();

            let latest_height = latest_block.as_ref().map(|(block, _)| block.height());
            let is_stalled = latest_block
                .as_ref()
                .map(|(_, is_stalled)| *is_stalled)
                .unwrap_or(true);

            (latest_height, is_stalled)
        };

        // If we've failed to get a block at startup, or we're stalled, the service is down.
        let status = if is_stalled || latest_height.is_none() {
            "down"
        } else {
            "up"
        };
        let msg = if is_stalled {
            "STALL"
        } else if latest_height.is_none() {
            "STARTUP"
        } else {
            "OK"
        };
        // We use the ping parameter to smuggle the latest block height into the uptime kuma status
        // board.
        let ping = if let Some(latest_height) = latest_height {
            format!("&ping={latest_height}")
        } else {
            String::new()
        };

        let uptime_kuma_url = format!("{uptime_kuma_url}?status={status}&msg={msg}{ping}");

        if let Err(error) = http_client.get(&uptime_kuma_url).send().await {
            warn!(
                %error,
                %uptime_kuma_url,
                ?latest_height,
                %is_stalled,
                "error sending uptime status to uptime kuma",
            );
        }
    }
}

/// Checks that the best block height increases every `BLOCK_SUBSCRIPTION_WATCHDOG_INTERVAL`, using
/// the latest best block height from `latest_best_block_rx`.
///
/// Returns with an error if the block height has not increased in the last interval.
async fn block_subscription_watchdog(
    mut latest_best_block_rx: watch::Receiver<Option<(BlockLink, bool)>>,
) -> anyhow::Result<()> {
    let mut timer = interval(BLOCK_SUBSCRIPTION_WATCHDOG_INTERVAL);
    // If there is a timer delay, after the delayed tick, wait for the complete period before
    // ticking again.
    timer.set_missed_tick_behavior(MissedTickBehavior::Delay);
    // Only check after the first interval (the default is immediately).
    timer.reset();

    let mut prev_highest_position: Option<BlockPosition> = None;
    let mut highest_position: Option<BlockPosition> = None;

    loop {
        select! {
            // Check the timer first, to avoid an always-ready watch channel starving it.
            biased;

            // Every time the timer ticks, check that the block height has increased since the last interval.
            watchdog_time = timer.tick() => {
                let Some(highest) = highest_position else {
                    // We haven't seen any blocks after the first interval, so the subscription is stalled.
                    // Do an internal reset to restart the subscription.
                    return Err(anyhow::anyhow!("watchdog: block subscription never received any blocks at {watchdog_time:?}"));
                };

                // Check the block height has increased since the last interval.
                let Some(prev_highest) = prev_highest_position else {
                    info!(highest_position = ?highest, "watchdog: first interval passed successfully, updating previous highest position at {watchdog_time:?}");
                    prev_highest_position = highest_position;
                    continue;
                };

                if highest.height <= prev_highest.height {
                    return Err(anyhow::anyhow!("watchdog: block height has not increased since the last interval: current: {highest:?} previous: {prev_highest:?} at {watchdog_time:?}"));
                }

                info!(
                    highest_position = ?highest,
                    prev_highest_position = ?prev_highest,
                    "watchdog: block height has increased since the last interval at {watchdog_time:?}",
                );
                prev_highest_position = highest_position;
            }

            // Every time the watch channel changes, record the new block height, if it is higher than the previous highest block.
            // Watch channels can have spurious change notifications, so we only update the highest position if the block height has increased.
            changed_result = latest_best_block_rx.changed() => {
                let Ok(()) = changed_result else {
                    return Err(anyhow::anyhow!("watchdog: block subscription channel disconnected, possible shutdown or internal restart"));
                };

                // Only borrow the watch channel contents temporarily, to avoid blocking channel updates.
                let latest_position = latest_best_block_rx.borrow().as_ref().map(|(block, _)| block.position);

                match (latest_position, highest_position) {
                    (Some(latest), Some(highest)) => {
                        // Ignore backwards and sideways reorgs.
                        if latest.height > highest.height {
                            trace!(
                                latest_position = ?latest,
                                highest_position = ?highest,
                                "watchdog: block height has increased, updating highest position",
                            );
                            highest_position = Some(latest);
                        } else {
                            trace!(
                                latest_position = ?latest,
                                highest_position = ?highest,
                                "watchdog: block height has not increased, ignoring",
                            );
                        }
                    }
                    // First block, so update unconditionally.
                    (Some(latest), None) => {
                        trace!(
                            latest_position = ?latest,
                            "watchdog: first block received, updating highest position",
                        );
                        highest_position = Some(latest);
                    }
                    (None, Some(_highest)) => {
                        unreachable!("After the first block, the watch channel is never set to None");
                    }
                    (None, None) => {
                        // Do nothing, we haven't seen any blocks yet.
                        trace!("watchdog: no blocks received yet, waiting for first block");
                    }
                }
            }
        }
    }
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
    let max_relaunch_attempts = if args.alert_limit.is_some() || args.test_startup {
        0
    } else {
        MAX_ALERTER_RELAUNCH_ATTEMPTS
    };

    for relaunch_attempt in 0..=max_relaunch_attempts {
        select! {
            _ = &mut shutdown_handle => {
                info!(%relaunch_attempt, "chain-alerter exited due to user shutdown");
                break;
            }

            // TODO:
            // - store the most recent block and pass it to run(), so we restart at the right place
            // - create the RPC client outside this method and re-use it (but this might be more error-prone)
            result = run(&mut args) => {
                let restart_message = if relaunch_attempt < max_relaunch_attempts {
                    ", relaunching internally..."
                } else {
                    ""
                };

                if let Err(error) = result {
                    error!(
                        %error,
                        alert_limit = ?args.alert_limit,
                        %relaunch_attempt,
                        %max_relaunch_attempts,
                        "chain-alerter exited with error{restart_message}",
                    );
                } else {
                    info!(
                        alert_limit = ?args.alert_limit,
                        %relaunch_attempt,
                        %max_relaunch_attempts,
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
