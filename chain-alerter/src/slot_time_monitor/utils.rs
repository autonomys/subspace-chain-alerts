use std::result::Result;

use crate::subspace::SubspaceConfig;
use subxt::blocks::Block;
use subxt::client::OnlineClient;
use subxt::config::substrate::DigestItem;
use tracing::{debug, trace};

#[derive(Debug)]
pub enum SlotExtractionError {
    Missing,
}

// Derived from mod 'subspace-runtime-primitives'
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

        match log {
            DigestItem::PreRuntime(_, pre_digest) => {
                let slot = decode_slot_number(pre_digest);
                if let Some(slot) = slot {
                    debug!("Found pre runtime digest {:?}", slot);
                    result = Some(slot);
                }
                break;
            }
            _ => {}
        }
    }

    result.ok_or(SlotExtractionError::Missing)
}

/// The offset of the slot number in the pre-digest.
const SLOT_OFFSET: usize = 1;

/// The length of the slot number in the pre-digest.
const SLOT_LEN: usize = 8;

/// Decodes the slot number from the pre-digest.
fn decode_slot_number(pre_digest: Vec<u8>) -> Option<u64> {
    pre_digest
        .as_slice()
        .get(SLOT_OFFSET..SLOT_OFFSET + SLOT_LEN)?
        .try_into()
        .ok()
        .map(u64::from_le_bytes)
}
