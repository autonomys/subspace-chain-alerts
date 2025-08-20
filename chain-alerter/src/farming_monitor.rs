//! Farming monitor that tracks the number of farmers with votes in the last `max_block_interval`
//! blocks and emits alerts if the number of farmers with votes is outside the alert thresholds.

use crate::alerts::{Alert, AlertKind, BlockCheckMode};
use crate::subspace::decode::decode_h256_from_composite;
use crate::subspace::{BlockInfo, BlockNumber, SubspaceConfig};
use scale_value::Composite;
use std::collections::{HashMap, VecDeque};
use subxt::events::Events;
use subxt::utils::H256;
use tracing::{debug, trace, warn};

/// The default threshold for the farming monitor.
pub const DEFAULT_LOW_END_FARMING_ALERT_THRESHOLD: f64 = 0.75;

/// The default threshold for the farming monitor.
pub const DEFAULT_HIGH_END_FARMING_ALERT_THRESHOLD: f64 = 1.25;

/// The default threshold for the farming monitor.
pub const DEFAULT_FARMING_INACTIVE_BLOCK_THRESHOLD: BlockNumber = 10;

/// The default minimum block interval for the farming monitor.
pub const DEFAULT_FARMING_MINIMUM_BLOCK_INTERVAL: BlockNumber = 100;

/// The default number of blocks to check for farming.
pub const DEFAULT_FARMING_MAX_BLOCK_INTERVAL: BlockNumber = 100;

/// Interface for farming monitors that consume blocks and perform checks.
pub trait FarmingMonitor {
    /// Ingest a block and update internal state; may emit alerts.
    async fn process_block(
        &mut self,
        block: BlockInfo,
        mode: BlockCheckMode,
        block_events: Events<SubspaceConfig>,
    );
}

#[derive(Debug, Clone)]
/// Configuration for the farming monitor.
pub struct FarmingMonitorConfig {
    /// Channel used to emit alerts.
    pub alert_tx: tokio::sync::mpsc::Sender<Alert>,
    /// The size of the window to check for farming.
    pub max_block_interval: BlockNumber,
    /// The percentage threshold for alerting a network from average within the checking window.
    pub low_end_change_threshold: f64,
    /// The percentage threshold for alerting a network from average within the checking
    /// window.
    pub high_end_change_threshold: f64,
    /// The number of blocks that a farmer should not vote until they are mark as inactive.
    pub inactive_block_threshold: BlockNumber,
    /// The minimum of blocks that must pass before any alert is emitted.
    pub minimum_block_interval: BlockNumber,
}

