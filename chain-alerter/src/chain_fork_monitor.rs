//! Monitoring and alerting for chain forks.

use crate::alerts::{Alert, BlockCheckMode};
use crate::subspace::{BlockInfo, BlockLink, BlockNumber, BlockPosition};
use std::cmp::max;
use std::collections::{BTreeMap, HashMap};
use std::mem;
use std::sync::Arc;
use subxt::utils::H256;
use tokio::sync::mpsc;
use tracing::{debug, info, trace, warn};

/// The buffer size for the chain fork monitor.
pub const CHAIN_FORK_BUFFER_SIZE: usize = 100;

/// The minimum fork depth to alert on.
pub const MIN_FORK_DEPTH: usize = 7;

/// The minimum fork depth to log as info.
pub const MIN_FORK_DEPTH_FOR_INFO_LOG: usize = 3;

/// The maximum number of blocks to replay when there are missed blocks.
pub const MAX_BLOCKS_TO_REPLAY: BlockNumber = 100;

/// The depth after the best tip to prune blocks from the chain fork state.
pub const MAX_BLOCK_DEPTH: usize = 1000;

/// The expected number of blocks in the state.
/// This is an estimate, used for memory optimisation only.
pub const EXPECTED_BLOCK_COUNT: usize = MAX_BLOCK_DEPTH * 5 / 4;

/// The expected number of tips in the state.
/// This is an estimate, used for memory optimisation only.
pub const EXPECTED_TIP_COUNT: usize = EXPECTED_BLOCK_COUNT.saturating_sub(MAX_BLOCK_DEPTH);

/// A chain fork or reorg event.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ChainForkEvent {
    /// A new chain fork was seen, which was not started by a best block.
    NewSideFork {
        /// The tip of the fork.
        tip: Arc<BlockLink>,

        /// The number of blocks from the fork tip to the fork point.
        fork_depth: usize,
    },

    /// A chain fork was extended by a non-best block.
    SideForkExtended {
        /// The tip of the fork.
        tip: Arc<BlockLink>,

        /// The number of blocks from the fork tip to the fork point.
        fork_depth: usize,
    },

    /// A reorg was seen to a best block on a side chain.
    /// This takes priority over fork events.
    Reorg {
        /// The new best block.
        new_best_block: Arc<BlockLink>,

        /// The old best block.
        old_best_block: Arc<BlockLink>,

        /// The number of blocks from the old best block to the reorg point (the fork point with
        /// the new best block).
        old_fork_depth: usize,

        /// The number of blocks from the new best block to the reorg point.
        /// If this is 1, the reorg is to a new side chain fork.
        new_fork_depth: usize,
    },
}

impl ChainForkEvent {
    /// Returns true if the event has a fork depth greater than the minimum fork depth for alerts.
    pub fn needs_alert(&self) -> bool {
        self.largest_fork_depth() >= MIN_FORK_DEPTH
    }

    /// Returns true if the event has a fork depth greater than the minimum fork depth for info
    /// logs.
    pub fn needs_info_log(&self) -> bool {
        self.largest_fork_depth() >= MIN_FORK_DEPTH_FOR_INFO_LOG
    }

    /// Returns the largest fork depth in the event.
    pub fn largest_fork_depth(&self) -> usize {
        match self {
            ChainForkEvent::NewSideFork { fork_depth, .. } => *fork_depth,
            ChainForkEvent::SideForkExtended { fork_depth, .. } => *fork_depth,
            ChainForkEvent::Reorg {
                new_fork_depth,
                old_fork_depth,
                ..
            } => max(*new_fork_depth, *old_fork_depth),
        }
    }
}

// TODO: Forks are rare, so treat the chain as a tree of continuous block segments, rather than a
// tree of blocks. This will improve performance when walking the chain.

