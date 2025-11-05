//! Tests for subspace chain alerts.
//!
//! Set the `NODE_URL` env var to the RPC URL of a Subspace node to override the default public
//! instance.

#![allow(clippy::unwrap_in_result, reason = "panics are ok in failing tests")]

use crate::alerts::account::ImportantAccountRole;
use crate::alerts::{self, Alert, AlertKind, BlockCheckMode, MIN_BALANCE_CHANGE};
use crate::slot_time_monitor::{MemorySlotTimeMonitor, SlotTimeMonitor, SlotTimeMonitorConfig};
use crate::subspace::test_utils::{
    alert_channel_only_setup, decode_event, decode_extrinsic, fetch_block_info, mock_block_info,
    node_rpc_urls, test_setup,
};
use crate::subspace::{
    AI3, Balance, BlockHash, BlockInfo, BlockNumber, EventIndex, ExtrinsicIndex, RawBlockHash, Slot,
};
use anyhow::Ok;
use std::assert_matches::assert_matches;
use subxt::ext::futures::{FutureExt, StreamExt};

/// The extrinsic and event for a recent sudo call.
/// <https://github.com/autonomys/subspace/releases/tag/runtime-mainnet-2025-jul-31>
///
/// TODO: turn this into a struct
const SUDO_EXTRINSIC_BLOCK: (BlockNumber, RawBlockHash, ExtrinsicIndex, EventIndex, Slot) = (
    3_795_487,
    hex_literal::hex!("18c2f211b752cbc2f06943788ed011ab1fe64fb2e28ffcd1aeb4490c2e8b1baa"),
    5,
    11,
    Slot(22_859_254),
);

/// Some extrinsics and events for large balance transfers.
/// <https://autonomys.subscan.io/transfer?page=1&time_dimension=date&value_dimension=token&value_start=1000000>
///
/// TODO: turn this into a struct
const LARGE_TRANSFER_BLOCKS: [(
    BlockNumber,
    RawBlockHash,
    ExtrinsicIndex,
    EventIndex,
    Balance,
    Slot,
); 2] = [
    (
        3_651_663,
        hex_literal::hex!("57d707e832379fa2e1a74f82b337178361f70770b252eff165027af5bdbff416"),
        // <https://autonomys.subscan.io/extrinsic/3651663-3>
        3,
        // <https://autonomys.subscan.io/event/3651663-6>
        6,
        1_139_874_580_721_948_575_925_918,
        Slot(21_994_481),
    ),
    (
        3_662_965,
        hex_literal::hex!("1eb7e3ac5af1142f9e1298cd75475f77e3527d602ca5033ce7ace96e681c95d7"),
        // <https://autonomys.subscan.io/extrinsic/3662965-19>
        19,
        // <https://autonomys.subscan.io/event/3662965-41>
        41,
        2_114_333_002_000_000_000_000_000,
        Slot(22_062_247),
    ),
];

