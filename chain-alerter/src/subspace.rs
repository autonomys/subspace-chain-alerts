//! Subspace chain connection and block parsing code.

use chrono::{DateTime, Utc};
use std::fmt::{self, Display};
use subxt::SubstrateConfig;
use subxt::blocks::{Block, Extrinsics};
use subxt::client::OnlineClientT;
use subxt::utils::H256;

/// The Subspace block height type.
/// Copied from subspace-core-primitives.
pub type BlockNumber = u32;

/// One Subspace Credit.
/// Copied from subspace-runtime-primitives.
pub const AI3: u128 = 10_u128.pow(18);

/// The config for basic Subspace block and extrinsic types.
/// TODO: create a custom SubspaceConfig type
pub type SubspaceConfig = SubstrateConfig;

/// Block info that can be formatted.
#[derive(Clone, Debug, PartialEq, Eq, Ord, PartialOrd)]
pub struct BlockInfo {
    pub block_height: BlockNumber,
    pub block_time: Option<BlockTime>,
    pub block_hash: H256,
    pub genesis_hash: H256,
}

impl Display for BlockInfo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "Height: {}", self.block_height)?;
        writeln!(
            f,
            "Time: {}",
            self.block_time
                .as_ref()
                .map(|bt| bt.to_string())
                .unwrap_or_else(|| "unknown".to_string())
        )?;
        // Show full block hash but truncated genesis hash.
        writeln!(f, "Hash: {:?}", self.block_hash)?;
        writeln!(f, "Genesis: {}", self.genesis_hash)?;
        Ok(())
    }
}

impl BlockInfo {
    pub fn new<Client>(
        block: &Block<SubspaceConfig, Client>,
        extrinsics: &Extrinsics<SubspaceConfig, Client>,
        genesis_hash: &H256,
    ) -> BlockInfo
    where
        Client: OnlineClientT<SubspaceConfig>,
    {
        BlockInfo {
            block_height: block.header().number,
            block_time: BlockTime::new(extrinsics),
            block_hash: block.hash(),
            genesis_hash: *genesis_hash,
        }
    }
}

/// A block time formatted different ways.
#[derive(Clone, Debug, PartialEq, Eq, Ord, PartialOrd)]
pub struct BlockTime {
    /// The block UNIX time (in milliseconds).
    pub unix_time: u128,

    /// A date time type.
    pub date_time: DateTime<Utc>,

    /// A human-readable time string.
    pub human_time: String,
}

impl Display for BlockTime {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} ({})", self.human_time, self.unix_time)
    }
}

impl BlockTime {
    /// Returns the block UNIX time (in milliseconds), a date time type, and a human-readable time
    /// string.
    ///
    /// If the block does not have a timestamp set extrinsic, or parsing fails, returns `None`.
    pub fn new<Client>(extrinsics: &Extrinsics<SubspaceConfig, Client>) -> Option<BlockTime>
    where
        Client: OnlineClientT<SubspaceConfig>,
    {
        // TODO: return a struct rather than a tuple

        // Find the timestamp set extrinsic (usually the first extrinsic).
        for extrinsic in extrinsics.iter() {
            let Ok(meta) = extrinsic.extrinsic_metadata() else {
                // If we can't get the extrinsic pallet and call name, there's nothing we can do.
                // We'll log it elsewhere, so just move on.
                continue;
            };

            if meta.pallet.name() != "Timestamp" || meta.variant.name != "set" {
                // Not the timestamp set extrinsic.
                continue;
            }

            // If we can't get the field value, there's only one timestamp extrinsic per block, and
            // only one field in it, so we just return None.
            let unix_time = extrinsic
                .field_values()
                .ok()?
                .into_values()
                .next()?
                .as_u128()?;

            let date_time = DateTime::from_timestamp_millis(unix_time as i64)?;
            let human_time = date_time.format("%Y-%m-%d %H:%M:%S UTC").to_string();

            return Some(BlockTime {
                unix_time,
                date_time,
                human_time,
            });
        }

        None
    }
}
