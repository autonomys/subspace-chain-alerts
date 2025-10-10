//! Farming monitor that tracks the number of farmers with votes in the last `max_block_interval`
//! blocks and emits alerts if the number of farmers with votes is outside the alert thresholds.

#[cfg(test)]
mod tests;

use crate::alerts::{Alert, AlertKind, BlockCheckMode};
use crate::subspace::decode::decode_h256_from_composite;
use crate::subspace::{BlockInfo, BlockNumber, RawEvent};
use scale_value::Composite;
use std::collections::{HashMap, VecDeque};
use tokio::sync::mpsc;
use tracing::{debug, trace, warn};

/// The default minimum allowed change from the average farmer votes within the checking
/// window.
pub const DEFAULT_LOW_END_FARMING_ALERT_THRESHOLD: f64 = 0.33;

/// The default maximum allowed change from the average farmer votes within the checking
/// window.
pub const DEFAULT_HIGH_END_FARMING_ALERT_THRESHOLD: f64 = 3.0;

/// The default farmer inactivity threshold for the farming monitor.
pub const DEFAULT_FARMING_INACTIVE_BLOCK_THRESHOLD: BlockNumber = 100;

/// The default minimum block interval for farming monitor alerts.
pub const DEFAULT_FARMING_MIN_ALERT_BLOCK_INTERVAL: usize = 1000;

/// The default maximum history size for the farming monitor.
pub const DEFAULT_FARMING_MAX_HISTORY_BLOCK_INTERVAL: usize = 1000;

/// The farmer vote event pallet name.
pub const FARMER_VOTE_EVENT_PALLET_NAME: &str = "Subspace";
/// The farmer vote event variant name.
pub const FARMER_VOTE_EVENT_VARIANT_NAME: &str = "FarmerVote";
/// The farmer vote event public key field name.
pub const FARMER_VOTE_EVENT_PUBLIC_KEY_FIELD_NAME: &str = "public_key";

/// The farmer vote event public key field type.
/// TODO: turn this into a wrapper type so we don't get it confused with other hashes.
type FarmerPublicKey = subxt::utils::H256;

/// Interface for farming monitors that consume blocks and perform checks.
pub trait FarmingMonitor {
    /// Ingest a block and update internal state; may emit alerts.
    async fn process_block(
        &mut self,
        mode: BlockCheckMode,
        block_info: &BlockInfo,
        events: &[RawEvent],
        node_rpc_url: &str,
    ) -> anyhow::Result<()>;
}

#[derive(Debug, Clone)]
/// Configuration for the farming monitor.
pub struct FarmingMonitorConfig {
    /// Channel used to emit alerts.
    pub alert_tx: mpsc::Sender<Alert>,
    /// The size of the window to check for farming.
    pub max_block_interval: usize,
    /// The minimum allowed change from the average farmer votes within the checking
    /// window.
    pub low_end_change_threshold: f64,
    /// The maximum allowed change from the average farmer votes within the checking
    /// window.
    pub high_end_change_threshold: f64,
    /// The number of blocks that a farmer should not vote until they are mark as inactive.
    pub inactive_block_threshold: BlockNumber,
    /// The minimum of blocks that must pass before any alert is emitted.
    pub minimum_block_interval: usize,
}

/// The type of alert issued by the farming monitor, if there was one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FarmingMonitorStatus {
    /// The farming monitor has emitted an increase alert.
    AlertingIncrease,
    /// The farming monitor has emitted a decrease alert.
    AlertingDecrease,
    /// The farming monitor did not emit an alert.
    NotAlerting,
}

/// State tracked by the farming monitor, and updated at the same time.
/// TODO:
///   - currently during a reorg, alerts are disabled (because of pruning, the threshold is not met
///     until the new blocks are at or above the previous tip height), but votes and active farmer
///     counts are kept for blocks that were reorged away from
///   - handle reorgs by deleting the votes and blocks that were reorged away from
#[derive(Debug, Clone)]
pub struct FarmingMonitorState {
    /// The last block voted by a farmer, by farmer public key hash.
    /// TODO: change this to `HashMap<FarmerPublicKey, BTreeSet<BlockNumber>>`, and delete the last
    /// few entries in the `BTreeSet` (or the whole `HashMap` entry) when a reorg happens.
    last_block_voted_by_farmer: HashMap<FarmerPublicKey, BlockNumber>,
    /// The number of farmers that have votes in the last `max_block_interval` blocks.
    /// TODO: delete the last few entries when a reorg happens.
    active_farmers_in_last_blocks: VecDeque<usize>,
    /// The last alert issued by the farming monitor.
    status: FarmingMonitorStatus,
}

