//! [Subscan.io](https://autonomys.subscan.io) is a block explorer for the Autonomys chain.
//! Alerts link to the relevant block, extrinsic, or event on Subscan.

use crate::subspace::{
    BlockHash, BlockNumber, BlockPosition, EventInfo, ExtrinsicHash, ExtrinsicInfo,
};

/// The base URL for Subscan.io.
pub const SUBSCAN_SITE: &str = "https://autonomys.subscan.io";

/// A trait for types that can be linked to a block on Subscan.io.
pub trait BlockUrl {
    /// Returns a URL for the block on Subscan.io.
    fn block_url(&self) -> String;

    /// Returns a different URL for the block on Subscan.io, which may be less reliable than
    /// `block_url()`.
    #[expect(dead_code, reason = "included for completeness")]
    fn alt_block_url(&self) -> Option<String>;
}

/// A trait for types that can be linked to an extrinsic on Subscan.io.
pub trait ExtrinsicUrl {
    /// Returns a URL for the extrinsic on Subscan.io.
    fn extrinsic_url(&self) -> String;

    /// Returns a different URL for the extrinsic on Subscan.io, which may be less reliable than
    /// `extrinsic_url()`.
    #[expect(dead_code, reason = "included for completeness")]
    fn alt_extrinsic_url(&self) -> Option<String>;
}

/// A trait for types that can be linked to an event on Subscan.io.
pub trait EventUrl {
    /// Returns a URL for the event on Subscan.io.
    fn event_url(&self) -> String;
}

impl BlockUrl for BlockNumber {
    fn block_url(&self) -> String {
        format!("{SUBSCAN_SITE}/block/{self}")
    }

    fn alt_block_url(&self) -> Option<String> {
        None
    }
}

impl BlockUrl for BlockHash {
    fn block_url(&self) -> String {
        format!("{SUBSCAN_SITE}/block/0x{}", hex::encode(self.0))
    }

    fn alt_block_url(&self) -> Option<String> {
        None
    }
}

impl BlockUrl for BlockPosition {
    fn block_url(&self) -> String {
        // We prefer to link to the block hash, because Subscan can silently present the wrong block
        // at that height if there has been a reorg.
        self.hash.block_url()
    }

    fn alt_block_url(&self) -> Option<String> {
        Some(self.height.block_url())
    }
}

impl ExtrinsicUrl for ExtrinsicHash {
    fn extrinsic_url(&self) -> String {
        format!("{SUBSCAN_SITE}/extrinsic/0x{}", hex::encode(self.0))
    }

    fn alt_extrinsic_url(&self) -> Option<String> {
        None
    }
}

impl ExtrinsicUrl for ExtrinsicInfo {
    fn extrinsic_url(&self) -> String {
        // We prefer to link to the extrinsic hash, because Subscan can silently present extrinsics
        // for the wrong block if there has been a reorg.
        self.hash.extrinsic_url()
    }

    fn alt_extrinsic_url(&self) -> Option<String> {
        Some(format!(
            "{SUBSCAN_SITE}/extrinsic/{}-{}",
            self.block.height(),
            self.index,
        ))
    }
}

impl EventUrl for EventInfo {
    fn event_url(&self) -> String {
        // Unfortunately, Subscan doesn't link to events by block hash or extrinsic hash.
        format!(
            "{SUBSCAN_SITE}/event/{}-{}",
            self.block.height(),
            self.index,
        )
    }
}
