//! Tests for subspace chain alerts.
//!
//! Set the `NODE_URL` env var to the RPC URL of a Subspace node to override the default public
//! instance.

use crate::slot_time_monitor::test_utils::mock_block_info;
use std::time::Duration;

use crate::alerts::{self, AlertKind};
use crate::slot_time_monitor::{MemorySlotTimeMonitor, SlotTimeMonitor, SlotTimeMonitorConfig};
use crate::subspace::tests::{
    decode_event, decode_extrinsic, fetch_block_info, node_rpc_url, test_setup,
};
use crate::subspace::{BlockNumber, Slot};
use anyhow::Ok;
use subxt::utils::H256;

/// The raw block hash literal type.
type RawBlockHash = [u8; 32];

/// The extrinsic index type.
type ExtrinsicIndex = u32;

/// The event index type.
type EventIndex = u32;

/// The extrinsic and event for a recent sudo call.
/// <https://github.com/autonomys/subspace/releases/tag/runtime-mainnet-2025-jul-31>
///
/// TODO: turn this into a struct
const SUDO_BLOCK: (BlockNumber, RawBlockHash, ExtrinsicIndex, EventIndex) = (
    3_795_487,
    hex_literal::hex!("18c2f211b752cbc2f06943788ed011ab1fe64fb2e28ffcd1aeb4490c2e8b1baa"),
    5,
    11,
);

/// Check that the startup alert works on the latest block.
#[tokio::test(flavor = "multi_thread")]
async fn test_startup_alert() -> anyhow::Result<()> {
    let (subspace_client, alert_tx, mut alert_rx, _update_task) =
        test_setup(node_rpc_url()).await?;

    let (block_info, _, _) = fetch_block_info(&subspace_client, None, None).await?;

    alerts::startup_alert(&alert_tx, &block_info).await?;
    let alert = alert_rx.try_recv().expect("no alert received");
    assert_eq!(alert.alert, AlertKind::Startup);
    assert_eq!(alert.block_info, block_info);

    Ok(())
}

/// Check that the sudo call and event alerts work on a known sudo block.
#[tokio::test(flavor = "multi_thread")]
async fn test_sudo_alerts() -> anyhow::Result<()> {
    let (subspace_client, alert_tx, mut alert_rx, _update_task) =
        test_setup(node_rpc_url()).await?;

    let (block_info, extrinsics, events) =
        fetch_block_info(&subspace_client, H256::from(SUDO_BLOCK.1), SUDO_BLOCK.0).await?;

    let (extrinsic, extrinsic_info) =
        decode_extrinsic(&block_info, &extrinsics, SUDO_BLOCK.2).await?;

    alerts::check_extrinsic(&alert_tx, &extrinsic, &block_info).await?;
    let alert = alert_rx.try_recv().expect("no alert received");
    assert_eq!(alert.alert, AlertKind::SudoCall { extrinsic_info });
    assert_eq!(alert.block_info, block_info);

    let (event, event_info) = decode_event(&block_info, &events, SUDO_BLOCK.3).await?;

    alerts::check_event(&alert_tx, &event, &block_info).await?;
    let alert = alert_rx.try_recv().expect("no alert received");
    assert_eq!(alert.alert, AlertKind::SudoEvent { event_info });
    assert_eq!(alert.block_info, block_info);

    Ok(())
}

/// Check that the slot time alert is not triggered when the time per slot is below the threshold.
#[tokio::test(flavor = "multi_thread")]
async fn no_expected_test_slot_time_alert() -> anyhow::Result<()> {
    let (_, alert_tx, mut alert_rx, _update_task) = test_setup(node_rpc_url()).await?;

    let first_block = mock_block_info(1000, Slot(100));
    let second_block = mock_block_info(2000, Slot(200));

    let mut naive_slot_time_monitor = MemorySlotTimeMonitor::new({
        SlotTimeMonitorConfig {
            check_interval: Duration::from_secs(1),
            alert_threshold: 10.0f64,
            alert_tx: alert_tx.clone(),
        }
    });

    naive_slot_time_monitor.process_block(&first_block).await;
    naive_slot_time_monitor.process_block(&second_block).await;

    alert_rx
        .try_recv()
        .expect_err("alert received when none expected");

    Ok(())
}

/// Check that the slot time alert is triggered when the time per slot is above the threshold and has elapsed enough time.
#[tokio::test(flavor = "multi_thread")]
async fn expected_test_slot_time_alert() -> anyhow::Result<()> {
    let (_, alert_tx, mut alert_rx, _update_task) = test_setup(node_rpc_url()).await?;

    let first_block = mock_block_info(1000, Slot(100));
    let second_block = mock_block_info(2000, Slot(200));

    let mut strict_slot_time_monitor = MemorySlotTimeMonitor::new({
        SlotTimeMonitorConfig {
            check_interval: Duration::from_secs(1),
            alert_threshold: 0f64,
            alert_tx: alert_tx.clone(),
        }
    });

    strict_slot_time_monitor.process_block(&first_block).await;
    strict_slot_time_monitor.process_block(&second_block).await;

    let alert = alert_rx
        .try_recv()
        .expect("alert not received when expected");

    assert_eq!(
        alert.alert,
        AlertKind::SlotTimeAlert {
            current_ratio: 0.01,
            threshold: 0.0,
            interval: Duration::from_secs(1),
        }
    );
    assert_eq!(alert.block_info, second_block);

    Ok(())
}

/// Check that the slot time alert is not triggered when the time per slot is above the threshold but has not elapsed enough time.
#[tokio::test(flavor = "multi_thread")]
async fn expected_test_slot_time_alert_but_not_yet() -> anyhow::Result<()> {
    let (_, alert_tx, mut alert_rx, _update_task) = test_setup(node_rpc_url()).await?;

    let first_block = mock_block_info(1000, Slot(100));
    let second_block = mock_block_info(2000, Slot(200));

    let mut strict_slot_time_monitor = MemorySlotTimeMonitor::new({
        SlotTimeMonitorConfig {
            check_interval: Duration::from_secs(3600 * 24),
            alert_threshold: 0f64,
            alert_tx: alert_tx.clone(),
        }
    });

    strict_slot_time_monitor.process_block(&first_block).await;
    strict_slot_time_monitor.process_block(&second_block).await;

    alert_rx
        .try_recv()
        .expect_err("alert received when none expected");

    Ok(())
}