/// The chain fork state, indexed for performance.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChainForkState {
    /// A list of block links by block hash.
    /// This lets us walk the chain backwards using parent hashes.
    pub blocks_by_hash: HashMap<H256, Arc<BlockLink>>,

    /// A list of block links by parent height and hash.
    /// This lets us walk the chain forwards using block heights/hashes.
    /// The height is redundant, but it is used for efficient pruning.
    pub blocks_by_parent: BTreeMap<BlockPosition, Vec<Arc<BlockLink>>>,

    /// The tips of each chain fork.
    pub tips_by_hash: HashMap<H256, Arc<BlockLink>>,

    /// The most recent best block, used to detect reorgs.
    /// If there are missed blocks, the first child of the best tip is assumed to be the new best
    /// block.
    // TODO: this could become a H256 index into tips_by_hash
    pub best_tip: Arc<BlockLink>,
}

impl ChainForkState {
    /// Create a new chain fork state from the first block link we've seen.
    pub fn from_first_block(block_info: &BlockInfo) -> Self {
        let block_link = BlockLink::from_block_info(block_info);
        let block_link = Arc::new(block_link);

        let mut blocks_by_hash = HashMap::with_capacity(EXPECTED_BLOCK_COUNT);
        blocks_by_hash.insert(block_link.hash(), block_link.clone());

        let mut tips_by_hash = HashMap::with_capacity(EXPECTED_TIP_COUNT);
        tips_by_hash.insert(block_link.hash(), block_link.clone());

        ChainForkState {
            blocks_by_hash,
            blocks_by_parent: BTreeMap::from([(
                block_link.parent_position(),
                vec![block_link.clone()],
            )]),
            tips_by_hash,
            best_tip: block_link,
        }
    }

    /// Returns true if the given block is on the same chain fork as the supplied fork tip.
    ///
    /// Automatically handles blocks above or below the fork tip.
    /// Assumes that the chain is connected, and there are no missing blocks.
    pub fn is_on_same_fork(&self, fork_tip: &BlockLink, block: &BlockLink) -> bool {
        // Starting at the higher block, walk the chain backwards until we find the target block.
        let (mut current_block, target_block) = if block.height() > fork_tip.height() {
            (block, fork_tip)
        } else {
            (fork_tip, block)
        };

        let target_hash = target_block.hash();
        let target_height = target_block.height();

        loop {
            if current_block.hash() == target_hash {
                return true;
            }

            if current_block.height() <= target_height {
                // We're below the best tip, so this block is not on the best fork.
                return false;
            }

            if let Some(parent_block) = self.blocks_by_hash.get(&current_block.parent_hash) {
                current_block = parent_block;
            } else {
                // We've reached the end of the chain, so this block is not on the best fork.
                return false;
            }
        }
    }

    /// Returns true if the given block is on the same chain fork as the best tip.
    ///
    /// Automatically handles blocks above or below the best tip.
    /// Assumes that the chain is connected, and there are no missing blocks.
    #[expect(dead_code, reason = "included for completeness")]
    pub fn is_on_best_fork(&self, block: &BlockLink) -> bool {
        self.is_on_same_fork(&self.best_tip, block)
    }

    /// Returns the fork tip a given block is on, if it exists.
    ///
    /// Automatically handles blocks above or below the tips.
    /// Assumes that the chain is connected, and there are no missing blocks.
    pub fn find_fork_tip(&self, block: &BlockLink) -> Option<&Arc<BlockLink>> {
        self.tips_by_hash
            .values()
            .find(|&tip| self.is_on_same_fork(tip, block))
    }

    /// Calculate the depth of a fork.
    pub fn fork_depth(&self, block_link: &BlockLink) -> usize {
        let mut depth = 1;
        let mut current_block = block_link;

        loop {
            // Find the blocks with the same parent as this block.
            let sibling_blocks = self
                .blocks_by_parent
                .get(&current_block.parent_position())
                .expect("always contains this block");

            if sibling_blocks.len() > 1 {
                // Chain is forked at the parent block, return this depth.
                return depth;
            }

            // Find the parent block and move back to it.
            if let Some(parent_block) = self.blocks_by_hash.get(&current_block.parent_hash) {
                current_block = parent_block;
            } else {
                // Chain is disconnected, just return the available depth.
                warn!(
                    ?block_link,
                    ?current_block,
                    "Chain is disconnected, returning minimum fork depth: {}",
                    depth
                );
                return depth;
            }
            depth += 1;
        }
    }

