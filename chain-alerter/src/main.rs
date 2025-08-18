//! Chain alerter process-specific code.
//!
//! Initializes logging, connects to a Subspace node, and runs monitoring tasks that
//! post alerts to Slack.

#![feature(assert_matches)]

mod alerts;
mod format;
mod slack;
mod slot_time_monitor;
mod subspace;

use crate::alerts::Alert;
use crate::slack::{SLACK_OAUTH_SECRET_PATH, SlackClientInfo};
use crate::slot_time_monitor::{
    DEFAULT_CHECK_INTERVAL, DEFAULT_SLOT_TIME_ALERT_THRESHOLD, SlotTimeMonitorConfig,
};
use crate::subspace::{
    BlockInfo, BlockNumber, LOCAL_SUBSPACE_NODE_URL, SubspaceClient, SubspaceConfig,
    create_subspace_client, spawn_metadata_update_task,
};
use clap::{Parser, ValueHint};
use slot_time_monitor::{MemorySlotTimeMonitor, SlotTimeMonitor};
use std::collections::VecDeque;
use std::panic;
use subspace_process::{AsyncJoinOnDrop, init_logger, set_exit_on_panic, shutdown_signal};
use subxt::blocks::{Block, Extrinsics};
use tokio::sync::{mpsc, watch};
use tokio::{select, task};
use tracing::{error, info, warn};

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

/// Set up the chain alerter process.
/// The metadata update task is aborted when the returned handle is dropped.
///
/// Any returned errors are fatal and require a restart.
///
/// This needs to be kept in sync with `subspace::tests::test_setup()`.
async fn setup(
    args: Args,
) -> anyhow::Result<(SlackClientInfo, SubspaceClient, AsyncJoinOnDrop<()>)> {
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

    // Create a client that subscribes to the configured Substrate node.
    let chain_client = create_subspace_client(&args.node_rpc_url).await?;

    let update_task = spawn_metadata_update_task(&chain_client).await;

    Ok((slack_client_info, chain_client, update_task))
}

/// Receives alerts on a channel and posts them to Slack.
/// This task might pause if the Slack API rate limit is exceeded.
async fn slack_poster(slack_client: SlackClientInfo, mut alert_rx: mpsc::Receiver<Alert>) {
    while let Some(alert) = alert_rx.recv().await {
        // We have a large number of retries in the Slack poster, so it is unlikely to fail.
        slack_client
            .post_message(alert)
            .await
            .expect("Slack message failures require a restart");
    }
}

