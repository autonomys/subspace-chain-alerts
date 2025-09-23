//! Balance transfer alerts.

use crate::alerts::{Alert, AlertKind, BlockCheckMode, MIN_BALANCE_CHANGE};
use crate::subspace::decode::decode_h256_from_composite;
use crate::subspace::{Balance, BlockInfo, EventInfo, ExtrinsicInfo};
use scale_value::Composite;
use std::fmt::{self, Display};
use std::str::FromStr;
use subxt::utils::AccountId32;
use tokio::sync::mpsc;
use tracing::{debug, trace, warn};

/// A list of known important addresses.
pub const IMPORTANT_ADDRESSES: &[(&str, &str)] = &[
    // Official foundation and labs addresses:
    // <https://forum.autonomys.xyz/t/subspace-foundation-autonomys-labs-wallets-official-addresses-for-transparency/4917>
    (
        "Investors",
        "sucGPHK3b4REe2DNRvNaUrmcoXVDDZVasm7zBNtev4zUpLrp4",
    ),
    (
        "Subspace Foundation Long-Term Treasury",
        "sugc77Zny6kg9X4mCs1pe2aunLSb1UcUrDokDLp1ho2FUE6wj",
    ),
    (
        "Team (Founders + Staff)",
        "suc7ykog1bVpzUAFCHDsobCtmtcVEXjTpPNJ7vEHGmcZA13kd",
    ),
    (
        "Subspace Foundation Near-Term Treasury",
        "sudqduciRx3fZcNRbW1mmmBAntuZLkwhcXyVctsGJyrjRTPaA",
    ),
    (
        "DevCo Treasury",
        "sucVFW97RnRYzXE1hncq4Q6E3ygxVv4Ap8KvcWMCEfF4QZwGY",
    ),
    (
        "Advisors",
        "sudUExCScaD4JEk3iALh69PFPUJcmu4ziwBByGiGnMgr1ouj5",
    ),
    (
        "Market Liquidity",
        "subTKWVig3CiwwnPfKccXZHQKwnE36vNHfNTEUx3yYja4UNR9",
    ),
    (
        "Vendors",
        "sugKyc3Qs9WWqeT2vVynQsnQukTrfGwofg8FdrtTPprQCUXS7",
    ),
    (
        "Ambassadors",
        "sufqKMnmLekD1NA8smBMLei7cZvvaHLpEXkExdsoi97ezCEtY",
    ),
    (
        "Operations",
        "suesYE9yAqNJrMiZPY4hKNMjMTXBkkD1rHgQrSNes1bUnw37U",
    ),
    (
        "Game of Domains",
        "sueCdBhsNJ9LH76wYyJYhK8fvcvYt1q3J3AwWq674rwPEvbKS",
    ),
];

/// If the address is an important address, returns the kind of important address, otherwise returns
/// `None`.
pub fn important_address_kind(address: &Account) -> Option<&'static str> {
    let account_id = address.account_id();

    IMPORTANT_ADDRESSES
        .iter()
        .find(|(_, addr)| {
            let addr_id = AccountId32::from_str(addr).expect("constants are valid addresses");
            trace!(?addr_id, ?account_id, "important address kind check");
            &addr_id == account_id
        })
        .map(|(kind, _)| *kind)
}

/// A trait for accessing the transfer value from an object.
pub trait TransferValue {
    /// Returns the transfer value, if it is present.
    fn transfer_value(&self) -> Option<Balance>;
}

impl TransferValue for ExtrinsicInfo {
    fn transfer_value(&self) -> Option<Balance> {
        if (self.pallet == "Balances" || self.pallet == "Transporter")
            && let Composite::Named(named_fields) = &self.fields
        {
            // subxt knows these field names, so we can search for the transfer value by name.
            // TODO:
            // - split this field search into a function which takes a field name, and another
            //   function which does the numeric conversion
            if let Some((_name, transfer_value)) = named_fields
                .iter()
                .find(|(name, _)| ["value", "amount", "new_free", "delta"].contains(&name.as_str()))
            {
                return transfer_value.as_u128();
            }
        }

        None
    }
}

