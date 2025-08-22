//! Monitoring and alerting for chain forks.

use crate::alerts::{Alert, BlockCheckMode};
use crate::subspace::{BlockInfo, BlockLink};
use std::cmp::max;
use std::collections::HashMap;
use std::sync::Arc;
use subxt::utils::H256;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

/// The buffer size for the chain fork monitor.
pub const CHAIN_FORK_BUFFER_SIZE: usize = 100;

/// The minimum fork depth to alert on.
pub const MIN_FORK_DEPTH: usize = 7;

/// The minimum fork depth to log as info.
pub const MIN_FORK_DEPTH_FOR_INFO_LOG: usize = 3;

/// A chain fork or reorg event.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ChainForkEvent {
    /// A new chain fork was seen.
    NewFork {
        /// The tip of the fork.
        tip: Arc<BlockLink>,

        /// The number of blocks from the fork tip to the fork point.
        fork_depth: usize,
    },

    /// A chain fork was extended by a non-best block.
    ForkExtended {
        /// The tip of the fork.
        tip: Arc<BlockLink>,

        /// The number of blocks from the fork tip to the fork point.
        fork_depth: usize,
    },

    /// A reorg was seen, this takes priority over fork events.
    Reorg {
        /// The new best block.
        new_best_block: Arc<BlockLink>,

        /// The old best block.
        old_best_block: Arc<BlockLink>,

        /// The number of blocks from the old best block to the reorg point (the fork point with
        /// the new best block).
        old_fork_depth: usize,

        /// The number of blocks from the new best block to the reorg point.
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
            ChainForkEvent::NewFork { fork_depth, .. } => *fork_depth,
            ChainForkEvent::ForkExtended { fork_depth, .. } => *fork_depth,
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

    /// A list of block links by parent hash.
    /// This lets us walk the chain forwards using block hashes.
    pub blocks_by_parent_hash: HashMap<H256, Vec<Arc<BlockLink>>>,

    /// The tips of each chain fork.
    pub tips_by_hash: HashMap<H256, Arc<BlockLink>>,

    /// The best block link, used to detect reorgs.
    pub best_block: Arc<BlockLink>,
}

impl ChainForkState {
    /// Create a new chain fork state from the first block link we've seen.
    pub fn from_first_block(block_info: &BlockInfo) -> Self {
        let block_link = BlockLink::from_block_info(block_info);
        let block_link = Arc::new(block_link);

        ChainForkState {
            blocks_by_hash: HashMap::from([(block_link.hash(), block_link.clone())]),
            blocks_by_parent_hash: HashMap::from([(
                block_link.parent_hash,
                vec![block_link.clone()],
            )]),
            tips_by_hash: HashMap::from([(block_link.hash(), block_link.clone())]),
            best_block: block_link.clone(),
        }
    }

    /// Calculate the depth of a fork.
    pub fn fork_depth(&self, block_link: &BlockLink) -> usize {
        let mut depth = 1;
        let mut current_block = block_link;

        loop {
            // Find the blocks with the same parent as this block.
            let sibling_blocks = self
                .blocks_by_parent_hash
                .get(&current_block.parent_hash)
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

    /// Add a new block link to the chain segments.
    pub fn add_block_link(
        &mut self,
        block_info: &BlockInfo,
        is_best_block: bool,
    ) -> Option<ChainForkEvent> {
        if self.blocks_by_hash.contains_key(&block_info.hash()) {
            // Block is already in the chain.
            return None;
        }

        let block_link = BlockLink::from_block_info(block_info);
        let block_link = Arc::new(block_link);

        // Add the block link to the chain.
        self.blocks_by_hash
            .insert(block_link.hash(), block_link.clone());
        self.blocks_by_parent_hash
            .entry(block_link.parent_hash)
            .or_default()
            .push(block_link.clone());

        let mut is_new_fork = false;
        let mut is_fork_extended = false;

        // If the tips contains the parent hash, replace it with this new tip.
        if self.tips_by_hash.remove(&block_link.parent_hash).is_some() {
            is_fork_extended = !is_best_block;
            self.tips_by_hash
                .insert(block_link.hash(), block_link.clone());
        } else if self.blocks_by_hash.contains_key(&block_link.parent_hash) {
            // Otherwise, add a new tip, if it connects to the chain.
            // TODO: handle this properly, by adding a list of disconnected blocks to the state, and
            // replaying them through this method when their connecting block is received.
            is_new_fork = true;
            self.tips_by_hash
                .insert(block_link.hash(), block_link.clone());
        }

        // Reorgs are best blocks which aren't a child of the current best block.
        // Our skipped block replay makes sure we don't get disconnected best blocks.
        let is_reorg = is_best_block && block_link.parent_hash != self.best_block.hash();

        let event = if is_reorg {
            Some(ChainForkEvent::Reorg {
                new_best_block: block_link.clone(),
                old_best_block: self.best_block.clone(),
                old_fork_depth: self.fork_depth(&self.best_block),
                new_fork_depth: self.fork_depth(&block_link),
            })
        } else if is_new_fork {
            Some(ChainForkEvent::NewFork {
                tip: block_link.clone(),
                // New forks are always 1 block deep, assuming they connect to the chain at all.
                fork_depth: 1,
            })
        } else if is_fork_extended {
            Some(ChainForkEvent::ForkExtended {
                tip: block_link.clone(),
                fork_depth: self.fork_depth(&block_link),
            })
        } else {
            None
        };

        // Update the best block if needed.
        if is_best_block {
            self.best_block = block_link.clone();
        }

        event
    }
}

/// The message sent when the alerter sees a block.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum BlockSeen {
    /// A new block has been seen, and we know it is the best block.
    /// Used to detect reorgs and forks.
    BestBlock(BlockInfo),

    /// A new block has been seen, which might not be the best block.
    /// This block might be treated as the best block if it is received early enough.
    /// Used to detect forks.
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
        debug!("Channel disconnected before first block, exiting");
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
    }

    debug!("Channel disconnected, exiting");
}
