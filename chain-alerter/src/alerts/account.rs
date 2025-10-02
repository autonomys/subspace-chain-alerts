//! Account and address tracking for alerts.

use crate::subspace::decode::decode_h256_from_composite;
use crate::subspace::{AccountId, EventInfo, ExtrinsicInfo};
use scale_value::Composite;
use sp_core::crypto::Ss58Codec;
use std::collections::{BTreeSet, HashMap};
use std::fmt::{self, Display};
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
    // <https://polkadot.js.org/apps/?rpc=wss%3A%2F%2Frpc-0.mainnet.autonomys.xyz%2Fws#/chainstate>
    // sudo.key()
    // <https://autonomys.subscan.io/account/subKQqsYRyVkugvKQqLXEuhsefa9728PBAqtwxpeM5N4VD6mv>
    // TODO: dynamically look this up from storage instead of hardcoding it
    ("Sudo", "subKQqsYRyVkugvKQqLXEuhsefa9728PBAqtwxpeM5N4VD6mv"),
];

/// If the address is an important address, returns the kind of important address, otherwise returns
/// `None`.
pub fn important_address_kind(account_id: &AccountId) -> Option<&'static str> {
    IMPORTANT_ADDRESSES
        .iter()
        .find(|(_, addr)| {
            let addr_id = if let Ok(account) = AccountId::from_string(addr) {
                account
            } else {
                let bytes = hex::decode(addr).expect("constants are valid ss58check or hex");
                let array = <[u8; 32]>::try_from(bytes).expect("hex constants are 32 bytes");
                AccountId::from(array)
            };

            trace!(?addr_id, ?account_id, "important address kind check");
            &addr_id == account_id
        })
        .map(|(kind, _)| *kind)
}

/// An account role type.
/// This can be used as a marker type, or to contain accounts or lists of accounts.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub enum AccountRole<T> {
    /// The signer of an extrinsic.
    Signer(T),

    /// A transfer sender.
    Sender(T),

    /// A transfer receiver.
    Receiver(T),
}

/// A marker type for an account role.
pub type AccountRoleMarker = AccountRole<()>;

/// A single account and its role.
pub type AccountIdRole = AccountRole<AccountId>;

/// A sorted list of accounts with the same role.
pub type AccountListRole = AccountRole<BTreeSet<AccountId>>;

/// A single account and its role.
pub type ImportantAccountRole = AccountRole<&'static str>;

/// A sorted list of important accounts with the same role.
pub type ImportantAccountListRole = AccountRole<BTreeSet<&'static str>>;

impl<T> AccountRole<T> {
    /// Returns the account ID.
    pub fn inner(&self) -> &T {
        match self {
            AccountRole::Signer(inner) => inner,
            AccountRole::Sender(inner) => inner,
            AccountRole::Receiver(inner) => inner,
        }
    }

    /// Returns the variant as a marker.
    pub fn as_marker(&self) -> AccountRoleMarker {
        match self {
            AccountRole::Signer(_) => AccountRoleMarker::SIGNER,
            AccountRole::Sender(_) => AccountRoleMarker::SENDER,
            AccountRole::Receiver(_) => AccountRoleMarker::RECEIVER,
        }
    }

    /// Transforms the inner value into a reference.
    #[expect(dead_code, reason = "included for completeness")]
    pub fn as_ref(&self) -> AccountRole<&T> {
        match self {
            AccountRole::Signer(inner) => AccountRole::Signer(inner),
            AccountRole::Sender(inner) => AccountRole::Sender(inner),
            AccountRole::Receiver(inner) => AccountRole::Receiver(inner),
        }
    }

    /// Maps an owned inner value to a new value, possibly of a different type.
    pub fn map<U, F: FnOnce(T) -> U>(self, f: F) -> AccountRole<U> {
        match self {
            AccountRole::Signer(inner) => AccountRole::Signer(f(inner)),
            AccountRole::Sender(inner) => AccountRole::Sender(f(inner)),
            AccountRole::Receiver(inner) => AccountRole::Receiver(f(inner)),
        }
    }
}

impl AccountRoleMarker {
    /// The receiver role marker.
    pub const RECEIVER: AccountRoleMarker = AccountRoleMarker::Receiver(());
    /// The sender role marker.
    pub const SENDER: AccountRoleMarker = AccountRoleMarker::Sender(());
    /// The signer role marker.
    pub const SIGNER: AccountRoleMarker = AccountRoleMarker::Signer(());
}

