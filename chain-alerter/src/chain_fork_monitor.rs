//! Monitoring and alerting for chain forks.

use crate::alerts::{Alert, BlockCheckMode};
use crate::subspace::{
    BlockHash, BlockInfo, BlockLink, BlockNumber, BlockPosition, PARENT_OF_GENESIS, RawEventList,
    RawExtrinsicList, RpcClientList, block_full_from_hash, node_best_block_hash,
};
use static_assertions::const_assert;
use std::cmp::max;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt::{self, Display, FormattingOptions};
use std::mem;
use std::sync::Arc;
use tokio::sync::mpsc;
use tracing::{debug, info, trace, warn};

/// The channel buffer size for the chain fork monitor.
/// When the buffer is full, the block subscription tasks will wait until the monitor has processed
/// some blocks.
///
/// This is used for performance and memory optimisation only.
pub const CHAIN_FORK_BUFFER_SIZE: usize = 50;

/// The minimum fork depth to alert on.
///
/// This is set to match subscan.io's "unfinalized" status. It is possible for subscan.io to show a
/// block as "finalized" when it really isn't, so we want to trigger an alert when a subscan.io
/// "finalized" block is reorged away from.
pub const MIN_FORK_DEPTH: usize = 6;

/// The minimum fork depth to log as info.
pub const MIN_FORK_DEPTH_FOR_WARN_LOG: usize = 5;

/// The minimum fork depth to log as info.
pub const MIN_FORK_DEPTH_FOR_INFO_LOG: usize = 4;

/// The minimum number of blocks to alert on for backwards reorgs.
/// Currently we alert on all backwards reorgs.
pub const MIN_BACKWARDS_REORG_DEPTH: usize = 1;

/// The minimum number of blocks to log as info for backwards reorgs.
/// Currently we log all backwards reorgs.
pub const MIN_BACKWARDS_REORG_DEPTH_FOR_WARN_LOG: usize = 1;

// Alerting without logging and warning without info-ing don't make sense.
const_assert!(MIN_FORK_DEPTH >= MIN_FORK_DEPTH_FOR_WARN_LOG);
const_assert!(MIN_FORK_DEPTH_FOR_WARN_LOG >= MIN_FORK_DEPTH_FOR_INFO_LOG);
const_assert!(MIN_BACKWARDS_REORG_DEPTH >= MIN_BACKWARDS_REORG_DEPTH_FOR_WARN_LOG);

/// The maximum number of blocks to keep in the chain fork state.
///
/// Synced nodes should very rarely get blocks further from the tip than this, to avoid spurious
/// side forks.
///
/// TODO: maybe make this configurable, but we'd need to test the minimum number required to avoid
/// the bug
pub const MAX_BLOCK_DEPTH: usize = 1000;

// We must keep enough blocks in the state to detect forks.
const_assert!(MAX_BLOCK_DEPTH > MIN_FORK_DEPTH);

/// The expected number of blocks in the state.
/// This is an estimate, used for memory optimisation only.
pub const ESTIMATED_BLOCK_COUNT: usize = MAX_BLOCK_DEPTH * 5 / 4;

/// The expected number of tips in the state.
/// This is an estimate, used for memory optimisation only.
pub const ESTIMATED_TIP_COUNT: usize = ESTIMATED_BLOCK_COUNT.saturating_sub(MAX_BLOCK_DEPTH);

/// The message sent when the fork monitor has loaded a best block and its ancestors.
///
/// TODO: make this into a struct
pub type NewBestBlockMessage = (
    BlockCheckMode,
    BlockInfo,
    RawExtrinsicList,
    RawEventList,
    // The RPC node that received the block.
    String,
);

/// Errors encountered when trying to add a block to the chain fork state.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum AddBlockError {
    /// The parent block needs to be added to the state before this block can be added.
    MissingParentBlock,

    /// The block is already in the state.
    AlreadyInState,

    /// The block is below the minimum allowed height from the best tip.
    /// Any ancestors can't be in the state either.
    HeightTooLow,

    /// The block hash is a placeholder for the genesis parent block, which does not exist.
    ParentOfGenesis,
}

