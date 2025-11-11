//! Module to monitor AI3 transfers and other events

use crate::Account;
use crate::error::Error;
use crate::event_types::{
    BalanceDeposit, BalanceTransfer, BalanceWithdraw, CodeUpdated, DomainInstantiated,
    DomainRuntimeUpgraded, Event, FraudProofProcessed, OperatorOffline, OperatorSlashed, Sudo,
    TransferDirection, TransferEvent, TransferKnownAccountEvent,
};
use crate::subspace::{AccountId, BlocksStream};
use log::{debug, info};
use std::collections::BTreeMap;
use std::str::FromStr;
use subxt::events::Events;
use subxt_core::config::SubstrateConfig;
use subxt_core::events::StaticEvent;

pub(crate) async fn watch_events(
    mut stream: BlocksStream,
    accounts: Vec<Account>,
) -> Result<(), Error> {
    info!("Watching block events...");
    let accounts = account_mapped_name(accounts);
    loop {
        let blocks_ext = stream.recv().await?;
        for block in blocks_ext.blocks {
            let block_events = block.events().await?;
            let mut events: Vec<Event> = vec![];
            let mut transfers = filter_known_account_transfers(
                block_events
                    .find::<BalanceTransfer>()
                    .try_collect::<Vec<_>>()?,
                &accounts,
            );
            transfers.extend(filter_known_account_transfers(
                block_events
                    .find::<BalanceDeposit>()
                    .try_collect::<Vec<_>>()?,
                &accounts,
            ));
            transfers.extend(filter_known_account_transfers(
                block_events
                    .find::<BalanceWithdraw>()
                    .try_collect::<Vec<_>>()?,
                &accounts,
            ));

            events.extend(transfers.into_iter().map(Into::into).collect::<Vec<_>>());
            events.extend(as_events::<DomainRuntimeUpgraded>(&block_events)?);
            events.extend(as_events::<DomainInstantiated>(&block_events)?);
            events.extend(as_events::<FraudProofProcessed>(&block_events)?);
            events.extend(as_events::<OperatorSlashed>(&block_events)?);
            events.extend(as_events::<OperatorOffline>(&block_events)?);
            events.extend(as_events::<Sudo>(&block_events)?);
            events.extend(as_events::<CodeUpdated>(&block_events)?);
            // TODO: send slack alert
            debug!(
                "Found {} events in block {}[{}]",
                events.len(),
                block.number,
                block.hash
            );
        }
    }
}

fn as_events<E: StaticEvent + Into<Event>>(
    block_events: &Events<SubstrateConfig>,
) -> Result<Vec<Event>, Error> {
    Ok(block_events
        .find::<E>()
        .try_collect::<Vec<_>>()?
        .into_iter()
        .map(Into::into)
        .collect())
}

fn account_mapped_name(accounts: Vec<Account>) -> BTreeMap<AccountId, Account> {
    accounts
        .into_iter()
        .map(|account| {
            (
                AccountId::from_str(&account.address).expect("Must be a valid SS58 address"),
                account,
            )
        })
        .collect()
}

fn filter_known_account_transfers<T: TransferEvent>(
    events: Vec<T>,
    accounts: &BTreeMap<AccountId, Account>,
) -> Vec<TransferKnownAccountEvent> {
    events
        .into_iter()
        .filter_map(|event| {
            let maybe_from = event.from();
            let maybe_to = event.to();
            let amount = event.amount();
            if let Some(from) = maybe_from
                && accounts.contains_key(&from)
            {
                let account = accounts.get(&from).expect("checked above; qed").clone();
                Some(TransferKnownAccountEvent {
                    direction: TransferDirection::Sender,
                    transfer_type: event.transfer_type(),
                    name: account.name,
                    address: account.address,
                    amount,
                })
            } else if let Some(to) = maybe_to
                && accounts.contains_key(&to)
            {
                let account = accounts.get(&to).expect("checked above; qed").clone();
                Some(TransferKnownAccountEvent {
                    direction: TransferDirection::Receiver,
                    transfer_type: event.transfer_type(),
                    name: account.name,
                    address: account.address,
                    amount,
                })
            } else {
                None
            }
        })
        .collect()
}
