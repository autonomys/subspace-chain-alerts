//! Slot time monitoring: derives slot timing from blocks and emits alerts.
//!
//! This module tracks Subspace slot progression and checks whether observed slot timings
//! exceed configured thresholds.
#[cfg(test)]
pub mod test_utils;

use crate::alerts::{Alert, AlertKind};
use crate::subspace::{BlockInfo, Slot};
use std::sync::Arc;
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
    async fn process_block(&mut self, block: &BlockInfo);
}

/// Configuration for the slot time monitor.
#[derive(Clone, Debug)]
pub struct SlotTimeMonitorConfig {
    /// Interval between checks of slot timing.
    pub check_interval: Duration,
    /// Threshold for alerting based on time-per-slot ratio.
    pub alert_threshold: f64,
    /// Channel used to emit alerts.
    pub alert_tx: Arc<tokio::sync::mpsc::Sender<Alert>>,
}

/// In-memory implementation of a slot time monitor.
pub struct MemorySlotTimeMonitor {
    /// Monitor configuration parameters.
    config: SlotTimeMonitorConfig,
    /// First slot observed in the current checking interval.
    first_slot_in_interval: Slot,
    /// Next wall-clock time when a check should occur.
    next_check_time: Option<u128>,
    /// Wall-clock time when the first slot of the interval was observed.
    first_slot_time: Option<u128>,
}

impl SlotTimeMonitor for MemorySlotTimeMonitor {
    /// Process a new block, updating internal scheduling and sending alerts when needed.
    #[allow(clippy::cast_precision_loss)]
    async fn process_block(&mut self, block_info: &BlockInfo) {
        let (block_time, block_slot) =
            match (block_info.clone().block_time, block_info.clone().block_slot) {
                (Some(block_time), Some(block_slot)) => (block_time, block_slot),
                (None, None) => {
                    warn!("Block time and slot not found");
                    return;
                }
                (None, Some(_)) => {
                    warn!("Block time not found");
                    return;
                }
                (Some(_), None) => {
                    warn!("Block slot not found");
                    return;
                }
            };

        debug!(
            "Extracted slot: {:?} for block {:?}",
            block_slot,
            block_info.clone().block_height
        );

        match (self.next_check_time, self.first_slot_time) {
            // slot available, we should check if we are in the interval
            (Some(next_check_time), Some(first_slot_time))
                if next_check_time <= block_time.unix_time =>
            {
                debug!("Checking slot time alert in interval...");

                let slot_diff = u128::from(block_slot - self.first_slot_in_interval);
                let time_diff: Option<u128> = next_check_time.checked_sub(first_slot_time);

                if let Some(time_diff) = time_diff {
                    let time_diff_in_seconds = time_diff / 1000;
                    let time_per_slot = time_diff_in_seconds as f64 / slot_diff as f64;
                    if time_per_slot > self.config.alert_threshold {
                        info!(
                            "Time per slot alert triggered, time_per_slot: {}",
                            time_per_slot
                        );
                        let _ = self.send_alert(time_per_slot, block_info.clone()).await;
                    } else {
                        debug!(
                            "Time per slot not triggered, time_per_slot: {}",
                            time_per_slot
                        );
                    }
                    self.schedule_next_check(block_time.unix_time, block_slot);
                }
            }
            // do nothing if we are in the interval
            (Some(_), Some(_)) => {}
            // we received a new block, so we init the first check
            (None, _) => {
                self.schedule_next_check(block_time.unix_time, block_slot);
            }
            // should not happen
            (Some(_), None) => {
                warn!("No first slot time found though we have a next check time");
                self.schedule_next_check(block_time.unix_time, block_slot);
            }
        }
    }
}

impl MemorySlotTimeMonitor {
    /// Create a new in-memory slot time monitor with the provided configuration.
    pub fn new(config: SlotTimeMonitorConfig) -> Self {
        Self {
            config,
            first_slot_in_interval: Slot(0),
            first_slot_time: None,
            next_check_time: None,
        }
    }

    /// Send a slot time alert with the computed ratio and block info.
    async fn send_alert(
        &self,
        slot_diff_per_time_diff: f64,
        block_info: BlockInfo,
    ) -> Result<(), SendError<Alert>> {
        self.config
            .alert_tx
            .send(Alert {
                alert: AlertKind::SlotTimeAlert {
                    current_ratio: slot_diff_per_time_diff.to_string(),
                    threshold: self.config.alert_threshold.to_string(),
                    interval: self.config.check_interval,
                },
                block_info,
            })
            .await
    }

    /// Schedule the next check at `current_time + check_interval` and record the slot.
    fn schedule_next_check(&mut self, current_time: u128, current_slot: Slot) {
        let next_check_time = current_time + self.config.check_interval.as_millis();
        debug!(
            "Scheduling next check for block time: {:?}",
            next_check_time
        );
        self.next_check_time = Some(next_check_time);
        self.first_slot_in_interval = current_slot;
        self.first_slot_time = Some(current_time);
    }
}
