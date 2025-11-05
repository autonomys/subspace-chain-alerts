//! Specific chain alerts.

pub mod account;
pub mod subscan;
pub mod transfer;

#[cfg(test)]
mod tests;

use crate::alerts::account::Accounts;
use crate::alerts::transfer::TransferValue;
use crate::chain_fork_monitor::ChainForkEvent;
use crate::format::{fmt_amount, fmt_time_delta};
use crate::subspace::{
    AI3, Balance, BlockHash, BlockInfo, BlockLink, BlockPosition, EventInfo, ExtrinsicInfo,
    RawEvent, RawExtrinsic, chain_time_gap_between_blocks, local_time_gap_between_blocks,
    local_time_gap_since_block,
};
use chrono::TimeDelta;
use std::fmt::{self, Display};
use std::sync::Arc;
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;
use tokio::time::sleep;
use tracing::{trace, warn};

/// The minimum balance change to alert on.
const MIN_BALANCE_CHANGE: Balance = 1_000_000 * AI3;

/// Hide the details of the block gap constants, so we only use the correct ones.
mod internal {
    use crate::subspace::TARGET_BLOCK_INTERVAL;
    use chrono::TimeDelta;
    use std::time::Duration;

    /// The minimum gap between block timestamps to alert on.
    /// The target block gap is 6 seconds, so we alert if it takes substantially longer.
    ///
    /// `pallet-timestamp` enforces a `MinimumPeriod` of 3 seconds in Subspace, and a
    /// `MAX_TIMESTAMP_DRIFT_MILLIS` of 30 seconds from each node's local clock.
    /// <https://github.com/paritytech/polkadot-sdk/blob/0034d178fff88a0fd87cf0ec1d8f122ae0011d78/substrate/frame/timestamp/src/lib.rs#L307>
    const MIN_BLOCK_GAP: Duration = Duration::from_secs(TARGET_BLOCK_INTERVAL * 10);

    /// The amount of time to add/subtract from the minimum block gap to account for consensus clock
    /// drift, async timer delays, and similar inaccuracies.
    const BLOCK_GAP_SLOP: Duration = Duration::from_secs(5);

    /// The minimum resume block gap to alert on.
    pub const MIN_RESUME_BLOCK_GAP: TimeDelta =
        match TimeDelta::from_std(MIN_BLOCK_GAP.saturating_sub(BLOCK_GAP_SLOP)) {
            Ok(gap) => gap,
            Err(_err) => {
                panic!("constants are small and always in range");
            }
        };

    /// The minimum stall block gap to alert on.
    ///
    /// Stall alerts without a resume are alarming to users, it looks like either the chain or
    /// alerter has stopped. So we add some slop to suppress borderline alerts. We've seen
    /// spurious stalls at exactly 60 seconds without the slop.
    pub const MIN_STALL_BLOCK_GAP: Duration = MIN_BLOCK_GAP.saturating_add(BLOCK_GAP_SLOP);
}

pub use internal::{MIN_RESUME_BLOCK_GAP, MIN_STALL_BLOCK_GAP};

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
    pub fn is_current(self) -> bool {
        match self {
            BlockCheckMode::Current => true,
            BlockCheckMode::Replay | BlockCheckMode::Startup => false,
        }
    }

    /// Whether we are checking replayed blocks.
    #[expect(dead_code, reason = "included for completeness")]
    pub fn is_replay(self) -> bool {
        !self.is_current()
    }

    /// Whether we are checking startup blocks.
    pub fn is_startup(self) -> bool {
        match self {
            BlockCheckMode::Current | BlockCheckMode::Replay => false,
            BlockCheckMode::Startup => true,
        }
    }

    /// Returns this block check mode, modified for replaying blocks.
    pub fn during_replay(self) -> Self {
        match self {
            // Blocks can't be current during a replay.
            BlockCheckMode::Current => BlockCheckMode::Replay,
            BlockCheckMode::Replay | BlockCheckMode::Startup => self,
        }
    }
}

/// A blockchain alert with context.
#[derive(Clone, Debug, PartialEq)]
pub struct Alert {
    /// The type of alert.
    pub alert: AlertKind,

    /// The mode the alert was triggered in.
    pub mode: BlockCheckMode,

    /// The block the alert occurred in.
    pub block_info: BlockInfo,

    /// The RPC node that received the block that triggered the alert.
    pub node_rpc_url: String,
}

impl Display for Alert {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let Self {
            alert,
            mode,
            block_info,
            // Displayed by the logs or in the Slack context.
            node_rpc_url: _,
        } = self;

        write!(f, "{mode:?} {alert}")?;
        write!(f, "\nBlock: {block_info}")?;

        Ok(())
    }
}

impl Alert {
    /// Create a new alert.
    pub fn new(
        alert: AlertKind,
        mode: BlockCheckMode,
        block_info: BlockInfo,
        node_rpc_url: &str,
    ) -> Self {
        Self {
            alert,
            mode,
            block_info,
            node_rpc_url: node_rpc_url.to_string(),
        }
    }

