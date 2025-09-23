//! Balance transfer alerts.

use crate::alerts::{Alert, AlertKind, BlockCheckMode, MIN_BALANCE_CHANGE};
use crate::subspace::{Balance, BlockInfo, EventInfo, ExtrinsicInfo};
use scale_value::Composite;
use tokio::sync::mpsc;
use tracing::{debug, warn};

/// A trait for accessing the transfer value from an object.
pub trait TransferValue {
    /// Returns the transfer value, if it is present.
    fn transfer_value(&self) -> Option<Balance>;
}

impl TransferValue for ExtrinsicInfo {
    fn transfer_value(&self) -> Option<Balance> {
        if self.pallet != "Balances" {
            return None;
        }

        let Composite::Named(named_fields) = &self.fields else {
            return None;
        };

        // subxt knows these field names, so we can search for the transfer value byname.
        // TODO:
        // - split this field search into a function which takes a field name, and another function
        //   which does the numeric conversion and range check
        let (_, transfer_value) = named_fields
            .iter()
            .find(|(name, _)| ["value", "amount", "new_free", "delta"].contains(&name.as_str()))?;

        transfer_value.as_u128()
    }
}

impl TransferValue for EventInfo {
    fn transfer_value(&self) -> Option<Balance> {
        if self.pallet != "Balances" {
            return None;
        }

        let Composite::Named(named_fields) = &self.fields else {
            return None;
        };

        let (_, transfer_value) = named_fields
            .iter()
            .find(|(name, _)| name.as_str() == "amount")?;

        transfer_value.as_u128()
    }
}

/// Check a Balance extrinsic for alerts.
/// Does nothing if the extrinsic is any other kind of extrinsic.
pub async fn check_balance_extrinsic(
    mode: BlockCheckMode,
    alert_tx: &mpsc::Sender<Alert>,
    extrinsic_info: &ExtrinsicInfo,
    block_info: &BlockInfo,
) -> anyhow::Result<()> {
    if extrinsic_info.pallet != "Balances" {
        return Ok(());
    }

    // "force*" calls and large balance changes are alerts.

    // TODO:
    // - track the total of recent transfers, so the threshold can't be bypassed by splitting the
    //   transfer into multiple calls
    let transfer_value = extrinsic_info.transfer_value();

    debug!(?mode, "transfer_value: {:?}", transfer_value);

    // TODO:
    // - test force alerts by checking a historic block with that call
    // - do we want to track burn calls? <https://autonomys.subscan.io/extrinsic/137324-31>
    //   this is a low priority because it is already covered by balance events
    if extrinsic_info.call.starts_with("force") {
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

    Ok(())
}

/// Check a Balance event for alerts.
/// Does nothing if the event is any other kind of event.
pub async fn check_balance_event(
    mode: BlockCheckMode,
    alert_tx: &mpsc::Sender<Alert>,
    event_info: &EventInfo,
    block_info: &BlockInfo,
) -> anyhow::Result<()> {
    if event_info.pallet != "Balances" {
        return Ok(());
    }

    // Large balance changes are alerts.

    // TODO:
    // - track the total of recent events, so the threshold can't be bypassed by splitting the
    //   transfer into multiple calls
    let Some(transfer_value) = event_info.transfer_value() else {
        return Ok(());
    };

    debug!(?mode, "transfer_value: {:?}", transfer_value);

    // TODO:
    // - do we want to track burn calls? <https://autonomys.subscan.io/extrinsic/137324-31>
    if transfer_value >= MIN_BALANCE_CHANGE {
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

    Ok(())
}
