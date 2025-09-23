//! Specific chain alerts.

#[cfg(test)]
mod tests;
mod transfer;

use crate::alerts::transfer::{check_balance_event, check_balance_extrinsic};
use crate::chain_fork_monitor::ChainForkEvent;
use crate::format::{fmt_amount, fmt_duration, fmt_timestamp};
use crate::subspace::{
    AI3, Balance, BlockInfo, BlockPosition, BlockTime, EventInfo, ExtrinsicInfo, RawEvent,
    RawExtrinsic, TARGET_BLOCK_INTERVAL, gap_since_last_block, gap_since_time,
};
use chrono::Utc;
use std::fmt::{self, Display};
use std::time::Duration;
use tokio::sync::{mpsc, watch};
use tokio::time::sleep;
use tracing::warn;

/// The minimum balance change to alert on.
const MIN_BALANCE_CHANGE: Balance = 1_000_000 * AI3;

/// The minimum gap between block timestamps to alert on.
/// The target block gap is 6 seconds, so we alert if it takes substantially longer.
///
/// `pallet-timestamp` enforces a `MinimumPeriod` of 3 seconds in Subspace, and a
/// `MAX_TIMESTAMP_DRIFT_MILLIS` of 30 seconds from each node's local clock.
/// <https://github.com/paritytech/polkadot-sdk/blob/0034d178fff88a0fd87cf0ec1d8f122ae0011d78/substrate/frame/timestamp/src/lib.rs#L307>
const MIN_BLOCK_GAP: Duration = Duration::from_secs(TARGET_BLOCK_INTERVAL * 10);

/// The amount of time to add/subtract from the minimum block gap to account for consensus clock
/// drift, async timer delays, and similar inaccuracies.
const BLOCK_GAP_SLOP: Duration = Duration::from_secs(1);

/// Whether we are replaying missed blocks, or checking current blocks.
/// This impacts block stall checks, which can only be spawned on new blocks.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum BlockCheckMode {
    /// We are checking current blocks, which have just arrived in a subscription from the node.
    /// All alerts are checked on current blocks.
    Current,

    /// We are replaying missed blocks, which are the ancestors of current blocks.
    /// Almost all alerts are checked on replayed blocks, except for block stalls.
    Replay,

    /// We are providing context at startup for checks that require a lot of historic blocks.
    /// Most alerts are checked on startup blocks.
    Startup,
}

impl BlockCheckMode {
    /// Whether we are checking current blocks.
    pub fn is_current(&self) -> bool {
        match self {
            BlockCheckMode::Current => true,
            BlockCheckMode::Replay | BlockCheckMode::Startup => false,
        }
    }

    /// Whether we are checking replayed blocks.
    #[expect(dead_code, reason = "included for completeness")]
    pub fn is_replay(&self) -> bool {
        !self.is_current()
    }

    /// Whether we are checking startup blocks.
    pub fn is_startup(&self) -> bool {
        match self {
            BlockCheckMode::Current | BlockCheckMode::Replay => false,
            BlockCheckMode::Startup => true,
        }
    }

    /// Returns this block check mode, modified for replaying blocks.
    pub fn during_replay(&self) -> Self {
        match self {
            // Blocks can't be current during a replay.
            BlockCheckMode::Current => BlockCheckMode::Replay,
            BlockCheckMode::Replay | BlockCheckMode::Startup => *self,
        }
    }
}

/// A blockchain alert with context.
#[derive(Clone, Debug, PartialEq)]
pub struct Alert {
    /// The type of alert.
    pub alert: AlertKind,

    /// The block the alert occurred in.
    pub block_info: BlockInfo,

    /// The mode the alert was triggered in.
    pub mode: BlockCheckMode,
}

impl Alert {
    /// Create a new alert.
    pub fn new(alert: AlertKind, block_info: BlockInfo, mode: BlockCheckMode) -> Self {
        Self {
            alert,
            block_info,
            mode,
        }
    }

