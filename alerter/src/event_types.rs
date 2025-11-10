//! Events types that are being monitored

use crate::subspace::{AccountId, Balance, BlockHash, BlockNumber};
use scale_decode_derive::DecodeAsType;
use std::fmt;
use subxt_core::events::StaticEvent;

#[derive(Clone, DecodeAsType)]
pub(crate) struct FarmerPublicKey([u8; 32]);

impl fmt::Debug for FarmerPublicKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", hex::encode(self.0))
    }
}

#[derive(Debug, Clone, DecodeAsType)]
pub(crate) struct BalanceTransferEvent {
    pub(crate) from: AccountId,
    pub(crate) to: AccountId,
    pub(crate) amount: Balance,
}

impl StaticEvent for BalanceTransferEvent {
    const PALLET: &'static str = "Balances";
    const EVENT: &'static str = "Transfer";
}

#[derive(Debug, Clone, DecodeAsType)]
pub(crate) struct FarmerVote {
    pub(crate) public_key: FarmerPublicKey,
    pub(crate) reward_address: AccountId,
    pub(crate) height: BlockNumber,
    pub(crate) parent_hash: BlockHash,
}

impl StaticEvent for FarmerVote {
    const PALLET: &'static str = "Subspace";
    const EVENT: &'static str = "FarmerVote";
}
