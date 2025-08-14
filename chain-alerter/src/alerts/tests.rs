//! Tests for subspace chain alerts.
//!
//! Set the `NODE_URL` env var to the RPC URL of a Subspace node to override the default public
//! instance.

use crate::alerts::{self, AlertKind};
use crate::subspace::tests::{node_rpc_url, test_setup};
use crate::subspace::{BlockInfo, BlockNumber, EventInfo, ExtrinsicInfo};
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

    // TODO: replace this with a subspace test helper function
    let block = subspace_client.blocks().at_latest().await?;
    let extrinsics = block.extrinsics().await?;
    let genesis_hash = subspace_client.genesis_hash();
    let block_info = BlockInfo::new(&block, &extrinsics, &genesis_hash);

    // TODO: extract this into a subspace test helper function
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

    // TODO: extract this into a subspace test helper function
    let block = subspace_client
        .blocks()
        .at(H256::from(SUDO_BLOCK.1))
        .await?;
    let extrinsics = block.extrinsics().await?;
    let events = block.events().await?;
    let genesis_hash = subspace_client.genesis_hash();
    let block_info = BlockInfo::new(&block, &extrinsics, &genesis_hash);

    // TODO: extract this into a subspace test helper function
    let extrinsic = extrinsics
        .iter()
        .nth(SUDO_BLOCK.2.try_into().expect("constant fits in usize"))
        .expect("extrinsic not found");
    let extrinsic_info =
        ExtrinsicInfo::new(&extrinsic, &block_info).expect("extrinsic info invalid");

    alerts::check_extrinsic(&alert_tx, &extrinsic, &block_info).await?;
    let alert = alert_rx.try_recv().expect("no alert received");
    assert_eq!(alert.alert, AlertKind::SudoCall { extrinsic_info });
    assert_eq!(alert.block_info, block_info);

    // TODO: extract this into a subspace test helper function
    let event = events
        .iter()
        .nth(SUDO_BLOCK.3.try_into().expect("constant fits in usize"))
        .expect("event not found")
        .expect("invalid event");
    let event_info = EventInfo::new(&event, &block_info);

    alerts::check_event(&alert_tx, &event, &block_info).await?;
    let alert = alert_rx.try_recv().expect("no alert received");
    assert_eq!(alert.alert, AlertKind::SudoEvent { event_info });
    assert_eq!(alert.block_info, block_info);

    Ok(())
}