/// A farming monitor that tracks the number of farmers with votes in the last `max_block_interval`
/// blocks and emits alerts if the number of farmers with votes is outside the alert thresholds.
#[derive(Debug, Clone)]
pub struct MemoryFarmingMonitor {
    /// Monitor configuration parameters.
    config: FarmingMonitorConfig,
    /// State tracked by the farming monitor, and updated at the same time.
    state: FarmingMonitorState,
}

impl FarmingMonitor for MemoryFarmingMonitor {
    async fn process_block(
        &mut self,
        mode: BlockCheckMode,
        block_info: &BlockInfo,
        events: &[RawEvent],
        node_rpc_url: &str,
    ) -> anyhow::Result<()> {
        // Update the last voted block for each farmer that voted in this block.
        self.update_last_voted_block(events, block_info.height());

        // Remove farmers that have not voted in the last `inactive_block_threshold` blocks.
        self.remove_inactive_farmers(block_info.height());

        // Update the number of farmers with votes in the last `max_block_interval` blocks.
        self.update_number_of_farmers_with_votes();

        // Run checks on the number of farmers.
        if self.has_passed_minimum_block_interval() {
            self.check_farmer_count(mode, block_info, node_rpc_url)
                .await?;
        }

        Ok(())
    }
}

impl MemoryFarmingMonitor {
    /// Create a new farming monitor.
    pub fn new(config: &FarmingMonitorConfig) -> Self {
        Self {
            config: config.clone(),
            state: FarmingMonitorState {
                last_block_voted_by_farmer: HashMap::new(),
                active_farmers_in_last_blocks: VecDeque::with_capacity(config.max_block_interval),
                status: FarmingMonitorStatus::NotAlerting,
            },
        }
    }

    /// Check if there are enough records in the state to pass the minimum block interval.
    pub fn has_passed_minimum_block_interval(&self) -> bool {
        self.state.active_farmers_in_last_blocks.len() >= self.config.minimum_block_interval
    }

    /// Update the last voted block for each farmer that voted in the block.
    fn update_last_voted_block(&mut self, events: &[RawEvent], block_height: BlockNumber) {
        for event in events.iter() {
            let event = match (event.pallet_name(), event.variant_name()) {
                (FARMER_VOTE_EVENT_PALLET_NAME, FARMER_VOTE_EVENT_VARIANT_NAME) => event,
                _ => continue,
            };

            let named_fields = match event.field_values() {
                Ok(Composite::Named(named_fields)) => named_fields,
                Err(e) => {
                    warn!("failed to get event details: {e}");
                    continue;
                }
                _ => continue,
            };

            let public_key_hash = match named_fields
                .iter()
                .find(|(name, _)| name == FARMER_VOTE_EVENT_PUBLIC_KEY_FIELD_NAME)
            {
                Some((_, public_key_value)) => decode_h256_from_composite(public_key_value),
                None => continue,
            };

            if let Some(public_key_hash) = public_key_hash {
                let public_key_hash_str = hex::encode(public_key_hash.as_bytes());
                trace!(
                    "Inserting farmer 0x{} into last voted by farmer",
                    public_key_hash_str,
                );
                self.state
                    .last_block_voted_by_farmer
                    .insert(public_key_hash, block_height);
            }
        }
    }