impl AddBlockError {
    /// Returns true if the parent block should be fetched and added to the state.
    /// Returns false if the block and its ancestors can't be added to the state.
    pub fn needs_parent_block(self) -> bool {
        match self {
            AddBlockError::MissingParentBlock => true,
            AddBlockError::AlreadyInState
            | AddBlockError::HeightTooLow
            | AddBlockError::ParentOfGenesis => false,
        }
    }

    /// Returns true if the error is serious (a likely bug).
    pub fn log_error(self) -> bool {
        match self {
            // An unexpected error which indicates a bug or invalid RPC data.
            AddBlockError::ParentOfGenesis => true,
            // Routine errors which happen during normal operation.
            AddBlockError::MissingParentBlock
            | AddBlockError::AlreadyInState
            | AddBlockError::HeightTooLow => false,
        }
    }
}

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
            || self.backwards_reorg_depth().unwrap_or_default() >= MIN_BACKWARDS_REORG_DEPTH
    }

    /// Returns true if the event has a fork depth greater than the minimum fork depth for warn
    /// logs.
    pub fn needs_warn_log(&self) -> bool {
        self.largest_fork_depth() >= MIN_FORK_DEPTH_FOR_WARN_LOG
            || self.backwards_reorg_depth().unwrap_or_default()
                >= MIN_BACKWARDS_REORG_DEPTH_FOR_WARN_LOG
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

    /// Returns the number of blocks that the reorg went backwards by.
    /// Reorgs to a lower height are possible but rare.
    pub fn backwards_reorg_depth(&self) -> Option<usize> {
        match self {
            ChainForkEvent::Reorg {
                new_fork_depth,
                old_fork_depth,
                ..
            } if old_fork_depth > new_fork_depth => Some(old_fork_depth - new_fork_depth),
            // Reorg to an equal or higher block, so it didn't go backwards.
            ChainForkEvent::Reorg { .. } => None,
            // Not a reorg, so no backwards depth.
            ChainForkEvent::NewSideFork { .. } | ChainForkEvent::SideForkExtended { .. } => None,
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
    pub blocks_by_hash: HashMap<BlockHash, Arc<BlockLink>>,

    /// A list of block links by parent height and hash.
    /// This lets us walk the chain forwards using block heights/hashes.
    /// The height is redundant, but it is used for efficient pruning.
    pub blocks_by_parent: BTreeMap<BlockPosition, BTreeSet<Arc<BlockLink>>>,

    /// The tips of each chain fork.
    pub tips_by_hash: HashMap<BlockHash, Arc<BlockLink>>,

    /// The most recent best block, used to detect reorgs.
    /// If there are missed blocks, the first child of the best tip is assumed to be the new best
    /// block.
    // TODO: this could become a BlockHash index into tips_by_hash
    pub best_tip: Arc<BlockLink>,
}

impl Display for ChainForkState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut f = f.with_options(FormattingOptions::default().alternate(true).to_owned());

        f.debug_struct("ChainForkState")
            .field("blocks_by_hash", &self.blocks_by_hash.len())
            .field("blocks_by_parent", &self.blocks_by_parent.len())
            .field("tips_by_hash", &self.tips_by_hash)
            .field("best_tip", &self.best_tip)
            .finish()
    }
}

impl ChainForkState {
    /// Create a new chain fork state from the first block link we've seen.
    pub fn from_first_block(block: Arc<BlockLink>) -> Self {
        let mut blocks_by_hash = HashMap::with_capacity(ESTIMATED_BLOCK_COUNT);
        blocks_by_hash.insert(block.hash(), block.clone());

        let mut tips_by_hash = HashMap::with_capacity(ESTIMATED_TIP_COUNT);
        tips_by_hash.insert(block.hash(), block.clone());

        ChainForkState {
            blocks_by_hash,
            blocks_by_parent: BTreeMap::from([(
                block.parent_position(),
                BTreeSet::from([block.clone()]),
            )]),
            tips_by_hash,
            best_tip: block,
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
    pub fn is_on_best_fork(&self, block: &BlockLink) -> bool {
        self.is_on_same_fork(&self.best_tip, block)
    }

    /// Returns the tips of all recent chain forks.
    pub fn fork_tips(&self) -> BTreeSet<&Arc<BlockLink>> {
        self.tips_by_hash.values().collect()
    }

    /// Returns the tips of the forks a given block is on, if there are any.
    ///
    /// Automatically handles blocks above or below the tip.
    /// Assumes that the chain is connected, and there are no missing blocks.
    ///
    /// Should only return one tip, except at startup, or when there are large gaps in the chain.
    pub fn find_fork_tips(&self, block: &BlockLink) -> BTreeSet<Arc<BlockLink>> {
        self.tips_by_hash
            .values()
            .filter(|tip| self.is_on_same_fork(tip, block))
            .cloned()
            .collect()
    }

    /// Calculate the depth of a fork.
    /// `mode` is only used for logging.
    pub fn fork_depth(
        &self,
        block: &BlockLink,
        mode: impl Into<Option<BlockCheckMode>> + Copy,
    ) -> usize {
        let mut depth = 1;
        let mut current_block = block;

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
                let mode = mode.into();
                if let Some(mode) = mode
                    && mode.is_startup()
                {
                    trace!(
                        ?mode,
                        ?block,
                        ?current_block,
                        "Chain is disconnected, returning minimum fork depth: {} (this is expected at startup)",
                        depth
                    );
                } else {
                    info!(
                        ?mode,
                        ?block,
                        ?current_block,
                        "Chain is disconnected, returning minimum fork depth: {}",
                        depth
                    );
                }
                return depth;
            }
            depth += 1;
        }
    }

