//! Tests for the farming monitor.

use super::*;
use crate::subspace::test_utils::mock_block_info;
use tokio::sync::mpsc;

const FAKE_NODE_RPC_URL: &str = "test";

fn farmers() -> [FarmerPublicKey; 3] {
    [
        FarmerPublicKey::from_low_u64_be(1),
        FarmerPublicKey::from_low_u64_be(2),
        FarmerPublicKey::from_low_u64_be(3),
    ]
}

async fn simulate_block_votes(
    farming_monitor: &mut MemoryFarmingMonitor,
    block_height: BlockNumber,
    farmers: &[FarmerPublicKey],
) -> anyhow::Result<()> {
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
    let mock_block_info = mock_block_info(None, None);
    if farming_monitor.has_passed_minimum_block_interval() {
        farming_monitor
            .check_farmer_count(BlockCheckMode::Current, &mock_block_info, FAKE_NODE_RPC_URL)
            .await?;
    }

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn test_farmers_going_inactive() -> anyhow::Result<()> {
    let alert_tx = mpsc::channel(100).0;
    let config = FarmingMonitorConfig {
        alert_tx,
        max_block_interval: DEFAULT_FARMING_MAX_HISTORY_BLOCK_INTERVAL,
        low_end_change_threshold: DEFAULT_LOW_END_FARMING_ALERT_THRESHOLD,
        high_end_change_threshold: DEFAULT_HIGH_END_FARMING_ALERT_THRESHOLD,
        inactive_block_threshold: DEFAULT_FARMING_INACTIVE_BLOCK_THRESHOLD,
        minimum_block_interval: DEFAULT_FARMING_MIN_ALERT_BLOCK_INTERVAL,
    };
    let mut farming_monitor = MemoryFarmingMonitor::new(&config);

    let farmers = self::farmers();

    // First block, all farmers vote.
    simulate_block_votes(&mut farming_monitor, 0, &farmers).await?;

    // Next 10 blocks, only the first farmer votes.
    for i in 1..=(DEFAULT_FARMING_INACTIVE_BLOCK_THRESHOLD + 1) {
        simulate_block_votes(&mut farming_monitor, i, &[farmers[0]])
            .await
            .unwrap_or_else(|error| panic!("unexpected error in block {i}: {error}"));
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

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
/// Test that an alert is emitted when the number of farmers with votes decreases suddenly.
async fn test_alert_emitted_on_drop_in_active_farmers() -> anyhow::Result<()> {
    let (alert_tx, mut alert_rx) = mpsc::channel(10);
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
            .insert(FarmerPublicKey::from_low_u64_be(u64::from(i)), 1);
    }

    let mock_block_info = mock_block_info(None, None);
    farming_monitor.update_number_of_farmers_with_votes();
    farming_monitor
        .check_farmer_count(BlockCheckMode::Current, &mock_block_info, FAKE_NODE_RPC_URL)
        .await?;

    let alert = alert_rx.recv().await.expect("expected decrease alert");

    assert_eq!(
        alert,
        Alert::new(
            AlertKind::FarmersDecreasedSuddenly {
                number_of_farmers_with_votes: 5,
                average_number_of_farmers_with_votes: f64::from(10 + 10 + 10 + 5) / 4.0f64,
                number_of_blocks: 4,
            },
            BlockCheckMode::Current,
            mock_block_info,
            FAKE_NODE_RPC_URL,
        )
    );

    assert_eq!(
        farming_monitor.state.status,
        FarmingMonitorStatus::AlertingDecrease
    );

    // Check that no alert is emitted again after the alert has been emitted.
    farming_monitor
        .check_farmer_count(BlockCheckMode::Current, &mock_block_info, FAKE_NODE_RPC_URL)
        .await?;

    alert_rx.try_recv().unwrap_err();

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
/// Test that an alert is emitted when the number of farmers with votes increases suddenly.
async fn test_alert_emitted_on_increase_in_active_farmers() -> anyhow::Result<()> {
    let (alert_tx, mut alert_rx) = mpsc::channel(10);
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
            .insert(FarmerPublicKey::from_low_u64_be(u64::from(i)), 1);
    }

    let mock_block_info = mock_block_info(None, None);
    farming_monitor.update_number_of_farmers_with_votes();
    farming_monitor
        .check_farmer_count(BlockCheckMode::Current, &mock_block_info, FAKE_NODE_RPC_URL)
        .await?;

    let alert = alert_rx.recv().await.expect("expected increase alert");

    assert_eq!(
        alert,
        Alert::new(
            AlertKind::FarmersIncreasedSuddenly {
                number_of_farmers_with_votes: 15,
                average_number_of_farmers_with_votes: f64::from(10 + 10 + 10 + 15) / 4.0f64,
                number_of_blocks: 4,
            },
            BlockCheckMode::Current,
            mock_block_info,
            FAKE_NODE_RPC_URL,
        )
    );

    assert_eq!(
        farming_monitor.state.status,
        FarmingMonitorStatus::AlertingIncrease
    );

    // Check that no alert is emitted again after the alert has been emitted.
    farming_monitor
        .check_farmer_count(BlockCheckMode::Current, &mock_block_info, FAKE_NODE_RPC_URL)
        .await?;

    alert_rx.try_recv().unwrap_err();

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
/// Test that no alert is emitted when the number of farmers with votes is within the
/// thresholds.
async fn test_no_alert_within_thresholds() -> anyhow::Result<()> {
    let (alert_tx, mut alert_rx) = mpsc::channel(10);
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
            .insert(FarmerPublicKey::from_low_u64_be(u64::from(i)), 1);
    }

    let mock_block_info = mock_block_info(None, None);
    farming_monitor.update_number_of_farmers_with_votes();
    farming_monitor
        .check_farmer_count(BlockCheckMode::Current, &mock_block_info, FAKE_NODE_RPC_URL)
        .await?;

    // No alert expected
    alert_rx.try_recv().unwrap_err();

    Ok(())
}
