//! Markdown format

use crate::event_types::{Event, TransferKnownAccountEvent};
use crate::slack::Alert;
use crate::slots::{SlotAndTimestamp, SlowSlot};
use crate::stall_and_reorg::{ChainRecovery, ChainReorg, ChainStall};
use crate::subspace::{Balance, Block};
use chrono::DateTime;
use humantime::format_duration;
use rust_decimal::Decimal;
use sp_blockchain::HashAndNumber;

/// Config for Slack formatter
pub(crate) struct FormatConfig {
    pub(crate) rpc_url: String,
    pub(crate) token_name: String,
    pub(crate) token_decimals: u8,
}

/// Markdown formatter
pub(crate) struct MdFormat(FormatConfig);

impl MdFormat {
    pub(crate) fn new(config: FormatConfig) -> Self {
        Self(config)
    }

    pub(crate) fn format_alert(&self, alert: Alert) -> String {
        match alert {
            Alert::Event(event) => self.format_event(event),
            Alert::Stall(chain_stall) => self.format_chain_stall(chain_stall),
            Alert::Recovery(recovery) => self.format_recovery(recovery),
            Alert::Reorg(reorg) => self.format_reorg(reorg),
            Alert::SlowSlot(slow_slot) => self.format_slow_slot(slow_slot),
        }
    }

    fn format_event(&self, event: Event) -> String {
        match event {
            Event::Transfer(transfer) => self.format_transfer(transfer),
            Event::DomainRuntimeUpgraded(e) => {
                format!("**Domain runtime upgraded**\nRuntime ID: {}", e.runtime_id)
            }
            Event::DomainInstantiated(e) => {
                format!("**Domain instantiated**\nDomain ID: {:?}", e.domain_id)
            }
            Event::FraudProofProcessed(e) => match e.new_head_receipt_number {
                Some(block_number) => {
                    format!(
                        "**Fraud proof processed**\nDomain ID: {:?}\nNew head receipt number: {}",
                        e.domain_id, block_number
                    )
                }
                None => {
                    format!("**Fraud proof processed**\nDomain ID: {:?}", e.domain_id)
                }
            },
            Event::OperatorSlashed(e) => {
                let reason = match e.reason {
                    crate::event_types::SlashedReason::InvalidBundle(n) => {
                        format!("Invalid bundle at block: {n}")
                    }
                    crate::event_types::SlashedReason::BadExecutionReceipt(h) => {
                        format!("Bad execution receipt: 0x{:?}", hex::encode(h))
                    }
                };
                format!(
                    "**Operator slashed**\nOperator ID: {}\nReason: {reason}",
                    e.operator_id
                )
            }
            Event::OperatorOffline(e) => format!(
                "**Operator offline**\nOperator ID: {}\nDomain ID: {:?}\nSubmitted bundles: {}\nExpected bundles: {}\nMin required bundles: {}",
                e.operator_id,
                e.domain_id,
                e.submitted_bundles,
                e.expectations.expected_bundles,
                e.expectations.min_required_bundles
            ),
            Event::Sudo(_) => "**Sudo event triggered**".to_string(),
            Event::CodeUpdated(_) => "**Runtime code updated**".to_string(),
        }
    }

    fn format_transfer(&self, transfer: TransferKnownAccountEvent) -> String {
        let TransferKnownAccountEvent {
            direction,
            transfer_type,
            name,
            address,
            amount,
        } = transfer;
        format!(
            "**Balance transfer**\nDirection: {direction:?}\nType: {transfer_type:?}\nAccount: {name}[{address}]\nAmount:{}",
            self.format_balance(amount)
        )
    }

    fn format_balance(&self, balance: Balance) -> String {
        let decimals = self.0.token_decimals;
        let token_name = &self.0.token_name;
        let scaled_balance = Decimal::from(balance) / Decimal::from(10u128.pow(decimals as u32));
        format!("{scaled_balance} {token_name}")
    }

    fn format_recovery(&self, recovery: ChainRecovery) -> String {
        let ChainRecovery {
            best_block,
            duration,
        } = recovery;
        format!(
            "**Block production resumed**\nBest block: {}\nResumed after: {}",
            self.format_hash_and_number(best_block),
            format_duration(duration)
        )
    }

    fn format_slow_slot(&self, slow_slot: SlowSlot) -> String {
        let SlowSlot {
            slot_and_timestamp,
            seconds_per_slot,
            block,
            slots_produced,
        } = slow_slot;
        let SlotAndTimestamp { slot, timestamp } = slot_and_timestamp;
        let timestamp = DateTime::from_timestamp_millis(timestamp as i64)
            .expect("Is always a valid block timestamp");
        format!(
            "**Slow slot**\nSeconds per slot: {}s\nSlot: {slot}\nBlock: {}\nBlock time: {}\nSlots produced: {}",
            seconds_per_slot,
            self.format_hash_and_number(block),
            timestamp.to_rfc3339(),
            slots_produced
        )
    }

    fn format_reorg(&self, reorg: ChainReorg) -> String {
        let ChainReorg {
            best_block,
            common_block,
            enacted,
            retracted,
        } = reorg;

        let reorg_depth = retracted.len();
        format!(
            "**Chain reorg**\nBest block: {}\nCommon block: {}\nRetracted blocks:\n{}\nEnacted blocks:\n{}\nReorg Depth: {reorg_depth}",
            self.format_hash_and_number(best_block),
            self.format_hash_and_number(common_block),
            self.format_hash_and_number_list(retracted),
            self.format_hash_and_number_list(enacted),
        )
    }

    fn format_chain_stall(&self, chain_stall: ChainStall) -> String {
        let ChainStall {
            last_block,
            duration,
        } = chain_stall;

        format!(
            "**Block production stalled**\nLast block: {}\nTime since last block: {}",
            self.format_hash_and_number(last_block),
            format_duration(duration)
        )
    }

    fn format_hash_and_number_list(
        &self,
        hash_and_number_list: Vec<HashAndNumber<Block>>,
    ) -> String {
        hash_and_number_list
            .iter()
            .map(|block| format!("- {}", self.format_hash_and_number(block.clone())))
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn format_hash_and_number(&self, hash_and_number: HashAndNumber<Block>) -> String {
        let HashAndNumber { number, hash } = hash_and_number;
        let link = format!(
            "https://polkadot.js.org/apps/?rpc={}#/explorer/query/0x{}",
            urlencoding::encode(&self.0.rpc_url),
            hex::encode(hash)
        );
        format!("[{hash}]({link}) ({number})",)
    }
}