    /// Create a new alert from a chain fork event.
    pub fn from_chain_fork_event(
        event: ChainForkEvent,
        block_info: BlockInfo,
        mode: BlockCheckMode,
    ) -> Self {
        let backwards_reorg_depth = event.backwards_reorg_depth();

        let alert_kind = match event {
            // The new block is always the same as block_info, so we ignore it.
            ChainForkEvent::NewSideFork { tip: _, fork_depth } => {
                AlertKind::NewSideFork { fork_depth }
            }
            ChainForkEvent::SideForkExtended { tip: _, fork_depth } => {
                AlertKind::SideForkExtended { fork_depth }
            }
            ChainForkEvent::Reorg {
                new_best_block: _,
                old_best_block,
                old_fork_depth,
                new_fork_depth,
            } => AlertKind::Reorg {
                old_best_block: old_best_block.position,
                old_fork_depth,
                new_fork_depth,
                backwards_reorg_depth,
            },
        };

        Self::new(alert_kind, block_info, mode)
    }
}

/// The type of alert.
#[derive(Clone, Debug, PartialEq)]
pub enum AlertKind {
    /// The alerter has started.
    Startup,

    /// Block production has stalled.
    BlockProductionStall {
        /// The gap between the previous block and now.
        ///
        /// Note: the previous block is `Alert.block_info`.
        gap: Option<Duration>,
    },

    /// Block production has resumed.
    BlockProductionResumed {
        /// The gap between the previous and current block.
        ///
        /// Note: the current block is `Alert.block_info`.
        gap: Duration,

        /// The previous block.
        prev_block_info: BlockInfo,
    },

    /// A new chain fork was seen, which was not started by a best block.
    /// The tip of the fork is `Alert.block_info`.
    NewSideFork {
        /// The number of blocks from the fork tip to the fork point.
        fork_depth: usize,
    },

    /// A chain fork was extended by a non-best block.
    /// The tip of the fork is `Alert.block_info`.
    SideForkExtended {
        /// The number of blocks from the fork tip to the fork point.
        fork_depth: usize,
    },

    /// A reorg was seen to a best block on a side chain.
    /// This takes priority over fork events.
    /// The new best block is `Alert.block_info`.
    Reorg {
        /// The old best block.
        old_best_block: BlockPosition,

        /// The number of blocks from the old best block to the reorg point (the fork point with
        /// the new best block).
        old_fork_depth: usize,

        /// The number of blocks from the new best block to the reorg point.
        new_fork_depth: usize,

        /// The number of blocks that the reorg went backwards by.
        backwards_reorg_depth: Option<usize>,
    },

    /// A `force_*` Balances call has been detected.
    ForceBalanceTransfer {
        /// The Balance call's extrinsic information.
        extrinsic_info: ExtrinsicInfo,

        /// The transfer value.
        transfer_value: Option<Balance>,
    },

    /// A large Balance transfer extrinsic has been detected.
    LargeBalanceTransfer {
        /// The Balance call's extrinsic information.
        extrinsic_info: ExtrinsicInfo,

        /// The transfer value.
        transfer_value: Balance,
    },

    /// A large Balance transfer event has been detected.
    LargeBalanceTransferEvent {
        /// The Balance event information.
        event_info: EventInfo,

        /// The transfer value.
        transfer_value: Balance,
    },

    /// A Sudo call has been detected.
    SudoCall {
        /// The sudo call's extrinsic information.
        extrinsic_info: ExtrinsicInfo,
    },

    /// A Sudo call event has been detected.
    SudoEvent {
        /// The sudo event information.
        event_info: EventInfo,
    },

    /// An operator slash event has been detected.
    OperatorSlashed {
        /// The operator slash event information.
        event_info: EventInfo,
    },

    /// Slot timing is outside the expected range.
    SlotTime {
        /// The current ratio of slots to time.
        current_ratio: f64,

        /// The applicable threshold for this alert.
        threshold: f64,

        /// The duration of the interval.
        interval: Duration,

        /// The time of the first slot in the interval.
        first_slot_time: BlockTime,
    },

    /// Farmers count has decreased suddenly.
    FarmersDecreasedSuddenly {
        /// The number of farmers with votes.
        number_of_farmers_with_votes: usize,

        /// The average number of farmers with votes.
        average_number_of_farmers_with_votes: f64,

        /// The number of blocks in the interval.
        number_of_blocks: u32,
    },

    /// Farmers count has increased suddenly.
    FarmersIncreasedSuddenly {
        /// The number of farmers with votes.
        number_of_farmers_with_votes: usize,

        /// The average number of farmers with votes.
        average_number_of_farmers_with_votes: f64,

        /// The number of blocks in the interval.
        number_of_blocks: u32,
    },
}

