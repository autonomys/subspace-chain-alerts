//! Specific chain alerts.

#[cfg(test)]
mod tests;

use crate::format::{fmt_amount, fmt_duration, fmt_timestamp};
use crate::subspace::{
    AI3, Balance, BlockInfo, BlockTime, EventInfo, ExtrinsicInfo, SubspaceConfig,
    gap_since_last_block, gap_since_time,
};
use chrono::Utc;
use scale_value::Composite;
use std::fmt::{self, Display};
use std::time::Duration;
use subxt::blocks::ExtrinsicDetails;
use subxt::client::OnlineClientT;
use subxt::events::EventDetails;
use tokio::sync::{mpsc, watch};
use tokio::time::sleep;
use tracing::{debug, warn};

/// The minimum balance change to alert on.
const MIN_BALANCE_CHANGE: Balance = 1_000_000 * AI3;

/// The minimum gap between block timestamps to alert on.
/// The target block gap is 6 seconds, so we alert if it takes substantially longer.
///
/// `pallet-timestamp` enforces a `MinimumPeriod` of 3 seconds in Subspace, and a
/// `MAX_TIMESTAMP_DRIFT_MILLIS` of 30 seconds from each node's local clock.
/// <https://github.com/paritytech/polkadot-sdk/blob/0034d178fff88a0fd87cf0ec1d8f122ae0011d78/substrate/frame/timestamp/src/lib.rs#L307>
const MIN_BLOCK_GAP: Duration = Duration::from_secs(60);

/// Whether we are replaying missed blocks, or checking current blocks.
/// This impacts block stall checks, which can only be spawned on new blocks.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum BlockCheckMode {
    /// We are checking current blocks.
    Current,

    /// We are replaying missed blocks.
    Replay,
}

impl BlockCheckMode {
    /// Whether we are checking current blocks.
    pub fn is_current(&self) -> bool {
        *self == BlockCheckMode::Current
    }
}

/// A blockchain alert with context.
#[derive(Clone, Debug, PartialEq)]
pub struct Alert {
    /// The type of alert.
    pub alert: AlertKind,

    /// The block the alert occurred in.
    pub block_info: BlockInfo,

    /// The mode the alert was triggered in.
    pub mode: BlockCheckMode,
}

impl Alert {
    /// Create a new alert.
    pub fn new(alert: AlertKind, block_info: BlockInfo, mode: BlockCheckMode) -> Self {
        Self {
            alert,
            block_info,
            mode,
        }
    }
}

/// The type of alert.
#[derive(Clone, Debug, PartialEq)]
pub enum AlertKind {
    /// The alerter has started.
    Startup,

    /// Block production has stalled.
    BlockProductionStall {
        /// The gap between the previous block and now.
        ///
        /// Note: the previous block is `Alert.block_info`.
        gap: Option<Duration>,
    },

    /// Block production has resumed.
    BlockProductionResumed {
        /// The gap between the previous and current block.
        ///
        /// Note: the current block is `Alert.block_info`.
        gap: Duration,

        /// The previous block.
        prev_block_info: BlockInfo,
    },

    /// A `force_*` Balances call has been detected.
    ForceBalanceTransfer {
        /// The Balance call's extrinsic information.
        extrinsic_info: ExtrinsicInfo,

        /// The transfer value.
        transfer_value: Option<Balance>,
    },

    /// A large Balance transfer has been detected.
    LargeBalanceTransfer {
        /// The Balance call's extrinsic information.
        extrinsic_info: ExtrinsicInfo,

        /// The transfer value.
        transfer_value: Balance,
    },

    /// A Sudo call has been detected.
    SudoCall {
        /// The sudo call's extrinsic information.
        extrinsic_info: ExtrinsicInfo,
    },

    /// A Sudo call event has been detected.
    SudoEvent {
        /// The sudo event information.
        event_info: EventInfo,
    },

    /// An operator slash event has been detected.
    OperatorSlashed {
        /// The operator slash event information.
        event_info: EventInfo,
    },