    /// Checks if the supplied block can be added to the chain fork state.
    ///
    /// If the block can be added immediately, returns `Ok(())`.
    /// Either the parent block is in the state, or the block will automatically be treated as a new
    /// tip. (Because the parent would be pruned from the state, or it is the genesis block.)
    ///
    /// Returns `Err(AddBlockError)` if the block can't be added immediately. Check
    /// `needs_parent_block()` to see if the parent block needs to be added first. If it returns
    /// false, then the block and its ancestors can't be added to the state.
    pub fn can_add_block(&self, block: &BlockLink) -> Result<(), AddBlockError> {
        if self.blocks_by_hash.contains_key(&block.hash()) {
            trace!(?block, "Block is already in the chain fork state, ignoring",);
            return Err(AddBlockError::AlreadyInState);
        }

        let min_allowed_height = self.min_allowed_height();

        // Either it has a parent, or it doesn't need one because it's very far from the tip (or
        // genesis).
        if self.blocks_by_hash.contains_key(&block.parent_hash)
            || block.height() == min_allowed_height
            || block.parent_hash == PARENT_OF_GENESIS
        {
            return Ok(());
        }

        // Would be ignored if we tried to add it, so stop getting ancestors.
        // This implies the parent can't be in blocks_by_hash.
        if block.height() < min_allowed_height {
            trace!(
                ?min_allowed_height,
                ?block,
                "Block is below the minimum allowed height, ignoring",
            );
            return Err(AddBlockError::HeightTooLow);
        }

        // The parent of the genesis block does not exist, so trying to add it is pointless.
        if block.hash() == PARENT_OF_GENESIS {
            trace!(
                ?block,
                "Block hash is the parent of the genesis block, ignoring",
            );
            return Err(AddBlockError::ParentOfGenesis);
        }

        // Above the minimum height, and needs a parent block.
        Err(AddBlockError::MissingParentBlock)
    }