/// State tracked by the farming monitor, and updated at the same time.
#[derive(Debug, Clone)]
pub struct FarmingMonitorState {
    /// The last block voted by a farmer.
    last_block_voted_by_farmer: HashMap<H256, BlockNumber>,
    /// The number of farmers that have votes in the last `max_block_interval` blocks.
    active_farmers_in_last_blocks: VecDeque<u32>,
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
        block: BlockInfo,
        mode: BlockCheckMode,
        block_events: Events<SubspaceConfig>,
    ) {
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
            self.check_farmer_count(block, mode).await;
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
                active_farmers_in_last_blocks: VecDeque::with_capacity(
                    config.max_block_interval as usize,
                ),
            },
        }
    }

    /// Update the last voted block for each farmer that voted in the block.
    fn update_last_voted_block(
        &mut self,
        block_events: &Events<SubspaceConfig>,
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

            let pallet_name = event.pallet_name();
            let variant_name = event.variant_name();

            let named_fields = match event.field_values() {
                Ok(Composite::Named(named_fields)) => named_fields,
                Err(e) => {
                    warn!("failed to get event details: {e}");
                    continue;
                }
                _ => continue,
            };

            debug!("Event {pallet_name:?}.{variant_name:?} named_fields: {named_fields:?}");

            let public_key_hash = match named_fields.iter().find(|(name, _)| name == "public_key") {
                Some((_, public_key_value)) => decode_h256_from_composite(public_key_value),
                None => continue,
            };

            if let Some(public_key_hash) = public_key_hash {
                let public_key_hash_str = hex::encode(public_key_hash.as_bytes());
                debug!(
                    "Inserting farmer 0x{} into last voted by farmer",
                    public_key_hash_str
                );
                self.state
                    .last_block_voted_by_farmer
                    .insert(public_key_hash, block_height);
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
        let blocks_in_deque = u32::try_from(self.state.active_farmers_in_last_blocks.len())
            .expect("blocks should fit in a u32 integer");

        if blocks_in_deque >= self.config.max_block_interval {
            self.state.active_farmers_in_last_blocks.pop_back();
        }

        self.state
            .active_farmers_in_last_blocks
            .push_front(number_of_farmers_with_votes);
    }

    /// Check the number of farmers with votes in the last `max_block_interval` blocks
    /// and emit alerts if the number of farmers with votes is outside the alert thresholds.
    async fn check_farmer_count(&mut self, block_info: BlockInfo, mode: BlockCheckMode) {
        if self.state.active_farmers_in_last_blocks.is_empty() {
            return;
        }

        // Calculate the average number of farmers with votes in the last `max_block_interval`
        // blocks.
        let average_number_of_farmers_with_votes =
            f64::from(self.state.active_farmers_in_last_blocks.iter().sum::<u32>())
                / f64::from(
                    u32::try_from(self.state.active_farmers_in_last_blocks.len())
                        .expect("farmers should fit in a u32 integer"),
                );

        let &number_of_farmers_with_votes = self
            .state
            .active_farmers_in_last_blocks
            .front()
            .expect("already checked not empty");

        let percentage_to_average =
            f64::from(number_of_farmers_with_votes) / average_number_of_farmers_with_votes;

        // Check if the current number of farmers with votes is greater than the alert threshold.
        if percentage_to_average < self.config.low_end_change_threshold {
            let _ = self
                .config
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
                    block_info,
                    mode,
                ))
                .await;
        } else if percentage_to_average > self.config.high_end_change_threshold {
            let _ = self
                .config
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
                    block_info,
                    mode,
                ))
                .await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use subxt::utils::H256;

    fn farmers() -> [H256; 3] {
        [
            H256::from_low_u64_be(1),
            H256::from_low_u64_be(2),
            H256::from_low_u64_be(3),
        ]
    }

    async fn simulate_block_votes(
        farming_monitor: &mut MemoryFarmingMonitor,
        block_height: BlockNumber,
        farmers: &[H256],
    ) {
        // Add farmers to the farming monitor.
        for farmer in farmers {
            farming_monitor
                .state
                .last_block_voted_by_farmer
                .insert(*farmer, block_height);
        }

        // Remove farmers that have not voted in the last `inactive_block_threshold` blocks.
        farming_monitor.remove_inactive_farmers(block_height);

        // Update the number of farmers with votes in the last `max_block_interval` blocks.
        farming_monitor.update_number_of_farmers_with_votes();

        // Run checks on the number of farmers.
        let has_passed_minimum_block_interval =
            block_height.saturating_sub(farming_monitor.config.minimum_block_interval) > 0;
        if has_passed_minimum_block_interval {
            farming_monitor
                .check_farmer_count(
                    BlockInfo {
                        block_height,
                        block_time: None,
                        block_hash: H256::default(),
                        genesis_hash: H256::zero(),
                        block_slot: None,
                        parent_hash: H256::zero(),
                    },
                    BlockCheckMode::Current,
                )
                .await;
        }
    }

    #[tokio::test]
    async fn test_farmers_going_inactive() {
        let alert_tx = tokio::sync::mpsc::channel(100).0;
        let config = FarmingMonitorConfig {
            alert_tx,
            max_block_interval: DEFAULT_FARMING_MAX_BLOCK_INTERVAL,
            low_end_change_threshold: DEFAULT_LOW_END_FARMING_ALERT_THRESHOLD,
            high_end_change_threshold: DEFAULT_HIGH_END_FARMING_ALERT_THRESHOLD,
            inactive_block_threshold: DEFAULT_FARMING_INACTIVE_BLOCK_THRESHOLD,
            minimum_block_interval: DEFAULT_FARMING_MINIMUM_BLOCK_INTERVAL,
        };
        let mut farming_monitor = MemoryFarmingMonitor::new(&config);

        let farmers = self::farmers();

        // First block, all farmers vote.
        simulate_block_votes(&mut farming_monitor, 0, &farmers).await;

        // Next 10 blocks, only the first farmer votes.
        for i in 1..=(DEFAULT_FARMING_INACTIVE_BLOCK_THRESHOLD + 1) {
            simulate_block_votes(&mut farming_monitor, i, &[farmers[0]]).await;
        }

        // Check the number of farmers with votes in the last `max_block_interval` blocks.
        assert_eq!(
            farming_monitor.state.active_farmers_in_last_blocks.front(),
            Some(&1)
        );

        assert!(
            farming_monitor
                .state
                .last_block_voted_by_farmer
                .contains_key(&farmers[0])
        );
        assert!(
            !farming_monitor
                .state
                .last_block_voted_by_farmer
                .contains_key(&farmers[1])
        );
        assert!(
            !farming_monitor
                .state
                .last_block_voted_by_farmer
                .contains_key(&farmers[2])
        );
    }

    #[tokio::test]
    /// Test that an alert is emitted when the number of farmers with votes decreases suddenly.
    async fn test_alert_emitted_on_drop_in_active_farmers() {
        let (alert_tx, mut alert_rx) = tokio::sync::mpsc::channel(10);
        let config = FarmingMonitorConfig {
            alert_tx,
            max_block_interval: 10,
            low_end_change_threshold: 0.8,
            high_end_change_threshold: 1.25,
            inactive_block_threshold: 10,
            minimum_block_interval: 0,
        };
        let mut farming_monitor = MemoryFarmingMonitor::new(&config);

        // Seed previous blocks with stable active farmer counts
        farming_monitor.state.active_farmers_in_last_blocks = VecDeque::from(vec![10, 10, 10]);

        // Current block has fewer active farmers
        farming_monitor.state.last_block_voted_by_farmer.clear();
        for i in 0..5u32 {
            // 5 active farmers now
            farming_monitor
                .state
                .last_block_voted_by_farmer
                .insert(H256::from_low_u64_be(u64::from(i)), 1);
        }

        let block_info = BlockInfo {
            block_height: 1,
            block_time: None,
            block_hash: H256::default(),
            genesis_hash: H256::zero(),
            block_slot: None,
            parent_hash: H256::zero(),
        };
        farming_monitor.update_number_of_farmers_with_votes();
        farming_monitor
            .check_farmer_count(block_info, BlockCheckMode::Current)
            .await;

        let alert = alert_rx.recv().await.expect("expected decrease alert");

        assert_eq!(
            alert,
            Alert::new(
                AlertKind::FarmersDecreasedSuddenly {
                    number_of_farmers_with_votes: 5,
                    average_number_of_farmers_with_votes: f64::from(10 + 10 + 10 + 5) / 4.0f64,
                    number_of_blocks: 4,
                },
                block_info,
                BlockCheckMode::Current,
            )
        );
    }

    #[tokio::test]
    /// Test that an alert is emitted when the number of farmers with votes increases suddenly.
    async fn test_alert_emitted_on_increase_in_active_farmers() {
        let (alert_tx, mut alert_rx) = tokio::sync::mpsc::channel(10);
        let config = FarmingMonitorConfig {
            alert_tx,
            max_block_interval: 10,
            low_end_change_threshold: 0.8,
            high_end_change_threshold: 1.25,
            inactive_block_threshold: 10,
            minimum_block_interval: 0,
        };
        let mut farming_monitor = MemoryFarmingMonitor::new(&config);

        // Seed previous blocks with stable active farmer counts
        farming_monitor.state.active_farmers_in_last_blocks = VecDeque::from(vec![10, 10, 10]);

        // Current block has more active farmers
        farming_monitor.state.last_block_voted_by_farmer.clear();
        for i in 0..15u32 {
            // 15 active farmers now
            farming_monitor
                .state
                .last_block_voted_by_farmer
                .insert(H256::from_low_u64_be(u64::from(i)), 1);
        }

        let mock_block_info = BlockInfo {
            block_height: 1,
            block_time: None,
            block_hash: H256::default(),
            genesis_hash: H256::zero(),
            block_slot: None,
            parent_hash: H256::zero(),
        };

        farming_monitor.update_number_of_farmers_with_votes();
        farming_monitor
            .check_farmer_count(mock_block_info, BlockCheckMode::Current)
            .await;

        let alert = alert_rx.recv().await.expect("expected increase alert");

        assert_eq!(
            alert,
            Alert::new(
                AlertKind::FarmersIncreasedSuddenly {
                    number_of_farmers_with_votes: 15,
                    average_number_of_farmers_with_votes: f64::from(10 + 10 + 10 + 15) / 4.0f64,
                    number_of_blocks: 4,
                },
                mock_block_info,
                BlockCheckMode::Current,
            )
        );
    }

    #[tokio::test]
    /// Test that no alert is emitted when the number of farmers with votes is within the
    /// thresholds.
    async fn test_no_alert_within_thresholds() {
        let (alert_tx, mut alert_rx) = tokio::sync::mpsc::channel(10);
        let config = FarmingMonitorConfig {
            alert_tx,
            max_block_interval: 10,
            low_end_change_threshold: 0.8,
            high_end_change_threshold: 1.25,
            inactive_block_threshold: 10,
            minimum_block_interval: 0,
        };
        let mut farming_monitor = MemoryFarmingMonitor::new(&config);

        // Seed previous blocks with stable active farmer counts
        farming_monitor.state.active_farmers_in_last_blocks = VecDeque::from(vec![10, 10, 10]);

        // Current block within thresholds (close to average)
        farming_monitor.state.last_block_voted_by_farmer.clear();
        for i in 0..11u32 {
            // 11 active farmers now
            farming_monitor
                .state
                .last_block_voted_by_farmer
                .insert(H256::from_low_u64_be(u64::from(i)), 1);
        }

        farming_monitor.update_number_of_farmers_with_votes();
        farming_monitor
            .check_farmer_count(
                BlockInfo {
                    block_height: 1,
                    block_time: None,
                    block_hash: H256::default(),
                    genesis_hash: H256::zero(),
                    block_slot: None,
                    parent_hash: H256::zero(),
                },
                BlockCheckMode::Current,
            )
            .await;

        // No alert expected
        alert_rx.try_recv().unwrap_err();
    }
}