    /// Slot timing is outside the expected range.
    SlotTime {
        /// The current ratio of slots to time.
        current_ratio: f64,

        /// The applicable threshold for this alert.
        threshold: f64,

        /// The duration of the interval.
        interval: Duration,

        /// The time of the first slot in the interval.
        first_slot_time: BlockTime,
    },
}

impl Display for AlertKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AlertKind::Startup => {
                write!(f, "**Launched and connected to the node**")
            }

            AlertKind::BlockProductionStall { gap } => {
                write!(
                    f,
                    "**Block production stalled**\n\
                    Gap: {}",
                    fmt_duration(*gap),
                )
            }

            AlertKind::BlockProductionResumed {
                gap,
                prev_block_info,
            } => {
                write!(
                    f,
                    "**Block production resumed**\n\
                    Gap: {}\n\n\
                    Previous block:\n\
                    {prev_block_info}",
                    fmt_duration(*gap),
                )
            }

            AlertKind::ForceBalanceTransfer {
                extrinsic_info,
                transfer_value,
            } => {
                write!(
                    f,
                    "**Force Balances call detected**\n\
                    Transfer value: {}\n\
                    {extrinsic_info}",
                    fmt_amount(*transfer_value),
                )
            }

            AlertKind::LargeBalanceTransfer {
                extrinsic_info,
                transfer_value,
            } => {
                write!(
                    f,
                    "**Large Balances call detected**\n\
                    Transfer value: {} (above {})\n\
                    {extrinsic_info}",
                    fmt_amount(*transfer_value),
                    fmt_amount(MIN_BALANCE_CHANGE),
                )
            }

            AlertKind::SudoCall { extrinsic_info } => {
                write!(
                    f,
                    "**Sudo call detected**\n\
                    {extrinsic_info}",
                )
            }

            AlertKind::SudoEvent { event_info } => {
                write!(
                    f,
                    "**Sudo event detected**\n\
                    {event_info}",
                )
            }

            AlertKind::OperatorSlashed { event_info } => {
                write!(
                    f,
                    "**Operator slash detected**\n\
                    {event_info}",
                )
            }

            AlertKind::SlotTime {
                current_ratio,
                threshold,
                interval,
                first_slot_time,
            } => {
                write!(
                    f,
                    "**Slot per time ratio alert**\n\
                    Current ratio: {current_ratio:.2} slots per second\n\
                    Threshold: {threshold:.2} slots per second\n\
                    Interval: {}\n\
                    First slot time: {}",
                    fmt_duration(*interval),
                    fmt_timestamp(first_slot_time.date_time()),
                )
            }
        }
    }
}

impl AlertKind {
    /// Extract the previous block from the alert, if present.
    #[allow(dead_code, reason = "TODO: use in tests")]
    pub fn prev_block_info(&self) -> Option<&BlockInfo> {
        match self {
            AlertKind::BlockProductionResumed {
                prev_block_info, ..
            } => Some(prev_block_info),
            // Deliberately repeat each enum variant here, so we can't forget to update this
            // method when adding new variants.
            AlertKind::Startup
            | AlertKind::BlockProductionStall { .. }
            | AlertKind::ForceBalanceTransfer { .. }
            | AlertKind::LargeBalanceTransfer { .. }
            | AlertKind::SudoCall { .. }
            | AlertKind::SudoEvent { .. }
            | AlertKind::OperatorSlashed { .. }
            | AlertKind::SlotTime { .. } => None,
        }
    }

    /// Extract the extrinsic from the alert, if present.
    #[cfg_attr(not(test), allow(dead_code, reason = "only used in tests"))]
    pub fn extrinsic_info(&self) -> Option<&ExtrinsicInfo> {
        match self {
            AlertKind::ForceBalanceTransfer { extrinsic_info, .. } => Some(extrinsic_info),
            AlertKind::LargeBalanceTransfer { extrinsic_info, .. } => Some(extrinsic_info),
            AlertKind::SudoCall { extrinsic_info } => Some(extrinsic_info),
            AlertKind::Startup
            | AlertKind::BlockProductionStall { .. }
            | AlertKind::BlockProductionResumed { .. }
            | AlertKind::SudoEvent { .. }
            | AlertKind::OperatorSlashed { .. }
            | AlertKind::SlotTime { .. } => None,
        }
    }

