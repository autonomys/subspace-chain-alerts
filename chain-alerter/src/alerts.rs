//! Specific chain alerts.

use crate::format::{fmt_amount, fmt_duration};
use crate::slack::SlackClientInfo;
use crate::subspace::{
    AI3, BlockInfo, ExtrinsicInfo, SubspaceConfig, gap_since_last_block, gap_since_time,
};
use chrono::Utc;
use scale_value::Composite;
use std::sync::Arc;
use std::time::Duration;
use subxt::blocks::ExtrinsicDetails;
use subxt::client::OnlineClientT;
use tokio::sync::watch;
use tokio::time::sleep;
use tracing::warn;

/// The minimum balance change to alert on.
const MIN_BALANCE_CHANGE: u128 = 1_000_000_000 * AI3;

/// The minimum gap between block timestamps to alert on.
/// The target block gap is 6 seconds, so we alert if it takes substantially longer.
///
/// `pallet-timestamp` enforces a `MinimumPeriod` of 3 seconds in Subspace, and a
/// `MAX_TIMESTAMP_DRIFT_MILLIS` of 30 seconds from each node's local clock.
/// <https://github.com/paritytech/polkadot-sdk/blob/0034d178fff88a0fd87cf0ec1d8f122ae0011d78/substrate/frame/timestamp/src/lib.rs#L307>
const MIN_BLOCK_GAP: Duration = Duration::from_secs(60);

/// Check a block for alerts, against the previous block.
pub async fn check_block(
    slack_client_info: &SlackClientInfo,
    block_info: &BlockInfo,
    prev_block_info: &Option<BlockInfo>,
) -> anyhow::Result<()> {
    let Some(prev_block_info) = prev_block_info else {
        // No last block to check against.
        return Ok(());
    };

    // Because it depends on the next block, this check logs after block production resumes.
    if let Some(gap) = gap_since_last_block(block_info.clone(), prev_block_info.clone()) {
        if gap >= MIN_BLOCK_GAP {
            slack_client_info
                .post_message(
                    format!(
                        "Block production resumed\n\
                        Gap: {}\n\n\
                        Previous block:\n\
                        {prev_block_info}",
                        fmt_duration(gap),
                    ),
                    block_info,
                )
                .await?;
        }
    } else {
        // No block time to check against.
        warn!(
            "Block time unavailable in block:\n\
            {block_info}\n\
            Previous block:\n\
            {prev_block_info}"
        );
    };

    Ok(())
}

/// Spawn a task that waits for `MIN_BLOCK_GAP`, then alerts if there was no block received on
/// `latest_block_rx` in that gap.
pub async fn check_for_block_stall(
    slack_client_info: Arc<SlackClientInfo>,
    block_info: BlockInfo,
    latest_block_rx: watch::Receiver<Option<BlockInfo>>,
) {
    let old_block_info = block_info;

    // Since we exit on panic, there's no need to check the result of the spawned task.
    tokio::spawn(async move {
        sleep(MIN_BLOCK_GAP).await;

        // Avoid a potential deadlock by cloning the watched value immediately.
        let latest_block_info = latest_block_rx
            .borrow()
            .clone()
            .expect("never empty, a block is sent before spawning this task");

        if latest_block_info.block_time > old_block_info.block_time {
            // There's a new block since we sent our block and spawned our task, so block
            // production hasn't stalled. But the latest block also spawned a task, so it
            // will alert if there is actually a stall.
            return;
        }

        let gap = gap_since_time(Utc::now(), old_block_info.clone());
        slack_client_info
            .post_message(
                format!(
                    "Block production stalled\n\
                    Gap: {}",
                    fmt_duration(gap),
                ),
                &old_block_info,
            )
            .await
            .expect("sending Slack alert failed");
    });
}

/// Check an extrinsic for alerts.
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