    /// Create a new alert from a chain fork event.
    pub fn from_chain_fork_event(
        event: ChainForkEvent,
        mode: BlockCheckMode,
        block_info: BlockInfo,
        node_rpc_url: &str,
    ) -> Self {
        let backwards_reorg_depth = event.backwards_reorg_depth();

        let alert_kind = match event {
            // The new block is always the same as block_info, so we ignore it.
            ChainForkEvent::NewSideFork { tip, fork_depth } => {
                assert_eq!(
                    *tip, block_info.link,
                    "block_info must be the tip of the new side fork",
                );
                AlertKind::NewSideFork { fork_depth }
            }
            ChainForkEvent::SideForkExtended { tip, fork_depth } => {
                assert_eq!(
                    *tip, block_info.link,
                    "block_info must be the tip of the side fork",
                );
                AlertKind::SideForkExtended { fork_depth }
            }
            ChainForkEvent::Reorg {
                new_best_block,
                old_best_block,
                old_fork_depth,
                new_fork_depth,
            } => {
                assert_eq!(
                    *new_best_block, block_info.link,
                    "block_info must be the new best block",
                );

                AlertKind::Reorg {
                    old_best_block: old_best_block.position,
                    old_fork_depth,
                    new_fork_depth,
                    backwards_reorg_depth,
                }
            }
        };

        Self::new(alert_kind, mode, block_info, node_rpc_url)
    }

    /// Returns the node RPC URLs that triggered this alert.
    pub fn node_rpc_urls(&self) -> Vec<String> {
        if let Some(node_rpc_urls) = self.alert.node_rpc_urls() {
            assert!(
                node_rpc_urls.contains(&self.node_rpc_url),
                "Alert.node_rpc_url must be in the additional URLs in the AlertKind",
            );

            node_rpc_urls
        } else {
            vec![self.node_rpc_url.clone()]
        }
    }
}

/// The type of alert.
#[derive(Clone, Debug, PartialEq)]
pub enum AlertKind {
    /// The alerter has started.
    Startup {
        /// All the connected RPC nodes.
        node_rpc_urls: Vec<String>,
    },

    /// A node has stopped receiving blocks.
    ///
    /// The previous block is `Alert.block_info`, and the stalled node is
    /// `Alert.node_rpc_url`.
    ///
    /// Only alerted for the primary node.
    BlockReceiveGap {
        /// The gap since the previous block from the node, based on alerter local time.
        local_time_gap: TimeDelta,
    },

    /// There was a gap where none of the node subscriptions sent blocks, and now a node has started
    /// sending blocks again.
    ///
    /// The associated `Alert.node_rpc_url` is the RPC node that resumed receiving blocks.
    /// Other nodes might have also received blocks, or might still be stalled.
    ///
    /// Only alerted after the primary node has had a gap.
    BlockReceiveResumed {
        /// The full gap between the blocks, based on alerter local time.
        local_time_gap: TimeDelta,

        /// The gap between the previous and current block header times.
        chain_time_gap: Option<TimeDelta>,

        /// The previous block.
        prev_block_info: BlockInfo,
    },

    /// A gap between block header times (with no stall according to the node time).
    BlockChainTimeGap {
        /// The gap between the previous and current block header times.
        ///
        /// Note: the current block is `Alert.block_info`.
        chain_time_gap: TimeDelta,

        /// The previous block.
        prev_block_info: BlockInfo,
    },

    /// A new chain fork was seen, which was not started by a best block.
    ///
    /// The tip of the fork is `Alert.block_info`, and the RPC node that received the latest
    /// block on the side fork is `Alert.node_rpc_url`.
    NewSideFork {
        /// The number of blocks from the fork tip to the fork point.
        /// This is typically 1 for new side forks, but might be greater if we filled in missed
        /// blocks.
        fork_depth: usize,
    },

    /// A chain fork was extended by a non-best block.
    ///
    /// The tip of the fork is `Alert.block_info`, and the RPC node that received the latest
    /// block on the side fork is `Alert.node_rpc_url`.
    SideForkExtended {
        /// The number of blocks from the fork tip to the fork point.
        fork_depth: usize,
    },

    /// A reorg was seen to a best block on a side chain.
    /// This takes priority over fork events.
    ///
    /// The new best block is `Alert.block_info`, and the RPC node that received the latest
    /// block on the new best chain is `Alert.node_rpc_url`. This is almost always the primary
    /// RPC node.
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
        extrinsic_info: Arc<ExtrinsicInfo>,

