//! Balance transfer alerts.

use crate::alerts::{Alert, AlertKind, BlockCheckMode, MIN_BALANCE_CHANGE};
use crate::subspace::{Balance, BlockInfo, ExtrinsicInfo};
use scale_value::Composite;
use tokio::sync::mpsc;
use tracing::{debug, warn};

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

    // subxt knows these field names, so we can search for the transfer value by
    // name.
    // TODO:
    // - track the total of recent transfers, so the threshold can't be bypassed by splitting the
    //   transfer into multiple calls
    // - split this field search into a function which takes a field name, and another function
    //   which does the numeric conversion and range check
    let transfer_value: Option<Balance> = if let Composite::Named(named_fields) =
        &extrinsic_info.fields
        && let Some((_, transfer_value)) = named_fields
            .iter()
            .find(|(name, _)| ["value", "amount", "new_free", "delta"].contains(&name.as_str()))
    {
        transfer_value.as_u128()
    } else {
        None
    };

    debug!(?mode, "transfer_value: {:?}", transfer_value);

    // TODO:
    // - test force alerts by checking a historic block with that call
    // - do we want to track burn calls? <https://autonomys.subscan.io/extrinsic/137324-31>
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
        // TODO: check transfer_all by accessing account storage to get the value
        warn!(
            ?mode,
            ?extrinsic_info,
            "Balance: extrinsic amount unavailable in block",
        );
    }

    Ok(())
}
