//! Utilities for extracting Subspace slot information from block digests.

use std::result::Result;

use crate::subspace::SubspaceConfig;
use subxt::blocks::Block;
use subxt::client::OnlineClient;
use subxt::config::substrate::DigestItem;
use tracing::{debug, trace};

#[derive(Debug)]
/// Error type for slot extraction from block pre-digests.
pub enum SlotExtractionError {
    /// No Subspace pre-digest was found in the block header.
    Missing,
}

/// Extract Subspace slot number from a block's pre-runtime digest.
///
/// Returns `Ok(0)` for the genesis block which doesn't contain a pre-digest.
///
/// Derived from `subspace-runtime-primitives` format.
pub fn extract_slot_from_pre_digest(
    block: &Block<SubspaceConfig, OnlineClient<SubspaceConfig>>,
) -> Result<u64, SlotExtractionError> {
    // genesis block doesn't contain a pre digest so let's generate a
    // dummy one to not break any invariants in the rest of the code
    if block.header().number == 0 {
        return Ok(0);
    }

    let mut result: Option<u64> = None;
    for log in block.header().digest.logs.clone() {
        trace!("Checking log {:?}, looking for pre runtime digest", log);

        if let DigestItem::PreRuntime(_, pre_digest) = log {
            let slot = decode_slot_number(pre_digest);
            if let Some(slot) = slot {
                debug!("Found pre runtime digest {:?}", slot);
                result = Some(slot);
            }
            break;
        }
    }

    result.ok_or(SlotExtractionError::Missing)
}

/// The offset of the slot number in the pre-digest.
const SLOT_OFFSET: usize = 1;

/// The length of the slot number in the pre-digest.
const SLOT_LEN: usize = 8;

/// Decode the slot number from the Subspace pre-digest payload.
fn decode_slot_number(pre_digest: Vec<u8>) -> Option<u64> {
    pre_digest
        .as_slice()
        .get(SLOT_OFFSET..=SLOT_LEN)?
        .try_into()
        .ok()
        .map(u64::from_le_bytes)
}