/// Some extrinsics and events for important address transfers.
#[expect(
    clippy::type_complexity,
    reason = "this is a test, TODO: turn this into a struct"
)]
const IMPORTANT_ADDRESS_TRANSFER_BLOCKS: [(
    BlockNumber,
    RawBlockHash,
    ExtrinsicIndex,
    EventIndex,
    // Extrinsic and event important accounts or roles can be different if the event has more or
    // less detail than the extrinsic.
    &str,
    &str,
    // Extrinsic and event transfer values can be different if the extrinsic causes multiple
    // events.
    Balance,
    Balance,
    Slot,
); 4] = [
    // <https://autonomys.subscan.io/account/sudqduciRx3fZcNRbW1mmmBAntuZLkwhcXyVctsGJyrjRTPaA>
    (
        4_259_127,
        hex_literal::hex!("db8e0ad0b8ced4483b3cd90780716ac3eab27b55ab667c59cd159ac40756d5c0"),
        // <https://autonomys.subscan.io/extrinsic/4259127-4>
        4,
        // <https://autonomys.subscan.io/event/4259127-8>
        8,
        "Guardians of Growth, Subspace Foundation Near-Term Treasury",
        "Guardians of Growth, Subspace Foundation Near-Term Treasury",
        4_999_990 * AI3,
        4_999_990 * AI3,
        Slot(25_647_927),
    ),
    // <https://autonomys.subscan.io/account/suesYE9yAqNJrMiZPY4hKNMjMTXBkkD1rHgQrSNes1bUnw37U>
    (
        4_204_424,
        hex_literal::hex!("dd369e3ea40398feda099d88b02ff1c9094b3df346efd47e8c78c1508f5da1aa"),
        // <https://autonomys.subscan.io/extrinsic/4204424-25>
        25,
        // <https://autonomys.subscan.io/event/4204424-50>
        50,
        "Signer: Market Liquidity",
        "Signer: Market Liquidity, Sender: Market Liquidity",
        20_000 * AI3,
        20_000 * AI3,
        Slot(25_314_662),
    ),
    // <https://autonomys.subscan.io/account/sudqduciRx3fZcNRbW1mmmBAntuZLkwhcXyVctsGJyrjRTPaA>
    (
        3_850_288,
        hex_literal::hex!("57e48f5cd1258de2f53400d774f6e4d3e4a57e935708d8a655cf85fde1e2dacb"),
        // <https://autonomys.subscan.io/extrinsic/3850288-13>
        13,
        // <https://autonomys.subscan.io/event/3850288-28>
        28,
        "Signer: Operations",
        "Sender: Operations",
        150 * AI3,
        150 * AI3,
        Slot(23_187_041),
    ),
    // <https://autonomys.subscan.io/account/sugQzjjyAfhzktFDdAkZrcTq5qzMaRoSV2qs1gTcjjuBeybWT>
    (
        4_273_214,
        hex_literal::hex!("0f186a758890e8653e4978064f929ccdc9d134b0abc8dcb04593a8d236bc0bc5"),
        // <https://autonomys.subscan.io/extrinsic/4273214-4>
        4,
        // <https://autonomys.subscan.io/event/4273214-15>
        15,
        "Signer: Guardians of Growth",
        "Sender: Guardians of Growth",
        499_999_999_999_999_991_611_392,
        399_999_999_999_999_993_289_114,
        Slot(25_732_135),
    ),
];

/// Some extrinsics for important address alerts (which aren't any other higher
/// priority alert).
/// TODO: find an event sent by an important address, that isn't a higher priority alert.
const IMPORTANT_ADDRESS_ONLY_BLOCKS: [(
    BlockNumber,
    RawBlockHash,
    ExtrinsicIndex,
    ImportantAccountRole,
    Slot,
); 1] = [
    // <https://autonomys.subscan.io/account/subKQqsYRyVkugvKQqLXEuhsefa9728PBAqtwxpeM5N4VD6mv>
    (
        3_497_809,
        hex_literal::hex!("bfa548573d1ff035e2009fdaa68499fe74c4ab30a775f5eb35624fdb9f95dc91"),
        // <https://autonomys.subscan.io/extrinsic/3497809-9>
        9,
        ImportantAccountRole::Signer("Sudo"),
        Slot(21_070_789),
    ),
];

// TODO: force transfer blocks:
// Failed force_transfer: <https://autonomys.subscan.io/extrinsic/2173351-7>
// Failed force_set_balance: <https://autonomys.subscan.io/extrinsic/1154587-11>
// TODO: add success/failure to balance checks

/// Check that the startup alert works on the latest block.
#[tokio::test(flavor = "multi_thread")]
async fn test_startup_alert() -> anyhow::Result<()> {
    let node_rpc_urls = node_rpc_urls();
    let (rpc_client_list, alert_tx, mut alert_rx, mut update_tasks) =
        test_setup(node_rpc_urls.clone()).await?;

    let block_info = BlockInfo::with_block_hash(None, &rpc_client_list).await?;

    alerts::startup_alert(
        BlockCheckMode::Current,
        &block_info,
        &alert_tx,
        rpc_client_list.node_rpc_urls().to_vec(),
    )
    .await?;
    let alert = alert_rx.try_recv().expect("no alert received");
    assert_eq!(
        alert,
        Alert::new(
            AlertKind::Startup {
                node_rpc_urls: node_rpc_urls.clone(),
            },
            BlockCheckMode::Current,
            block_info,
            &node_rpc_urls[0],
        ),
    );

    // There should be no other alerts.
    alert_rx
        .try_recv()
        .expect_err("alert received when none expected");

    // Check block slot parsing works on real blocks.
    assert_matches!(alert.block_info.slot, Some(Slot(_)));

    while let Some(result) = update_tasks.next().now_or_never() {
        assert!(
            result.is_none(),
            "metadata update task exited unexpectedly with: {result:?}"
        );
    }

    Ok(())
}

