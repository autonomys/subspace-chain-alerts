//! Test utilities for the slot time monitor.

use crate::subspace::{
    BlockHash, BlockInfo, BlockLink, BlockPosition, BlockTime, ChainTime, RawTime, Slot,
};

/// Create a mock block info for testing with a given time and slot.
pub fn mock_block_info(
    time: impl Into<Option<RawTime>> + Copy,
    slot: impl Into<Option<Slot>> + Copy,
) -> BlockInfo {
    BlockInfo {
        link: BlockLink::new(
            BlockPosition::new(100, BlockHash::zero()),
            BlockHash::zero(),
        ),
        chain_time: time.into().map(|t| BlockTime {
            unix_time: t,
            source: ChainTime,
        }),
        local_time: BlockTime::new_from_local_time(),
        slot: slot.into(),
        genesis_hash: BlockHash::zero(),
    }
}
