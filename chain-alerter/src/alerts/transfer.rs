//! Balance transfer alerts.

use crate::subspace::decode::decode_h256_from_composite;
use crate::subspace::{Balance, EventInfo, ExtrinsicInfo};
use scale_value::Composite;
use std::collections::BTreeSet;
use std::fmt::{self, Display};
use std::str::FromStr;
use subxt::utils::AccountId32;
use tracing::{error, trace};

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
    // <https://forum.autonomys.xyz/t/boosting-early-staking-the-guardians-of-growth-initiative/4962/2>
    (
        "Guardians of Growth",
        "sugQzjjyAfhzktFDdAkZrcTq5qzMaRoSV2qs1gTcjjuBeybWT",
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
    /// Returns the total transfer value, if it is present.
    fn transfer_value(&self) -> Option<Balance>;
}

impl TransferValue for ExtrinsicInfo {
    fn transfer_value(&self) -> Option<Balance> {
        if self.pallet == "Balances" {
            // subxt knows the field names, so we can search for the transfer value by name.
            return total_transfer_value(&self.fields, &["value", "amount", "new_free", "delta"]);
        } else if self.pallet == "Transporter" || self.pallet == "Domains" {
            // Operator nomination is a kind of transfer to an operator stake.
            return total_transfer_value(&self.fields, &["amount"]);
        }

        None
    }
}

impl TransferValue for EventInfo {
    fn transfer_value(&self) -> Option<Balance> {
        if self.pallet == "Balances" || self.pallet == "Transporter" || self.pallet == "Domains" {
            return total_transfer_value(&self.fields, &["amount"]);
        } else if self.pallet == "Transactionpayment" {
            return total_transfer_value(&self.fields, &["actual_fee", "tip"]);
        }

        None
    }
}

// TODO: split accounts into a separate file

/// An account ID and attached role type.
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd)]
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
    /// Returns the signer for extrinsics or the `who` for events.
    fn initiator_account(&self) -> Option<Account>;

    /// Returns the account ID and type for the initiator account, if any.
    #[expect(dead_code, reason = "included for completeness")]
    fn initiator_account_str(&self) -> Option<String> {
        self.initiator_account().map(|account| account.to_string())
    }

    /// Returns the important address kind for the initiator account, if any.
    fn initiator_account_kind(&self) -> Option<&'static str> {
        self.initiator_account()
            .and_then(|account| account.important_address_kind())
    }

    /// Returns sorted account IDs and types, if present.
    fn accounts(&self) -> BTreeSet<Account>;

    /// Returns a sorted list of account IDs and types, separated by commas.
    /// Returns `None` if there are no accounts.
    fn accounts_str(&self) -> Option<String> {
        let accounts = self.accounts();

        if accounts.is_empty() {
            return None;
        }

        Some(
            accounts
                .iter()
                .map(|account| account.to_string())
                .collect::<Vec<String>>()
                .join(", "),
        )
    }

    /// Returns sorted important address kinds, if the addresses are important.
    fn important_address_kinds(&self) -> BTreeSet<&str> {
        self.accounts()
            .iter()
            .flat_map(|account| account.important_address_kind())
            .collect()
    }

    /// Returns a sorted list of important address kinds, separated by commas.
    /// Returns `None` if there are no important address kinds.
    fn important_address_kinds_str(&self) -> Option<String> {
        let important_address_kinds = self.important_address_kinds();

        if important_address_kinds.is_empty() {
            return None;
        }

        Some(
            important_address_kinds
                .into_iter()
                .collect::<Vec<&str>>()
                .join(", "),
        )
    }
}

impl Accounts for ExtrinsicInfo {
    fn initiator_account(&self) -> Option<Account> {
        self.signing_address
            .as_ref()
            .map(|signing_address| Account::Signer(signing_address.clone()))
    }

    fn accounts(&self) -> BTreeSet<Account> {
        let mut account_list = BTreeSet::new();

        if let Some(signing_account) = self.initiator_account() {
            // Handle signer for Balances, Transporter, Domains, etc.
            account_list.insert(signing_account);
        }

        if self.pallet == "Balances" {
            let accounts = list_accounts(&self.fields, &["dest"])
                .into_iter()
                .map(Account::Receiver);

            account_list.extend(accounts);
        }

        // TODO: add `dst_location` AccountId20 from Transporter events:
        // <https://autonomys.subscan.io/extrinsic/4525962-7>

        account_list
    }
}

impl Accounts for EventInfo {
    fn initiator_account(&self) -> Option<Account> {
        let who = list_accounts(&self.fields, &["who"]);

        if who.len() > 1 {
            // This is technically possible in the data format, but it should never actually happen.
            error!("multiple 'who' accounts in event, alerts might have been missed");
            None
        } else {
            who.into_iter().next().map(Account::Signer)
        }
    }

    fn accounts(&self) -> BTreeSet<Account> {
        let mut account_list = BTreeSet::new();

        if self.pallet == "Balances" {
            let receiver_accounts = list_accounts(&self.fields, &["who", "to"]);
            let sender_accounts = list_accounts(&self.fields, &["from"]);

            // Burning or withdrawing means "who" becomes the sender.
            if ["Burned", "Withdraw"].contains(&self.kind.as_str()) {
                account_list.extend(receiver_accounts.into_iter().map(Account::Sender));
            } else {
                account_list.extend(receiver_accounts.into_iter().map(Account::Receiver));
            }

            account_list.extend(sender_accounts.into_iter().map(Account::Sender));
        } else if self.pallet == "Transactionpayment" {
            // Transaction payments are always about the sender.
            let accounts = list_accounts(&self.fields, &["who"]);
            account_list.extend(accounts.into_iter().map(Account::Sender));
        } else if self.pallet == "Domains" {
            let accounts = list_accounts(&self.fields, &["nominator_id"]);
            account_list.extend(accounts.into_iter().map(Account::Sender));
        }

        account_list
    }
}

/// Returns the sum of the transfer values from the supplied named fields.
/// If there are no fields with those names, returns `None`.
pub fn total_transfer_value(fields: &Composite<u32>, field_names: &[&str]) -> Option<Balance> {
    if let Composite::Named(named_fields) = fields {
        let transfer_values: Vec<u128> = named_fields
            .iter()
            .filter(|(name, _)| field_names.contains(&name.as_str()))
            .flat_map(|(_, value)| value.as_u128())
            .collect();

        if transfer_values.is_empty() {
            return None;
        }

        return Some(transfer_values.iter().sum());
    }

    None
}

/// Returns a list of the accounts from the supplied named fields.
/// If there are no fields with those names, returns an empty list.
///
/// Accounts can be duplicated if they perform different roles in the extrinsic or event.
pub fn list_accounts(fields: &Composite<u32>, field_names: &[&str]) -> BTreeSet<AccountId32> {
    if let Composite::Named(named_fields) = fields {
        named_fields
            .iter()
            .filter(|(name, _)| field_names.contains(&name.as_str()))
            .flat_map(|(_, value)| decode_h256_from_composite(value))
            .map(|account_id| account_id.0.into())
            .collect()
    } else {
        BTreeSet::new()
    }
}
