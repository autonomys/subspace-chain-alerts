//! Farming monitor that tracks the number of farmers with votes in the last `max_block_interval`
//! blocks and emits alerts if the number of farmers with votes is outside the alert thresholds.

use crate::alerts::{Alert, AlertKind};
use crate::subspace::artifacts::api::subspace::events::FarmerVote;
use crate::subspace::{BlockInfo, BlockNumber};
use std::collections::{HashMap, VecDeque};
use subxt::SubstrateConfig;
use subxt::events::Events;
use tracing::{trace, warn};

/// The default threshold for the farming monitor.
pub const DEFAULT_LOW_END_FARMING_ALERT_THRESHOLD: f64 = 0.75;

/// The default threshold for the farming monitor.
pub const DEFAULT_HIGH_END_FARMING_ALERT_THRESHOLD: f64 = 1.25;

/// The default threshold for the farming monitor.
pub const DEFAULT_FARMING_INACTIVE_BLOCK_THRESHOLD: u32 = 10;

/// The default minimum block interval for the farming monitor.
pub const DEFAULT_FARMING_MINIMUM_BLOCK_INTERVAL: u32 = 30;

/// The default number of blocks to check for farming.
pub const DEFAULT_FARMING_MAX_BLOCK_INTERVAL: u32 = 100;

/// Interface for farming monitors that consume blocks and perform checks.
pub trait FarmingMonitor {
    /// Ingest a block and update internal state; may emit alerts.
    async fn process_block(&mut self, block: BlockInfo, block_events: Events<SubstrateConfig>);
}

#[derive(Debug, Clone)]
/// Configuration for the farming monitor.
pub struct FarmingMonitorConfig {
    /// Channel used to emit alerts.
    pub alert_tx: tokio::sync::mpsc::Sender<Alert>,
    /// The size of the window to check for farming.
    pub max_block_interval: u32,
    /// The percentage threshold for alerting a network from average within the checking window.
    pub low_end_percentage_threshold: f64,
    /// The percentage threshold for alerting a network from average within the checking
    /// window.
    pub high_end_percentage_threshold: f64,
    /// The number of blocks that a farmer should not vote until they are mark as inactive.
    pub inactive_block_threshold: u32,
    /// The minimum of blocks that must pass before any alert is emitted.
    pub minimum_block_interval: u32,
}

/// State tracked by the farming monitor, and updated at the same time.
pub struct FarmingMonitorState {
    /// The last block voted by a farmer.
    last_block_voted_by_farmer: HashMap<String, BlockNumber>,
    /// The number of farmers that have votes in the last `max_block_interval` blocks.
    number_of_farmers_with_votes: VecDeque<u32>,
}

/// A farming monitor that tracks the number of farmers with votes in the last `max_block_interval`
/// blocks and emits alerts if the number of farmers with votes is outside the alert thresholds.
pub struct MemoryFarmingMonitor {
    /// Monitor configuration parameters.
    config: FarmingMonitorConfig,
    /// State tracked by the farming monitor, and updated at the same time.
    state: FarmingMonitorState,
}

impl FarmingMonitor for MemoryFarmingMonitor {
    async fn process_block(&mut self, block: BlockInfo, block_events: Events<SubstrateConfig>) {
        // Update the last voted block for each farmer that voted in this block.
        self.update_last_voted_block(&block_events, block.block_height);

        // Remove farmers that have not voted in the last `inactive_block_threshold` blocks.
        self.remove_inactive_farmers(block.block_height);

        // Update the number of farmers with votes in the last `max_block_interval` blocks.
        self.update_number_of_farmers_with_votes();

        // Run checks on the number of farmers.
        let has_passed_minimum_block_interval = block
            .block_height
            .saturating_sub(self.config.minimum_block_interval)
            > 0;
        if has_passed_minimum_block_interval {
            self.check_farmer_count(block).await;
        }
    }
}

impl MemoryFarmingMonitor {
    /// Create a new farming monitor.
    pub fn new(config: &FarmingMonitorConfig) -> Self {
        Self {
            config: config.clone(),
            state: FarmingMonitorState {
                last_block_voted_by_farmer: HashMap::new(),
                number_of_farmers_with_votes: VecDeque::with_capacity(
                    config.max_block_interval as usize,
                ),
            },
        }
    }

