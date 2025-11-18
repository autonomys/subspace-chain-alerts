//! slot monitor

use crate::cli::SlotsConfig;
use crate::error::Error;
use crate::p2p_network::{GossipProof, PoTStream};
use crate::slack::{Alert, AlertSink};
use crate::subspace::Slot;
use humantime::format_duration;
use log::{debug, error, info};
use sp_runtime::BoundedVec;
use sp_runtime::traits::ConstU32;
use std::ops::Div;
use std::time::{Duration, Instant};
use tokio::time;

#[derive(Debug, Clone)]
struct SlotDetails {
    proof: GossipProof,
    duration: Duration,
}

#[derive(Debug)]
pub(crate) struct TimekeeperStall {
    pub(crate) last_slot: Slot,
    pub(crate) duration: Duration,
}

#[derive(Debug)]
pub(crate) struct TimekeeperRecovery {
    pub(crate) slot: Slot,
    pub(crate) duration: Duration,
}

#[derive(Debug)]
pub(crate) struct SlowSlot {
    pub(crate) slot: Slot,
    pub(crate) previous_slot: Slot,
    pub(crate) slot_time: Duration,
    pub(crate) threshold: Duration,
}

#[derive(Debug)]
pub(crate) struct AvgSlowSlot {
    pub(crate) slot: Slot,
    pub(crate) slot_count: usize,
    pub(crate) avg_slot_time: Duration,
    pub(crate) threshold: Duration,
}

pub(crate) async fn monitor_slots(
    mut slot_stream: PoTStream,
    config: SlotsConfig,
    alert_sink: AlertSink,
) -> Result<(), Error> {
    info!("🚀 Starting slot monitor with config {config:?} ...");
    let slot_timeout = Duration::from_secs(30);
    let mut timeout_fired = None;
    let mut last_instant = Instant::now();
    let mut slots = BoundedVec::<SlotDetails, ConstU32<600>>::new();
    loop {
        match time::timeout(slot_timeout, slot_stream.recv()).await {
            Ok(slot) => {
                let slot = slot?;
                debug!("Received slot {}", slot.slot);
                let now = Instant::now();
                let duration = now.duration_since(last_instant);
                let Some(last_slot) = slots.last() else {
                    slots.force_push(SlotDetails {
                        proof: slot,
                        duration,
                    });
                    last_instant = now;
                    continue;
                };

                if slot.slot != last_slot.proof.slot + 1 {
                    continue;
                }

                info!(
                    "✅ Timekeeper chain extended from[{}] to [{}] in {}",
                    last_slot.proof.slot,
                    slot.slot,
                    format_duration(duration)
                );

                let last_timeout = timeout_fired.take();
                if let Some(timeout) = last_timeout {
                    info!(
                        "✅ Timekeeper recovered: slot[{}] after: {} ⏱️",
                        slot.slot,
                        format_duration(timeout)
                    );

                    let alert = Alert::TimekeeperRecovery(TimekeeperRecovery {
                        slot: slot.slot,
                        duration: timeout,
                    });

                    if let Err(err) = alert_sink.send(alert) {
                        error!("⛔️ failed to send Timekeeper recovery alert: {err}");
                    }
                }

                if duration.gt(config.per_slot_threshold.as_ref()) {
                    info!(
                        "⛔️ Per slot threshold breached: Last Slot[{}] New Slot[{}] Time[{}]",
                        last_slot.proof.slot,
                        slot.slot,
                        format_duration(duration)
                    );

                    let alert = Alert::SlowSlot(SlowSlot {
                        slot: slot.slot,
                        previous_slot: last_slot.proof.slot,
                        slot_time: duration,
                        threshold: config.per_slot_threshold.into(),
                    });

                    if let Err(err) = alert_sink.send(alert) {
                        error!("⛔️ failed to send Slow slot alert: {err}");
                    }
                }

                // extension
                let slot_details = SlotDetails {
                    proof: slot,
                    duration,
                };
                last_instant = now;

                slots.force_push(slot_details);
                let length = slots.len();
                let avg_slot_time = slots
                    .iter()
                    .fold(Duration::default(), |acc, x| acc + x.duration)
                    .div(length as u32);
                if slots.is_full() && avg_slot_time.gt(config.avg_slot_threshold.as_ref()) {
                    info!(
                        "⛔️ Average slot threshold breached: Slots: {}: Avg Time: {}: Threshold: {}",
                        length,
                        format_duration(avg_slot_time),
                        config.avg_slot_threshold
                    );

                    let alert = Alert::AvgSlowSlots(AvgSlowSlot {
                        slot: slot.slot,
                        slot_count: length,
                        threshold: config.avg_slot_threshold.into(),
                        avg_slot_time,
                    });

                    if let Err(err) = alert_sink.send(alert) {
                        error!("⛔️ failed to send Avg Slow slot alert: {err}");
                    }
                }
            }
            Err(_) => {
                let non_import_duration = timeout_fired
                    .map(|timeout| timeout.saturating_add(slot_timeout))
                    .unwrap_or(slot_timeout);
                error!(
                    "⛔️ Timekeeper stalled! No slots in last {} 🕒",
                    format_duration(non_import_duration)
                );
                if let Some(last_slot) = slots.last() {
                    let alert = Alert::TimekeeperStall(TimekeeperStall {
                        last_slot: last_slot.proof.slot,
                        duration: non_import_duration,
                    });
                    if let Err(err) = alert_sink.send(alert) {
                        error!("⛔️ failed to send timekeeper stall alert: {err}");
                    }
                }

                timeout_fired = Some(non_import_duration);
            }
        }
    }
}