/// Check that the sudo call and event alerts work on a known sudo block.
#[tokio::test(flavor = "multi_thread")]
async fn test_sudo_alerts() -> anyhow::Result<()> {
    let node_rpc_urls = node_rpc_urls();
    let (rpc_client_list, alert_tx, mut alert_rx, mut update_tasks) =
        test_setup(node_rpc_urls.clone()).await?;

    let (block_info, extrinsics, events) = fetch_block_info(
        BlockHash::from(SUDO_EXTRINSIC_BLOCK.1),
        true,
        &rpc_client_list,
        SUDO_EXTRINSIC_BLOCK.0,
    )
    .await?;

    let (extrinsic, extrinsic_info) =
        decode_extrinsic(&block_info, &extrinsics, SUDO_EXTRINSIC_BLOCK.2)?;

    let checked_extrinsic_info = alerts::check_extrinsic(
        BlockCheckMode::Replay,
        &extrinsic,
        &block_info,
        &alert_tx,
        &node_rpc_urls[0],
    )
    .await?;
    assert_eq!(
        Some(&extrinsic_info),
        checked_extrinsic_info.as_ref(),
        "extrinsic info mismatch",
    );

    let alert = alert_rx.try_recv().expect("no alert received");
    assert_eq!(
        alert,
        Alert::new(
            AlertKind::SudoCall {
                extrinsic_info: extrinsic_info.clone(),
            },
            BlockCheckMode::Replay,
            block_info,
            &node_rpc_urls[0],
        )
    );
    assert_eq!(
        alert
            .alert
            .extrinsic_info()
            .map(|ei| (ei.pallet.as_str(), ei.call.as_str())),
        Some(("Sudo", "sudo"))
    );

    let (event, event_info) = decode_event(
        &block_info,
        Some(extrinsic_info.clone()),
        &events.unwrap(),
        SUDO_EXTRINSIC_BLOCK.3,
    )?;

    alerts::check_event(
        BlockCheckMode::Replay,
        &event,
        &block_info,
        Some(extrinsic_info),
        &alert_tx,
        &node_rpc_urls[0],
    )
    .await?;
    let alert = alert_rx.try_recv().expect("no alert received");
    assert_eq!(
        alert,
        Alert::new(
            AlertKind::SudoEvent { event_info },
            BlockCheckMode::Replay,
            block_info,
            &node_rpc_urls[0],
        )
    );
    assert_eq!(
        alert
            .alert
            .event_info()
            .map(|ei| (ei.pallet.as_str(), ei.kind.as_str())),
        Some(("Sudo", "Sudid"))
    );

    alert_rx
        .try_recv()
        .expect_err("alert received when none expected");

    // Check block slot parsing works on a known slot value.
    assert_eq!(alert.block_info.slot, Some(SUDO_EXTRINSIC_BLOCK.4));

    while let Some(result) = update_tasks.next().now_or_never() {
        assert!(
            result.is_none(),
            "metadata update task exited unexpectedly with: {result:?}"
        );
    }

    Ok(())
}