impl Display for AlertKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AlertKind::Startup => {
                write!(f, "**Launched and connected to the node**")
            }

            AlertKind::BlockProductionStall { gap } => {
                write!(
                    f,
                    "**Block production stalled**\n\
                    Time since last best block: {}",
                    fmt_duration(*gap),
                )
            }

            AlertKind::BlockProductionResumed {
                gap,
                prev_block_info,
            } => {
                write!(
                    f,
                    "**Block production resumed**\n\
                    Gap: {}\n\n\
                    Previous best block:\n\
                    {prev_block_info}",
                    fmt_duration(*gap),
                )
            }

            AlertKind::NewSideFork { fork_depth } => {
                write!(
                    f,
                    "**New side chain fork detected**\n\
                    Fork depth: {fork_depth}",
                )
            }

            AlertKind::SideForkExtended { fork_depth } => {
                write!(
                    f,
                    "**Side chain fork extended**\n\
                    Fork depth: {fork_depth}",
                )
            }

            AlertKind::Reorg {
                old_best_block,
                old_fork_depth,
                new_fork_depth,
                backwards_reorg_depth,
            } => {
                write!(
                    f,
                    "**{}Reorg detected**\n\
                    New fork depth: {new_fork_depth}\n\
                    Old fork depth: {old_fork_depth}\n\
                    Old best block: {old_best_block}",
                    if backwards_reorg_depth.is_some() {
                        "Backwards "
                    } else {
                        ""
                    },
                )?;

                if let Some(backwards_reorg_depth) = backwards_reorg_depth {
                    write!(f, "\nBackwards reorg depth: {backwards_reorg_depth}")?;
                }

                Ok(())
            }

            AlertKind::ForceBalanceTransfer {
                extrinsic_info,
                transfer_value,
            } => {
                write!(
                    f,
                    "**Force Balances call detected**\n\
                    Transfer value: {}\n\
                    {extrinsic_info}",
                    fmt_amount(*transfer_value),
                )
            }

            AlertKind::LargeBalanceTransfer {
                extrinsic_info,
                transfer_value,
            } => {
                write!(
                    f,
                    "**Large Balances call detected**\n\
                    Transfer value: {} (above {})\n\
                    {extrinsic_info}",
                    fmt_amount(*transfer_value),
                    fmt_amount(MIN_BALANCE_CHANGE),
                )
            }

            AlertKind::LargeBalanceTransferEvent {
                event_info,
                transfer_value,
            } => {
                write!(
                    f,
                    "**Large Balances event detected**\n\
                    Transfer value: {} (above {})\n\
                    {event_info}",
                    fmt_amount(*transfer_value),
                    fmt_amount(MIN_BALANCE_CHANGE),
                )
            }

            AlertKind::SudoCall { extrinsic_info } => {
                write!(
                    f,
                    "**Sudo call detected**\n\
                    {extrinsic_info}",
                )
            }

            AlertKind::SudoEvent { event_info } => {
                write!(
                    f,
                    "**Sudo event detected**\n\
                    {event_info}",
                )
            }

            AlertKind::OperatorSlashed { event_info } => {
                write!(
                    f,
                    "**Operator slash detected**\n\
                    {event_info}",
                )
            }

            AlertKind::SlotTime {
                current_ratio,
                threshold,
                interval,
                first_slot_time,
            } => {
                write!(
                    f,
                    "**Slot per time ratio alert**\n\
                    Current ratio: {current_ratio:.2} slots per second\n\
                    Threshold: {threshold:.2} slots per second\n\
                    Interval: {}\n\
                    First slot time: {}",
                    fmt_duration(*interval),
                    fmt_timestamp(first_slot_time.date_time()),
                )
            }

            AlertKind::FarmersDecreasedSuddenly {
                number_of_farmers_with_votes,
                average_number_of_farmers_with_votes,
                number_of_blocks,
            } => {
                write!(
                    f,
                    "**Farmers count has decreased significantly in the last blocks**\n\
                    Number of farmers with votes: {number_of_farmers_with_votes}\n\
                    Average number of farmers with votes in previous {number_of_blocks} blocks: {average_number_of_farmers_with_votes}",
                )
            }
            AlertKind::FarmersIncreasedSuddenly {
                number_of_farmers_with_votes,
                average_number_of_farmers_with_votes,
                number_of_blocks,
            } => {
                write!(
                    f,
                    "**Farmers count has increased significantly in the last blocks**\n\
                    Number of farmers with votes: {number_of_farmers_with_votes}\n\
                    Average number of farmers with votes in previous {number_of_blocks} blocks: {average_number_of_farmers_with_votes}",
                )
            }
        }
    }
}

