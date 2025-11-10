#![feature(iterator_try_collect)]
#![allow(clippy::result_large_err)]
// TODO: remove once connected
#![allow(dead_code)]
extern crate core;

mod blocks;
mod cli;
mod error;
mod slots;
mod stall_and_reorg;
mod uptime;

use crate::blocks::Blocks;
use crate::cli::Config;
use crate::error::Error;
use crate::uptime::push_uptime_status;
use clap::Parser;
use env_logger::{Builder, Env, Target};
use std::io::Write;
use tokio::task::JoinSet;

/// Initiate logger with either RUST_LOG or default to info
pub fn init_logger() {
    let env = Env::default().default_filter_or("info");
    Builder::from_env(env)
        .format(move |buf, record| {
            let style = buf.default_level_style(record.level());
            let timestamp = buf.timestamp();
            writeln!(
                buf,
                "[{timestamp} {style}{}{style:#} {}] {}",
                record.level(),
                record.target(),
                record.args()
            )
        })
        .target(Target::Stdout)
        .init();
}

#[tokio::main]
async fn main() -> Result<(), Error> {
    init_logger();
    let cli = Config::parse();
    let blocks = Blocks::new_from_url(&cli.rpc_url).await?;

    let mut join_set = JoinSet::default();
    let updater = blocks.runtime_metadata_updater();
    join_set.spawn(async move { updater.perform_runtime_updates().await.map_err(Into::into) });

    if let Some(uptimekuma_url) = cli.uptimekuma.uptimekuma_url {
        join_set.spawn(push_uptime_status(
            uptimekuma_url,
            cli.uptimekuma.uptimekuma_interval,
        ));
    }

    // monitor chain stall
    join_set.spawn({
        let stream = blocks.blocks_stream();
        async move {
            stall_and_reorg::watch_chain_stall_and_reorg(
                stream,
                cli.stall_and_reorg,
            )
            .await
        }
    });

    // monitor slot times
    join_set.spawn({
        let stream = blocks.blocks_stream();
        async move { slots::monitor_chain_slots(stream, cli.slot).await }
    });

    join_set.spawn(async move { blocks.listen_for_all_blocks().await });

    // no task in the join set should exit
    // if exits, it is a failure
    if let Some(update) = join_set.join_next().await {
        update??;
    }

    Ok(())
}
