//! Slot time monitoring: derives slot timing from blocks and emits alerts.
//!
//! This module tracks Subspace slot progression and checks whether observed slot timings
//! exceed configured thresholds.

use crate::alerts::{Alert, AlertKind, BlockCheckMode};
use crate::subspace::BlockInfo;
use anyhow::Ok;
use std::collections::VecDeque;
use std::time::Duration;
use tokio::sync::mpsc;
use tracing::warn;

/// The default threshold for the slot time alert.
///
/// Increased from 1.05 on 10 October 2025, this threshold alerts occasionally when one timekeeper
/// is down.
pub const DEFAULT_SLOW_SLOTS_THRESHOLD: f64 = 1.10;

/// The default fast slots threshold for the slot time alert.
///
/// Decreased from 0.95 on 10 October 2025, this threshold alerts occasionally when one timekeeper
/// is down.
pub const DEFAULT_FAST_SLOTS_THRESHOLD: f64 = 0.94;

/// The default maximum block buffer size.
pub const DEFAULT_MAX_BLOCK_BUFFER: usize = 100;

/// The default check interval for the slot time monitor.
pub const DEFAULT_CHECK_INTERVAL: Duration = Duration::from_secs(600);

/// Interface for slot time monitors that consume blocks and perform checks.
pub trait SlotTimeMonitor {
    /// Ingest a block and update internal state; may emit alerts.
    async fn process_block(
        &mut self,
        mode: BlockCheckMode,
        block: &BlockInfo,
        node_rpc_url: &str,
    ) -> anyhow::Result<()>;
}

/// Configuration for the slot time monitor.
#[derive(Clone, Debug)]
pub struct SlotTimeMonitorConfig {
    /// Interval between checks of slot timing.
    pub check_interval: Duration,
    /// Maximum block buffer
    pub max_block_buffer: usize,
    /// Minimum threshold for alerting based on time-per-slot ratio.
    pub slow_slots_threshold: f64,
    /// Maximum threshold for alerting based on time-per-slot ratio.
    pub fast_slots_threshold: f64,
    /// Channel used to emit alerts.
    pub alert_tx: mpsc::Sender<Alert>,
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
///
/// Reorgs will provoke to reduce the block height check window since some blocks heights will be
/// duplicated, which is not precise but good enough for the slot time monitor.
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub struct SlotTimeMonitorState {
    /// Block buffer
    block_buffer: VecDeque<BlockInfo>,
    /// The status of the slot time monitor.
    alerting_status: AlertingStatus,
}

/// The status of the slot time monitor.
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub enum AlertingStatus {
    /// The slot time monitor is alerting.
    FastSlotTime,
    /// The slot time monitor is alerting.
    SlowSlotTime,
    /// The slot time monitor is not alerting.
    NotAlerting,
}

impl SlotTimeMonitorConfig {
    /// Create a new slot time monitor configuration with the provided parameters.
    pub fn new(
        check_interval: Duration,
        max_block_buffer: usize,
        slow_slots_threshold: f64,
        fast_slots_threshold: f64,
        alert_tx: mpsc::Sender<Alert>,
    ) -> Self {
        assert!(
            u64::try_from(check_interval.as_millis()).is_ok(),
            "unexpectedly large check interval"
        );

        Self {
            check_interval,
            max_block_buffer,
            slow_slots_threshold,
            fast_slots_threshold,
            alert_tx,
        }
    }
}

impl SlotTimeMonitor for MemorySlotTimeMonitor {
    /// Process a new block, updating internal scheduling and sending alerts when needed.
    async fn process_block(
        &mut self,
        mode: BlockCheckMode,
        block_info: &BlockInfo,
        node_rpc_url: &str,
    ) -> anyhow::Result<()> {
        self.push_block_to_buffer(*block_info);

        let result = self.check_slot_time(mode, block_info, node_rpc_url).await;
        if result.is_err() {
            warn!("error checking slot time: {result:?}");
        }
        result
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

    /// Push a block to the buffer and remove the oldest block if the buffer is full.
    fn push_block_to_buffer(&mut self, block_info: BlockInfo) {
        // Initialize state if it doesn't exist
        if self.state.is_none() {
            self.state = Some(SlotTimeMonitorState {
                block_buffer: VecDeque::new(),
                alerting_status: AlertingStatus::NotAlerting,
            });
        }

        if let Some(state) = self.state.as_mut() {
            state.block_buffer.push_front(block_info);
            if state.block_buffer.len() > self.config.max_block_buffer {
                state.block_buffer.pop_back();
            }
        }
    }

    /// Check the slot time and send alerts if needed.
    async fn check_slot_time(
        &mut self,
        mode: BlockCheckMode,
        block_info: &BlockInfo,
        node_rpc_url: &str,
    ) -> anyhow::Result<()> {
        // Ignore alerts during startup mode
        if mode.is_startup() {
            return Ok(());
        }

        // Only check slot timing when the buffer is full
        let state = match &self.state {
            Some(state) => state,
            None => return Ok(()),
        };

        // Check if buffer is full (has reached max_block_buffer)
        if state.block_buffer.len() < self.config.max_block_buffer {
            return Ok(());
        }

        let (lowest_block, last_block) = (
            state.block_buffer.back().cloned(),
            state.block_buffer.front().cloned(),
        );

        let (lowest_block, last_block) = match (lowest_block, last_block) {
            (Some(lowest_block), Some(last_block)) => (lowest_block, last_block),
            _ => return Ok(()),
        };

        let (lowest_block_slot, last_block_slot) = match (lowest_block.slot, last_block.slot) {
            (Some(lowest_block_slot), Some(last_block_slot)) => {
                (lowest_block_slot, last_block_slot)
            }
            // If either block doesn't have a slot, return Ok(())
            _ => return Ok(()),
        };

        let (lowest_block_time_in_seconds, last_block_time_in_seconds) =
            match (lowest_block.chain_time, last_block.chain_time) {
                (Some(lowest_block_time), Some(last_block_time)) => (
                    lowest_block_time.unix_time / 1000,
                    last_block_time.unix_time / 1000,
                ),
                // If either block doesn't have a time, return Ok(())
                _ => return Ok(()),
            };

        let slot_diff = last_block_slot - lowest_block_slot;
        let time_diff = last_block_time_in_seconds - lowest_block_time_in_seconds;

        // If time diff is 0, return an error should never happen though
        if time_diff == 0 {
            return Err(anyhow::anyhow!("time diff is 0"));
        }
        #[allow(
            clippy::cast_precision_loss,
            reason = "The range of slot diff and time diff is small enough that precision loss is acceptable"
        )]
        let time_per_slot = time_diff as f64 / slot_diff as f64;

