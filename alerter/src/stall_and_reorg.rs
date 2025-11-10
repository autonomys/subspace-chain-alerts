//! Monitoring and alerting for chain stalls and re-orgs.
use crate::blocks::{BlocksStream, ReorgData};
use crate::cli::StallAndReorgConfig;
use crate::error::Error;
use humantime::format_duration;
use log::{debug, error, info};
use tokio::time;

pub(crate) async fn watch_chain_stall_and_reorg(
    mut stream: BlocksStream,
    config: StallAndReorgConfig,
) -> Result<(), Error> {
    info!("Starting stall and reorg monitor with config {config:?} ...");
    let mut timeout_fired = None;
    let stall_threshold = config.non_block_import_threshold.into();
    let reorg_depth_threshold = config.reorg_depth_threshold;
    loop {
        match time::timeout(stall_threshold, stream.recv()).await {
            Ok(blocks_ext) => {
                let blocks_ext = blocks_ext?;
                let last_timeout = timeout_fired.take();
                let latest_block = blocks_ext
                    .blocks
                    .last()
                    .expect("There is always at least one block imported; qed");

                if let Some(timeout) = last_timeout {
                    // TODO: send slack message on recovery
                    info!(
                        "Block: {}[{}] imported after: {}",
                        latest_block.number,
                        latest_block.hash,
                        format_duration(timeout)
                    );
                }

                if let Some(reorg_data) = blocks_ext.maybe_reorg_data {
                    let ReorgData {
                        enacted,
                        retracted,
                        common_block,
                    } = reorg_data;

                    debug!("Enacted {enacted:?} retracted {retracted:?}");
                    info!(
                        "🔄 Chain Reorg to: {}[{}]. Common block: {}[{}]. Depth: {}",
                        latest_block.number,
                        latest_block.hash,
                        common_block.number,
                        common_block.hash,
                        retracted.len()
                    );

                    if retracted.len() > reorg_depth_threshold {
                        // TODO: send slack alert for reorg threshold breach
                        info!("Reorg threshold breach: {}", retracted.len());
                    }
                }
            }
            Err(_) => {
                let non_import_duration = timeout_fired
                    .map(|timeout| timeout.saturating_add(stall_threshold))
                    .unwrap_or(stall_threshold);
                // TODO: push slack message on no block imports
                error!(
                    "No block imported in last {}",
                    format_duration(non_import_duration)
                );
                timeout_fired = Some(non_import_duration);
            }
        }
    }
}