    /// Extract the transfer value from the alert, if present.
    #[allow(dead_code, reason = "TODO: use in tests")]
    pub fn transfer_value(&self) -> Option<Balance> {
        match self {
            AlertKind::ForceBalanceTransfer { transfer_value, .. } => *transfer_value,
            AlertKind::LargeBalanceTransfer { transfer_value, .. } => Some(*transfer_value),
            AlertKind::Startup
            | AlertKind::BlockProductionStall { .. }
            | AlertKind::BlockProductionResumed { .. }
            | AlertKind::SudoCall { .. }
            | AlertKind::SudoEvent { .. }
            | AlertKind::OperatorSlashed { .. }
            | AlertKind::SlotTime { .. } => None,
        }
    }

    /// Extract the event from the alert, if present.
    #[cfg_attr(not(test), allow(dead_code, reason = "only used in tests"))]
    pub fn event_info(&self) -> Option<&EventInfo> {
        match self {
            AlertKind::SudoEvent { event_info } => Some(event_info),
            AlertKind::OperatorSlashed { event_info } => Some(event_info),
            AlertKind::Startup
            | AlertKind::BlockProductionStall { .. }
            | AlertKind::BlockProductionResumed { .. }
            | AlertKind::ForceBalanceTransfer { .. }
            | AlertKind::LargeBalanceTransfer { .. }
            | AlertKind::SudoCall { .. }
            | AlertKind::SlotTime { .. } => None,
        }
    }
}

/// Post a startup alert.
///
/// Any returned errors are fatal and require a restart.
pub async fn startup_alert(
    mode: BlockCheckMode,
    alert_tx: &mpsc::Sender<Alert>,
    block_info: &BlockInfo,
) -> anyhow::Result<()> {
    assert!(
        mode.is_current(),
        "should only be called on the first current block"
    );

    // TODO:
    // - always post this to the test channel, because it's not a real "alert"
    // - link to the prod channel from this message: <https://docs.slack.dev/messaging/formatting-message-text/#linking-channels>

    alert_tx
        .send(Alert::new(AlertKind::Startup, *block_info, mode))
        .await?;

    Ok(())
}