/// Check that the large balance transfer alert works on known transfer blocks.
#[tokio::test(flavor = "multi_thread")]
async fn test_large_balance_transfer_alerts() -> anyhow::Result<()> {
    let node_rpc_urls = node_rpc_urls();
    let (rpc_client_list, alert_tx, mut alert_rx, mut update_tasks) =
        test_setup(node_rpc_urls.clone()).await?;

    for (block_number, block_hash, extrinsic_index, event_index, transfer_value, slot) in
        LARGE_TRANSFER_BLOCKS
    {
        let (block_info, extrinsics, events) = fetch_block_info(
            BlockHash::from(block_hash),
            true,
            &rpc_client_list,
            block_number,
        )
        .await?;

        let (extrinsic, extrinsic_info) =
            decode_extrinsic(&block_info, &extrinsics, extrinsic_index)?;
        let (event, event_info) = decode_event(
            &block_info,
            Some(extrinsic_info.clone()),
            &events.unwrap(),
            event_index,
        )?;

        let checked_extrinsic_info = alerts::check_extrinsic(
            BlockCheckMode::Replay,
            &extrinsic,
            &block_info,
            &alert_tx,
            &node_rpc_urls[0],
        )
        .await?;
        assert_eq!(
            Some(&extrinsic_info),
            checked_extrinsic_info.as_ref(),
            "extrinsic info mismatch",
        );

        let alert = alert_rx.try_recv().expect("no extrinsic alert received");
        assert_eq!(
            alert,
            Alert::new(
                AlertKind::LargeBalanceTransfer {
                    extrinsic_info: extrinsic_info.clone(),
                    transfer_value,
                },
                BlockCheckMode::Replay,
                block_info,
                &node_rpc_urls[0],
            )
        );

        alert_rx
            .try_recv()
            .expect_err("alert received when none expected");

        alerts::check_event(
            BlockCheckMode::Replay,
            &event,
            &block_info,
            Some(extrinsic_info),
            &alert_tx,
            &node_rpc_urls[0],
        )
        .await?;
        let alert = alert_rx.try_recv().expect("no event alert received");
        assert_eq!(
            alert,
            Alert::new(
                AlertKind::LargeBalanceTransferEvent {
                    event_info,
                    transfer_value,
                },
                BlockCheckMode::Replay,
                block_info,
                &node_rpc_urls[0],
            )
        );

        alert_rx
            .try_recv()
            .expect_err("alert received when none expected");

        // Check block slot parsing works on a known slot value.
        assert_eq!(alert.block_info.slot, Some(slot));
    }

    while let Some(result) = update_tasks.next().now_or_never() {
        assert!(
            result.is_none(),
            "metadata update task exited unexpectedly with: {result:?}"
        );
    }

    Ok(())
}

/// Check that important address transfer alerts work on known important address transfer blocks.
#[tokio::test(flavor = "multi_thread")]
async fn test_important_address_transfer_alerts() -> anyhow::Result<()> {
    let node_rpc_urls = node_rpc_urls();
    let (rpc_client_list, alert_tx, mut alert_rx, mut update_tasks) =
        test_setup(node_rpc_urls.clone()).await?;

    for (
        block_number,
        block_hash,
        extrinsic_index,
        event_index,
        extrinsic_address_kinds,
        event_address_kinds,
        extrinsic_transfer_value,
        event_transfer_value,
        slot,
    ) in IMPORTANT_ADDRESS_TRANSFER_BLOCKS
    {
        let (block_info, extrinsics, events) = fetch_block_info(
            BlockHash::from(block_hash),
            true,
            &rpc_client_list,
            block_number,
        )
        .await?;

        let (extrinsic, extrinsic_info) =
            decode_extrinsic(&block_info, &extrinsics, extrinsic_index)?;
        let (event, event_info) = decode_event(
            &block_info,
            Some(extrinsic_info.clone()),
            &events.unwrap(),
            event_index,
        )?;

        let checked_extrinsic_info = alerts::check_extrinsic(
            BlockCheckMode::Replay,
            &extrinsic,
            &block_info,
            &alert_tx,
            &node_rpc_urls[0],
        )
        .await?;
        assert_eq!(
            Some(&extrinsic_info),
            checked_extrinsic_info.as_ref(),
            "extrinsic info mismatch",
        );

        let alert = alert_rx.try_recv().expect("no extrinsic alert received");

        // There should only be one alert per extrinsic, and one per event, because more important
        // alerts override less important/specific alerts.
        if extrinsic_transfer_value >= MIN_BALANCE_CHANGE {
            assert_eq!(
                alert,
                Alert::new(
                    AlertKind::LargeBalanceTransfer {
                        extrinsic_info: extrinsic_info.clone(),
                        transfer_value: extrinsic_transfer_value,
                    },
                    BlockCheckMode::Replay,
                    block_info,
                    &node_rpc_urls[0],
                )
            );
        } else {
            assert_eq!(
                alert,
                Alert::new(
                    AlertKind::ImportantAddressTransfer {
                        address_kinds: extrinsic_address_kinds.to_string(),
                        extrinsic_info: extrinsic_info.clone(),
                        transfer_value: Some(extrinsic_transfer_value),
                    },
                    BlockCheckMode::Replay,
                    block_info,
                    &node_rpc_urls[0],
                )
            );
        }

        alert_rx
            .try_recv()
            .expect_err("alert received when none expected");

        alerts::check_event(
            BlockCheckMode::Replay,
            &event,
            &block_info,
            Some(extrinsic_info),
            &alert_tx,
            &node_rpc_urls[0],
        )
        .await?;
        let alert = alert_rx.try_recv().expect("no event alert received");

        if event_transfer_value >= MIN_BALANCE_CHANGE {
            assert_eq!(
                alert,
                Alert::new(
                    AlertKind::LargeBalanceTransferEvent {
                        event_info: event_info.clone(),
                        transfer_value: event_transfer_value,
                    },
                    BlockCheckMode::Replay,
                    block_info,
                    &node_rpc_urls[0],
                )
            );
        } else {
            assert_eq!(
                alert,
                Alert::new(
                    AlertKind::ImportantAddressTransferEvent {
                        address_kinds: event_address_kinds.to_string(),
                        event_info,
                        transfer_value: Some(event_transfer_value),
                    },
                    BlockCheckMode::Replay,
                    block_info,
                    &node_rpc_urls[0],
                )
            );
        }

        alert_rx
            .try_recv()
            .expect_err("alert received when none expected");

        // Check block slot parsing works on a known slot value.
        assert_eq!(alert.block_info.slot, Some(slot));
    }

    while let Some(result) = update_tasks.next().now_or_never() {
        assert!(
            result.is_none(),
            "metadata update task exited unexpectedly with: {result:?}"
        );
    }

    Ok(())
}