        /// The transfer value.
        transfer_value: Option<Balance>,
    },

    /// A large Balance transfer extrinsic has been detected.
    LargeBalanceTransfer {
        /// The Balance call's extrinsic information.
        extrinsic_info: Arc<ExtrinsicInfo>,

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

    /// A transfer to or from an important address has been detected.
    ImportantAddressTransfer {
        /// The list of important address kinds.
        address_kinds: String,

        /// The Balance call's extrinsic information.
        extrinsic_info: Arc<ExtrinsicInfo>,

        /// The transfer value.
        transfer_value: Option<Balance>,
    },

    /// A transfer event to or from an important address has been detected.
    ImportantAddressTransferEvent {
        /// The list of important address kinds.
        address_kinds: String,

        /// The Balance event information.
        event_info: EventInfo,

        /// The transfer value.
        transfer_value: Option<Balance>,
    },

    /// An extrinsic initiated by an important address has been detected.
    ImportantAddressExtrinsic {
        /// The important address kind.
        address_kind: String,

        /// The extrinsic information.
        extrinsic_info: Arc<ExtrinsicInfo>,
    },

    /// An event initiated by an important address has been detected.
    ImportantAddressEvent {
        /// The important address kind.
        address_kind: String,

        /// The event information.
        event_info: EventInfo,
    },

    /// A Sudo call has been detected.
    SudoCall {
        /// The sudo call's extrinsic information.
        extrinsic_info: Arc<ExtrinsicInfo>,
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

    /// Slot timing is slower than expected.
    ///
    /// The associated `Alert.node_rpc_url` is the RPC node that received the last block in the
    /// state, which triggered this alert.
    SlowSlotTime {
        /// The amount of slots.
        slot_amount: u64,

        /// The number of seconds between the block times of the first and last block in the
        /// state.
        time_elapsed: u64,

        /// The current ratio of slots to time.
        current_ratio: f64,

        /// The applicable threshold for this alert.
        threshold: f64,
    },

    /// Slot timing is faster than expected.
    ///
    /// The associated `Alert.node_rpc_url` is the RPC node that received the last block in the
    /// state, which triggered this alert.
    FastSlotTime {
        /// The amount of slots.
        slot_amount: u64,

        /// The number of seconds between the block times of the first and last block in the
        /// state.
        time_elapsed: u64,

        /// The current ratio of slots to time.
        current_ratio: f64,

        /// The applicable threshold for this alert.
        threshold: f64,
    },

    /// Farmers count has decreased suddenly.
    ///
    /// The associated `Alert.node_rpc_url` is the RPC node that received the last block in the
    /// state, which triggered this alert.
    FarmersDecreasedSuddenly {
        /// The number of farmers with votes.
        number_of_farmers_with_votes: usize,

        /// The average number of farmers with votes.
        average_number_of_farmers_with_votes: f64,

        /// The number of blocks in the interval.
        number_of_blocks: u32,
    },

    /// Farmers count has increased suddenly.
    ///
    /// The associated `Alert.node_rpc_url` is the RPC node that received the last block in the
    /// state, which triggered this alert.
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
            Self::Startup {
                // Already printed as part of the alert context.
                node_rpc_urls: _,
            } => {
                write!(f, "**Launched and connected to the nodes**")
            }

            Self::BlockReceiveGap { local_time_gap } => {
                write!(
                    f,
                    "**Node stopped receiving blocks**\n\
                    Local time since last block: {}",
                    fmt_time_delta(*local_time_gap),
                )
            }

            Self::BlockReceiveResumed {
                local_time_gap,
                chain_time_gap,
                prev_block_info,
            } => {
                write!(
                    f,
                    "**Node resumed receiving blocks**\n\
                    Local time gap: {}\n\
                    Header time gap: {}\n\n\
                    Previous best block:\n\
                    {prev_block_info}",
                    fmt_time_delta(*local_time_gap),
                    fmt_time_delta(*chain_time_gap),
                )
            }

            Self::BlockChainTimeGap {
                chain_time_gap,
                prev_block_info,
            } => {
                write!(
                    f,
                    "**Block chain time gap**\n\
                    Gap: {}\n\n\
                    Previous best block:\n\
                    {prev_block_info}",
                    fmt_time_delta(*chain_time_gap),
                )
            }

            Self::NewSideFork { fork_depth } => {
                write!(
                    f,
                    "**New side chain fork detected**\n\
                    Fork depth: {fork_depth}",
                )
            }

            Self::SideForkExtended { fork_depth } => {
                write!(
                    f,
                    "**Side chain fork extended**\n\
                    Fork depth: {fork_depth}",
                )
            }

            Self::Reorg {
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

                write!(f, "\n\n_Check Subscan 'finalized' blocks for changes_\n")?;

                Ok(())
            }

            Self::ForceBalanceTransfer {
                extrinsic_info,
                // Already printed in the extrinsic info.
                transfer_value: _,
            } => {
                write!(
                    f,
                    "**Force Balances call detected**\n\
                    {extrinsic_info}",
                )
            }

            Self::LargeBalanceTransfer {
                extrinsic_info,
                transfer_value: _,
            } => {
                write!(
                    f,
                    "**Large Balances call detected**\n\
                    Transfer above {}\n\
                    {extrinsic_info}",
                    fmt_amount(MIN_BALANCE_CHANGE),
                )
            }

            Self::LargeBalanceTransferEvent {
                event_info,
                // Already printed in the event info.
                transfer_value: _,
            } => {
                write!(
                    f,
                    "**Large Balances event detected**\n\
                    Transfer above {}\n\
                    {event_info}",
                    fmt_amount(MIN_BALANCE_CHANGE),
                )
            }

            Self::ImportantAddressTransfer {
                address_kinds,
                extrinsic_info,
                transfer_value: _,
            } => {
                write!(
                    f,
                    "**Important address transfer detected**\n\
                    Kind(s): {address_kinds}\n\
                    {extrinsic_info}",
                )
            }

            Self::ImportantAddressTransferEvent {
                address_kinds,
                event_info,
                transfer_value: _,
            } => {
                write!(
                    f,
                    "**Important address transfer detected**\n\
                    Kind(s): {address_kinds}\n\
                    {event_info}",
                )
            }

            Self::ImportantAddressExtrinsic {
                address_kind,
                extrinsic_info,
            } => {
                write!(
                    f,
                    "**Important address sent an extrinsic**\n\
                    Kind: {address_kind}\n\
                    {extrinsic_info}",
                )
            }

            Self::ImportantAddressEvent {
                address_kind,
                event_info,
            } => {
                write!(
                    f,
                    "**Important address initiated an event**\n\
                    Kind: {address_kind}\n\
                    {event_info}",
                )
            }

            Self::SudoCall { extrinsic_info } => {
                write!(
                    f,
                    "**Sudo call detected**\n\
                    {extrinsic_info}",
                )
            }

            Self::SudoEvent { event_info } => {
                write!(
                    f,
                    "**Sudo event detected**\n\
                    {event_info}",
                )
            }

            Self::OperatorSlashed { event_info } => {
                write!(
                    f,
                    "**Operator slash detected**\n\
                    {event_info}",
                )
            }

            Self::SlowSlotTime {
                slot_amount,
                time_elapsed,
                current_ratio,
                threshold,
            } => {
                write!(
                    f,
                    "**Slow slot time alert**\n\
                    Current ratio: {current_ratio:.2} seconds per slot\n\
                    Threshold: {threshold:.2} seconds per slot\n\
                    Slot amount: {slot_amount}\n\
                    Time elapsed: {time_elapsed} seconds",
                )
            }

            Self::FastSlotTime {
                slot_amount,
                time_elapsed,
                current_ratio,
                threshold,
            } => {
                write!(
                    f,
                    "**Fast slot time alert**\n\
                    Current ratio: {current_ratio:.2} seconds per slot\n\
                    Threshold: {threshold:.2} seconds per slot\n\
                    Slot amount: {slot_amount}\n\
                    Time elapsed: {time_elapsed} seconds",
                )
            }

            Self::FarmersDecreasedSuddenly {
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
            Self::FarmersIncreasedSuddenly {
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
    /// Returns true if the alert always goes to the test channel.
    pub fn is_test_alert(&self) -> bool {
        match self {
            Self::Startup { .. } => true,
            Self::BlockReceiveGap { .. }
            | Self::BlockReceiveResumed { .. }
            | Self::BlockChainTimeGap { .. }
            | Self::NewSideFork { .. }
            | Self::SideForkExtended { .. }
            | Self::Reorg { .. }
            | Self::ForceBalanceTransfer { .. }
            | Self::LargeBalanceTransfer { .. }
            | Self::LargeBalanceTransferEvent { .. }
            | Self::ImportantAddressTransfer { .. }
            | Self::ImportantAddressTransferEvent { .. }
            | Self::ImportantAddressExtrinsic { .. }
            | Self::ImportantAddressEvent { .. }
            | Self::SudoCall { .. }
            | Self::SudoEvent { .. }
            | Self::OperatorSlashed { .. }
            | Self::SlowSlotTime { .. }
            | Self::FastSlotTime { .. }
            | Self::FarmersDecreasedSuddenly { .. }
            | Self::FarmersIncreasedSuddenly { .. } => false,
        }
    }

    /// Returns true if this alert is always a duplicate of another alert.
    pub fn is_duplicate(&self) -> bool {
        match self {
            // Always a duplicate of ImportantAddress*, because all extrinsics for important
            // addresses get an alert.
            Self::ImportantAddressTransferEvent { .. } | Self::ImportantAddressEvent { .. }
            // Always a duplicate of Sudo, because all sudo extrinsics get an alert.
            | Self::SudoEvent { .. }=> true,
            // Usually a duplicate, but not for transfer_all, because transfer_all doesn't have an
            // amount.
            // TODO: get the transfer_all amount from storage during the extrinsic check.
            Self::LargeBalanceTransferEvent { event_info, .. } => if let Some(extrinsic) = event_info.extrinsic_info.as_ref() {
                extrinsic.call != "transfer_all"
            } else {
                true
            },
            Self::Startup { .. }
            | Self::BlockReceiveGap { .. }
            | Self::BlockReceiveResumed { .. }
            | Self::BlockChainTimeGap { .. }
            | Self::NewSideFork { .. }
            | Self::SideForkExtended { .. }
            | Self::Reorg { .. }
            | Self::ForceBalanceTransfer { .. }
            | Self::LargeBalanceTransfer { .. }
            | Self::ImportantAddressTransfer { .. }
            | Self::ImportantAddressExtrinsic { .. }
            | Self::SudoCall { .. }
            | Self::OperatorSlashed { .. }
            | Self::SlowSlotTime { .. }
            | Self::FastSlotTime { .. }
            | Self::FarmersDecreasedSuddenly { .. }
            | Self::FarmersIncreasedSuddenly { .. } => false,
        }
    }

    /// Extract the previous block info from the alert, if present.
    /// The Reorg alert doesn't have a previous block info, but it does have a previous block
    /// position.
    #[allow(dead_code, reason = "TODO: use in tests")]
    pub fn prev_block_info(&self) -> Option<&BlockInfo> {
        match self {
            Self::BlockReceiveResumed {
                prev_block_info, ..
            }
            | Self::BlockChainTimeGap {
                prev_block_info, ..
            } => Some(prev_block_info),
            // Deliberately repeat each enum variant here, so we can't forget to update this
            // method when adding new variants.
            Self::Startup { .. }
            | Self::BlockReceiveGap { .. }
            | Self::NewSideFork { .. }
            | Self::SideForkExtended { .. }
            | Self::Reorg { .. }
            | Self::ForceBalanceTransfer { .. }
            | Self::LargeBalanceTransfer { .. }
            | Self::LargeBalanceTransferEvent { .. }
            | Self::ImportantAddressTransfer { .. }
            | Self::ImportantAddressTransferEvent { .. }
            | Self::ImportantAddressExtrinsic { .. }
            | Self::ImportantAddressEvent { .. }
            | Self::SudoCall { .. }
            | Self::SudoEvent { .. }
            | Self::OperatorSlashed { .. }
            | Self::SlowSlotTime { .. }
            | Self::FastSlotTime { .. }
            | Self::FarmersDecreasedSuddenly { .. }
            | Self::FarmersIncreasedSuddenly { .. } => None,
        }
    }

    /// Extract the previous block link from the alert, if present.
    /// The Reorg alert doesn't have a previous block info, but it does have a previous block
    /// position.
    #[allow(dead_code, reason = "TODO: use in tests")]
    pub fn prev_block_position(&self) -> Option<BlockPosition> {
        if let Some(prev_block_info) = self.prev_block_info() {
            return Some(prev_block_info.position());
        }

        match self {
            Self::BlockReceiveResumed {
                prev_block_info: _, ..
            }
            | Self::BlockChainTimeGap {
                prev_block_info: _, ..
            } => unreachable!("already handled above"),
            Self::Reorg { old_best_block, .. } => Some(*old_best_block),
            Self::Startup { .. }
            | Self::BlockReceiveGap { .. }
            | Self::NewSideFork { .. }
            | Self::SideForkExtended { .. }
            | Self::ForceBalanceTransfer { .. }
            | Self::LargeBalanceTransfer { .. }
            | Self::LargeBalanceTransferEvent { .. }
            | Self::ImportantAddressTransfer { .. }
            | Self::ImportantAddressTransferEvent { .. }
            | Self::ImportantAddressExtrinsic { .. }
            | Self::ImportantAddressEvent { .. }
            | Self::SudoCall { .. }
            | Self::SudoEvent { .. }
            | Self::OperatorSlashed { .. }
            | Self::SlowSlotTime { .. }
            | Self::FastSlotTime { .. }
            | Self::FarmersDecreasedSuddenly { .. }
            | Self::FarmersIncreasedSuddenly { .. } => None,
        }
    }

    /// Extract the extrinsic from the alert, if present.
    #[cfg_attr(not(test), allow(dead_code, reason = "only used in tests"))]
    pub fn extrinsic_info(&self) -> Option<&Arc<ExtrinsicInfo>> {
        match self {
            Self::ForceBalanceTransfer { extrinsic_info, .. }
            | Self::LargeBalanceTransfer { extrinsic_info, .. }
            | Self::ImportantAddressTransfer { extrinsic_info, .. }
            | Self::ImportantAddressExtrinsic { extrinsic_info, .. }
            | Self::SudoCall { extrinsic_info } => Some(extrinsic_info),
            Self::Startup { .. }
            | Self::BlockReceiveGap { .. }
            | Self::BlockReceiveResumed { .. }
            | Self::BlockChainTimeGap { .. }
            | Self::NewSideFork { .. }
            | Self::SideForkExtended { .. }
            | Self::LargeBalanceTransferEvent { .. }
            | Self::ImportantAddressTransferEvent { .. }
            | Self::ImportantAddressEvent { .. }
            | Self::Reorg { .. }
            | Self::SudoEvent { .. }
            | Self::OperatorSlashed { .. }
            | Self::SlowSlotTime { .. }
            | Self::FastSlotTime { .. }
            | Self::FarmersDecreasedSuddenly { .. }
            | Self::FarmersIncreasedSuddenly { .. } => None,
        }
    }

    /// Extract the transfer value from the alert, if present.
    #[allow(dead_code, reason = "TODO: use in tests")]
    pub fn transfer_value(&self) -> Option<Balance> {
        match self {
            Self::ForceBalanceTransfer { transfer_value, .. }
            | Self::ImportantAddressTransfer { transfer_value, .. }
            | Self::ImportantAddressTransferEvent { transfer_value, .. } => *transfer_value,
            Self::LargeBalanceTransfer { transfer_value, .. }
            | Self::LargeBalanceTransferEvent { transfer_value, .. } => Some(*transfer_value),
            Self::Startup { .. }
            | Self::BlockReceiveGap { .. }
            | Self::BlockReceiveResumed { .. }
            | Self::BlockChainTimeGap { .. }
            | Self::NewSideFork { .. }
            | Self::SideForkExtended { .. }
            | Self::Reorg { .. }
            | Self::ImportantAddressExtrinsic { .. }
            | Self::ImportantAddressEvent { .. }
            | Self::SudoCall { .. }
            | Self::SudoEvent { .. }
            | Self::OperatorSlashed { .. }
            | Self::SlowSlotTime { .. }
            | Self::FastSlotTime { .. }
            | Self::FarmersDecreasedSuddenly { .. }
            | Self::FarmersIncreasedSuddenly { .. } => None,
        }
    }

    /// Extract the event from the alert, if present.
    #[cfg_attr(not(test), allow(dead_code, reason = "only used in tests"))]
    pub fn event_info(&self) -> Option<&EventInfo> {
        match self {
            Self::LargeBalanceTransferEvent { event_info, .. }
            | Self::ImportantAddressTransferEvent { event_info, .. }
            | Self::SudoEvent { event_info }
            | Self::OperatorSlashed { event_info }
            | Self::ImportantAddressEvent { event_info, .. } => Some(event_info),
            Self::Startup { .. }
            | Self::BlockReceiveGap { .. }
            | Self::BlockReceiveResumed { .. }
            | Self::BlockChainTimeGap { .. }
            | Self::NewSideFork { .. }
            | Self::SideForkExtended { .. }
            | Self::Reorg { .. }
            | Self::ForceBalanceTransfer { .. }
            | Self::LargeBalanceTransfer { .. }
            | Self::ImportantAddressTransfer { .. }
            | Self::ImportantAddressExtrinsic { .. }
            | Self::SudoCall { .. }
            | Self::SlowSlotTime { .. }
            | Self::FastSlotTime { .. }
            | Self::FarmersDecreasedSuddenly { .. }
            | Self::FarmersIncreasedSuddenly { .. } => None,
        }
    }

    /// Extract the node RPC URLs from the alert, if present.
    fn node_rpc_urls(&self) -> Option<Vec<String>> {
        match self {
            Self::Startup { node_rpc_urls } => Some(node_rpc_urls.clone()),
            Self::BlockReceiveGap { .. }
            | Self::BlockReceiveResumed { .. }
            | Self::BlockChainTimeGap { .. }
            | Self::NewSideFork { .. }
            | Self::SideForkExtended { .. }
            | Self::Reorg { .. }
            | Self::ForceBalanceTransfer { .. }
            | Self::LargeBalanceTransfer { .. }
            | Self::LargeBalanceTransferEvent { .. }
            | Self::ImportantAddressTransfer { .. }
            | Self::ImportantAddressTransferEvent { .. }
            | Self::ImportantAddressExtrinsic { .. }
            | Self::ImportantAddressEvent { .. }
            | Self::SudoCall { .. }
            | Self::SudoEvent { .. }
            | Self::OperatorSlashed { .. }
            | Self::SlowSlotTime { .. }
            | Self::FastSlotTime { .. }
            | Self::FarmersDecreasedSuddenly { .. }
            | Self::FarmersIncreasedSuddenly { .. } => None,
        }
    }
}

/// Post a startup alert.
///
/// Any returned errors are fatal and require a restart.
pub async fn startup_alert(
    mode: BlockCheckMode,
    block_info: &BlockInfo,
    alert_tx: &mpsc::Sender<Alert>,
    node_rpc_urls: Vec<String>,
) -> anyhow::Result<()> {
    // We could check this is only called once, but that's a low priority.
    assert!(
        mode.is_current(),
        "should only be called on the first current block",
    );

    // TODO:
    // - always post this to the test channel, because it's not a real "alert"
    // - link to the prod channel from this message: <https://docs.slack.dev/messaging/formatting-message-text/#linking-channels>

    alert_tx
        .send(Alert::new(
            AlertKind::Startup {
                node_rpc_urls: node_rpc_urls.clone(),
            },
            mode,
            *block_info,
            // This is ignored in alerts, so we might as well use the primary node.
            &node_rpc_urls[0],
        ))
        .await?;

    Ok(())
}

/// The status of the block gap check.
///
/// Alerts are prioritised in the listed order, so if we're in a chain gap, we'll still issue a
/// local gap alert.
#[must_use = "the status must be tracked"]
#[derive(Copy, Clone, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub enum BlockGapAlertStatus {
    /// A local resume alert was issued after a block receive gap.
    LocalResume,

    /// A chain time resume alert was issued after a chain time gap.
    ChainResume,

    /// No resume or gap alert was issued - blocks are being produced and received normally.
    NoAlert,
}

impl BlockGapAlertStatus {
    /// Returns true if the new status deserves an alert.
    pub fn needs_new_alert(self, new_status: Self) -> bool {
        // Equal statuses and "no alert" statuses don't need an alert.
        if self == new_status || new_status == Self::NoAlert {
            return false;
        }

        // Local resume alerts are more important than chain time resume alerts,
        // because the local resume alert contains the chain time gap.
        if self == Self::LocalResume && new_status == Self::ChainResume {
            return false;
        }

        true
    }
}

/// Check a block for alerts, against the previous block.
///
/// Any returned errors are fatal and require a restart.
pub async fn check_block(
    // TODO: when we add a check that doesn't work on replayed blocks, skip it using mode
    mode: BlockCheckMode,
    block_info: &BlockInfo,
    prev_block_info: Option<&BlockInfo>,
    prev_block_gap_status: BlockGapAlertStatus,
    alert_tx: &mpsc::Sender<Alert>,
    node_rpc_url: &str,
) -> anyhow::Result<BlockGapAlertStatus> {
    let mut gap_alert_status = prev_block_gap_status;

    let Some(prev_block_info) = prev_block_info else {
        // No last block to check against.
        return Ok(gap_alert_status);
    };

    // Because they depend on the next block, these checks log after block production/propagation
    // resumes.
    let local_time_gap = local_time_gap_between_blocks(*block_info, *prev_block_info);
    let chain_time_gap = chain_time_gap_between_blocks(*block_info, *prev_block_info);

    // Gap alerts without a stall are harmless, and might actually be interesting in themselves.
    // We prioritise the block receive resume alert, because it also contains the chain time gap.
    if local_time_gap >= MIN_RESUME_BLOCK_GAP {
        if prev_block_gap_status.needs_new_alert(BlockGapAlertStatus::LocalResume) {
            gap_alert_status = BlockGapAlertStatus::LocalResume;
            alert_tx
                .send(Alert::new(
                    AlertKind::BlockReceiveResumed {
                        local_time_gap,
                        chain_time_gap,
                        prev_block_info: *prev_block_info,
                    },
                    mode,
                    *block_info,
                    node_rpc_url,
                ))
                .await?;
        }
    } else if let Some(chain_time_gap) = chain_time_gap
        && chain_time_gap >= MIN_RESUME_BLOCK_GAP
    {
        // Prioritise the local resume status by only changing status if we send an alert.
        if prev_block_gap_status.needs_new_alert(BlockGapAlertStatus::ChainResume) {
            gap_alert_status = BlockGapAlertStatus::ChainResume;
            alert_tx
                .send(Alert::new(
                    AlertKind::BlockChainTimeGap {
                        chain_time_gap,
                        prev_block_info: *prev_block_info,
                    },
                    mode,
                    *block_info,
                    node_rpc_url,
                ))
                .await?;
        }
    } else {
        gap_alert_status = BlockGapAlertStatus::NoAlert;
    }

    if chain_time_gap.is_none() {
        // No chain time to check against.
        warn!(
            ?mode,
            ?block_info,
            ?prev_block_info,
            "Block time unavailable in block",
        );
    };

    Ok(gap_alert_status)
}

/// Spawn a task that waits for `MIN_BLOCK_GAP`, then alerts if there was no block received on
/// `latest_best_block_rx` in that gap.
///
/// Fatal errors will be returned from the spawned task's join handle.
///
/// # Panics
///
/// On replayed blocks, because the chain has already resumed after a replayed block
/// gap.
#[must_use = "the spawned task must be joined"]
pub async fn check_for_block_stall(
    mode: BlockCheckMode,
    block: BlockLink,
    genesis_hash: &BlockHash,
    alert_tx: mpsc::Sender<Alert>,
    latest_best_block_rx: watch::Receiver<Option<(BlockLink, bool)>>,
    node_rpc_url: String,
) -> JoinHandle<anyhow::Result<Option<bool>>> {
    // It doesn't make sense to check the local clock for stalls on replayed blocks, because we
    // already know there's a new current block. It's expensive to spawn a task, so return
    // early.
    assert!(mode.is_current(), "should only be called on current blocks");

    let old_block = BlockInfo::minimal_from_link(block, genesis_hash);

    // We handle channel errors by restarting all tasks.
    tokio::spawn(async move {
        sleep(MIN_STALL_BLOCK_GAP).await;

        // Avoid a potential deadlock by copying the watched value immediately.
        let (latest_block, is_stalled_already): (BlockLink, bool) = latest_best_block_rx
            .borrow()
            .expect("never empty, a block is sent before spawning this task");

        if latest_block.height() > old_block.height() {
            // There's a new block since we sent our block and spawned our task, so block
            // production hasn't stalled. Start spawning tasks again if we were in a stall.
            return Ok(Some(false));
        }

        if is_stalled_already {
            // We've already sent an alert, so we don't need to send another.
            // But we want to stay in the stall if no other task resets it.
            return Ok(None);
        }

        let local_time_gap = local_time_gap_since_block(old_block);

        // If there is an error, we will restart, and the new alerter will replay missed blocks.
        alert_tx
            .send(Alert::new(
                AlertKind::BlockReceiveGap { local_time_gap },
                mode,
                old_block,
                &node_rpc_url,
            ))
            .await?;

        // We are in a stall.
        Ok(Some(true))
    })
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
    extrinsic: &RawExtrinsic,
    block_info: &BlockInfo,
    alert_tx: &mpsc::Sender<Alert>,
    node_rpc_url: &str,
) -> anyhow::Result<Option<Arc<ExtrinsicInfo>>> {
    let Some(extrinsic_info) = ExtrinsicInfo::new(extrinsic, block_info) else {
        // Invalid extrinsic, skip it.
        return Ok(None);
    };

    // TODO:
    // - add tests to make sure we can parse the extrinsics for each alert
    // - link extrinsic and account to subscan
    // - add extrinsic success/failure to alerts

    // TODO:
    // - track the total of recent transfers, so the threshold can't be bypassed by splitting the
    //   transfer into multiple calls
    let transfer_value = extrinsic_info.transfer_value();
    trace!(?mode, "transfer_value: {:?}", transfer_value);

    let initiator_account_kind = extrinsic_info.initiator_account_kind();
    let important_address_kinds = extrinsic_info.important_address_kinds_str();
    trace!(
        ?mode,
        ?initiator_account_kind,
        ?important_address_kinds,
        ?extrinsic_info,
        ?block_info,
        "extrinsic account list",
    );

    // The signing account is listed in the extrinsic, therefore:
    // - Sudo extrinsics override balance transfers by sudo,
    // - Large balance transfers override transfers initiated by important addresses, and
    // - An important address alert is only issued if no other alert is triggered.
    if extrinsic_info.pallet == "Sudo" {
        // All sudo calls are alerts.
        // TODO:
        // - test this alert by checking a historic block with a sudo call
        // - check if the call is from the sudo account
        // - decode the inner call
        alert_tx
            .send(Alert::new(
                AlertKind::SudoCall {
                    extrinsic_info: extrinsic_info.clone(),
                },
                mode,
                *block_info,
                node_rpc_url,
            ))
            .await?;
    } else if let Some(transfer_value) = transfer_value
        && transfer_value >= MIN_BALANCE_CHANGE
    {
        alert_tx
            .send(Alert::new(
                AlertKind::LargeBalanceTransfer {
                    extrinsic_info: extrinsic_info.clone(),
                    transfer_value,
                },
                mode,
                *block_info,
                node_rpc_url,
            ))
            .await?;
    } else if extrinsic_info.pallet == "Balances" && extrinsic_info.call.starts_with("force") {
        // TODO:
        // - test force alerts by checking a historic block with that call
        // - do we want to track burn calls? <https://autonomys.subscan.io/extrinsic/137324-31>
        //   - this is a low priority because it is already covered by balance events
        alert_tx
            .send(Alert::new(
                AlertKind::ForceBalanceTransfer {
                    extrinsic_info: extrinsic_info.clone(),
                    transfer_value,
                },
                mode,
                *block_info,
                node_rpc_url,
            ))
            .await?;
    } else if (extrinsic_info.pallet == "Balances" || transfer_value.is_some())
        && let Some(important_address_kinds) = important_address_kinds
    {
        alert_tx
            .send(Alert::new(
                AlertKind::ImportantAddressTransfer {
                    address_kinds: important_address_kinds,
                    extrinsic_info: extrinsic_info.clone(),
                    // The transfer value can be missing for a transfer_all call.
                    transfer_value,
                },
                mode,
                *block_info,
                node_rpc_url,
            ))
            .await?;
    } else if let Some(initiator_account_kind) = initiator_account_kind {
        alert_tx
            .send(Alert::new(
                AlertKind::ImportantAddressExtrinsic {
                    address_kind: initiator_account_kind.to_string(),
                    extrinsic_info: extrinsic_info.clone(),
                },
                mode,
                *block_info,
                node_rpc_url,
            ))
            .await?;
    }

    if transfer_value.is_none()
        && extrinsic_info.pallet == "Balances"
        && !["transfer_all", "upgrade_accounts"].contains(&extrinsic_info.call.as_str())
    {
        // Every other Balances extrinsic should have an amount.
        // TODO:
        // - check transfer_all by accessing account storage to get the value, this is a low
        //   priority because it is already covered by balance events
        warn!(
            ?mode,
            ?extrinsic_info,
            "Balance: extrinsic amount unavailable in block",
        );
    }

    Ok(Some(extrinsic_info))
}

/// Check an event for alerts.
///
/// Event parsing should never fail, see `check_extrinsic` for more details.
///
/// Any returned errors are fatal and require a restart.
pub async fn check_event(
    // TODO: when we add a check that doesn't work on replayed blocks, skip it using mode
    mode: BlockCheckMode,
    event: &RawEvent,
    block_info: &BlockInfo,
    extrinsic_info: Option<Arc<ExtrinsicInfo>>,
    alert_tx: &mpsc::Sender<Alert>,
    node_rpc_url: &str,
) -> anyhow::Result<()> {
    let event_info = EventInfo::new(event, block_info, extrinsic_info);

    // TODO:
    // - combine extrinsics and events, but only if they are redundant (for example: sudo/sudid)
    // - add tests to make sure we can parse the events for each alert
    // - link event and account to subscan

    // TODO:
    // - track the total of recent events, so the threshold can't be bypassed by splitting the
    //   transfer into multiple calls
    let transfer_value = event_info.transfer_value();
    trace!(?mode, "transfer_value: {:?}", transfer_value);

    let initiator_account_kind = event_info.initiator_account_kind();
    let important_address_kinds = event_info.important_address_kinds_str();
    trace!(
        ?mode,
        ?initiator_account_kind,
        ?important_address_kinds,
        ?event_info,
        ?block_info,
        "event account list",
    );

    // All operator slashes are alerts, and they don't override any other alerts.
    // TODO:
    // - test this alert by checking a historic block with an operator slash event
    // - check the case of these names
    if event_info.pallet == "Domains" && event_info.kind == "OperatorSlashed" {
        alert_tx
            .send(Alert::new(
                AlertKind::OperatorSlashed {
                    event_info: event_info.clone(),
                },
                mode,
                *block_info,
                node_rpc_url,
            ))
            .await?;
    }

    // The initiating account is listed in the event when it is in the `who` field, therefore:
    // - Sudo events override balance transfers by sudo,
    // - Large balance transfers override transfers initiated by important addresses, and
    // - An important address alert is only issued if no other alert is triggered.
    if event_info.pallet == "Sudo" {
        alert_tx
            .send(Alert::new(
                AlertKind::SudoEvent {
                    event_info: event_info.clone(),
                },
                mode,
                *block_info,
                node_rpc_url,
            ))
            .await?;
    } else if let Some(transfer_value) = transfer_value
        && transfer_value >= MIN_BALANCE_CHANGE
    {
        // TODO:
        // - do we want to track burned events? <https://autonomys.subscan.io/event/137324-62>
        alert_tx
            .send(Alert::new(
                AlertKind::LargeBalanceTransferEvent {
                    event_info: event_info.clone(),
                    transfer_value,
                },
                mode,
                *block_info,
                node_rpc_url,
            ))
            .await?;
    } else if transfer_value.is_some()
        && let Some(important_address_kinds) = important_address_kinds
    {
        alert_tx
            .send(Alert::new(
                AlertKind::ImportantAddressTransferEvent {
                    address_kinds: important_address_kinds,
                    event_info: event_info.clone(),
                    // The transfer value shouldn't be missing, but we can't rely on the data
                    // format.
                    transfer_value,
                },
                mode,
                *block_info,
                node_rpc_url,
            ))
            .await?;
    } else if let Some(initiator_account_kind) = initiator_account_kind {
        alert_tx
            .send(Alert::new(
                AlertKind::ImportantAddressEvent {
                    address_kind: initiator_account_kind.to_string(),
                    event_info: event_info.clone(),
                },
                mode,
                *block_info,
                node_rpc_url,
            ))
            .await?;
    }

    Ok(())
}
