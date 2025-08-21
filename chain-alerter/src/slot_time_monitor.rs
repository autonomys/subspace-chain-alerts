//! Slot time monitoring: derives slot timing from blocks and emits alerts.
//!
//! This module tracks Subspace slot progression and checks whether observed slot timings
//! exceed configured thresholds.

#[cfg(test)]
pub mod test_utils;

use crate::alerts::{Alert, AlertKind, BlockCheckMode};
use crate::subspace::{BlockInfo, BlockTime, RawTime, Slot};
use std::time::Duration;
use tokio::sync::mpsc::error::SendError;
use tracing::{debug, info, warn};

/// The default threshold for the slot time alert.
pub const DEFAULT_SLOT_TIME_ALERT_THRESHOLD: f64 = 1.05;

/// The default check interval for the slot time monitor.
pub const DEFAULT_CHECK_INTERVAL: Duration = Duration::from_secs(3600);

/// Interface for slot time monitors that consume blocks and perform checks.
pub trait SlotTimeMonitor {
    /// Ingest a block and update internal state; may emit alerts.
    async fn process_block(&mut self, mode: BlockCheckMode, block: &BlockInfo);
}

/// Configuration for the slot time monitor.
#[derive(Clone, Debug)]
pub struct SlotTimeMonitorConfig {
    /// Interval between checks of slot timing.
    pub check_interval: Duration,
    /// Minimum threshold for alerting based on time-per-slot ratio.
    /// TODO: also alert on a maximum threshold
    pub alert_threshold: f64,
    /// Channel used to emit alerts.
    pub alert_tx: tokio::sync::mpsc::Sender<Alert>,
}

/// In-memory implementation of a slot time monitor.
#[derive(Clone, Debug)]
pub struct MemorySlotTimeMonitor {
    /// Monitor configuration parameters.
    config: SlotTimeMonitorConfig,
    /// State tracked by the slot time monitor, and updated at the same time.
    state: Option<SlotTimeMonitorState>,
}

/// State tracked by the slot time monitor, and updated at the same time.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub struct SlotTimeMonitorState {
    /// First slot observed in the current checking interval.
    first_slot_in_interval: Slot,
    /// Consensus wall-clock time when the first slot of the interval was observed.
    first_slot_time: BlockTime,
    /// Next consensus wall-clock time when a check should occur.
    next_check_time: BlockTime,
}

impl SlotTimeMonitorConfig {
    /// Create a new slot time monitor configuration with the provided parameters.
    pub fn new(
        check_interval: Duration,
        alert_threshold: f64,
        alert_tx: tokio::sync::mpsc::Sender<Alert>,
    ) -> Self {
        assert!(
            u64::try_from(check_interval.as_millis()).is_ok(),
            "unexpectedly large check interval"
        );

        Self {
            check_interval,
            alert_threshold,
            alert_tx,
        }
    }
}

impl SlotTimeMonitor for MemorySlotTimeMonitor {
    /// Process a new block, updating internal scheduling and sending alerts when needed.
    async fn process_block(&mut self, mode: BlockCheckMode, block_info: &BlockInfo) {
        let (block_time, block_slot) = match (block_info.time, block_info.slot) {
            (Some(block_time), Some(block_slot)) => (block_time, block_slot),
            (None, None) => {
                warn!(?mode, "Block time and slot not found");
                return;
            }
            (None, Some(_)) => {
                warn!(?mode, "Block time not found");
                return;
            }
            (Some(_), None) => {
                warn!(?mode, "Block slot not found");
                return;
            }
        };

        debug!(
            ?mode,
            "Extracted slot: {:?} for block {:?}",
            block_slot,
            block_info.height(),
        );

        match self.state {
            // slot available, we should check if we are in the interval
            Some(state) if state.next_check_time <= block_time => {
                debug!(?mode, "Checking slot time alert in interval...");

                let slot_diff = block_slot - state.first_slot_in_interval;
                let time_diff = state
                    .next_check_time
                    .unix_time
                    .checked_sub(state.first_slot_time.unix_time);

                if let Some(time_diff) = time_diff {
                    let time_diff_in_seconds = time_diff / 1000;
                    #[allow(
                        clippy::cast_precision_loss,
                        reason = "time and slot differences are much smaller than 52 bits in practice"
                    )]
                    let time_per_slot = time_diff_in_seconds as f64 / slot_diff as f64;
                    if time_per_slot > self.config.alert_threshold {
                        info!(
                            ?mode,
                            "Time per slot alert triggered, time_per_slot: {time_per_slot}",
                        );
                        let _ = self.send_alert(time_per_slot, *block_info, mode).await;
                    } else {
                        debug!(
                            ?mode,
                            "Time per slot not triggered, time_per_slot: {time_per_slot}",
                        );
                    }
                    self.schedule_next_check(block_time, block_slot);
                } else {
                    warn!(
                        ?mode,
                        "Unexpected slot time, earlier than first block in interval: {block_info:?} - {state:?}",
                    );
                }
            }
            // do nothing if we are in the interval
            Some(_) => {}
            // we received a new block, so we init the first check
            None => {
                self.schedule_next_check(block_time, block_slot);
            }
        }
    }
}

impl MemorySlotTimeMonitor {
    /// Create a new in-memory slot time monitor with the provided configuration.
    pub fn new(config: SlotTimeMonitorConfig) -> Self {
        Self {
            config,
            state: None,
        }
    }

    /// Send a slot time alert with the computed ratio and block info.
    async fn send_alert(
        &self,
        slot_diff_per_time_diff: f64,
        block_info: BlockInfo,
        mode: BlockCheckMode,
    ) -> Result<(), SendError<Alert>> {
        self.config
            .alert_tx
            .send(Alert::new(
                AlertKind::SlotTime {
                    current_ratio: slot_diff_per_time_diff,
                    threshold: self.config.alert_threshold,
                    interval: self.config.check_interval,
                    first_slot_time: self
                        .state
                        .expect("alerts are only triggered when state is present")
                        .first_slot_time,
                },
                block_info,
                mode,
            ))
            .await
    }

    /// Schedule the next check at `current_time + check_interval` and record the slot.
    fn schedule_next_check(&mut self, current_time: BlockTime, current_slot: Slot) {
        let next_check_time = BlockTime {
            unix_time: current_time.unix_time
                + RawTime::try_from(self.config.check_interval.as_millis())
                    .expect("already checked when constructing config"),
        };
        debug!(
            "Scheduling next check for block time: {:?}",
            next_check_time
        );
        self.state = Some(SlotTimeMonitorState {
            first_slot_in_interval: current_slot,
            first_slot_time: current_time,
            next_check_time,
        });
    }
}
