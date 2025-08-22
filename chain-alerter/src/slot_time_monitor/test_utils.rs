/// Test utilities for the slot time monitor.
use crate::subspace::{BlockInfo, BlockPosition, BlockTime, RawTime, Slot};
use subxt::utils::H256;

/// Create a mock block info for testing with a given time and slot.
pub fn mock_block_info(time: RawTime, slot: Slot) -> BlockInfo {
    BlockInfo {
        position: BlockPosition::new(100, H256::zero()),
        time: Some(BlockTime { unix_time: time }),
        slot: Some(slot),
        parent_hash: H256::zero(),
        genesis_hash: H256::zero(),
    }
}
