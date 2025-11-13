//! Monitoring and alerting for chain stalls and re-orgs.

use crate::cli::StallAndReorgConfig;
use crate::error::Error;
use crate::slack::{Alert, AlertSink};
use crate::subspace::{Block, BlocksStream, ReorgData};
use humantime::format_duration;
use log::{debug, error, info};
use sp_blockchain::HashAndNumber;
use std::time::Duration;
use tokio::time;

#[derive(Debug)]
pub(crate) struct ChainStall {
    pub(crate) last_block: HashAndNumber<Block>,
    pub(crate) duration: Duration,
}

#[derive(Debug)]
pub(crate) struct ChainRecovery {
    pub(crate) best_block: HashAndNumber<Block>,
    pub(crate) duration: Duration,
}

#[derive(Debug)]
pub(crate) struct ChainReorg {
    pub(crate) best_block: HashAndNumber<Block>,
    pub(crate) common_block: HashAndNumber<Block>,
    pub(crate) enacted: Vec<HashAndNumber<Block>>,
    pub(crate) retracted: Vec<HashAndNumber<Block>>,
}

pub(crate) async fn watch_chain_stall_and_reorg(
    mut stream: BlocksStream,
    config: StallAndReorgConfig,
    alert_sink: AlertSink,
) -> Result<(), Error> {
    info!("🚀 Starting stall and reorg monitor with config {config:?} ...");
    let mut timeout_fired = None;
    let stall_threshold = config.non_block_import_threshold.into();
    let reorg_depth_threshold = config.reorg_depth_threshold;
    let mut maybe_last_best_block = None;
    loop {
        match time::timeout(stall_threshold, stream.recv()).await {
            Ok(blocks_ext) => {
                let blocks_ext = blocks_ext?;
                let last_timeout = timeout_fired.take();
                let latest_block = blocks_ext
                    .blocks
                    .last()
                    .expect("There is always at least one block imported; qed");

                maybe_last_best_block = Some(HashAndNumber {
                    number: latest_block.number,
                    hash: latest_block.hash,
                });

                if let Some(timeout) = last_timeout {
                    info!(
                        "✅ Block production resumed: {}[{}] after: {} ⏱️",
                        latest_block.number,
                        latest_block.hash,
                        format_duration(timeout)
                    );

                    let alert = Alert::Recovery(ChainRecovery {
                        best_block: maybe_last_best_block
                            .clone()
                            .expect("There is always at least one block imported; qed"),
                        duration: timeout,
                    });

                    if let Err(err) = alert_sink.send(alert) {
                        error!("⛔️ failed to send Block production recovery alert: {err}");
                    }
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

                    if retracted.len() >= reorg_depth_threshold {
                        info!("⚠️ Reorg threshold breach: {}", retracted.len());
                        let alert = Alert::Reorg(ChainReorg {
                            best_block: maybe_last_best_block
                                .clone()
                                .expect("There is always at least one block imported; qed"),
                            common_block,
                            enacted,
                            retracted,
                        });

                        if let Err(err) = alert_sink.send(alert) {
                            error!("⛔️ failed to send Block reorg alert: {err}");
                        }
                    }
                }
            }
            Err(_) => {
                let non_import_duration = timeout_fired
                    .map(|timeout| timeout.saturating_add(stall_threshold))
                    .unwrap_or(stall_threshold);
                error!(
                    "⛔️ Chain stalled! No block imported in last {} 🕒",
                    format_duration(non_import_duration)
                );
                if let Some(last_best_block) = maybe_last_best_block.clone() {
                    let alert = Alert::Stall(ChainStall {
                        last_block: last_best_block,
                        duration: non_import_duration,
                    });
                    if let Err(err) = alert_sink.send(alert) {
                        error!("⛔️ failed to send chain stall alert: {err}");
                    }
                }

                timeout_fired = Some(non_import_duration);
            }
        }
    }
}