        if time_per_slot > self.config.slow_slots_threshold {
            self.send_slow_slot_time_alert(
                mode,
                *block_info,
                slot_diff,
                time_per_slot,
                node_rpc_url,
            )
            .await?;
        } else if time_per_slot < self.config.fast_slots_threshold {
            self.send_fast_slot_time_alert(
                mode,
                *block_info,
                slot_diff,
                time_per_slot,
                node_rpc_url,
            )
            .await?;
        } else {
            self.set_alerting_status(AlertingStatus::NotAlerting);
        }

        Ok(())
    }

    /// Send a slot time alert with the computed ratio and block info.
    async fn send_slow_slot_time_alert(
        &mut self,
        mode: BlockCheckMode,
        block_info: BlockInfo,
        slot_diff: u64,
        time_per_slot: f64,
        node_rpc_url: &str,
    ) -> anyhow::Result<()> {
        // Only send alert if we're not already alerting for slow slot time
        if let Some(state) = self.state.as_mut()
            && state.alerting_status != AlertingStatus::SlowSlotTime
        {
            self.set_alerting_status(AlertingStatus::SlowSlotTime);
            self.config
                .alert_tx
                .send(Alert::new(
                    AlertKind::SlowSlotTime {
                        slot_amount: slot_diff,
                        current_ratio: time_per_slot,
                        threshold: self.config.slow_slots_threshold,
                        interval: self.config.check_interval,
                    },
                    mode,
                    block_info,
                    node_rpc_url,
                ))
                .await?;
        }
        Ok(())
    }

    /// Send a fast slot time alert with the computed ratio and block info.
    async fn send_fast_slot_time_alert(
        &mut self,
        mode: BlockCheckMode,
        block_info: BlockInfo,
        slot_diff: u64,
        time_per_slot: f64,
        node_rpc_url: &str,
    ) -> anyhow::Result<()> {
        // Only send alert if we're not already alerting for fast slot time
        if let Some(state) = self.state.as_mut()
            && state.alerting_status != AlertingStatus::FastSlotTime
        {
            self.set_alerting_status(AlertingStatus::FastSlotTime);
            self.config
                .alert_tx
                .send(Alert::new(
                    AlertKind::FastSlotTime {
                        slot_amount: slot_diff,
                        current_ratio: time_per_slot,
                        threshold: self.config.fast_slots_threshold,
                        interval: self.config.check_interval,
                    },
                    mode,
                    block_info,
                    node_rpc_url,
                ))
                .await?;
        }
        Ok(())
    }

    /// Helper function to set the alerting status.
    fn set_alerting_status(&mut self, alert_status: AlertingStatus) {
        if let Some(state) = self.state.as_mut() {
            state.alerting_status = alert_status;
        }
    }
}