    /// Remove farmers that have not voted in the last `inactive_block_threshold` blocks.
    fn remove_inactive_farmers(&mut self, block_height: BlockNumber) {
        let inactive_block_threshold = self.config.inactive_block_threshold;

        // Remove the farmers that have not voted in the last `inactive_block_threshold` blocks.
        self.state
            .last_block_voted_by_farmer
            .retain(|farmer_public_key, ref last_block_voted| {
                let active_block_threshold = block_height.saturating_sub(inactive_block_threshold);
                let retain = **last_block_voted >= active_block_threshold;
                if !retain {
                    trace!("Farmer {farmer_public_key} is going inactive");
                }
                retain
            });
    }

    /// Update the number of farmers with votes in the last `max_block_interval` blocks.
    fn update_number_of_farmers_with_votes(&mut self) {
        let number_of_farmers_with_votes = self.state.last_block_voted_by_farmer.len();

        self.state
            .active_farmers_in_last_blocks
            .push_front(number_of_farmers_with_votes);

        self.state
            .active_farmers_in_last_blocks
            .truncate(self.config.max_block_interval);

        debug!(
            "Number of farmers with votes in the last {} blocks: {}",
            self.config.max_block_interval, number_of_farmers_with_votes
        );
    }

    /// Returns the current number of farmers with votes, and the fraction that number is of the
    /// average.
    /// Returns `None` if there are no blocks in the window.
    ///
    /// TODO: return a struct here
    #[allow(
        clippy::cast_precision_loss,
        reason = "numbers are much smaller than 52 bits"
    )]
    fn compare_to_average(&self) -> Option<(usize, f64, f64)> {
        let number_of_farmers_with_votes = *self.state.active_farmers_in_last_blocks.front()?;

        // Calculate the average number of farmers with votes in the last `max_block_interval`
        // blocks.
        let average_number_of_farmers_with_votes = (self
            .state
            .active_farmers_in_last_blocks
            .iter()
            .sum::<usize>() as f64)
            / (self.state.active_farmers_in_last_blocks.len() as f64);

        let fraction_of_average =
            (number_of_farmers_with_votes as f64) / average_number_of_farmers_with_votes;

        Some((
            number_of_farmers_with_votes,
            average_number_of_farmers_with_votes,
            fraction_of_average,
        ))
    }

    /// Check the number of farmers with votes in the last `max_block_interval` blocks
    /// and emit alerts if the number of farmers with votes is outside the alert thresholds.
    async fn check_farmer_count(
        &mut self,
        mode: BlockCheckMode,
        block_info: &BlockInfo,
        node_rpc_url: &str,
    ) -> anyhow::Result<()> {
        let Some((
            number_of_farmers_with_votes,
            average_number_of_farmers_with_votes,
            fraction_of_average,
        )) = self.compare_to_average()
        else {
            return Ok(());
        };

        // Check if the current number of farmers with votes is greater than the alert threshold.
        if fraction_of_average < self.config.low_end_change_threshold {
            if self.state.status != FarmingMonitorStatus::AlertingDecrease {
                self.config
                    .alert_tx
                    .send(Alert::new(
                        AlertKind::FarmersDecreasedSuddenly {
                            number_of_farmers_with_votes,
                            average_number_of_farmers_with_votes,
                            number_of_blocks: u32::try_from(
                                self.state.active_farmers_in_last_blocks.len(),
                            )
                            .expect("farmers should fit in a u32 integer"),
                        },
                        mode,
                        *block_info,
                        node_rpc_url,
                    ))
                    .await?;
                self.state.status = FarmingMonitorStatus::AlertingDecrease;
            }
        } else if fraction_of_average > self.config.high_end_change_threshold {
            if self.state.status != FarmingMonitorStatus::AlertingIncrease {
                self.config
                    .alert_tx
                    .send(Alert::new(
                        AlertKind::FarmersIncreasedSuddenly {
                            number_of_farmers_with_votes,
                            average_number_of_farmers_with_votes,
                            number_of_blocks: u32::try_from(
                                self.state.active_farmers_in_last_blocks.len(),
                            )
                            .expect("farmers should fit in a u32 integer"),
                        },
                        mode,
                        *block_info,
                        node_rpc_url,
                    ))
                    .await?;
                self.state.status = FarmingMonitorStatus::AlertingIncrease;
            }
        } else {
            self.state.status = FarmingMonitorStatus::NotAlerting;
        }

        Ok(())
    }
}