    /// Add a new block link to the chain fork state, if is allowed in the state.
    /// See `can_add_block()` for more details.
    ///
    /// `mode` is only used for logging and invariant checks.
    #[expect(clippy::unwrap_in_result, reason = "panic can't actually happen")]
    pub fn add_block(
        &mut self,
        mode: impl Into<Option<BlockCheckMode>> + Copy,
        is_best_block: bool,
        block: &Arc<BlockLink>,
    ) -> Result<Option<ChainForkEvent>, AddBlockError> {
        self.can_add_block(block)?;

        let old_best_block = self.best_tip.clone();

        // Add the block link to the chain.
        self.blocks_by_hash.insert(block.hash(), block.clone());
        self.blocks_by_parent
            .entry(block.parent_position())
            .or_default()
            .insert(block.clone());

        let mut is_new_side_fork = false;
        let mut is_side_fork_extended = false;

        // Replace all the tips on the same fork with the highest block in the tip set.
        let mut replaced_best_tip = false;
        let mut fork_tips = self.find_fork_tips(block);
        if !fork_tips.is_empty() {
            is_side_fork_extended = !fork_tips.contains(&self.best_tip);

            fork_tips.insert(block.clone());
            let highest_tip = fork_tips.pop_last().expect("just inserted a block");

            for fork_tip in fork_tips {
                if fork_tip.height() < highest_tip.height() {
                    if fork_tip == self.best_tip {
                        replaced_best_tip = true;
                    }
                    trace!(
                        ?is_side_fork_extended,
                        ?is_best_block,
                        ?replaced_best_tip,
                        ?fork_tip,
                        ?highest_tip,
                        ?block,
                        ?self.best_tip,
                        "Replacing lower fork tip with higher fork tip on same chain fork",
                    );
                    self.tips_by_hash.remove(&fork_tip.hash());
                }
            }

            // Now restore the tips and best tips invariants.
            if replaced_best_tip || is_best_block {
                self.best_tip = highest_tip.clone();
            }
            self.tips_by_hash.insert(highest_tip.hash(), highest_tip);
        } else {
            // Otherwise, add a new tip, whether or not it connects to the chain.
            // (If the new tip is at the minimum allowed height, it is added disconnected.)
            if !self.blocks_by_hash.contains_key(&block.parent_hash) {
                // This indicates a large gap or a bug in the fork monitor.
                info!(
                    ?is_best_block,
                    ?block,
                    "Block is not connected to the chain, adding as a new fork tip anyway \
                    (this is expected at startup)",
                );
            }

            is_new_side_fork = true;
            trace!(
                ?is_new_side_fork,
                ?is_best_block,
                ?block,
                "Block is a new fork tip",
            );

            // Keep tips and best tips in sync.
            if is_best_block {
                self.best_tip = block.clone();
            }
            self.tips_by_hash.insert(block.hash(), block.clone());
        }

        // Reorgs are best blocks which aren't a descendant of the current best block.
        // Our skipped block replay makes sure we don't get disconnected blocks.
        let is_reorg = is_best_block && (is_side_fork_extended || is_new_side_fork);

        let mode = mode.into();
        let event = if is_reorg {
            Some(ChainForkEvent::Reorg {
                new_best_block: block.clone(),
                old_fork_depth: self.fork_depth(&old_best_block, mode),
                new_fork_depth: self.fork_depth(block, mode),
                old_best_block,
            })
        } else if is_new_side_fork {
            // TODO: check new and extended side forks above a threshold amount for stealing attacks
            Some(ChainForkEvent::NewSideFork {
                tip: block.clone(),
                // New forks are always 1 block deep, assuming they connect to the chain at all.
                fork_depth: 1,
            })
        } else if is_side_fork_extended {
            Some(ChainForkEvent::SideForkExtended {
                tip: block.clone(),
                fork_depth: self.fork_depth(block, mode),
            })
        } else {
            None
        };

        if mode.is_none_or(|m| !m.is_startup()) {
            self.check_invariants();
        }

        Ok(event)
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
        trace!(
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

        self.check_invariants();
    }

    /// Prune a single block from the chain fork state.
    fn prune_block(&mut self, block: &BlockLink) {
        self.blocks_by_hash.remove(&block.hash());
        self.tips_by_hash.remove(&block.hash());

        // If a block is being pruned, all its siblings will also be pruned.
        // This is currently redundant, but it's here for completeness.
        self.blocks_by_parent.remove(&block.parent_position());
    }

    /// Check data structure invariants, which are valid after each `prune_blocks()`.
    /// (And after each `add_block()`, except at startup.)
    pub fn check_invariants(&self) {
        assert_eq!(
            self.tips_by_hash.get(&self.best_tip.hash()),
            Some(&self.best_tip),
            "best tip missing from tips by hash: {self}",
        );

        // There can't be duplicates in a HashMap or BTreeSet. So this is just a check that the
        // blocks in both collections are the same.
        // TODO: if this impacts performance, only do it in debug builds using debug_assert!().
        assert_eq!(
            self.blocks_by_hash.values().collect::<BTreeSet<_>>(),
            self.blocks_by_parent
                .values()
                .flatten()
                .collect::<BTreeSet<_>>(),
            "blocks by hash and by parent should be identical: {self:?}",
        );
    }
}

/// A block seen message, containing the block and the node RPC URL that provided it.
pub type BlockSeenMessage = (
    BlockSeen,
    // The node RPC URL that provided the block.
    String,
);

/// The message sent when a node subscription sees a block.
/// Used to detect reorgs and forks.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BlockSeen {
    /// A new block has been seen, and we know it is a best block (part of a fork containing
    /// the best block).
    ///
    /// These blocks come from the best blocks subscription.
    IsBestBlock(Arc<BlockLink>),

