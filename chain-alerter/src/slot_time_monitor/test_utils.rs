//! Test utilities for the slot time monitor.

use crate::subspace::{BlockHash, BlockInfo, BlockLink, BlockPosition, BlockTime, RawTime, Slot};

/// Create a mock block info for testing with a given time and slot.
pub fn mock_block_info(time: RawTime, slot: Slot) -> BlockInfo {
    BlockInfo {
        link: BlockLink::new(
            BlockPosition::new(100, BlockHash::zero()),
            BlockHash::zero(),
        ),
        time: Some(BlockTime { unix_time: time }),
        slot: Some(slot),
        genesis_hash: BlockHash::zero(),
    }
}
