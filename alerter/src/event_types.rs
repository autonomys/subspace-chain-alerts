//! Events types that are being monitored

use crate::subspace::{AccountId, Balance, BlockHash, BlockNumber};
use scale_decode_derive::DecodeAsType;
use sp_runtime::DispatchResult;
use std::fmt;
use subxt_core::events::StaticEvent;
use subxt_core::utils::Static;

/// Overarching event type
#[derive(Debug)]
pub(crate) enum Event {
    Transfer(TransferKnownAccountEvent),
    DomainRuntimeUpgraded(DomainRuntimeUpgraded),
    DomainInstantiated(DomainInstantiated),
    FraudProofProcessed(FraudProofProcessed),
    OperatorSlashed(OperatorSlashed),
    OperatorOffline(OperatorOffline),
    Sudo,
    CodeUpdated(CodeUpdated),
}

/// Type representing the runtime ID.
pub(crate) type RuntimeId = u32;

/// Type representing operator ID
pub(crate) type OperatorId = u64;

/// Unique identifier of a domain.
#[derive(Debug, Clone, DecodeAsType)]
pub(crate) struct DomainId(u32);

impl fmt::Display for DomainId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Domain({})", self.0)
    }
}

#[derive(Debug, Clone, DecodeAsType)]
pub(crate) struct DomainRuntimeUpgraded {
    pub(crate) runtime_id: RuntimeId,
}

impl From<DomainRuntimeUpgraded> for Event {
    fn from(value: DomainRuntimeUpgraded) -> Self {
        Self::DomainRuntimeUpgraded(value)
    }
}

impl StaticEvent for DomainRuntimeUpgraded {
    const PALLET: &'static str = "Domains";
    const EVENT: &'static str = "DomainRuntimeUpgraded";
}

#[derive(Debug, Clone, DecodeAsType)]
pub(crate) struct DomainInstantiated {
    pub(crate) domain_id: DomainId,
}

impl From<DomainInstantiated> for Event {
    fn from(value: DomainInstantiated) -> Self {
        Self::DomainInstantiated(value)
    }
}

impl StaticEvent for DomainInstantiated {
    const PALLET: &'static str = "Domains";
    const EVENT: &'static str = "DomainInstantiated";
}

#[derive(Debug, Clone, DecodeAsType)]
pub(crate) struct FraudProofProcessed {
    pub(crate) domain_id: DomainId,
    pub(crate) new_head_receipt_number: Option<BlockNumber>,
}

impl From<FraudProofProcessed> for Event {
    fn from(value: FraudProofProcessed) -> Self {
        Self::FraudProofProcessed(value)
    }
}

impl StaticEvent for FraudProofProcessed {
    const PALLET: &'static str = "Domains";
    const EVENT: &'static str = "FraudProofProcessed";
}

#[derive(Debug, Clone, DecodeAsType)]
pub(crate) enum SlashedReason {
    /// Operator produced bad bundle.
    InvalidBundle(BlockNumber),
    /// Operator submitted bad Execution receipt.
    BadExecutionReceipt(BlockHash),
}

#[derive(Debug, Clone, DecodeAsType)]
pub(crate) struct OperatorSlashed {
    pub(crate) operator_id: OperatorId,
    pub(crate) reason: SlashedReason,
}

impl From<OperatorSlashed> for Event {
    fn from(value: OperatorSlashed) -> Self {
        Self::OperatorSlashed(value)
    }
}

impl StaticEvent for OperatorSlashed {
    const PALLET: &'static str = "Domains";
    const EVENT: &'static str = "OperatorSlashed";
}

#[derive(Debug, Clone, DecodeAsType)]
pub(crate) struct OperatorEpochExpectations {
    /// floor(μ) = floor(S * p_slot_exact): integer expected bundles this epoch.
    pub(crate) expected_bundles: u64,
    /// Chernoff lower-bound r: minimum bundles to pass with false-positive ≤ τ.
    pub(crate) min_required_bundles: u64,
}

#[derive(Debug, Clone, DecodeAsType)]
pub(crate) struct OperatorOffline {
    pub(crate) operator_id: OperatorId,
    pub(crate) domain_id: DomainId,
    pub(crate) submitted_bundles: u64,
    pub(crate) expectations: OperatorEpochExpectations,
}

