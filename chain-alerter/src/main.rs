//! Chain alerter process-specific code.
//!
//! Initializes logging, connects to a Subspace node, and runs monitoring tasks that
//! post alerts to Slack.

#![feature(assert_matches)]

mod alerts;
mod chain_fork_monitor;
mod farming_monitor;
mod format;
mod slack;
mod slot_time_monitor;
mod subspace;

use crate::alerts::{Alert, BlockCheckMode};
use crate::chain_fork_monitor::{
    BlocksSeen, BlocksSeenMessage, CHAIN_FORK_BUFFER_SIZE, MAX_BLOCK_DEPTH, MAX_BLOCKS_TO_REPLAY,
    check_for_chain_forks,
};
use crate::farming_monitor::{
    DEFAULT_FARMING_INACTIVE_BLOCK_THRESHOLD, DEFAULT_FARMING_MAX_HISTORY_BLOCK_INTERVAL,
    DEFAULT_FARMING_MIN_ALERT_BLOCK_INTERVAL, DEFAULT_HIGH_END_FARMING_ALERT_THRESHOLD,
    DEFAULT_LOW_END_FARMING_ALERT_THRESHOLD, FarmingMonitor, FarmingMonitorConfig,
    MemoryFarmingMonitor,
};
use crate::slack::{SLACK_OAUTH_SECRET_PATH, SlackClientInfo};
use crate::slot_time_monitor::{
    DEFAULT_CHECK_INTERVAL, DEFAULT_SLOT_TIME_ALERT_THRESHOLD, SlotTimeMonitorConfig,
};
use crate::subspace::{
    BlockInfo, BlockNumber, Event, LOCAL_SUBSPACE_NODE_URL, MAX_RECONNECTION_ATTEMPTS,
    MAX_RECONNECTION_DELAY, RawBlockHash, RawRpcClient, SubspaceClient, SubspaceConfig,
    create_subspace_client,
};
use clap::{Parser, ValueHint};
use slot_time_monitor::{MemorySlotTimeMonitor, SlotTimeMonitor};
use std::collections::VecDeque;
use std::panic;
use std::time::Duration;
use subspace_process::{AsyncJoinOnDrop, init_logger, set_exit_on_panic, shutdown_signal};
use subxt::blocks::{Block, Extrinsics};
use subxt::ext::futures::FutureExt;
use subxt::utils::H256;
use tokio::sync::{mpsc, watch};
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
    SlackClientInfo,
    SubspaceClient,
    RawRpcClient,
    AsyncJoinOnDrop<anyhow::Result<()>>,
)> {
    // Avoid a crypto provider conflict: jsonrpsee activates ring, and hyper-rustls activates
    // aws-lc, but there can only be one per process. We use the library with more formal
    // verification.
    //
    // We expect errors here during reconnections, so we log and ignore them.
    //
    // TODO: remove ring to reduce compile time/size
    let _ = rustls::crypto::aws_lc_rs::default_provider()
        .install_default()
        .inspect_err(|_| {
            warn!(
                "Selecting default TLS crypto provider failed, this is expected during reconnections"
            )
        });

    // Connect to Slack and get basic info.
    let slack_client_info = SlackClientInfo::new(
        &args.name,
        args.icon.clone(),
        &args.node_rpc_url,
        SLACK_OAUTH_SECRET_PATH,
    )
    .await?;

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
    slack_client: SlackClientInfo,
    mut alert_rx: mpsc::Receiver<Alert>,
) -> anyhow::Result<()> {
    while let Some(alert) = alert_rx.recv().await {
        // We have a large number of retries in the Slack poster, so it is unlikely to fail.
        let response = slack_client.post_message(alert).await?;
        trace!(?response, "posted alert to Slack");
    }

    Ok(())
}

