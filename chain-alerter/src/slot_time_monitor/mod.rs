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

pub trait SlotTimeMonitor {
    async fn process_block(&mut self, block: &Block<SubspaceConfig, OnlineClient<SubspaceConfig>>);
}

pub struct SlotTimeMonitorConfig {
    pub genesis_hash: H256,
    pub check_interval: Duration,
    pub alert_threshold: f64,
    pub alert_tx: Arc<tokio::sync::mpsc::Sender<Alert>>,
}

pub struct MemorySlotTimeMonitor {
    config: SlotTimeMonitorConfig,
    first_slot_in_interval: u64,
    next_check_time: Option<Duration>,
    first_slot_time: Option<Duration>,
}

impl SlotTimeMonitor for MemorySlotTimeMonitor {
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
    pub fn new(config: SlotTimeMonitorConfig) -> Self {
        Self {
            config,
            first_slot_in_interval: 0,
            first_slot_time: None,
            next_check_time: None,
        }
    }

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