    /// Add a new block link to the chain fork state.
    /// Blocks are skipped if they are duplicates, or below the minimum allowed chain state height.
    pub fn add_block_link(
        &mut self,
        block_info: &BlockInfo,
        is_best_block: bool,
    ) -> Option<ChainForkEvent> {
        if self.blocks_by_hash.contains_key(&block_info.hash()) {
            trace!(
                ?is_best_block,
                ?block_info,
                "Block is already in the chain fork state, ignoring",
            );
            return None;
        } else {
            let min_allowed_height = self.min_allowed_height();
            if block_info.height() < min_allowed_height {
                trace!(
                    ?min_allowed_height,
                    ?is_best_block,
                    ?block_info,
                    "Block is below the minimum allowed height, ignoring",
                );
                return None;
            }
        }

        let block_link = BlockLink::from_block_info(block_info);
        let block_link = Arc::new(block_link);

        // Add the block link to the chain.
        self.blocks_by_hash
            .insert(block_link.hash(), block_link.clone());
        self.blocks_by_parent
            .entry(block_link.parent_position())
            .or_default()
            .push(block_link.clone());

        let mut is_new_side_fork = false;
        let mut is_side_fork_extended = false;

        // If the tips contain an ancestor of this block, replace it with this new tip.
        if let Some(fork_tip) = self.find_fork_tip(&block_link) {
            is_side_fork_extended = fork_tip != &self.best_tip;
            trace!(
                ?is_side_fork_extended,
                ?is_best_block,
                ?block_link,
                ?fork_tip,
                ?self.best_tip,
                "Block is on an existing fork",
            );

            self.tips_by_hash.remove(&fork_tip.hash());
            self.tips_by_hash
                .insert(block_link.hash(), block_link.clone());
        } else {
            // Otherwise, add a new tip, whether or not it connects to the chain.
            // (If a lot of blocks are missing, the new tip will be disconnected.)
            if !self.blocks_by_hash.contains_key(&block_link.parent_hash) {
                warn!(
                    ?is_best_block,
                    ?block_link,
                    "Block is not connected to the chain, adding as a new fork tip anyway",
                );
            }

            is_new_side_fork = true;
            trace!(
                ?is_new_side_fork,
                ?is_best_block,
                ?block_link,
                "Block is a new fork tip",
            );

            self.tips_by_hash
                .insert(block_link.hash(), block_link.clone());
        }

        // Reorgs are best blocks which aren't a descendant of the current best block.
        // Our skipped block replay makes sure we don't get disconnected blocks.
        let is_reorg = is_best_block && (is_side_fork_extended || is_new_side_fork);

        // TODO: If there is a reorg, all unchecked blocks on the new best fork are checked for
        // alerts.
        let event = if is_reorg {
            Some(ChainForkEvent::Reorg {
                new_best_block: block_link.clone(),
                old_best_block: self.best_tip.clone(),
                old_fork_depth: self.fork_depth(&self.best_tip),
                new_fork_depth: self.fork_depth(&block_link),
            })
        } else if is_new_side_fork {
            // TODO: check new and extended side forks above a threshold amount for stealing attacks
            Some(ChainForkEvent::NewSideFork {
                tip: block_link.clone(),
                // New forks are always 1 block deep, assuming they connect to the chain at all.
                fork_depth: 1,
            })
        } else if is_side_fork_extended {
            Some(ChainForkEvent::SideForkExtended {
                tip: block_link.clone(),
                fork_depth: self.fork_depth(&block_link),
            })
        } else {
            None
        };

        // Update the best block if needed.
        if is_best_block {
            self.best_tip = block_link.clone();
        }

        event
    }

    /// Returns the minimum permitted height in the chain fork state.
    pub fn min_allowed_height(&self) -> BlockNumber {
        self.best_tip
            .height()
            .saturating_sub(u32::try_from(MAX_BLOCK_DEPTH).expect("constant is small"))
    }