    /// A new block has been seen, and we don't know if it is a best block yet.
    /// This block might be treated as the best block if it is received early enough.
    ///
    /// These blocks come from the all blocks subscription.
    MaybeBestBlock(Arc<BlockLink>),
}

impl BlockSeen {
    /// Create a new `BestBlock` received message for a single block.
    pub fn from_best_block(block: Arc<BlockLink>) -> Self {
        Self::IsBestBlock(block)
    }

    /// Create a new `AnyBlock` received message for a single block.
    pub fn from_any_block(block: Arc<BlockLink>) -> Self {
        Self::MaybeBestBlock(block)
    }

    /// Returns the block links for this message.
    pub fn into_block(self) -> Arc<BlockLink> {
        match self {
            Self::IsBestBlock(block) => block,
            Self::MaybeBestBlock(block) => block,
        }
    }

    /// Returns true if this is definitely a best block.
    /// Returns false if we don't know if it is a best block.
    pub fn is_best_block(&self) -> bool {
        matches!(self, Self::IsBestBlock(_))
    }
}

/// Send a new block to the other alert checks.
pub async fn send_best_fork_block(
    mode: BlockCheckMode,
    is_best_block: bool,
    block_info: BlockInfo,
    extrinsics: RawExtrinsicList,
    events: RawEventList,
    best_fork_tx: &mpsc::Sender<NewBestBlockMessage>,
    node_rpc_url: &str,
) -> anyhow::Result<()> {
    if is_best_block {
        best_fork_tx
            .send((
                mode,
                block_info,
                extrinsics,
                events,
                node_rpc_url.to_string(),
            ))
            .await?;
    }

    Ok(())
}