impl TransferValue for EventInfo {
    fn transfer_value(&self) -> Option<Balance> {
        if (self.pallet == "Balances" || self.pallet == "Transporter")
            && let Composite::Named(named_fields) = &self.fields
            && let Some((_name, transfer_value)) = named_fields
                .iter()
                .find(|(name, _)| name.as_str() == "amount")
        {
            return transfer_value.as_u128();
        }

        if self.pallet == "Transactionpayment"
            && let Composite::Named(named_fields) = &self.fields
        {
            let transfer_values: Vec<u128> = named_fields
                .iter()
                .filter(|(name, _)| ["actual_fee", "tip"].contains(&name.as_str()))
                .flat_map(|(_, value)| value.as_u128())
                .collect();

            if transfer_values.is_empty() {
                return None;
            }

            return Some(transfer_values.iter().sum());
        }

        None
    }
}

/// A typed account ID.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Account {
    /// The signer of an extrinsic.
    Signer(AccountId32),

    /// A transfer sender account ID.
    Sender(AccountId32),

    /// A transfer receiver account ID.
    Receiver(AccountId32),
}

impl Account {
    /// Returns the account ID.
    pub fn account_id(&self) -> &AccountId32 {
        match self {
            Account::Signer(id) => id,
            Account::Sender(id) => id,
            Account::Receiver(id) => id,
        }
    }

    /// Returns the important address kind for this account, if it is an important address.
    pub fn important_address_kind(&self) -> Option<&'static str> {
        important_address_kind(self)
    }
}

impl Display for Account {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Account::Signer(id) => write!(f, "Signer: {id}"),
            Account::Sender(id) => write!(f, "Sender: {id}"),
            Account::Receiver(id) => write!(f, "Receiver: {id}"),
        }
    }
}

/// A trait for accessing the account IDs and account types from an object.
pub trait Accounts {
    /// Returns the account IDs and types, if present.
    fn accounts(&self) -> Vec<Account>;
}

impl Accounts for ExtrinsicInfo {
    fn accounts(&self) -> Vec<Account> {
        let mut account_list = vec![];

        if let Some(signing_address) = self.signing_address.as_ref() {
            account_list.push(Account::Signer(signing_address.clone()));
        }

        if self.pallet == "Balances"
            && let Composite::Named(named_fields) = &self.fields
            && let Some((_name, account_id)) = named_fields
                .iter()
                .find(|(name, _)| name.as_str() == "dest")
            && let Some(account_id) = decode_h256_from_composite(account_id)
        {
            account_list.push(Account::Receiver(account_id.0.into()));
        }

        // TODO: add `dst_location` AccountId20 from Transporter events:
        // <https://autonomys.subscan.io/extrinsic/4525962-7>

        account_list
    }
}

impl Accounts for EventInfo {
    fn accounts(&self) -> Vec<Account> {
        let mut account_list = vec![];

        if self.pallet == "Balances"
            && let Composite::Named(named_fields) = &self.fields
            && let Some((name, account_id)) = named_fields
                .iter()
                .find(|(name, _)| ["who", "from", "to"].contains(&name.as_str()))
            && let Some(account_id) = decode_h256_from_composite(account_id)
        {
            if ["who", "to"].contains(&name.as_str()) {
                if ["Burned", "Withdraw"].contains(&self.kind.as_str()) {
                    account_list.push(Account::Sender(account_id.0.into()));
                } else {
                    account_list.push(Account::Receiver(account_id.0.into()));
                }
            } else if name.as_str() == "from" {
                account_list.push(Account::Sender(account_id.0.into()));
            }
        }

        if self.pallet == "Transactionpayment"
            && let Composite::Named(named_fields) = &self.fields
            && let Some((_name, account_id)) =
                named_fields.iter().find(|(name, _)| name.as_str() == "who")
            && let Some(account_id) = decode_h256_from_composite(account_id)
        {
            account_list.push(Account::Sender(account_id.0.into()));
        }

        account_list
    }
}