    /// Prune blocks from the chain fork state.
    pub fn prune_blocks(&mut self) {
        // If we don't have enough parent blocks to start pruning, exit early.
        if self.blocks_by_parent.len() <= MAX_BLOCK_DEPTH {
            return;
        }

        let best_block_height = self.best_tip.height();
        let height_threshold = self.min_allowed_height();
        debug!(
            ?best_block_height,
            ?height_threshold,
            "Pruning blocks from the chain fork state",
        );

        // There's no split below method, so we split above, then swap.
        let mut blocks_to_prune = self
            .blocks_by_parent
            .split_off(&BlockPosition::min_for_height_range(height_threshold));
        mem::swap(&mut self.blocks_by_parent, &mut blocks_to_prune);

        for blocks in blocks_to_prune.values() {
            for block in blocks {
                self.prune_block(block);
            }
        }
    }

    /// Prune a single block from the chain fork state.
    fn prune_block(&mut self, block_link: &BlockLink) {
        self.blocks_by_hash.remove(&block_link.hash());
        self.tips_by_hash.remove(&block_link.hash());

        // If a block is being pruned, all its siblings will also be pruned.
        // This is currently redundant, but it's here for completeness.
        self.blocks_by_parent.remove(&block_link.parent_position());
    }
}

/// The message sent when the alerter sees a block.
/// Used to detect reorgs and forks.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum BlockSeen {
    /// A new block has been seen, and we know it is the best block.
    /// These blocks can come from the best or all blocks subscriptions.
    BestBlock(BlockInfo),

    /// A new block has been seen, which is not the best block.
    /// This block might be treated as the best block if it is received early enough.
    /// These blocks only come from the all blocks subscription.
    AnyBlock(BlockInfo),
}

impl BlockSeen {
    /// Create a new `BestBlock` received message from a block link.
    pub fn from_best_block(block_info: BlockInfo) -> Self {
        BlockSeen::BestBlock(block_info)
    }

    /// Create a new `AnyBlock` received message from a block link.
    pub fn from_any_block(block_info: BlockInfo) -> Self {
        BlockSeen::AnyBlock(block_info)
    }

    /// Returns the block link for this message.
    pub fn block_info(&self) -> BlockInfo {
        match self {
            BlockSeen::BestBlock(block_info) => *block_info,
            BlockSeen::AnyBlock(block_info) => *block_info,
        }
    }

    /// Returns true if this is the best block.
    pub fn is_best_block(&self) -> bool {
        matches!(self, BlockSeen::BestBlock(_))
    }
}

/// A task that monitors chain forks and reorgs, using blocks from multiple sources.
/// TODO:
/// - at startup, load ~100 recent blocks without alerting, to avoid disconnected forks
/// - write tests for this, or for ChainForkState
pub async fn check_for_chain_forks(
    mut new_blocks_rx: mpsc::Receiver<(BlockCheckMode, BlockSeen)>,
    alert_tx: mpsc::Sender<Alert>,
) {
    let Some((_first_mode, first_block)) = new_blocks_rx.recv().await else {
        info!("Channel disconnected before first block, exiting");
        return;
    };

    // The first block is always a new tip, so we ignore that event.
    let mut state = ChainForkState::from_first_block(&first_block.block_info());

    while let Some((mode, block_seen)) = new_blocks_rx.recv().await {
        let event = state.add_block_link(&block_seen.block_info(), block_seen.is_best_block());
        if let Some(event) = event {
            if event.needs_info_log() {
                info!(?event, "Chain fork or reorg event");
            } else {
                debug!(?event, "Chain fork or reorg event");
            }

            if event.needs_alert() {
                // If we have restarted, the new alerter will replay missed blocks, so we can ignore
                // any send errors to dropped channels. This also avoids panics during process
                // shutdown.
                let _ = alert_tx
                    .send(Alert::from_chain_fork_event(
                        event,
                        block_seen.block_info(),
                        mode,
                    ))
                    .await;
            }
        }

        state.prune_blocks();
    }

    debug!("Channel disconnected, exiting");
}