impl AlertKind {
    /// Extract the previous block info from the alert, if present.
    /// The Reorg alert doesn't have a previous block info, but it does have a previous block
    /// position.
    #[allow(dead_code, reason = "TODO: use in tests")]
    pub fn prev_block_info(&self) -> Option<&BlockInfo> {
        match self {
            AlertKind::BlockProductionResumed {
                prev_block_info, ..
            } => Some(prev_block_info),
            // Deliberately repeat each enum variant here, so we can't forget to update this
            // method when adding new variants.
            AlertKind::Startup
            | AlertKind::BlockProductionStall { .. }
            | AlertKind::NewSideFork { .. }
            | AlertKind::SideForkExtended { .. }
            | AlertKind::Reorg { .. }
            | AlertKind::ForceBalanceTransfer { .. }
            | AlertKind::LargeBalanceTransfer { .. }
            | AlertKind::LargeBalanceTransferEvent { .. }
            | AlertKind::SudoCall { .. }
            | AlertKind::SudoEvent { .. }
            | AlertKind::OperatorSlashed { .. }
            | AlertKind::SlotTime { .. }
            | AlertKind::FarmersDecreasedSuddenly { .. }
            | AlertKind::FarmersIncreasedSuddenly { .. } => None,
        }
    }

    /// Extract the previous block link from the alert, if present.
    /// The Reorg alert doesn't have a previous block info, but it does have a previous block
    /// position.
    #[allow(dead_code, reason = "TODO: use in tests")]
    pub fn prev_block_position(&self) -> Option<BlockPosition> {
        match self {
            AlertKind::BlockProductionResumed {
                prev_block_info, ..
            } => Some(prev_block_info.position()),
            AlertKind::Reorg { old_best_block, .. } => Some(*old_best_block),
            AlertKind::Startup
            | AlertKind::BlockProductionStall { .. }
            | AlertKind::NewSideFork { .. }
            | AlertKind::SideForkExtended { .. }
            | AlertKind::ForceBalanceTransfer { .. }
            | AlertKind::LargeBalanceTransfer { .. }
            | AlertKind::LargeBalanceTransferEvent { .. }
            | AlertKind::SudoCall { .. }
            | AlertKind::SudoEvent { .. }
            | AlertKind::OperatorSlashed { .. }
            | AlertKind::SlotTime { .. }
            | AlertKind::FarmersDecreasedSuddenly { .. }
            | AlertKind::FarmersIncreasedSuddenly { .. } => None,
        }
    }

    /// Extract the extrinsic from the alert, if present.
    #[cfg_attr(not(test), allow(dead_code, reason = "only used in tests"))]
    pub fn extrinsic_info(&self) -> Option<&ExtrinsicInfo> {
        match self {
            AlertKind::ForceBalanceTransfer { extrinsic_info, .. } => Some(extrinsic_info),
            AlertKind::LargeBalanceTransfer { extrinsic_info, .. } => Some(extrinsic_info),
            AlertKind::SudoCall { extrinsic_info } => Some(extrinsic_info),
            AlertKind::Startup
            | AlertKind::FarmersDecreasedSuddenly { .. }
            | AlertKind::FarmersIncreasedSuddenly { .. }
            | AlertKind::BlockProductionStall { .. }
            | AlertKind::BlockProductionResumed { .. }
            | AlertKind::NewSideFork { .. }
            | AlertKind::SideForkExtended { .. }
            | AlertKind::LargeBalanceTransferEvent { .. }
            | AlertKind::Reorg { .. }
            | AlertKind::SudoEvent { .. }
            | AlertKind::OperatorSlashed { .. }
            | AlertKind::SlotTime { .. } => None,
        }
    }

