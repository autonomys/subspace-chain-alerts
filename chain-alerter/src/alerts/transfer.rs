//! Balance transfer tracking for alerts.

use crate::subspace::{Balance, EventInfo, ExtrinsicInfo};
use scale_value::Composite;

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
