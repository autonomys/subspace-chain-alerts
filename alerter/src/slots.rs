//! Slot time monitoring: derives slot timing from blocks and emits alerts.
//!
//! This module tracks Subspace slot progression and checks whether observed slot timings
//! exceed configured thresholds.

use crate::cli::SlotConfig;
use crate::error::Error;
use crate::subspace::{BlockNumber, BlocksStream, Slot, Timestamp};
use log::{debug, error, info, warn};
use rust_decimal::prelude::FromPrimitive;
use rust_decimal::{Decimal, RoundingStrategy};
use std::collections::BTreeMap;
use std::ops::Div;

/// Cache size of the tracked blocks with slot and timestamp.
const CACHE_SIZE: usize = 100;

type Cache = BTreeMap<BlockNumber, SlotAndTimestamp>;

#[derive(Debug, Clone)]
struct SlotAndTimestamp {
    slot: Slot,
    timestamp: Timestamp,
}

pub(crate) async fn monitor_chain_slots(
    mut stream: BlocksStream,
    config: SlotConfig,
) -> Result<(), Error> {
    info!("Starting slot monitor with config {config:?} ...");
    let mut cache = Cache::default();
    let seconds_in_millis = Decimal::from_u64(1_000).expect("Always a valid conversion");
    // TODO: try average from last 100 blocks.
    let mut maybe_best_block = None;
    loop {
        let blocks_ext = stream.recv().await?;
        if let Some(reorg_data) = blocks_ext.maybe_reorg_data {
            let common_block = reorg_data.common_block.number;
            maybe_best_block = Some(common_block);
            remove_cached_child_blocks(&mut cache, common_block);
        }

        'inner: for block in blocks_ext.blocks.iter() {
            let block_number = block.number;
            let block_hash = block.hash;
            let slot = block.slot().await?;
            let timestamp = block.timestamp().await?;
            cache.insert(block_number, SlotAndTimestamp { slot, timestamp });
            debug!(
                "Storing slot[{slot}] and timestamp[{timestamp}] for Block: {block_number}[{block_hash}]"
            );

            let Some(last_best_block) = maybe_best_block else {
                maybe_best_block = Some(block_number);
                continue 'inner;
            };

            maybe_best_block = Some(block_number);
            let last_block_slot_and_timestamp =
                cache.get(&last_best_block).expect("Should always exist");
            let slot_diff =
                Decimal::from_u64(slot.saturating_sub(last_block_slot_and_timestamp.slot))
                    .expect("Always valid conversion");
            let time_diff = Decimal::from_u64(
                timestamp.saturating_sub(last_block_slot_and_timestamp.timestamp),
            )
            .expect("Always a valid conversion")
            .div(seconds_in_millis);

            if time_diff.is_zero() {
                // should never happen
                error!(
                    "Time difference between Block {last_best_block} and Block {block_number} is zero"
                );
                continue;
            }

            let seconds_per_slot = time_diff
                .div(slot_diff)
                .round_dp_with_strategy(2, RoundingStrategy::ToZero);
            if seconds_per_slot.ge(&config.slow_slot_threshold) {
                // TODO: send slack alert for slow slots
                warn!(
                    "🐢 Slow slot[{slot}[{slot_diff}]] time[{timestamp}[{time_diff}]] for block[{block_number}]: {seconds_per_slot}s",
                )
            } else if seconds_per_slot.le(&config.fast_slot_threshold) {
                // TODO: send slack alert for fast slots
                warn!(
                    "⚡ Fast slot[{slot}[{slot_diff}]] time[{timestamp}[{time_diff}]] for block[{block_number}]: {seconds_per_slot}s",
                )
            } else {
                debug!("Slot[{slot}] time for block[{block_number}]: {seconds_per_slot}s ",)
            }

            // cleanup
            remove_cached_parent_blocks(&mut cache, block_number.saturating_sub(CACHE_SIZE as u32));
        }
    }
}

fn remove_cached_child_blocks(cache: &mut Cache, number: BlockNumber) {
    let mut to_remove = number.saturating_add(1);
    while cache.remove(&to_remove).is_some() {
        to_remove = to_remove.saturating_add(1);
    }
}

fn remove_cached_parent_blocks(cache: &mut Cache, number: BlockNumber) {
    let mut to_remove = number.saturating_sub(1);
    while cache.remove(&to_remove).is_some() {
        to_remove = to_remove.saturating_sub(1);
    }
}