    /// Extract the transfer value from the alert, if present.
    #[allow(dead_code, reason = "TODO: use in tests")]
    pub fn transfer_value(&self) -> Option<Balance> {
        match self {
            AlertKind::ForceBalanceTransfer { transfer_value, .. } => *transfer_value,
            AlertKind::LargeBalanceTransfer { transfer_value, .. }
            | AlertKind::LargeBalanceTransferEvent { transfer_value, .. } => Some(*transfer_value),
            AlertKind::Startup
            | AlertKind::FarmersDecreasedSuddenly { .. }
            | AlertKind::FarmersIncreasedSuddenly { .. }
            | AlertKind::BlockProductionStall { .. }
            | AlertKind::BlockProductionResumed { .. }
            | AlertKind::NewSideFork { .. }
            | AlertKind::SideForkExtended { .. }
            | AlertKind::Reorg { .. }
            | AlertKind::SudoCall { .. }
            | AlertKind::SudoEvent { .. }
            | AlertKind::OperatorSlashed { .. }
            | AlertKind::SlotTime { .. } => None,
        }
    }

    /// Extract the event from the alert, if present.
    #[cfg_attr(not(test), allow(dead_code, reason = "only used in tests"))]
    pub fn event_info(&self) -> Option<&EventInfo> {
        match self {
            AlertKind::LargeBalanceTransferEvent { event_info, .. }
            | AlertKind::SudoEvent { event_info }
            | AlertKind::OperatorSlashed { event_info } => Some(event_info),
            AlertKind::Startup
            | AlertKind::BlockProductionStall { .. }
            | AlertKind::BlockProductionResumed { .. }
            | AlertKind::NewSideFork { .. }
            | AlertKind::SideForkExtended { .. }
            | AlertKind::Reorg { .. }
            | AlertKind::ForceBalanceTransfer { .. }
            | AlertKind::LargeBalanceTransfer { .. }
            | AlertKind::SudoCall { .. }
            | AlertKind::SlotTime { .. }
            | AlertKind::FarmersDecreasedSuddenly { .. }
            | AlertKind::FarmersIncreasedSuddenly { .. } => None,
        }
    }
}

/// Post a startup alert.
///
/// Any returned errors are fatal and require a restart.
pub async fn startup_alert(
    mode: BlockCheckMode,
    alert_tx: &mpsc::Sender<Alert>,
    block_info: &BlockInfo,
) -> anyhow::Result<()> {
    assert!(mode.is_startup(), "should only be called at startup");

    // TODO:
    // - always post this to the test channel, because it's not a real "alert"
    // - link to the prod channel from this message: <https://docs.slack.dev/messaging/formatting-message-text/#linking-channels>

    alert_tx
        .send(Alert::new(AlertKind::Startup, *block_info, mode))
        .await?;

    Ok(())
}

/// Check a block for alerts, against the previous block.
///
/// Any returned errors are fatal and require a restart.
pub async fn check_block(
    // TODO: when we add a check that doesn't work on replayed blocks, skip it using mode
    mode: BlockCheckMode,
    alert_tx: &mpsc::Sender<Alert>,
    block_info: &BlockInfo,
    prev_block_info: &Option<BlockInfo>,
) -> anyhow::Result<()> {
    let Some(prev_block_info) = prev_block_info else {
        // No last block to check against.
        return Ok(());
    };

    // Because it depends on the next block, this check logs after block production resumes.
    if let Some(gap) = gap_since_last_block(*block_info, *prev_block_info) {
        // Resume alerts without a stall are harmless, and might actually be interesting in
        // themselves.
        if gap >= MIN_BLOCK_GAP.saturating_sub(BLOCK_GAP_SLOP) {
            alert_tx
                .send(Alert::new(
                    AlertKind::BlockProductionResumed {
                        gap,
                        prev_block_info: *prev_block_info,
                    },
                    *block_info,
                    mode,
                ))
                .await?;
        }
    } else {
        // No block time to check against.
        warn!(
            ?mode,
            ?block_info,
            ?prev_block_info,
            "Block time unavailable in block",
        );
    };

    Ok(())
}