/// Check that the important address alert works on known important address blocks.
#[tokio::test(flavor = "multi_thread")]
async fn test_important_address_only_alerts() -> anyhow::Result<()> {
    let node_rpc_urls = node_rpc_urls();
    let (rpc_client_list, alert_tx, mut alert_rx, mut update_tasks) =
        test_setup(node_rpc_urls.clone()).await?;

    for (block_number, block_hash, extrinsic_index, address_kind, slot) in
        IMPORTANT_ADDRESS_ONLY_BLOCKS
    {
        let (block_info, extrinsics, _no_events) = fetch_block_info(
            BlockHash::from(block_hash),
            false,
            &rpc_client_list,
            block_number,
        )
        .await?;

        let (extrinsic, extrinsic_info) =
            decode_extrinsic(&block_info, &extrinsics, extrinsic_index)?;

        let checked_extrinsic_info = alerts::check_extrinsic(
            BlockCheckMode::Replay,
            &extrinsic,
            &block_info,
            &alert_tx,
            &node_rpc_urls[0],
        )
        .await?;
        assert_eq!(
            Some(&extrinsic_info),
            checked_extrinsic_info.as_ref(),
            "extrinsic info mismatch",
        );

        let alert = alert_rx.try_recv().expect("no extrinsic alert received");

        assert_eq!(
            alert,
            Alert::new(
                AlertKind::ImportantAddressExtrinsic {
                    address_kind: address_kind.to_string(),
                    extrinsic_info: extrinsic_info.clone(),
                },
                BlockCheckMode::Replay,
                block_info,
                &node_rpc_urls[0],
            )
        );

        alert_rx
            .try_recv()
            .expect_err("alert received when none expected");

        // Check block slot parsing works on a known slot value.
        assert_eq!(alert.block_info.slot, Some(slot));
    }

    while let Some(result) = update_tasks.next().now_or_never() {
        assert!(
            result.is_none(),
            "metadata update task exited unexpectedly with: {result:?}"
        );
    }

    Ok(())
}

/// Check that the slot time alert is not triggered when the time per slot is below the threshold.
#[tokio::test(flavor = "multi_thread")]
async fn no_expected_test_slot_time_alert() -> anyhow::Result<()> {
    let node_rpc_url = node_rpc_urls().pop().expect("no RPC URL");
    let (alert_tx, mut alert_rx) = alert_channel_only_setup();

    let first_block = mock_block_info(100000, Slot(100));
    let second_block = mock_block_info(200000, Slot(200));

    let mut naive_slot_time_monitor = MemorySlotTimeMonitor::new(SlotTimeMonitorConfig::new(
        2,      // max_block_buffer - small buffer for testing
        2f64,   // slow_slots_threshold
        0.1f64, // fast_slots_threshold
        alert_tx.clone(),
    ));

    // Process blocks to fill the buffer
    naive_slot_time_monitor
        .process_block(BlockCheckMode::Replay, &first_block, &node_rpc_url)
        .await?;
    naive_slot_time_monitor
        .process_block(BlockCheckMode::Replay, &second_block, &node_rpc_url)
        .await?;

    alert_rx
        .try_recv()
        .expect_err("alert received when none expected");

    Ok(())
}