/// Run the chain alerter process.
///
/// Returns fatal errors like connection failures, but logs and ignores recoverable errors.
async fn run() -> anyhow::Result<()> {
    let args = Args::parse();

    let (slack_client_info, chain_client, raw_rpc_client, metadata_update_task) =
        setup(&args).await?;

    // Spawn a background task to post alerts to Slack.
    // We don't need to wait for the task to finish, because it will panic on failure.
    let (alert_tx, alert_rx) = mpsc::channel(ALERT_BUFFER_SIZE);
    let slack_alert_task: AsyncJoinOnDrop<anyhow::Result<()>> = AsyncJoinOnDrop::new(
        tokio::spawn(slack_poster(slack_client_info, alert_rx)),
        true,
    );

    // Chain fork monitor is used to detect chain forks and reorgs from the best and all block
    // subscriptions.
    let (new_blocks_tx, new_blocks_rx) = mpsc::channel(CHAIN_FORK_BUFFER_SIZE);
    let chain_fork_monitor_task: AsyncJoinOnDrop<()> = AsyncJoinOnDrop::new(
        tokio::spawn(check_for_chain_forks(new_blocks_rx, alert_tx.clone())),
        true,
    );

    let best_chain_client = chain_client.clone();
    let best_blocks_fut = AsyncJoinOnDrop::new(
        tokio::spawn(run_on_best_blocks_subscription(
            best_chain_client,
            alert_tx,
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

    select! {
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
        result = slack_alert_task => {
            match result {
                Ok(Ok(())) => {
                    info!("slack alert task finished");
                }
                Ok(Err(error)) => {
                    error!(%error, "slack alert task failed");
                }
                Err(error) => {
                    error!(%error, "slack alert task panicked or was cancelled");
                }
            }
        }
        result = chain_fork_monitor_task => {
            match result {
                Ok(()) => {
                    info!("chain fork monitor task finished");
                }
                Err(error) => {
                    error!(%error, "chain fork monitor task panicked or was cancelled");
                }
            }
        }
    }

    Ok(())
}

/// Run "all blocks" subscription checks.
async fn run_on_all_blocks_subscription(
    chain_client: SubspaceClient,
    raw_rpc_client: RawRpcClient,
    new_blocks_tx: mpsc::Sender<BlocksSeenMessage>,
) -> anyhow::Result<()> {
    // TODO: add a network name table and look up the network name by genesis hash
    let genesis_hash = chain_client.genesis_hash();

    // Keep the previous block's info to detect gaps, side chains, and reorgs.
    let mut prev_block_info: Option<BlockInfo> = None;

    // Subscribe to all blocks, including side forks and best blocks.
    let mut blocks_sub = chain_client.blocks().subscribe_all().await?;

    while let Some(block) = blocks_sub.next().await {
        let block = block?;
        // These errors represent a connection failure or similar, and require a restart.
        let extrinsics = block.extrinsics().await?;
        let block_info = BlockInfo::new(&block, &extrinsics, &genesis_hash);

        let best_block_hash = node_best_block_hash(&raw_rpc_client).await?;
        let is_best_block = block_info.hash() == best_block_hash;
        debug!(
            %is_best_block,
            ?best_block_hash,
            ?block_info,
            "checking if block is the current best block",
        );

        // Let the user know we're still alive.
        if block_info
            .height()
            .is_multiple_of(BLOCK_UPDATE_LOGGING_INTERVAL)
        {
            info!(%is_best_block, ?block_info, "Processed block from all blocks subscription");
        }

        if prev_block_info.is_none() {
            // Load chain fork monitor context at startup.
            load_initial_context_blocks_for_fork_monitor(
                &chain_client,
                &block_info,
                &new_blocks_tx,
            )
            .await?;
        } else if let Some(prev_block_info) = prev_block_info
            && (block_info.height() != prev_block_info.height() + 1
                || block_info.parent_hash != prev_block_info.hash())
        {
            // TODO: the check above is incorrect, it will replay more blocks than needed during
            // reorgs and new forks. Instead, use the fork monitor to detect gaps.

            // Check for a gap in the subscribed blocks.
            // "All block" subscriptions do not automatically recover missed blocks after a
            // reconnection: <https://github.com/paritytech/subxt/issues/1568>

            // Go back in the chain, find missed blocks, and replay them into the fork monitor.
            let missed_block_hashes = find_missing_blocks(
                &chain_client,
                &block_info,
                &Some(prev_block_info),
                MAX_BLOCKS_TO_REPLAY,
            )
            .await?;

            replay_previous_blocks_to_fork_monitor(
                missed_block_hashes,
                is_best_block,
                &chain_client,
                &new_blocks_tx,
            )
            .await?;
        } else {
            // Notify the fork monitor that we've seen a new block.
            let block_seen = if is_best_block {
                BlocksSeen::from_best_block(block_info)
            } else {
                BlocksSeen::from_any_block(block_info)
            };
            new_blocks_tx
                .send((BlockCheckMode::Current, block_seen))
                .await?;
        }

        prev_block_info = Some(block_info);

        // Give tasks (that are spawned by other tasks) an opportunity to run on any new blocks.
        task::yield_now().await;
    }

    Ok(())
}

/// Get the hash of the best block from the node RPCs.
pub async fn node_best_block_hash(raw_rpc_client: &RawRpcClient) -> anyhow::Result<H256> {
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
    let best_block_hash = H256::from(best_block_hash);

    Ok(best_block_hash)
}

/// At startup, load the initial context blocks for the fork monitor.
#[expect(clippy::missing_panics_doc, reason = "can't actually panic")]
pub async fn load_initial_context_blocks_for_fork_monitor(
    chain_client: &SubspaceClient,
    block_info: &BlockInfo,
    new_blocks_tx: &mpsc::Sender<BlocksSeenMessage>,
) -> anyhow::Result<()> {
    // We have to load the entire context, to avoid spurious new fork alerts on old ancestor
    // blocks.
    let missed_block_hashes = find_missing_blocks(
        chain_client,
        block_info,
        &None,
        MAX_BLOCK_DEPTH.try_into().expect("constant is small"),
    )
    .await?;

    replay_previous_blocks_to_fork_monitor(
        missed_block_hashes,
        // All initial context blocks are treated as best blocks.
        true,
        chain_client,
        new_blocks_tx,
    )
    .await?;

    Ok(())
}

/// Send missed or context blocks to the fork monitor.
/// Used when we've missed some "all blocks" subscription blocks, or at startup for context for both
/// subscriptions.
pub async fn replay_previous_blocks_to_fork_monitor(
    missed_block_hashes: VecDeque<H256>,
    is_best_block: bool,
    chain_client: &SubspaceClient,
    new_blocks_tx: &mpsc::Sender<BlocksSeenMessage>,
) -> anyhow::Result<()> {
    // Now walk forwards, sending the blocks to the fork monitor.
    let genesis_hash = chain_client.genesis_hash();
    let missed_blocks = missed_block_hashes.len();

    for missed_block_hash in missed_block_hashes {
        let block = chain_client.blocks().at(missed_block_hash).await?;
        let extrinsics = block.extrinsics().await?;
        let block_info = BlockInfo::new(&block, &extrinsics, &genesis_hash);

        // Replaying the start of the gap into the fork monitor is harmless, because it ignores
        // duplicate blocks.

        // Let the user know we're still alive.
        if block_info
            .height()
            .is_multiple_of(BLOCK_UPDATE_LOGGING_INTERVAL)
        {
            debug!(
                %is_best_block,
                %missed_blocks,
                ?block_info,
                "Replaying missed or context blocks",
            );
        }

        // Notify the fork monitor that we've seen a new block.
        // If the tip block is the best block, we consider all of its ancestors to be best blocks.
        let block_seen = if is_best_block {
            BlocksSeen::from_best_block(block_info)
        } else {
            BlocksSeen::from_any_block(block_info)
        };
        new_blocks_tx
            .send((BlockCheckMode::Replay, block_seen))
            .await?;

        // We don't spawn any tasks, so we don't need to yield here.
    }

    Ok(())
}

/// Run best blocks subscription checks.
async fn run_on_best_blocks_subscription(
    chain_client: SubspaceClient,
    alert_tx: mpsc::Sender<Alert>,
    new_blocks_tx: mpsc::Sender<BlocksSeenMessage>,
) -> anyhow::Result<()> {
    // TODO: add a network name table and look up the network name by genesis hash
    let genesis_hash = chain_client.genesis_hash();

    // Tracks special actions for the first block.
    let mut first_block = true;

    // Keep the previous block's info for block to block alerts.
    let mut prev_block_info: Option<BlockInfo> = None;
    // A channel that shares the latest block info with concurrently running tasks.
    let latest_block_tx = watch::Sender::new(None);

    // Subscribe to best blocks (before they are finalized, because finalization can take hours or
    // days).
    let mut blocks_sub = chain_client.blocks().subscribe_best().await?;

    // Slot time monitor is used to check if the slot time is within the expected range.
    let mut slot_time_monitor = MemorySlotTimeMonitor::new(SlotTimeMonitorConfig::new(
        DEFAULT_CHECK_INTERVAL,
        DEFAULT_SLOT_TIME_ALERT_THRESHOLD,
        alert_tx.clone(),
    ));

    // TODO: now that the farming monitor has a 1000 block history, it takes a long time to start
    // alerting. At startup, re-load 1000 previous blocks into its history. Disable alerts using a
    // new `BlockCheckMode::ContextOnly`.
    let mut farming_monitor = MemoryFarmingMonitor::new(&FarmingMonitorConfig {
        alert_tx: alert_tx.clone(),
        max_block_interval: DEFAULT_FARMING_MAX_HISTORY_BLOCK_INTERVAL,
        low_end_change_threshold: DEFAULT_LOW_END_FARMING_ALERT_THRESHOLD,
        high_end_change_threshold: DEFAULT_HIGH_END_FARMING_ALERT_THRESHOLD,
        inactive_block_threshold: DEFAULT_FARMING_INACTIVE_BLOCK_THRESHOLD,
        minimum_block_interval: DEFAULT_FARMING_MIN_ALERT_BLOCK_INTERVAL,
    });

    while let Some(block) = blocks_sub.next().await {
        let block = block?;
        // These errors represent a connection failure or similar, and require a restart.
        let extrinsics = block.extrinsics().await?;
        let block_info = BlockInfo::new(&block, &extrinsics, &genesis_hash);

        if first_block {
            alerts::startup_alert(BlockCheckMode::Current, &alert_tx, &block_info).await?;
            first_block = false;
        } else if block_info
            .height()
            .is_multiple_of(BLOCK_UPDATE_LOGGING_INTERVAL)
        {
            // Let the user know we're still alive.
            info!(?block_info, "Processed block from best blocks subscription");
        }

        // Notify spawned tasks that a new block has arrived, and give them time to process that
        // block. This is needed even if there is a block gap, because replaying missed blocks can
        // take a while.
        latest_block_tx.send_replace(Some(block_info));
        task::yield_now().await;

        if prev_block_info.is_none() {
            // Load chain fork monitor context at startup.
            // TODO: also replay MAX_BLOCKS_TO_REPLAY blocks to check for missed alerts
            load_initial_context_blocks_for_fork_monitor(
                &chain_client,
                &block_info,
                &new_blocks_tx,
            )
            .await?;
        } else if let Some(prev_block_info) = prev_block_info
            && (block_info.height() != prev_block_info.height() + 1
                || block_info.parent_hash != prev_block_info.hash())
        {
            // Check for a gap in the subscribed blocks.
            // Best block subscriptions do not automatically recover missed blocks after a
            // reconnection: <https://github.com/paritytech/subxt/issues/1568>

            // Go back in the chain and check missed blocks for alerts.
            let missed_block_hashes = find_missing_blocks(
                &chain_client,
                &block_info,
                &Some(prev_block_info),
                MAX_BLOCKS_TO_REPLAY,
            )
            .await?;

            replay_previous_best_blocks(
                missed_block_hashes,
                &chain_client,
                &mut slot_time_monitor,
                &mut farming_monitor,
                &new_blocks_tx,
                &alert_tx,
            )
            .await?;
        } else {
            // Run alerts on a current block, when there is no gap.

            // We only check for block stalls on current blocks.
            alerts::check_for_block_stall(
                BlockCheckMode::Current,
                alert_tx.clone(),
                block_info,
                latest_block_tx.subscribe(),
            )
            .await;

            // We notify the fork monitor after filling the gap, to avoid disconnected blocks.
            run_on_best_block(
                BlockCheckMode::Current,
                &block,
                &block_info,
                &extrinsics,
                &prev_block_info,
                &mut slot_time_monitor,
                &mut farming_monitor,
                &new_blocks_tx,
                &alert_tx,
            )
            .await?;
        }

        // Give spawned tasks another opportunity to run.
        task::yield_now().await;

        prev_block_info = Some(block_info);
    }

    Ok(())
}

/// Returns missed block hashes, starting at `block_info`, and going back through its ancestors to
/// `prev_block_info`. The number of blocks to find is limited by `max_blocks_to_find`.
#[expect(clippy::missing_panics_doc, reason = "can't actually panic")]
pub async fn find_missing_blocks(
    chain_client: &SubspaceClient,
    block_info: &BlockInfo,
    prev_block_info: &Option<BlockInfo>,
    max_blocks_to_find: u32,
) -> anyhow::Result<VecDeque<H256>> {
    // TODO: delete these logs once we have the chain fork monitor fully tested
    let (gap_start, gap_end) = if let Some(prev_block_info) = prev_block_info {
        if block_info.height() <= prev_block_info.height() {
            // Multiple blocks at the same height, a chain fork.
            debug!(
                ?max_blocks_to_find,
                "chain fork detected: {} ({}) -> {} ({}), checking skipped blocks",
                prev_block_info.height(),
                prev_block_info.hash(),
                block_info.height(),
                block_info.hash(),
            );

            // We don't know where the fork is.
            // TODO: use the chain fork detection from the chain fork monitor
            (None, block_info)
        } else {
            // A gap in the chain of blocks.
            debug!(
                ?max_blocks_to_find,
                "{} block gap detected: {} ({}) -> {} ({}), checking skipped blocks",
                block_info.height() - prev_block_info.height() - 1,
                prev_block_info.height(),
                prev_block_info.hash(),
                block_info.height(),
                block_info.hash(),
            );

            // We know the exact gap, but it could be a fork, so keep the height as well.
            (Some(prev_block_info.position), block_info)
        }
    } else {
        // No previous block, this is the initial context for the fork monitor.
        info!(
            ?max_blocks_to_find,
            "loading initial context blocks for the fork monitor, backwards from: {} ({})",
            block_info.height(),
            block_info.hash(),
        );

        (None, block_info)
    };

    // Stop at the gap start, or when we've reached the maximum number of blocks to replay.
    // We subtract 1 so we include the gap start block, this lets us use `>` in the while loop,
    // which works even if the height limit is at genesis.
    let height_limit = if let Some(gap_start) = gap_start {
        gap_start
            .height
            .max(gap_end.height().saturating_sub(max_blocks_to_find))
    } else {
        gap_end.height().saturating_sub(max_blocks_to_find)
    }
    .saturating_sub(1);

    // Walk the chain backwards, saving the block hashes.
    let mut missed_block_hashes = VecDeque::from([gap_end.hash()]);
    let mut missed_block_height = gap_end.height();

    // When we reach the gap start height, insert that block hash, then stop.
    // TODO:
    // - use the chain fork monitor to find the fork point, even if it is below the missed block
    //   height
    while missed_block_height > height_limit {
        // We don't store the full missed block info, because they could take up a lot of memory.
        let missed_block = chain_client
            .blocks()
            .at(*missed_block_hashes
                .front()
                .expect("always contains the final hash"))
            .await?;
        missed_block_hashes.push_front(missed_block.header().parent_hash);
        missed_block_height = missed_block.number() - 1;
    }

    if gap_start.map(|position| position.hash) != missed_block_hashes.front().cloned() {
        let second_fork_hash = missed_block_hashes
            .front()
            .expect("always contains the final hash");

        // The gap was a fork, we found a sibling/cousin of the gap start.
        // TODO: delete this log once the chain fork monitor has replaced other fork checking code
        debug!(
            ?max_blocks_to_find,
            ?height_limit,
            "chain fork at {:?} confirmed: {} is another fork of {} ({})",
            gap_start,
            second_fork_hash,
            gap_end.hash(),
            gap_end.height(),
        );
    }

    Ok(missed_block_hashes)
}

/// When we've missed some best blocks, re-check them, but without spawning block stall checks.
/// (The blocks have already happened, so we can only alert on stall resumes.)
pub async fn replay_previous_best_blocks(
    missed_block_hashes: VecDeque<H256>,
    chain_client: &SubspaceClient,
    slot_time_monitor: &mut MemorySlotTimeMonitor,
    farming_monitor: &mut MemoryFarmingMonitor,
    new_blocks_tx: &mpsc::Sender<BlocksSeenMessage>,
    alert_tx: &mpsc::Sender<Alert>,
) -> anyhow::Result<()> {
    // Now walk forwards, checking for alerts.
    // We skip block stall checks, because we know these blocks already have children.
    // (But we still do stall resume checks.)
    let genesis_hash = chain_client.genesis_hash();
    let mut prev_block_info: Option<BlockInfo> = None;

    for missed_block_hash in missed_block_hashes {
        let block = chain_client.blocks().at(missed_block_hash).await?;
        let extrinsics = block.extrinsics().await?;
        let block_info = BlockInfo::new(&block, &extrinsics, &genesis_hash);

        // We already checked the start of the gap, so skip checking it again.
        // (We're just using it for the previous block info.)
        if prev_block_info.is_some() {
            // Let the user know we're still alive.
            if block_info
                .height()
                .is_multiple_of(BLOCK_UPDATE_LOGGING_INTERVAL)
            {
                debug!(
                    ?block_info,
                    "Replayed missed block from best blocks subscription",
                );
            }

            run_on_best_block(
                BlockCheckMode::Replay,
                &block,
                &block_info,
                &extrinsics,
                &prev_block_info,
                slot_time_monitor,
                farming_monitor,
                new_blocks_tx,
                alert_tx,
            )
            .await?;
        }

        // We don't spawn any tasks, so we don't need to yield here.
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
    block: &Block<SubspaceConfig, SubspaceClient>,
    block_info: &BlockInfo,
    extrinsics: &Extrinsics<SubspaceConfig, SubspaceClient>,
    prev_block_info: &Option<BlockInfo>,
    slot_time_monitor: &mut MemorySlotTimeMonitor,
    farming_monitor: &mut MemoryFarmingMonitor,
    new_blocks_tx: &mpsc::Sender<BlocksSeenMessage>,
    alert_tx: &mpsc::Sender<Alert>,
) -> anyhow::Result<()> {
    // Notify the fork monitor that we've seen a new block.
    // All missed blocks are ancestors of a best block, so we consider them to be best blocks.
    new_blocks_tx
        .send((mode, BlocksSeen::from_best_block(*block_info)))
        .await?;

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
        .collect::<Vec<Event>>();

    // Check the block itself for alerts, including stall resumes.
    alerts::check_block(mode, alert_tx, block_info, prev_block_info).await?;
    slot_time_monitor.process_block(mode, block_info).await;
    farming_monitor
        .process_block(mode, block_info, &events)
        .await;

    // Check each extrinsic and event for alerts.
    for extrinsic in extrinsics.iter() {
        alerts::check_extrinsic(mode, alert_tx, &extrinsic, block_info).await?;
    }

    for event in events.iter() {
        alerts::check_event(mode, alert_tx, event, block_info).await?;
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

    let shutdown_handle = shutdown_signal("chain-alerter").fuse();
    pin!(shutdown_handle);

    for reconnection_attempt in 0..=MAX_RECONNECTION_ATTEMPTS {
        select! {
            _ = &mut shutdown_handle => {
                info!(%reconnection_attempt, "chain-alerter exited due to user shutdown");
                break;
            }

            // TODO:
            // - store the most recent block and pass it to run()
            // - create the RPC client outside this method and re-use it (but this might be more error-prone)
            result = run() => {
                if let Err(error) = result {
                    error!(
                        %reconnection_attempt,
                        %error,
                        "chain-alerter exited with error, restarting..."
                    );
                } else {
                    info!(%reconnection_attempt, "chain-alerter exited, restarting...");
                }
            }
        }

        // Wait for RPC reconnection before restarting.
        sleep(Duration::from_millis(MAX_RECONNECTION_DELAY)).await;
    }

    Ok(())
}