/// Spawn a task that waits for `MIN_BLOCK_GAP`, then alerts if there was no block received on
/// `latest_block_rx` in that gap.
///
/// Doesn't work on replayed blocks, because the chain has already resumed after a replayed block
/// gap.
///
/// Fatal errors will panic in the spawned task.
pub async fn check_for_block_stall(
    mode: BlockCheckMode,
    alert_tx: mpsc::Sender<Alert>,
    block_info: BlockInfo,
    latest_block_rx: watch::Receiver<Option<BlockInfo>>,
) {
    // It doesn't make sense to check the local clock for stalls on replayed blocks, because we
    // already know there's a new current block. It's expensive to spawn a task, so return
    // early.
    if !mode.is_current() {
        return;
    }

    let old_block_info = block_info;

    // There's no need to check the result of the spawned task, panics are impossible, and other
    // errors are handled by replaying missed blocks on restart.
    tokio::spawn(async move {
        // Stall alerts without a resume are alarming, it looks like either the chain or alerter
        // has stopped. We've seen a spurious stall at exactly 60 seconds, but only once.
        sleep(MIN_BLOCK_GAP.saturating_add(BLOCK_GAP_SLOP)).await;

        // Avoid a potential deadlock by copying the watched value immediately.
        let latest_block_info: BlockInfo = latest_block_rx
            .borrow()
            .expect("never empty, a block is sent before spawning this task");

        if latest_block_info.time > old_block_info.time {
            // There's a new block since we sent our block and spawned our task, so block
            // production hasn't stalled. But the latest block also spawned a task, so it
            // will alert if there is actually a stall.
            return;
        }

        let gap = gap_since_time(Utc::now(), old_block_info);

        // If we have restarted, the new alerter will replay missed blocks, so we can ignore any
        // send errors to dropped channels. This also avoids panics during process shutdown.
        let _ = alert_tx
            .send(Alert::new(
                AlertKind::BlockProductionStall { gap },
                old_block_info,
                mode,
            ))
            .await;
    });
}

/// Check an extrinsic for alerts.
///
/// Extrinsic parsing should never fail, if it does, the runtime metdata is likely wrong.
/// But we don't want to panic or exit when that happens, instead we warn, and hope to
/// recover after we pick up the runtime upgrade in the next block.
///
/// Any returned errors are fatal and require a restart.
pub async fn check_extrinsic(
    // TODO: when we add a check that doesn't work on replayed blocks, skip it using mode
    mode: BlockCheckMode,
    alert_tx: &mpsc::Sender<Alert>,
    extrinsic: &RawExtrinsic,
    block_info: &BlockInfo,
) -> anyhow::Result<()> {
    let Some(extrinsic_info) = ExtrinsicInfo::new(extrinsic, block_info) else {
        return Ok(());
    };

    // TODO:
    // - extract each alert into a pallet-specific function or trait object
    // - add tests to make sure we can parse the extrinsics for each alert
    // - format account IDs as ss58 with prefix 6094
    // - link extrinsic and account to subscan
    // - add extrinsic success/failure to alerts

    check_balance_extrinsic(mode, alert_tx, &extrinsic_info, block_info).await?;

    // All sudo calls are alerts.
    // TODO:
    // - test this alert by checking a historic block with a sudo call
    // - check if the call is from the sudo account
    // - decode the inner call
    if extrinsic_info.pallet == "Sudo" {
        alert_tx
            .send(Alert::new(
                AlertKind::SudoCall { extrinsic_info },
                *block_info,
                mode,
            ))
            .await?;
    }

    Ok(())
}

/// Check an event for alerts.
///
/// Event parsing should never fail, see `check_extrinsic` for more details.
///
/// Any returned errors are fatal and require a restart.
pub async fn check_event(
    // TODO: when we add a check that doesn't work on replayed blocks, skip it using mode
    mode: BlockCheckMode,
    alert_tx: &mpsc::Sender<Alert>,
    event: &RawEvent,
    block_info: &BlockInfo,
) -> anyhow::Result<()> {
    let event_info = EventInfo::new(event, block_info);

    // TODO:
    // - extract each alert into a pallet-specific function or trait object
    // - add tests to make sure we can parse the events for each alert
    // - format account IDs as ss58 with prefix 6094
    // - link event and account to subscan

    check_balance_event(mode, alert_tx, &event_info, block_info).await?;

    // All operator slashes are alerts.
    // TODO:
    // - test this alert by checking a historic block with an operator slash event
    // - check the case of these names
    if event_info.pallet == "Domains" && event_info.kind == "OperatorSlashed" {
        alert_tx
            .send(Alert::new(
                AlertKind::OperatorSlashed { event_info },
                *block_info,
                mode,
            ))
            .await?;
    } else if event_info.pallet == "Sudo" {
        // We already alert on sudo calls, so this exists mainly to test events.
        alert_tx
            .send(Alert::new(
                AlertKind::SudoEvent { event_info },
                *block_info,
                mode,
            ))
            .await?;
    }

    Ok(())
}