/// Check a Balance extrinsic for alerts.
/// Does nothing if the extrinsic is any other kind of extrinsic.
pub async fn check_transfer_extrinsic(
    mode: BlockCheckMode,
    alert_tx: &mpsc::Sender<Alert>,
    extrinsic_info: &ExtrinsicInfo,
    block_info: &BlockInfo,
) -> anyhow::Result<()> {
    // "force*" calls and large balance changes are alerts.

    // TODO:
    // - track the total of recent transfers, so the threshold can't be bypassed by splitting the
    //   transfer into multiple calls
    let transfer_value = extrinsic_info.transfer_value();

    debug!(?mode, "transfer_value: {:?}", transfer_value);

    // TODO:
    // - test force alerts by checking a historic block with that call
    // - do we want to track burn calls? <https://autonomys.subscan.io/extrinsic/137324-31> this is
    //   a low priority because it is already covered by balance events
    if extrinsic_info.pallet == "Balances" && extrinsic_info.call.starts_with("force") {
        alert_tx
            .send(Alert::new(
                AlertKind::ForceBalanceTransfer {
                    extrinsic_info: extrinsic_info.clone(),
                    transfer_value,
                },
                *block_info,
                mode,
            ))
            .await?;
    } else if let Some(transfer_value) = transfer_value
        && transfer_value >= MIN_BALANCE_CHANGE
    {
        alert_tx
            .send(Alert::new(
                AlertKind::LargeBalanceTransfer {
                    extrinsic_info: extrinsic_info.clone(),
                    transfer_value,
                },
                *block_info,
                mode,
            ))
            .await?;
    } else if transfer_value.is_none()
        && extrinsic_info.pallet == "Balances"
        && !["transfer_all", "upgrade_accounts"].contains(&extrinsic_info.call.as_str())
    {
        // Every other Balances extrinsic should have an amount.
        // TODO:
        // - check transfer_all by accessing account storage to get the value, this is a low
        //   priority because it is already covered by balance events
        warn!(
            ?mode,
            ?extrinsic_info,
            "Balance: extrinsic amount unavailable in block",
        );
    }

    let accounts = extrinsic_info.accounts();
    let important_address_kinds = accounts
        .iter()
        .flat_map(|account| account.important_address_kind())
        .collect::<Vec<_>>();
    trace!(
        ?mode,
        ?accounts,
        ?important_address_kinds,
        ?extrinsic_info,
        ?block_info,
        "extrinsic account list"
    );

    if !important_address_kinds.is_empty() {
        alert_tx
            .send(Alert::new(
                AlertKind::ImportantAddressTransfer {
                    address_kinds: important_address_kinds.join(", "),
                    extrinsic_info: extrinsic_info.clone(),
                    // The transfer value can be missing for a transfer_all call.
                    // TODO: check account storage to get the transfer value if it is missing
                    transfer_value,
                },
                *block_info,
                mode,
            ))
            .await?;
    }

    Ok(())
}

/// Check a Balance event for alerts.
/// Does nothing if the event is any other kind of event.
pub async fn check_transfer_event(
    mode: BlockCheckMode,
    alert_tx: &mpsc::Sender<Alert>,
    event_info: &EventInfo,
    block_info: &BlockInfo,
) -> anyhow::Result<()> {
    // Large balance changes are alerts.

    // TODO:
    // - track the total of recent events, so the threshold can't be bypassed by splitting the
    //   transfer into multiple calls
    let transfer_value = event_info.transfer_value();
    debug!(?mode, "transfer_value: {:?}", transfer_value);

    // TODO:
    // - do we want to track burn calls? <https://autonomys.subscan.io/extrinsic/137324-31>
    if let Some(transfer_value) = transfer_value
        && transfer_value >= MIN_BALANCE_CHANGE
    {
        alert_tx
            .send(Alert::new(
                AlertKind::LargeBalanceTransferEvent {
                    event_info: event_info.clone(),
                    transfer_value,
                },
                *block_info,
                mode,
            ))
            .await?;
    }

    let accounts = event_info.accounts();
    let important_address_kinds = accounts
        .iter()
        .flat_map(|account| account.important_address_kind())
        .collect::<Vec<_>>();
    trace!(
        ?mode,
        ?accounts,
        ?important_address_kinds,
        ?event_info,
        ?block_info,
        "event account list"
    );

    if !important_address_kinds.is_empty() {
        alert_tx
            .send(Alert::new(
                AlertKind::ImportantAddressTransferEvent {
                    address_kinds: important_address_kinds.join(", "),
                    event_info: event_info.clone(),
                    // The transfer value shouldn't be missing, but we can't rely on the data
                    // format.
                    transfer_value,
                },
                *block_info,
                mode,
            ))
            .await?;
    }

    Ok(())
}
