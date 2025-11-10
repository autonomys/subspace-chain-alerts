//! Module to monitor AI3 transfers and other events

use crate::Account;
use crate::error::Error;
use crate::event_types::{BalanceTransferEvent, FarmerVote};
use crate::subspace::{BlocksStream, NetworkDetails};
use log::info;

pub(crate) async fn watch_events(
    mut stream: BlocksStream,
    _network_details: NetworkDetails,
    _accounts: Vec<Account>,
) -> Result<(), Error> {
    info!("Watching block events...");

    loop {
        let blocks_ext = stream.recv().await?;
        for block in blocks_ext.blocks {
            let events = block.events().await?;
            let _transfer_events = events
                .find::<BalanceTransferEvent>()
                .try_collect::<Vec<_>>()?;
            let _farmer_votes = events.find::<FarmerVote>().try_collect::<Vec<_>>()?;
        }
    }
}
