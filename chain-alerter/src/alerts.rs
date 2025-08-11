//! Specific chain alerts.

use crate::format::fmt_amount;
use crate::slack::SlackClientInfo;
use crate::subspace::{AI3, BlockInfo, ExtrinsicInfo, SubspaceConfig};
use scale_value::Composite;
use std::time::Duration;
use subxt::blocks::ExtrinsicDetails;
use subxt::client::OnlineClientT;
use tracing::warn;

/// The minimum balance change to alert on.
const MIN_BALANCE_CHANGE: u128 = 1_000_000_000 * AI3;

pub async fn check_extrinsic<Client>(
    slack_client_info: &SlackClientInfo,
    extrinsic: &ExtrinsicDetails<SubspaceConfig, Client>,
    block_info: &BlockInfo,
) -> anyhow::Result<()>
where
    Client: OnlineClientT<SubspaceConfig>,
{
    let Some(extrinsic_info) = ExtrinsicInfo::new(extrinsic, block_info) else {
        return Ok(());
    };

    // TODO:
    // - extract each alert into a pallet-specific function or trait object
    // - add tests to make sure we can parse the extrinsics for each alert
    // - format account IDs as ss58 with prefix 6094
    // - link extrinsic and account to subscan

    // All sudo calls are alerts.
    // TODO:
    // - check if the call is from the sudo account
    // - decode the inner call
    if extrinsic_info.pallet == "Sudo" {
        slack_client_info
            .post_message(
                format!(
                    "Sudo call detected\n\
                    {extrinsic_info}",
                ),
                block_info,
            )
            .await?;
    } else if extrinsic_info.pallet == "Balances" {
        // "force*" calls and large balance changes are alerts.

        // subxt knows these field names, so we can search for the transfer value by
        // name.
        // TODO:
        // - track the total of recent transfers, so the threshold can't be bypassed by
        //   splitting the transfer into multiple calls
        // - split this field search into a function which takes a field name,
        //   and another function which does the numeric conversion and range check
        let transfer_value = if let Composite::Named(named_fields) = &extrinsic_info.fields
            && let Some((_, transfer_value)) = named_fields
                .iter()
                .find(|(name, _)| ["value", "amount", "new_free", "delta"].contains(&name.as_str()))
        {
            transfer_value.as_u128()
        } else {
            None
        };

        if extrinsic_info.call.starts_with("force") {
            slack_client_info
                .post_message(
                    format!(
                        "Force Balances call detected\n\
                        Transfer value: {}\n\
                        {extrinsic_info}",
                        fmt_amount(transfer_value),
                    ),
                    block_info,
                )
                .await?;
        } else if let Some(transfer_value) = transfer_value
            && transfer_value >= MIN_BALANCE_CHANGE
        {
            slack_client_info
                .post_message(
                    format!(
                        "Large Balances call detected\n\
                        Transfer value: {} (above {})\n\
                        {extrinsic_info}",
                        fmt_amount(transfer_value),
                        fmt_amount(MIN_BALANCE_CHANGE),
                    ),
                    block_info,
                )
                .await?;
        } else if transfer_value.is_none()
            && !["transfer_all", "upgrade_accounts"].contains(&extrinsic_info.call.as_str())
        {
            // Every other Balances extrinsic should have an amount.
            // TODO: check transfer_all by accessing account storage to get the value
            warn!(
                "Balance: extrinsic amount unavailable in block:\n\
                {extrinsic_info}",
            );
        }
    }

    Ok(())
}