/// Check that the slot time alert is triggered when the time per slot is above the threshold and
/// has elapsed enough time.
#[tokio::test(flavor = "multi_thread")]
async fn expected_test_slot_time_alert() -> anyhow::Result<()> {
    let node_rpc_url = node_rpc_urls().pop().expect("no RPC URL");
    let (alert_tx, mut alert_rx) = alert_channel_only_setup();

    let first_block = mock_block_info(100000, Slot(100));
    let second_block = mock_block_info(200000, Slot(200));

    let mut strict_slot_time_monitor = MemorySlotTimeMonitor::new(SlotTimeMonitorConfig::new(
        2,      // max_block_buffer - small buffer for testing
        0.2f64, // slow_slots_threshold
        0.1f64, // fast_slots_threshold
        alert_tx.clone(),
    ));

    // Process blocks to fill the buffer
    strict_slot_time_monitor
        .process_block(BlockCheckMode::Replay, &first_block, &node_rpc_url)
        .await?;
    strict_slot_time_monitor
        .process_block(BlockCheckMode::Replay, &second_block, &node_rpc_url)
        .await?;

    let alert = alert_rx
        .try_recv()
        .expect("alert not received when expected");

    assert_eq!(
        alert,
        Alert::new(
            AlertKind::SlowSlotTime {
                slot_amount: 100,
                seconds_elapsed: 100,
                seconds_per_slot: 1.0,
                threshold: 0.2f64,
            },
            BlockCheckMode::Replay,
            second_block,
            &node_rpc_url,
        )
    );

    alert_rx
        .try_recv()
        .expect_err("alert received when none expected");

    Ok(())
}

/// Check that the slot time alert is triggered
/// when the time per slot ratio gets above the slow threshold.
#[tokio::test(flavor = "multi_thread")]
async fn test_slot_time_above_slow_threshold() -> anyhow::Result<()> {
    let node_rpc_url = node_rpc_urls().pop().expect("no RPC URL");
    let (alert_tx, mut alert_rx) = alert_channel_only_setup();

    // Create blocks with a very fast slot progression (high ratio)
    // First block at time 100000ms, slot 100
    // Second block at time 101000ms, slot 200
    // This gives us 1 slots in 1000ms = 1 slots per second
    let first_block = mock_block_info(100000, Slot(100));
    let second_block = mock_block_info(200000, Slot(101));

    let mut slot_time_monitor = MemorySlotTimeMonitor::new(SlotTimeMonitorConfig::new(
        2,       // max_block_buffer - small buffer for testing
        2f64,    // slow_slots_threshold
        0.01f64, // fast_slots_threshold
        alert_tx.clone(),
    ));

    // Process blocks to fill the buffer
    slot_time_monitor
        .process_block(BlockCheckMode::Replay, &first_block, &node_rpc_url)
        .await?;
    slot_time_monitor
        .process_block(BlockCheckMode::Replay, &second_block, &node_rpc_url)
        .await?;

    let alert = alert_rx
        .try_recv()
        .expect("SlowSlotTime alert not received when expected");

    assert_eq!(
        alert,
        Alert::new(
            AlertKind::SlowSlotTime {
                slot_amount: 1,
                seconds_elapsed: 100,
                seconds_per_slot: 100.0, // 100_000ms / 1 slots = 100 seconds per slot
                threshold: 2f64,
            },
            BlockCheckMode::Replay,
            second_block,
            &node_rpc_url,
        )
    );

    alert_rx
        .try_recv()
        .expect_err("alert received when none expected");

    Ok(())
}

