//! Slot time monitoring: derives slot timing from blocks and emits alerts.
//!
//! This module tracks Subspace slot progression and checks whether observed slot timings
//! exceed configured thresholds.
#[cfg(test)]
pub mod test_utils;
mod utils;

use crate::alerts::{Alert, AlertKind};
use crate::subspace::{BlockInfo, SubspaceConfig};
use std::sync::Arc;
use std::time::Duration;
use subxt::blocks::Block;
use subxt::client::OnlineClient;
use subxt::utils::H256;
use tokio::sync::mpsc::error::SendError;
use utils::extract_slot_from_pre_digest;

use tracing::{debug, info, warn};

/// Interface for slot time monitors that consume blocks and perform checks.
pub trait SlotTimeMonitor {
    /// Ingest a block and update internal state; may emit alerts.
    async fn process_block(&mut self, block: &Block<SubspaceConfig, OnlineClient<SubspaceConfig>>);
}

/// Configuration for the slot time monitor.
pub struct SlotTimeMonitorConfig {
    /// Genesis hash of the chain being monitored.
    pub genesis_hash: H256,
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
    first_slot_in_interval: u64,
    /// Next wall-clock time when a check should occur.
    next_check_time: Option<Duration>,
    /// Wall-clock time when the first slot of the interval was observed.
    first_slot_time: Option<Duration>,
}

impl SlotTimeMonitor for MemorySlotTimeMonitor {
    /// Process a new block, updating internal scheduling and sending alerts when needed.
    #[allow(clippy::cast_precision_loss)]
    async fn process_block(&mut self, block: &Block<SubspaceConfig, OnlineClient<SubspaceConfig>>) {
        let slot = extract_slot_from_pre_digest(block);
        debug!(
            "Extracted slot: {:?} for block {:?}",
            slot,
            block.header().number
        );

        let extrinsics = match block.extrinsics().await {
            Ok(extrinsics) => extrinsics,
            Err(e) => {
                warn!("Error extracting extrinsics: {:?}", e);
                return;
            }
        };

        let block_info = BlockInfo::new(block, &extrinsics, &self.config.genesis_hash);

        let block_time = match block_info.clone().block_time {
            Some(bt) => {
                let unix_time: Result<u64, _> = bt.unix_time.try_into();
                match unix_time {
                    Ok(unix_time) => Duration::from_millis(unix_time),
                    Err(e) => {
                        warn!("Error converting block time to unix time: {:?}", e);
                        return;
                    }
                }
            }
            None => {
                warn!("Block time not found");
                return;
            }
        };

        match (slot, self.next_check_time, self.first_slot_time) {
            // slot available, we should check if we are in the interval
            (Ok(slot), Some(next_check_time), Some(first_slot_time))
                if next_check_time < block_time =>
            {
                debug!("Checking slot time alert in interval...");
                let slot_diff = slot - self.first_slot_in_interval;
                let time_diff: Option<Duration> = block_time.checked_sub(first_slot_time);
                if let Some(time_diff) = time_diff {
                    let time_per_slot = time_diff.as_secs_f64() / slot_diff as f64;
                    if time_per_slot > self.config.alert_threshold {
                        info!(
                            "Slot time alert triggered, time_per_slot: {}",
                            time_per_slot
                        );
                        let _ = self.send_alert(time_per_slot, block_info.clone()).await;
                    } else {
                        debug!(
                            "Slot time alert not triggered, time_per_slot: {}",
                            time_per_slot
                        );
                    }
                    self.schedule_next_check(block_time, slot);
                }
            }
            // we received a new block, so we init the first check
            (Ok(slot), None, _) => {
                self.schedule_next_check(block_time, slot);
            }
            (Err(e), _, _) => {
                warn!("Error extracting slot from block: {:?}", e);
            }
            // do nothing if we are in the interval
            (Ok(_), Some(_), _) => {}
        }
    }
}

impl MemorySlotTimeMonitor {
    /// Create a new in-memory slot time monitor with the provided configuration.
    pub fn new(config: SlotTimeMonitorConfig) -> Self {
        Self {
            config,
            first_slot_in_interval: 0,
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
    fn schedule_next_check(&mut self, current_time: Duration, current_slot: u64) {
        let next_check_time = current_time + self.config.check_interval;
        debug!(
            "Scheduling next check for block time: {:?}",
            next_check_time
        );
        self.next_check_time = Some(next_check_time);
        self.first_slot_in_interval = current_slot;
        self.first_slot_time = Some(current_time);
    }
}