/// Add a block and any missing ancestors to the chain fork state, sending alerts as needed.
pub async fn add_blocks_to_chain_fork_state(
    rpc_client_list: &RpcClientList,
    state: &mut ChainForkState,
    mode: BlockCheckMode,
    block_seen: BlockSeenMessage,
    best_fork_tx: &mpsc::Sender<NewBestBlockMessage>,
    alert_tx: &mpsc::Sender<Alert>,
) -> anyhow::Result<()> {
    let (block_seen, block_seen_node_rpc_url) = block_seen;
    let mut is_best_block = block_seen.is_best_block();
    let block = block_seen.into_block();

    let can_add_block = state.can_add_block(&block);
    if let Err(err) = can_add_block
        && !err.needs_parent_block()
    {
        trace!(
            ?block,
            ?is_best_block,
            ?err,
            node_rpc_url = ?block_seen_node_rpc_url,
            "Block can't be added to the chain fork state, ignoring",
        );
        return Ok(());
    }

    let mut pending_blocks = BTreeMap::from([(block, block_seen_node_rpc_url.to_string())]);

    while let Some((block, block_node_rpc_url)) = pending_blocks.pop_first() {
        let mode = if pending_blocks.is_empty() {
            mode
        } else {
            // There is at least one pending block ahead of us, so we must be a replayed block.
            mode.during_replay()
        };

        // We can add the block, but we don't know if this is the best block yet.
        if state.can_add_block(&block).is_ok() && !is_best_block {
            // First, check if the block is a descendant of the best block.
            // This is a common case, where we can avoid an RPC call to fetch the best block hash.
            if state.is_on_best_fork(&block) {
                is_best_block = true;
            } else {
                let best_block_hash = node_best_block_hash(rpc_client_list.raw_primary()).await?;
                // Handle a multiple server race condition, where the primary server has the parent
                // of the best block, but doesn't have the next block yet.
                //
                // TODO: this could be expensive for a long side chain, if that happens a lot, turn
                // is_best_block into a Yes/No/Unknown tri-state, and only RPC if Unknown.
                is_best_block =
                    block.hash() == best_block_hash || block.parent_hash == best_block_hash;

                trace!(
                    ?block,
                    ?best_block_hash,
                    ?is_best_block,
                    node_rpc_url = ?block_node_rpc_url,
                    "Checked if an all blocks subscription sent the best block",
                );
            }
        }

        let event = state.add_block(mode, is_best_block, &block);

        match event {
            // Block was successfully added to the state, and there was a chain fork change or
            // reorg.
            // Continue to add any descendant blocks.
            // TODO: silence these startup logs at a lower level (they only seem to happen with
            // multiple RPC servers)
            Ok(Some(event)) => {
                if event.needs_warn_log() && !mode.is_startup() {
                    warn!(?mode, ?event, node_rpc_url = ?block_node_rpc_url, "Chain fork or reorg event");
                } else if event.needs_info_log() && !mode.is_startup() {
                    info!(?mode, ?event, node_rpc_url = ?block_node_rpc_url, "Chain fork or reorg event");
                } else {
                    debug!(?mode, ?event, node_rpc_url = ?block_node_rpc_url, "Chain fork or reorg event");
                }

                let block_info = if is_best_block {
                    // Get the block info for the best block checker.
                    // Delaying fetching this info saves a lot of work during initial block context
                    // and replays.

                    // TODO: only get extrinsics and events if
                    // TODO: if the node has pruned this block, just send the alert without it
                    let (block, extrinsics, events) =
                        block_full_from_hash(block.hash(), true, rpc_client_list).await?;
                    let block_info =
                        BlockInfo::new(&block, &extrinsics, &rpc_client_list.genesis_hash());

                    send_best_fork_block(
                        mode,
                        is_best_block,
                        block_info,
                        extrinsics,
                        events.expect("asked for events"),
                        best_fork_tx,
                        &block_node_rpc_url,
                    )
                    .await?;

                    Some(block_info)
                } else {
                    None
                };

                // We deliberately create spurious forks during startup, while populating the
                // history.
                // TODO: fix this at a lower level in the init, startup, or fork detection code
                // instead
                if event.needs_alert() && !mode.is_startup() {
                    let block_info = if let Some(block_info) = block_info {
                        block_info
                    } else {
                        BlockInfo::with_block_hash(block.hash(), rpc_client_list).await?
                    };

                    alert_tx
                        .send(Alert::from_chain_fork_event(
                            event,
                            mode,
                            block_info,
                            &block_node_rpc_url,
                        ))
                        .await?;
                }
            }

            // Block was successfully added to the state, but there were no events.
            // Continue to add any descendant blocks.
            Ok(None) => {
                trace!(
                    ?mode,
                    ?block,
                    "Block was successfully added to the chain fork state",
                );

                if is_best_block {
                    let (block, extrinsics, events) =
                        block_full_from_hash(block.hash(), true, rpc_client_list).await?;
                    let block_info =
                        BlockInfo::new(&block, &extrinsics, &rpc_client_list.genesis_hash());

                    send_best_fork_block(
                        mode,
                        is_best_block,
                        block_info,
                        extrinsics,
                        events.expect("asked for events"),
                        best_fork_tx,
                        &block_node_rpc_url,
                    )
                    .await?;
                }
            }

            // Block couldn't be added to the state yet, we need some ancestor blocks first.
            // Add this block and its parent to the pending blocks.
            Err(error) if error.needs_parent_block() => {
                // Mode could be incorrect here, so we don't log it.
                trace!(
                    ?block,
                    ?error,
                    pending_block_count = %pending_blocks.len(),
                    node_rpc_url = ?block_node_rpc_url,
                    "Block needs ancestors before being added to the chain fork state",
                );

                // Put it back, and add its parent.
                // The insertion order doesn't matter here, because we're using a BTreeSet.
                // TODO: if the node has pruned this block, just stop here and insert the rest of
                // the pending blocks
                let (parent_block, parent_block_node_rpc_url) =
                    BlockLink::with_block_hash(block.parent_hash, rpc_client_list).await?;
                let parent_block = Arc::new(parent_block);

                if parent_block.height() >= block.height() {
                    warn!(
                        ?block,
                        ?parent_block,
                        "Block has a parent at equal or greater height, ignoring all pending replayed blocks",
                    );
                    break;
                }

                pending_blocks.insert(parent_block, parent_block_node_rpc_url);
                pending_blocks.insert(block, block_node_rpc_url);
            }

            // Block and its ancestors can never be added to the state, so skip it and all pending
            // blocks.
            Err(error) => {
                if error.log_error() {
                    warn!(
                        ?mode,
                        ?error,
                        ?block,
                        "Ignoring invalid block data from the node, and all pending replayed blocks",
                    );
                } else {
                    trace!(
                        ?mode,
                        ?error,
                        ?block,
                        "Block can't be added to the chain fork state, ignoring all pending replayed blocks",
                    );
                }

                // When walking backwards from a valid block, we should always get to a minimum
                // height or genesis block, then add it and all its descendants. So if we get to
                // this point, we want to discard all pending blocks.
                break;
            }
        }
    }

    Ok(())
}