/// Check that the slot time alert is triggered when the time per slot ratio gets below the fast
/// threshold.
#[tokio::test(flavor = "multi_thread")]
async fn test_slot_time_below_fast_threshold() -> anyhow::Result<()> {
    let node_rpc_url = node_rpc_urls().pop().expect("no RPC URL");
    let (alert_tx, mut alert_rx) = alert_channel_only_setup();

    // Create blocks with a very slow slot progression (low ratio)
    // First block at time 100000, slot 100
    // Second block at time 200000, slot 200
    // This gives us 100 slots in 100000ms = 1 slots per second
    let first_block = mock_block_info(100000, Slot(100));
    let second_block = mock_block_info(200000, Slot(200));

    let mut slot_time_monitor = MemorySlotTimeMonitor::new(SlotTimeMonitorConfig::new(
        2,     // max_block_buffer - small buffer for testing
        10f64, // slow_slots_threshold
        2f64,  // fast_slots_threshold
        alert_tx.clone(),
    ));

    // Process blocks to fill the buffer
    slot_time_monitor
        .process_block(BlockCheckMode::Replay, &first_block, &node_rpc_url)
        .await?;
    slot_time_monitor
        .process_block(BlockCheckMode::Replay, &second_block, &node_rpc_url)
        .await?;

    let alert = alert_rx
        .try_recv()
        .expect("FastSlotTime alert not received when expected");

    assert_eq!(
        alert,
        Alert::new(
            AlertKind::FastSlotTime {
                slot_amount: 100,
                seconds_elapsed: 100,
                seconds_per_slot: 1.0, // 100 slots / 100000ms = 1.0 slots per second
                threshold: 2f64,
            },
            BlockCheckMode::Replay,
            second_block,
            &node_rpc_url,
        )
    );

    alert_rx
        .try_recv()
        .expect_err("alert received when none expected");

    Ok(())
}

/// Check that slot time alerts are ignored during startup mode.
#[tokio::test(flavor = "multi_thread")]
async fn test_slot_time_alerts_ignored_during_startup() -> anyhow::Result<()> {
    let node_rpc_url = node_rpc_urls().pop().expect("no RPC URL");
    let (alert_tx, mut alert_rx) = alert_channel_only_setup();

    // Create blocks that would normally trigger an alert
    let first_block = mock_block_info(1000, Slot(100));
    let second_block = mock_block_info(2000, Slot(200));

    let mut slot_time_monitor = MemorySlotTimeMonitor::new(SlotTimeMonitorConfig::new(
        2,      // max_block_buffer - small buffer for testing
        0.5f64, // slow_slots_threshold (0.1 < 0.5, so would normally trigger)
        0.1f64, // fast_slots_threshold
        alert_tx.clone(),
    ));

    // Process blocks in Startup mode - should not trigger alerts
    slot_time_monitor
        .process_block(BlockCheckMode::Startup, &first_block, &node_rpc_url)
        .await?;
    slot_time_monitor
        .process_block(BlockCheckMode::Startup, &second_block, &node_rpc_url)
        .await?;

    // No alerts should be received during startup
    alert_rx
        .try_recv()
        .expect_err("alert received during startup when none expected");

    Ok(())
}

/// Check that slot time alerts are not triggered when the buffer is not full.
#[tokio::test(flavor = "multi_thread")]
async fn test_slot_time_alerts_not_triggered_when_buffer_not_full() -> anyhow::Result<()> {
    let node_rpc_url = node_rpc_urls().pop().expect("no RPC URL");
    let (alert_tx, mut alert_rx) = alert_channel_only_setup();

    // Create blocks that would normally trigger an alert
    let first_block = mock_block_info(100000, Slot(100));
    let second_block = mock_block_info(200000, Slot(200));

    let mut slot_time_monitor = MemorySlotTimeMonitor::new(SlotTimeMonitorConfig::new(
        3,      // max_block_buffer - need 3 blocks to fill buffer
        0.5f64, // slow_slots_threshold (1.0 > 0.5, so would trigger if buffer was full)
        0.1f64, // fast_slots_threshold
        alert_tx.clone(),
    ));

    // Process only 2 blocks - buffer not full, so no alerts should be triggered
    slot_time_monitor
        .process_block(BlockCheckMode::Replay, &first_block, &node_rpc_url)
        .await?;
    slot_time_monitor
        .process_block(BlockCheckMode::Replay, &second_block, &node_rpc_url)
        .await?;

    // No alerts should be received when buffer is not full
    alert_rx
        .try_recv()
        .expect_err("alert received when buffer not full");

    Ok(())
}
