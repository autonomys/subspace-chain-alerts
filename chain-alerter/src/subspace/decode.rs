//! Subspace chain decoding utilities for events and extrinsics.

use scale_value::{Primitive, Value, ValueDef};
use subxt::ext::codec::Encode;
use subxt::utils::H256;
use tracing::{debug, trace};

/// Decode a H256 from an item, possibly nested within composites or variants.
pub fn decode_h256_from_composite(item: &Value<u32>) -> Option<H256> {
    let mut item = item.clone().remove_context();

    // Peel outer composite and variant wrappers, but only if they have a single inner value.
    loop {
        match item.value {
            ValueDef::Composite(inner) => {
                if inner.len() == 1 {
                    item = inner
                        .into_values()
                        .next()
                        .expect("already checked for one value");
                } else {
                    item = Value::from(inner);
                    break;
                }
            }
            ValueDef::Variant(inner) => {
                item = Value::from(inner.values);
            }
            ValueDef::BitSequence(_) | ValueDef::Primitive(_) => {
                break;
            }
        }
    }

    let bytes: [u8; 32] = match item.value {
        ValueDef::Composite(items) => {
            // Fail if any element doesn't convert to u8
            let maybe_bytes: Option<Vec<u8>> = items
                .into_values()
                .map(|v| v.as_u128().and_then(|n| u8::try_from(n).ok()))
                .collect();

            let Some(mut bytes) = maybe_bytes else {
                debug!("Was not able to convert composite to bytes");
                return None;
            };

            // Remove the leading SCALE variant byte if present.
            // This can happen for types like `Option<H256>` and `MultiAddress`.
            if bytes.len() == 33 {
                bytes.remove(0);
            }

            match bytes.try_into() {
                Ok(bytes) => bytes,
                Err(error_vec) => {
                    debug!("composite length was {} (expected 32)", error_vec.len());
                    return None;
                }
            }
        }
        ValueDef::Variant(variant) => {
            debug!("variant had more than one element: {variant:?}");
            return None;
        }
        ValueDef::BitSequence(bits) => {
            if bits.len() != 256 {
                debug!("bit sequence length was {} (expected 256)", bits.len());
                return None;
            };

            let bytes = bits.encode();

            // We know it is the correct length, so just strip the leading compact size.
            bytes.as_rchunks::<32>().1[0]
        }

        ValueDef::Primitive(Primitive::U256(bytes) | Primitive::I256(bytes)) => bytes,

        ValueDef::Primitive(primitive) => {
            debug!("non-256-bit primitive: {primitive:?}");
            return None;
        }
    };

    let value_h256 = H256::from(&bytes);

    trace!(
        "Extracted H256 from composite: {}",
        hex::encode(value_h256.as_bytes())
    );

    Some(value_h256)
}