impl From<OperatorOffline> for Event {
    fn from(value: OperatorOffline) -> Self {
        Self::OperatorOffline(value)
    }
}

impl StaticEvent for OperatorOffline {
    const PALLET: &'static str = "Domains";
    const EVENT: &'static str = "OperatorOffline";
}

#[derive(Debug, Clone, DecodeAsType)]
pub(crate) struct BalanceWithdraw {
    who: AccountId,
    amount: Balance,
}

impl TransferEvent for BalanceWithdraw {
    fn transfer_type(&self) -> TransferType {
        TransferType::Withdraw
    }

    fn amount(&self) -> Balance {
        self.amount
    }

    fn from(&self) -> Option<AccountId> {
        Some(self.who.clone())
    }

    fn to(&self) -> Option<AccountId> {
        None
    }
}

impl StaticEvent for BalanceWithdraw {
    const PALLET: &'static str = "Balances";
    const EVENT: &'static str = "Withdraw";
}

#[derive(Debug, Clone, DecodeAsType)]
pub(crate) struct BalanceDeposit {
    who: AccountId,
    amount: Balance,
}

impl TransferEvent for BalanceDeposit {
    fn transfer_type(&self) -> TransferType {
        TransferType::Deposit
    }

    fn amount(&self) -> Balance {
        self.amount
    }

    fn from(&self) -> Option<AccountId> {
        None
    }

    fn to(&self) -> Option<AccountId> {
        Some(self.who.clone())
    }
}

impl StaticEvent for BalanceDeposit {
    const PALLET: &'static str = "Balances";
    const EVENT: &'static str = "Deposit";
}

#[derive(Debug, Clone, DecodeAsType)]
pub(crate) struct Sudo {
    /// The result of the call made by the sudo user.
    pub(crate) _sudo_result: Static<DispatchResult>,
}

impl From<Sudo> for Event {
    fn from(_: Sudo) -> Self {
        Self::Sudo
    }
}

impl StaticEvent for Sudo {
    const PALLET: &'static str = "Sudo";
    const EVENT: &'static str = "Sudid";
}

#[derive(Debug, Clone, DecodeAsType)]
pub(crate) struct CodeUpdated {}

impl From<CodeUpdated> for Event {
    fn from(value: CodeUpdated) -> Self {
        Self::CodeUpdated(value)
    }
}

impl StaticEvent for CodeUpdated {
    const PALLET: &'static str = "System";
    const EVENT: &'static str = "CodeUpdated";
}

#[derive(Debug, Clone, DecodeAsType)]
pub(crate) struct BalanceTransfer {
    pub(crate) from: AccountId,
    pub(crate) to: AccountId,
    pub(crate) amount: Balance,
}

impl TransferEvent for BalanceTransfer {
    fn transfer_type(&self) -> TransferType {
        TransferType::Transfer
    }

    fn amount(&self) -> Balance {
        self.amount
    }

    fn from(&self) -> Option<AccountId> {
        Some(self.from.clone())
    }

    fn to(&self) -> Option<AccountId> {
        Some(self.to.clone())
    }
}

impl StaticEvent for BalanceTransfer {
    const PALLET: &'static str = "Balances";
    const EVENT: &'static str = "Transfer";
}

#[derive(Debug, Clone)]
pub(crate) enum TransferDirection {
    Sender,
    Receiver,
}

#[derive(Debug, Clone)]
pub(crate) enum TransferType {
    Transfer,
    Withdraw,
    Deposit,
}

#[derive(Debug, Clone)]
pub(crate) struct TransferKnownAccountEvent {
    pub(crate) direction: TransferDirection,
    pub(crate) transfer_type: TransferType,
    pub(crate) name: String,
    pub(crate) address: String,
    pub(crate) amount: Balance,
}

impl From<TransferKnownAccountEvent> for Event {
    fn from(value: TransferKnownAccountEvent) -> Self {
        Self::Transfer(value)
    }
}

pub(crate) trait TransferEvent {
    fn transfer_type(&self) -> TransferType;
    fn amount(&self) -> Balance;
    fn from(&self) -> Option<AccountId>;
    fn to(&self) -> Option<AccountId>;
}
