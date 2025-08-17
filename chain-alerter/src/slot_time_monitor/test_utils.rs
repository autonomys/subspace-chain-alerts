/// Test utilities for the slot time monitor.
use crate::subspace::{BlockInfo, BlockTime, RawTime, Slot};
use subxt::utils::H256;

/// Create a mock block info for testing with a given time and slot.
pub fn mock_block_info(time: RawTime, slot: Slot) -> BlockInfo {
    BlockInfo {
        block_height: 100,
        block_time: Some(BlockTime { unix_time: time }),
        block_hash: H256::zero(),
        genesis_hash: H256::zero(),
        block_slot: Some(slot),
    }
}