/// Check a block for alerts, against the previous block.
///
/// Any returned errors are fatal and require a restart.
pub async fn check_block(
    // TODO: when we add a check that doesn't work on replayed blocks, skip it using mode
    mode: BlockCheckMode,
    alert_tx: &mpsc::Sender<Alert>,
    block_info: &BlockInfo,
    prev_block_info: &Option<BlockInfo>,
) -> anyhow::Result<()> {
    let Some(prev_block_info) = prev_block_info else {
        // No last block to check against.
        return Ok(());
    };

    // Because it depends on the next block, this check logs after block production resumes.
    if let Some(gap) = gap_since_last_block(*block_info, *prev_block_info) {
        if gap >= MIN_BLOCK_GAP {
            alert_tx
                .send(Alert::new(
                    AlertKind::BlockProductionResumed {
                        gap,
                        prev_block_info: *prev_block_info,
                    },
                    *block_info,
                    mode,
                ))
                .await?;
        }
    } else {
        // No block time to check against.
        warn!(
            ?mode,
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
///
/// Doesn't work on replayed blocks, because the chain has already resumed after a replayed block
/// gap.
///
/// Fatal errors will panic in the spawned task.
pub async fn check_for_block_stall(
    mode: BlockCheckMode,
    alert_tx: mpsc::Sender<Alert>,
    block_info: BlockInfo,
    latest_block_rx: watch::Receiver<Option<BlockInfo>>,
) {
    assert!(mode.is_current(), "doesn't work on replayed blocks");

    let old_block_info = block_info;

    // Since we exit on panic, there's no need to check the result of the spawned task.
    tokio::spawn(async move {
        sleep(MIN_BLOCK_GAP).await;

        // Avoid a potential deadlock by copying the watched value immediately.
        let latest_block_info: BlockInfo = latest_block_rx
            .borrow()
            .expect("never empty, a block is sent before spawning this task");

        if latest_block_info.block_time > old_block_info.block_time {
            // There's a new block since we sent our block and spawned our task, so block
            // production hasn't stalled. But the latest block also spawned a task, so it
            // will alert if there is actually a stall.
            return;
        }

        let gap = gap_since_time(Utc::now(), old_block_info);

        // Send errors are fatal and require a restart.
        alert_tx
            .send(Alert::new(
                AlertKind::BlockProductionStall { gap },
                old_block_info,
                mode,
            ))
            .await
            .expect("sending Slack alert failed");
    });
}

/// Check an extrinsic for alerts.
///
/// Extrinsic parsing should never fail, if it does, the runtime metdata is likely wrong.
/// But we don't want to panic or exit when that happens, instead we warn, and hope to
/// recover after we pick up the runtime upgrade in the next block.
///
/// Any returned errors are fatal and require a restart.
pub async fn check_extrinsic<Client>(
    // TODO: when we add a check that doesn't work on replayed blocks, skip it using mode
    mode: BlockCheckMode,
    alert_tx: &mpsc::Sender<Alert>,
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
    // - add extrinsic success/failure to alerts

    // All sudo calls are alerts.
    // TODO:
    // - test this alert by checking a historic block with a sudo call
    // - check if the call is from the sudo account
    // - decode the inner call
    if extrinsic_info.pallet == "Sudo" {
        alert_tx
            .send(Alert::new(
                AlertKind::SudoCall { extrinsic_info },
                *block_info,
                mode,
            ))
            .await?;
    } else if extrinsic_info.pallet == "Balances" {
        // "force*" calls and large balance changes are alerts.

        // subxt knows these field names, so we can search for the transfer value by
        // name.
        // TODO:
        // - track the total of recent transfers, so the threshold can't be bypassed by splitting
        //   the transfer into multiple calls
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
                        extrinsic_info,
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
                        extrinsic_info,
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
                "Balance: extrinsic amount unavailable in block:\n\
                {extrinsic_info}",
            );
        }
    }

    Ok(())
}

/// Check an event for alerts.
///
/// Event parsing should never fail, see `check_extrinsic` for more details.
///
/// Any returned errors are fatal and require a restart.
pub async fn check_event(
    // TODO: when we add a check that doesn't work on replayed blocks, skip it using mode
    mode: BlockCheckMode,
    alert_tx: &mpsc::Sender<Alert>,
    event: &EventDetails<SubspaceConfig>,
    block_info: &BlockInfo,
) -> anyhow::Result<()> {
    let event_info = EventInfo::new(event, block_info);

    // TODO:
    // - extract each alert into a pallet-specific function or trait object
    // - add tests to make sure we can parse the events for each alert
    // - format account IDs as ss58 with prefix 6094
    // - link event and account to subscan

    // All operator slashes are alerts.
    // TODO:
    // - test this alert by checking a historic block with an operator slash event
    // - check the case of these names
    if event_info.pallet == "Domains" && event_info.kind == "OperatorSlashed" {
        alert_tx
            .send(Alert::new(
                AlertKind::OperatorSlashed { event_info },
                *block_info,
                mode,
            ))
            .await?;
    } else if event_info.pallet == "Sudo" {
        // We already alert on sudo calls, so this exists mainly to test events.
        alert_tx
            .send(Alert::new(
                AlertKind::SudoEvent { event_info },
                *block_info,
                mode,
            ))
            .await?;
    }

    Ok(())
}
