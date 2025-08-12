//! Subspace chain connection and block parsing code.

use crate::format::{MAX_EXTRINSIC_DEBUG_LENGTH, fmt_timestamp, truncate};
use chrono::{DateTime, Utc};
use scale_value::Composite;
use std::fmt::{self, Display};
use std::time::Duration;
use subxt::SubstrateConfig;
use subxt::blocks::{Block, ExtrinsicDetails, Extrinsics};
use subxt::client::OnlineClientT;
use subxt::utils::H256;
use tracing::warn;

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
        write!(f, "Genesis: {}", self.genesis_hash)?;
        Ok(())
    }
}

impl BlockInfo {
    /// Create a block info from a block and its extrinsics.
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

            // If the time is out of range, return None.
            let date_time = DateTime::from_timestamp_millis(i64::try_from(unix_time).ok()?)?;

            return Some(BlockTime {
                unix_time,
                date_time,
                human_time: fmt_timestamp(&date_time),
            });
        }

        None
    }
}

/// Extrinsic info that can be formatted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExtrinsicInfo {
    pub pallet: String,
    pub call: String,
    pub index: u32,
    pub hash: H256,
    pub fields: Composite<u32>,
    pub fields_str: String,
}

impl Display for ExtrinsicInfo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "{}::{} (index {})", self.pallet, self.call, self.index)?;
        writeln!(f, "Hash: {:?}", self.hash)?;
        write!(f, "{}", self.fields_str)?;
        Ok(())
    }
}

impl ExtrinsicInfo {
    /// Check and collect an extrinsic's info.
    pub fn new<Client>(
        extrinsic: &ExtrinsicDetails<SubspaceConfig, Client>,
        block_info: &BlockInfo,
    ) -> Option<ExtrinsicInfo>
    where
        Client: OnlineClientT<SubspaceConfig>,
    {
        let Ok(meta) = extrinsic.extrinsic_metadata() else {
            // If we can't get the extrinsic pallet and call name, there's nothing we can do.
            // Just log it and move on.
            warn!(
                "extrinsic {} pallet/name unavailable in block:\n\
                {block_info}",
                extrinsic.index(),
            );
            return None;
        };

        // We can usually get the extrinsic fields, but we don't need the fields for some
        // extrinsic alerts. So we just warn and substitute empty fields.
        let fields = extrinsic.field_values().unwrap_or_else(|_| {
            warn!(
                "extrinsic {}:{} {} fields unavailable in block:\n\
                Hash: {:?}\n\
                {block_info}",
                meta.pallet.name(),
                meta.variant.name,
                extrinsic.index(),
                extrinsic.hash(),
            );
            Composite::unnamed(Vec::new())
        });
        let fields_str = {
            // The decoded value debug format is extremely verbose, display seems a bit better.
            let mut fields_str = format!("{fields}");
            truncate(&mut fields_str, MAX_EXTRINSIC_DEBUG_LENGTH);
            fields_str
        };

        Some(ExtrinsicInfo {
            hash: extrinsic.hash(),
            pallet: meta.pallet.name().to_string(),
            call: meta.variant.name.to_string(),
            index: extrinsic.index(),
            fields,
            fields_str,
        })
    }
}

/// Calculates the timestamp gap between a block and a later time, if the block is present and has a timestamp.
/// Returns `None` if the block info is missing, or the block is missing a timestamp.
pub fn gap_since_time(
    latest_time: DateTime<Utc>,
    prev_block_info: impl Into<Option<BlockInfo>>,
) -> Option<Duration> {
    let prev_block_info = prev_block_info.into()?;
    let prev_block_time = prev_block_info.block_time?;

    let gap = latest_time.signed_duration_since(prev_block_time.date_time);

    gap.to_std().ok()
}

/// Calculates the timestamp gap between two blocks, if both are present and have timestamps.
/// Returns `None` if either block info is missing, or a block is missing a timestamp.
pub fn gap_since_last_block(
    block_info: impl Into<Option<BlockInfo>>,
    prev_block_info: impl Into<Option<BlockInfo>>,
) -> Option<Duration> {
    let block_info = block_info.into()?;
    let block_time = block_info.block_time?;

    gap_since_time(block_time.date_time, prev_block_info)
}