impl<T: Ord> AccountRole<BTreeSet<T>> {
    /// Creates a new account role from a marker and a single value.
    #[allow(dead_code, reason = "only used in tests")]
    pub fn with_marker_and_value(marker: AccountRoleMarker, value: T) -> Self {
        match marker {
            AccountRoleMarker::Signer(_) => AccountRole::Signer(BTreeSet::from([value])),
            AccountRoleMarker::Sender(_) => AccountRole::Sender(BTreeSet::from([value])),
            AccountRoleMarker::Receiver(_) => AccountRole::Receiver(BTreeSet::from([value])),
        }
    }

    /// Creates a new account role from a single `AccountIdRole` (or similar type).
    #[expect(dead_code, reason = "included for completeness")]
    pub fn from_value(value: AccountRole<T>) -> Self {
        value.map(|value| BTreeSet::from([value]))
    }
}

impl Display for AccountRoleMarker {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // At startup, we set the correct SS58 prefix, so this Display impl shows Subspace
        // addresses.
        match self {
            AccountRoleMarker::Signer(()) => write!(f, "Signer"),
            AccountRoleMarker::Sender(()) => write!(f, "Sender"),
            AccountRoleMarker::Receiver(()) => write!(f, "Receiver"),
        }
    }
}

impl Display for AccountIdRole {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.as_marker(), self.inner())
    }
}

impl Display for ImportantAccountRole {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.as_marker(), self.inner())
    }
}

impl<T: Display> Display for AccountRole<BTreeSet<T>> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}: {}",
            self.as_marker(),
            self.inner()
                .iter()
                .map(|id| id.to_string())
                .collect::<Vec<String>>()
                .join(", ")
        )
    }
}

/// A trait for accessing the account IDs and account types from an object.
pub trait Accounts {
    /// Returns the signer for extrinsics or the `who` for events.
    fn initiator_account(&self) -> Option<AccountIdRole>;

    /// Returns the account ID and role for the initiator account, if any.
    #[expect(dead_code, reason = "included for completeness")]
    fn initiator_account_str(&self) -> Option<String> {
        self.initiator_account().map(|account| account.to_string())
    }

    /// Returns the important address kind for the initiator account, if any.
    fn initiator_account_kind(&self) -> Option<ImportantAccountRole> {
        self.initiator_account().and_then(|account| {
            let important_address_kind = important_address_kind(account.inner())?;
            Some(account.map(|_account_id| important_address_kind))
        })
    }

    /// Returns accounts, grouped by role, if any are present.
    fn accounts(&self) -> HashMap<AccountRoleMarker, BTreeSet<AccountId>>;

    // Returns account address strings, grouped by role then sorted, separated by commas.
    /// Returns `None` if there are no accounts.
    fn accounts_str(&self) -> Option<String> {
        let accounts = self.accounts();

        if accounts.is_empty() {
            return None;
        }

        let sorted_grouped_accounts = accounts
            .into_iter()
            .map(|(role, accounts)| role.map(|()| accounts))
            .collect::<BTreeSet<AccountListRole>>();

        Some(
            sorted_grouped_accounts
                .into_iter()
                .map(|account_list| account_list.to_string())
                .collect::<Vec<String>>()
                .join(", "),
        )
    }

    /// Returns important address kinds, grouped by role, if the addresses are important.
    fn important_address_kinds(&self) -> HashMap<AccountRoleMarker, BTreeSet<&'static str>> {
        self.accounts()
            .into_iter()
            .filter_map(|(role_marker, account_list)| {
                let important_address_kinds: BTreeSet<&'static str> = account_list
                    .iter()
                    .filter_map(|account| important_address_kind(account))
                    .collect();

                if important_address_kinds.is_empty() {
                    None
                } else {
                    Some((role_marker, important_address_kinds))
                }
            })
            .collect()
    }

    /// Returns a sorted list of important address kinds, separated by commas.
    /// Returns `None` if there are no important address kinds.
    fn important_address_kinds_str(&self) -> Option<String> {
        let important_address_kinds = self.important_address_kinds();

        if important_address_kinds.is_empty() {
            return None;
        }

        let sorted_grouped_accounts = important_address_kinds
            .into_iter()
            .map(|(role, accounts)| role.map(|()| accounts))
            .collect::<BTreeSet<ImportantAccountListRole>>();

        Some(
            sorted_grouped_accounts
                .into_iter()
                .map(|account_list| account_list.to_string())
                .collect::<Vec<String>>()
                .join(", "),
        )
    }
}

impl Accounts for ExtrinsicInfo {
    fn initiator_account(&self) -> Option<AccountIdRole> {
        self.signing_address
            .as_ref()
            .map(|signing_address| AccountIdRole::Signer(signing_address.clone()))
    }

