//! Tests for subspace chain alerts.
//!
//! Set the `NODE_URL` env var to the RPC URL of a Subspace node to override the default public
//! instance.

use crate::alerts::{self, AlertKind, BlockCheckMode};
use crate::slot_time_monitor::test_utils::mock_block_info;
use crate::slot_time_monitor::{MemorySlotTimeMonitor, SlotTimeMonitor, SlotTimeMonitorConfig};
use crate::subspace::tests::{
    decode_event, decode_extrinsic, fetch_block_info, node_rpc_url, test_setup,
};
use crate::subspace::{Balance, BlockNumber, EventIndex, ExtrinsicIndex, RawBlockHash, Slot};
use anyhow::Ok;
use std::assert_matches::assert_matches;
use std::time::Duration;
use subxt::utils::H256;

/// The extrinsic and event for a recent sudo call.
/// <https://github.com/autonomys/subspace/releases/tag/runtime-mainnet-2025-jul-31>
///
/// TODO: turn this into a struct
const SUDO_BLOCK: (BlockNumber, RawBlockHash, ExtrinsicIndex, EventIndex, Slot) = (
    3_795_487,
    hex_literal::hex!("18c2f211b752cbc2f06943788ed011ab1fe64fb2e28ffcd1aeb4490c2e8b1baa"),
    5,
    11,
    Slot(22_859_254),
);

/// Some extrinsics for large balance transfers.
/// <https://autonomys.subscan.io/transfer?page=1&time_dimension=date&value_dimension=token&value_start=1000000>
///
/// TODO: turn this into a struct
const LARGE_TRANSFER_BLOCKS: [(BlockNumber, RawBlockHash, ExtrinsicIndex, Balance, Slot); 2] = [
    // <https://autonomys.subscan.io/extrinsic/3651663-3>
    (
        3_651_663,
        hex_literal::hex!("57d707e832379fa2e1a74f82b337178361f70770b252eff165027af5bdbff416"),
        3,
        1_139_874_580_721_948_575_925_918,
        Slot(21_994_481),
    ),
    // <https://autonomys.subscan.io/extrinsic/3662965-19>
    (
        3_662_965,
        hex_literal::hex!("1eb7e3ac5af1142f9e1298cd75475f77e3527d602ca5033ce7ace96e681c95d7"),
        19,
        2_114_333_002_000_000_000_000_000,
        Slot(22_062_247),
    ),
];

// TODO: force transfer blocks:
// Failed force_transfer: <https://autonomys.subscan.io/extrinsic/2173351-7>
// Failed force_set_balance: <https://autonomys.subscan.io/extrinsic/1154587-11>
// TODO: add success/failure to balance checks

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

    // Check block slot parsing works on real blocks.
    assert_matches!(alert.block_info.block_slot, Some(Slot(_)));

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

    alerts::check_extrinsic(BlockCheckMode::Replay, &alert_tx, &extrinsic, &block_info).await?;
    let alert = alert_rx.try_recv().expect("no alert received");
    assert_eq!(alert.alert, AlertKind::SudoCall { extrinsic_info });
    assert_eq!(
        alert
            .alert
            .extrinsic_info()
            .map(|ei| (ei.pallet.as_str(), ei.call.as_str())),
        Some(("Sudo", "sudo"))
    );
    assert_eq!(alert.block_info, block_info);

    let (event, event_info) = decode_event(&block_info, &events, SUDO_BLOCK.3).await?;

    alerts::check_event(BlockCheckMode::Replay, &alert_tx, &event, &block_info).await?;
    let alert = alert_rx.try_recv().expect("no alert received");
    assert_eq!(alert.alert, AlertKind::SudoEvent { event_info });
    assert_eq!(
        alert
            .alert
            .event_info()
            .map(|ei| (ei.pallet.as_str(), ei.kind.as_str())),
        Some(("Sudo", "Sudid"))
    );
    assert_eq!(alert.block_info, block_info);

    // Check block slot parsing works on a known slot value.
    assert_eq!(alert.block_info.block_slot, Some(SUDO_BLOCK.4));

    Ok(())
}

/// Check that the large balance transfer alert works on known transfer blocks.
#[tokio::test(flavor = "multi_thread")]
async fn test_large_balance_transfer_alerts() -> anyhow::Result<()> {
    let (subspace_client, alert_tx, mut alert_rx, _update_task) =
        test_setup(node_rpc_url()).await?;

    for (block_number, block_hash, extrinsic_index, transfer_value, slot) in LARGE_TRANSFER_BLOCKS {
        let (block_info, extrinsics, _events) =
            fetch_block_info(&subspace_client, H256::from(block_hash), block_number).await?;

        let (extrinsic, extrinsic_info) =
            decode_extrinsic(&block_info, &extrinsics, extrinsic_index).await?;

        alerts::check_extrinsic(BlockCheckMode::Replay, &alert_tx, &extrinsic, &block_info).await?;
        let alert = alert_rx.try_recv().expect("no alert received");
        assert_eq!(
            alert.alert,
            AlertKind::LargeBalanceTransfer {
                extrinsic_info,
                transfer_value,
            }
        );
        assert_eq!(alert.block_info, block_info);

        // Check block slot parsing works on a known slot value.
        assert_eq!(alert.block_info.block_slot, Some(slot));
    }

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

    naive_slot_time_monitor
        .process_block(BlockCheckMode::Replay, &first_block)
        .await;
    naive_slot_time_monitor
        .process_block(BlockCheckMode::Replay, &second_block)
        .await;

    alert_rx
        .try_recv()
        .expect_err("alert received when none expected");

    Ok(())
}

/// Check that the slot time alert is triggered when the time per slot is above the threshold and
/// has elapsed enough time.
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

    strict_slot_time_monitor
        .process_block(BlockCheckMode::Replay, &first_block)
        .await;
    strict_slot_time_monitor
        .process_block(BlockCheckMode::Replay, &second_block)
        .await;

    let alert = alert_rx
        .try_recv()
        .expect("alert not received when expected");

    assert_eq!(
        alert.alert,
        AlertKind::SlotTime {
            current_ratio: 0.01,
            threshold: 0.0,
            interval: Duration::from_secs(1),
            first_slot_time: first_block
                .block_time
                .expect("block must have time to trigger alert"),
        }
    );
    assert_eq!(alert.block_info, second_block);

    Ok(())
}

/// Check that the slot time alert is not triggered when the time per slot is above the threshold
/// but has not elapsed enough time.
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

    strict_slot_time_monitor
        .process_block(BlockCheckMode::Replay, &first_block)
        .await;
    strict_slot_time_monitor
        .process_block(BlockCheckMode::Replay, &second_block)
        .await;

    alert_rx
        .try_recv()
        .expect_err("alert received when none expected");

    Ok(())
}