    /// Update the last voted block for each farmer that voted in the block.
    fn update_last_voted_block(
        &mut self,
        block_events: &Events<SubstrateConfig>,
        block_height: BlockNumber,
    ) {
        for event in block_events.iter() {
            let event = match event {
                Ok(event) => event,
                Err(e) => {
                    warn!("failed to get event details: {e}");
                    continue;
                }
            };

            if let Ok(Some(FarmerVote { public_key, .. })) = event.as_event::<FarmerVote>() {
                trace!("FarmerVote event: {public_key:?}");
                let public_key = hex::encode(public_key.0);
                self.state
                    .last_block_voted_by_farmer
                    .insert(public_key, block_height);
            }
        }
    }

    /// Remove farmers that have not voted in the last `inactive_block_threshold` blocks.
    fn remove_inactive_farmers(&mut self, block_height: BlockNumber) {
        let config = self.config.clone();
        let last_block_voted_by_farmer = self.state.last_block_voted_by_farmer.clone();

        // Filter in the farmers that have not voted in the last `inactive_block_threshold` blocks.
        let farmers_going_inactive =
            last_block_voted_by_farmer
                .iter()
                .filter(|(_, last_voted_block)| {
                    let last_block_voted =
                        block_height.saturating_sub(config.clone().inactive_block_threshold);
                    last_voted_block.lt(&&last_block_voted)
                });

        // Remove the farmers that have not voted in the last `inactive_block_threshold` blocks.
        farmers_going_inactive.for_each(|(public_key, _)| {
            trace!("Farmer {public_key} is going inactive");
            self.state.last_block_voted_by_farmer.remove(public_key);
        });
    }

    /// Update the number of farmers with votes in the last `max_block_interval` blocks.
    fn update_number_of_farmers_with_votes(&mut self) {
        let number_of_farmers_with_votes =
            u32::try_from(self.state.last_block_voted_by_farmer.len())
                .expect("farmers should fit in a u32 integer");
        self.state
            .number_of_farmers_with_votes
            .push_front(number_of_farmers_with_votes);
    }

    /// Check the number of farmers with votes in the last `max_block_interval` blocks
    /// and emit alerts if the number of farmers with votes is outside the alert thresholds.
    async fn check_farmer_count(&mut self, block_info: BlockInfo) {
        // Calculate the average number of farmers with votes in the last `max_block_interval`
        // blocks.
        let average_number_of_farmers_with_votes =
            self.state.number_of_farmers_with_votes.iter().sum::<u32>()
                / u32::try_from(self.state.number_of_farmers_with_votes.len())
                    .expect("farmers should fit in a u32 integer");

        let &number_of_farmers_with_votes = match self.state.number_of_farmers_with_votes.front() {
            Some(number_of_farmers_with_votes) => number_of_farmers_with_votes,
            None => {
                warn!("No number of farmers with votes found");
                return;
            }
        };

        let percentage_to_average = f64::from(number_of_farmers_with_votes)
            / f64::from(average_number_of_farmers_with_votes);

        // Check if the current number of farmers with votes is greater than the alert threshold.
        if percentage_to_average < self.config.low_end_percentage_threshold {
            let _ = self
                .config
                .alert_tx
                .send(Alert::new(
                    AlertKind::FarmersDecreasedSuddenly {
                        number_of_farmers_with_votes,
                        average_number_of_farmers_with_votes,
                        number_of_blocks: u32::try_from(
                            self.state.number_of_farmers_with_votes.len(),
                        )
                        .expect("farmers should fit in a u32 integer"),
                    },
                    block_info,
                ))
                .await;
        } else if percentage_to_average > self.config.high_end_percentage_threshold {
            let _ = self
                .config
                .alert_tx
                .send(Alert::new(
                    AlertKind::FarmersIncreasedSuddenly {
                        number_of_farmers_with_votes,
                        average_number_of_farmers_with_votes,
                        number_of_blocks: u32::try_from(
                            self.state.number_of_farmers_with_votes.len(),
                        )
                        .expect("farmers should fit in a u32 integer"),
                    },
                    block_info,
                ))
                .await;
        }
    }
}