    fn accounts(&self) -> HashMap<AccountRoleMarker, BTreeSet<AccountId>> {
        let mut account_list = HashMap::new();

        if let Some(signing_account) = self.initiator_account() {
            // Handle signer for Balances, Transporter, Domains, etc.
            let prev_value = account_list.insert(
                AccountRoleMarker::SIGNER,
                BTreeSet::from([signing_account.inner().clone()]),
            );
            assert_eq!(prev_value, None, "signer already exists");
        }

        let accounts = list_accounts(&self.fields, &["dest"]);
        if self.pallet == "Balances"
            && let Some(accounts) = accounts
        {
            let prev_value = account_list.insert(AccountRoleMarker::RECEIVER, accounts);
            assert_eq!(prev_value, None, "receiver already exists");
        }

        // TODO: add `dst_location` AccountId20 from Transporter events:
        // <https://autonomys.subscan.io/extrinsic/4525962-7>

        account_list
    }
}

impl Accounts for EventInfo {
    fn initiator_account(&self) -> Option<AccountIdRole> {
        let who = list_accounts(&self.fields, &["who"])?;

        if who.len() > 1 {
            // This is technically possible in the data format, but it should never actually happen.
            error!("multiple 'who' accounts in event, alerts might have been missed");
            None
        } else {
            who.into_iter().next().map(AccountIdRole::Signer)
        }
    }

    fn accounts(&self) -> HashMap<AccountRoleMarker, BTreeSet<AccountId>> {
        let mut account_list = HashMap::new();

        if let Some(signing_account) = self.initiator_account() {
            // Handle initiator for Balances, Transporter, Domains, etc.
            let prev_value = account_list.insert(
                AccountRoleMarker::SIGNER,
                BTreeSet::from([signing_account.inner().clone()]),
            );
            assert_eq!(prev_value, None, "signer already exists");
        }

        let who_accounts = list_accounts(&self.fields, &["who"]);

        if self.pallet == "Balances" {
            let sender_accounts = list_accounts(&self.fields, &["from"]);
            let receiver_accounts = list_accounts(&self.fields, &["to", "account"]);

            if let Some(who_accounts) = who_accounts {
                if ["Burned", "Withdraw"].contains(&self.kind.as_str()) {
                    // Burning or withdrawing means "who" becomes the sender.
                    let prev_value = account_list.insert(AccountRoleMarker::SENDER, who_accounts);
                    assert_eq!(prev_value, None, "sender already exists");
                } else {
                    // Otherwise, "who" is the receiver.
                    let prev_value = account_list.insert(AccountRoleMarker::RECEIVER, who_accounts);
                    assert_eq!(prev_value, None, "receiver already exists");
                }
            }

            if let Some(sender_accounts) = sender_accounts {
                account_list
                    .entry(AccountRoleMarker::SENDER)
                    .or_default()
                    .extend(sender_accounts);
            }

            if let Some(receiver_accounts) = receiver_accounts {
                account_list
                    .entry(AccountRoleMarker::RECEIVER)
                    .or_default()
                    .extend(receiver_accounts);
            }
        } else if self.pallet == "Transactionpayment"
            && let Some(who_accounts) = who_accounts
        {
            // In transaction payments, "who" is always the sender.
            let prev_value = account_list.insert(AccountRoleMarker::SENDER, who_accounts);
            assert_eq!(prev_value, None, "sender already exists");
        } else if self.pallet == "Domains" {
            let accounts = list_accounts(&self.fields, &["nominator_id"]);

            if let Some(accounts) = accounts {
                let prev_value = account_list.insert(AccountRoleMarker::SENDER, accounts);
                assert_eq!(prev_value, None, "sender already exists");
            }
        }

        account_list
    }
}

/// Returns a list of the accounts from the supplied named fields.
/// If there are no fields with those names, returns `None`.
///
/// Accounts can be duplicated if they perform different roles in the extrinsic or event.
pub fn list_accounts(fields: &Composite<u32>, field_names: &[&str]) -> Option<BTreeSet<AccountId>> {
    if let Composite::Named(named_fields) = fields {
        let accounts: BTreeSet<AccountId> = named_fields
            .iter()
            .filter(|(name, _)| field_names.contains(&name.as_str()))
            .flat_map(|(_, value)| decode_h256_from_composite(value))
            .map(|account_id| account_id.0.into())
            .collect();

        if accounts.is_empty() {
            None
        } else {
            Some(accounts)
        }
    } else {
        None
    }
}