/// Run the chain alerter process.
///
/// Returns fatal errors like connection failures, but logs and ignores recoverable errors.
async fn run() -> anyhow::Result<()> {
    let args = Args::parse();

    let (slack_client_info, chain_client, _metadata_update_task) = setup(args).await?;

    // Spawn a background task to post alerts to Slack.
    // We don't need to wait for the task to finish, because it will panic on failure.
    let (alert_tx, alert_rx) = mpsc::channel(ALERT_BUFFER_SIZE);
    let _alert_task = AsyncJoinOnDrop::new(
        tokio::spawn(slack_poster(slack_client_info, alert_rx)),
        true,
    );

    // TODO: add a network name table and look up the network name by genesis hash
    let genesis_hash = chain_client.genesis_hash();

    // Tracks special actions for the first block.
    let mut first_block = true;

    // Keep the previous block's info for block to block alerts.
    let mut prev_block_info: Option<BlockInfo> = None;
    // A channel that shares the latest block info with concurrently running tasks.
    let latest_block_tx = watch::Sender::new(None);

    // Subscribe to best blocks (before they are finalized, because finalization can take hours or
    // days). TODO: do we need to subscribe to all blocks from all forks here?
    let mut blocks_sub = chain_client.blocks().subscribe_best().await?;

    // Slot time monitor is used to check if the slot time is within the expected range.
    let mut slot_time_monitor = MemorySlotTimeMonitor::new(SlotTimeMonitorConfig::new(
        DEFAULT_CHECK_INTERVAL,
        DEFAULT_SLOT_TIME_ALERT_THRESHOLD,
        alert_tx.clone(),
    ));

    while let Some(block) = blocks_sub.next().await {
        let block = block?;
        // These errors represent a connection failure or similar, and require a restart.
        let extrinsics = block.extrinsics().await?;
        let block_info = BlockInfo::new(&block, &extrinsics, &genesis_hash);

        if first_block {
            alerts::startup_alert(&alert_tx, &block_info).await?;
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

        // Check for a gap in the subscribed blocks.
        if let Some(prev_block_info) = prev_block_info
            && block_info.block_height != prev_block_info.block_height + 1
        {
            // Go back in the chain and check missed blocks for alerts.
            replay_previous_blocks(
                &chain_client,
                &block_info,
                &prev_block_info,
                &latest_block_tx,
                &mut slot_time_monitor,
                &alert_tx,
            )
            .await?;
        } else {
            // Only run alerts on a block if there is no gap.
            run_on_block(
                &block,
                &block_info,
                &extrinsics,
                &prev_block_info,
                &latest_block_tx,
                &mut slot_time_monitor,
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

/// When we've missed some blocks, re-check them, but without spawning block stall checks.
/// (The blocks have already happened, so we can only alert on stall resumes.)
#[expect(clippy::missing_panics_doc, reason = "can't actually panic")]
pub async fn replay_previous_blocks(
    chain_client: &SubspaceClient,
    block_info: &BlockInfo,
    prev_block_info: &BlockInfo,
    latest_block_tx: &watch::Sender<Option<BlockInfo>>,
    slot_time_monitor: &mut MemorySlotTimeMonitor,
    alert_tx: &mpsc::Sender<Alert>,
) -> anyhow::Result<()> {
    let genesis_hash = block_info.genesis_hash;

    let (gap_start, gap_end) = if block_info.block_height <= prev_block_info.block_height {
        // Multiple blocks at the same height, a chain fork.
        info!(
            "chain fork detected: {} ({}) -> {} ({})\n\
            checking skipped blocks",
            prev_block_info.block_height,
            prev_block_info.block_hash,
            block_info.block_height,
            block_info.block_hash,
        );

        // For now, just assume the fork is at the previous block.
        // TODO: add proper chain fork detection and alerts
        (
            // TODO: turn this into a struct
            (block_info.parent_hash, block_info.block_height - 1),
            block_info,
        )
    } else {
        // A gap in the chain of blocks.
        warn!(
            "{} block gap detected: {} ({}) -> {} ({})\n\
            checking skipped blocks",
            block_info.block_height - prev_block_info.block_height - 1,
            prev_block_info.block_height,
            prev_block_info.block_hash,
            block_info.block_height,
            block_info.block_hash,
        );

        // We know the exact gap, but it could be a fork, so keep the height as well.
        (
            (prev_block_info.block_hash, prev_block_info.block_height),
            block_info,
        )
    };

    // Walk the chain backwards, saving the block hashes.
    let mut missed_block_hashes = VecDeque::from([gap_end.block_hash]);
    let mut missed_block_height = gap_end.block_height;

    // When we reach the gap start height, insert that block hash, then stop.
    while gap_start.1 < missed_block_height {
        // We don't store the missed blocks because they could take up a lot of memory.
        let missed_block = chain_client
            .blocks()
            .at(*missed_block_hashes
                .front()
                .expect("always contains the final hash"))
            .await?;
        missed_block_hashes.push_front(missed_block.header().parent_hash);
        missed_block_height = missed_block.number() - 1;
    }

    if Some(&gap_start.0) != missed_block_hashes.front() {
        // The gap was a fork, we found a sibling/cousin of the gap start.
        info!(
            "chain fork at {} confirmed: {} is a fork of {} -> {} ({})",
            gap_start.1,
            missed_block_hashes
                .front()
                .expect("always contains the final hash"),
            gap_start.0,
            gap_end.block_hash,
            gap_end.block_height,
        );
    }

    // Now walk forwards, checking for alerts.
    // We skip block stall checks, because we know these blocks already have children.
    // (But we still do stall resume checks.)
    let mut prev_block_info: Option<BlockInfo> = None;

    for missed_block_hash in missed_block_hashes {
        let block = chain_client.blocks().at(missed_block_hash).await?;
        let extrinsics = block.extrinsics().await?;
        let block_info = BlockInfo::new(&block, &extrinsics, &genesis_hash);

        // We already checked the start of the gap, so skip checking it again.
        // (We're just using it for the previous block info.)
        if prev_block_info.is_some() {
            if block_info
                .block_height
                .is_multiple_of(BLOCK_UPDATE_LOGGING_INTERVAL)
            {
                // Let the user know we're still alive
                info!(
                    "Replayed missed block:\n\
                    {block_info}"
                );
            }

            run_on_block(
                &block,
                &block_info,
                &extrinsics,
                &prev_block_info,
                latest_block_tx,
                slot_time_monitor,
                alert_tx,
            )
            .await?;
        }

        // We don't spawn any tasks, so we don't need to yield here.
        prev_block_info = Some(block_info);
    }

    Ok(())
}

/// Run checks on a single block, against its previous block.
pub async fn run_on_block(
    block: &Block<SubspaceConfig, SubspaceClient>,
    block_info: &BlockInfo,
    extrinsics: &Extrinsics<SubspaceConfig, SubspaceClient>,
    prev_block_info: &Option<BlockInfo>,
    latest_block_tx: &watch::Sender<Option<BlockInfo>>,
    slot_time_monitor: &mut MemorySlotTimeMonitor,
    alert_tx: &mpsc::Sender<Alert>,
) -> anyhow::Result<()> {
    let events = block.events().await?;

    // Notify spawned tasks that a new block has arrived, and give them time to process that
    // block.
    latest_block_tx.send_replace(Some(*block_info));
    task::yield_now().await;

    // Check for block stalls, and check the block itself for alerts.
    alerts::check_for_block_stall(alert_tx.clone(), *block_info, latest_block_tx.subscribe()).await;

    alerts::check_block(alert_tx, block_info, prev_block_info).await?;
    slot_time_monitor.process_block(block_info).await;

    // Check each extrinsic and event for alerts.
    for extrinsic in extrinsics.iter() {
        alerts::check_extrinsic(alert_tx, &extrinsic, block_info).await?;
    }

    for event in events.iter() {
        match event {
            Ok(event) => {
                alerts::check_event(alert_tx, &event, block_info).await?;
            }
            Err(e) => {
                warn!("error parsing event, other events in this block have been skipped: {e}");
            }
        }
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
