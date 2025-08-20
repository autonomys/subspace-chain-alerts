//! Subspace chain decoding utilities for events and extrinsics.

use scale_value::{Composite, Value, ValueDef};
use subxt::utils::H256;
use tracing::debug;

/// Decode a H256 from a composite value.
pub fn decode_h256_from_composite(composite: &Value<u32>) -> Option<H256> {
    if let ValueDef::Composite(Composite::Unnamed(outer)) = composite.clone().value {
        let items = if outer.len() == 1 {
            if let ValueDef::Composite(Composite::Unnamed(inner)) = &outer[0].value {
                inner
            } else {
                return None;
            }
        } else {
            &outer
        };

        // Collect exactly 32 bytes; fail if any element doesn't convert to u8
        let maybe_bytes: Option<Vec<u8>> = items
            .iter()
            .map(|v| v.as_u128().and_then(|n| u8::try_from(n).ok()))
            .collect();

        let Some(bytes) = maybe_bytes else {
            debug!("Was not able to convert composite to bytes");
            return None;
        };

        if bytes.len() != 32 {
            debug!("composite length was {} (expected 32)", bytes.len());
            return None;
        };

        let value_h256 = H256::from_slice(&bytes);

        debug!(
            "Extracted H256 from composite: {}",
            hex::encode(value_h256.as_bytes())
        );

        return Some(value_h256);
    }

    None
}
