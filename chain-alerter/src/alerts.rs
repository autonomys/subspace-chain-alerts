//! Specific chain alerts.

use crate::format::{MAX_EXTRINSIC_DEBUG_LENGTH, fmt_amount, truncate};
use crate::slack::SlackClientInfo;
use crate::subspace::{AI3, BlockInfo, SubspaceConfig};
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
    let Ok(meta) = extrinsic.extrinsic_metadata() else {
        // If we can't get the extrinsic pallet and call name, there's nothing we can do.
        // Just log it and move on.
        warn!(
            "extrinsic {} pallet/name unavailable in block:\n\
            {block_info}",
            extrinsic.index(),
        );
        return Ok(());
    };

    // We can always hex-print the extrinsic hash.
    let hash = hex::encode(extrinsic.hash());

    // We can usually get the extrinsic fields, but we don't need the fields for some
    // extrinsic alerts. So we just warn and substitute empty fields.
    let fields = extrinsic.field_values().unwrap_or_else(|_| {
        warn!(
            "extrinsic {}:{} {} fields unavailable in block:\n\
            hash: {hash}\n\
            {block_info}",
            meta.pallet.name(),
            meta.variant.name,
            extrinsic.index(),
        );
        Composite::unnamed(Vec::new())
    });
    let fields_str = {
        // The decoded value debug format is extremely verbose, display seems a bit better.
        let mut fields_str = format!("{fields}");
        truncate(&mut fields_str, MAX_EXTRINSIC_DEBUG_LENGTH);
        fields_str
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
    if meta.pallet.name() == "Sudo" {
        slack_client_info
            .post_message(
                format!(
                    "Sudo::{} call detected at extrinsic {}\n\
                    hash: {hash}\n\
                    {fields_str}",
                    meta.variant.name,
                    extrinsic.index()
                ),
                block_info,
            )
            .await?;
    } else if meta.pallet.name() == "Balances" {
        // "force*" calls and large balance changes are alerts.

        // subxt knows these field names, so we can search for the transfer value by
        // name.
        // TODO:
        // - track the total of recent transfers, so the threshold can't be bypassed by
        //   splitting the transfer into multiple calls
        // - split this field search into a function which takes a field name,
        //   and another function which does the numeric conversion and range check
        let transfer_value = if let Composite::Named(named_fields) = fields
            && let Some((_, transfer_value)) = named_fields
                .iter()
                .find(|(name, _)| ["value", "amount", "new_free", "delta"].contains(&name.as_str()))
        {
            transfer_value.as_u128()
        } else {
            None
        };

        if meta.variant.name.starts_with("force") {
            slack_client_info
                .post_message(
                    format!(
                        "Force Balances::{} call detected at extrinsic {}\n\
                            Transfer value: {}\n\
                            hash: {hash}\n\
                            {fields_str}",
                        meta.variant.name,
                        extrinsic.index(),
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
                        "Large Balances::{} call detected at extrinsic {}\n\
                            Transfer value: {} (above {})\n\
                            hash: {hash}\n\
                            {fields_str}",
                        meta.variant.name,
                        extrinsic.index(),
                        fmt_amount(transfer_value),
                        fmt_amount(MIN_BALANCE_CHANGE),
                    ),
                    block_info,
                )
                .await?;
        } else if transfer_value.is_none()
            && !["transfer_all", "upgrade_accounts"].contains(&meta.variant.name.as_str())
        {
            // Every other Balances extrinsic should have an amount.
            // TODO: check transfer_all by accessing account storage to get the value
            warn!(
                "Balance::{} extrinsic {} amount unavailable in block:\n\
                hash: {hash}\n\
                {fields_str}\n\
                {block_info}",
                meta.variant.name,
                extrinsic.index()
            );
        }
    }

    Ok(())
}