/// A task that monitors chain forks and reorgs, using blocks from multiple sources.
/// TODO:
/// - write tests for this, or for ChainForkState
pub async fn check_for_chain_forks(
    rpc_client_list: RpcClientList,
    mut new_blocks_rx: mpsc::Receiver<BlockSeenMessage>,
    best_fork_tx: mpsc::Sender<NewBestBlockMessage>,
    alert_tx: mpsc::Sender<Alert>,
) -> anyhow::Result<()> {
    // The first block is always a new tip, and the alert is ignored, so we don't need the node RPC
    // URL.
    let Some((first_block, first_block_node_rpc_url)) = new_blocks_rx.recv().await else {
        info!("Channel disconnected before first block, exiting");
        return Ok(());
    };

    let is_best_block = first_block.is_best_block();
    let first_block = first_block.into_block();

    // We need to fetch the full fork monitor context. We can re-use existing code by initialising
    // the state with the first block as the tip, then adding its parent block as well.
    // This will fetch the context for the parent block (and therefore the first block), and fix up
    // the duplicate tips as needed.
    let (parent_of_first_block, parent_of_first_block_node_rpc_url) =
        BlockLink::with_block_hash(first_block.parent_hash, &rpc_client_list).await?;
    let parent_of_first_block = Arc::new(parent_of_first_block);
    // We treat the context blocks as best blocks, because that simplifies the code.
    let parent_of_first_block = BlockSeen::from_best_block(parent_of_first_block);

    // The first block is always a new tip, so we ignore that alert.
    let mut state = ChainForkState::from_first_block(first_block.clone());

    // The context blocks shouldn't have any alerts, because we've gone back using a single chain.
    info!(
        ?is_best_block,
        ?first_block,
        ?first_block_node_rpc_url,
        ?rpc_client_list,
        "Adding {MAX_BLOCK_DEPTH} previous blocks to chain fork state, this may take some time...",
    );

    add_blocks_to_chain_fork_state(
        &rpc_client_list,
        &mut state,
        BlockCheckMode::Startup,
        (
            parent_of_first_block,
            parent_of_first_block_node_rpc_url.clone(),
        ),
        &best_fork_tx,
        &alert_tx,
    )
    .await?;
    state.prune_blocks();

    // Check we populated the state correctly at startup.
    assert_eq!(
        state.best_tip, first_block,
        "wrong best tip after startup: {state}"
    );
    assert_eq!(
        state.fork_tips(),
        BTreeSet::from([&first_block]),
        "extra, missing, or wrong fork tips after startup: {state}",
    );
    assert_eq!(
        state.blocks_by_parent.len(),
        MAX_BLOCK_DEPTH,
        "wrong number of parent blocks after startup: {state}",
    );

    // These are only invariants after the state is full and pruned.
    assert_eq!(
        state.blocks_by_parent.values().flatten().count(),
        MAX_BLOCK_DEPTH,
        "wrong number of child blocks after startup: {state}",
    );
    assert_eq!(
        state.blocks_by_hash.len(),
        MAX_BLOCK_DEPTH,
        "wrong number of blocks by hash after startup: {state}",
    );

    info!(
        ?is_best_block,
        %state,
        ?first_block_node_rpc_url,
        ?parent_of_first_block_node_rpc_url,
        "Successfully added context blocks to chain fork state",
    );

    while let Some(block_seen) = new_blocks_rx.recv().await {
        add_blocks_to_chain_fork_state(
            &rpc_client_list,
            &mut state,
            BlockCheckMode::Current,
            block_seen,
            &best_fork_tx,
            &alert_tx,
        )
        .await?;

        state.prune_blocks();
    }

    debug!("Channel disconnected, exiting");

    Ok(())
}
