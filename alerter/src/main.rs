#![feature(iterator_try_collect)]
#![allow(clippy::result_large_err)]
// TODO: remove once connected
#![allow(dead_code)]

mod cli;
mod error;
mod event_types;
mod events;
mod md_format;
mod slack;
mod slots;
mod stall_and_reorg;
mod subspace;
mod uptime;

use crate::cli::Config;
use crate::error::Error;
use crate::md_format::FormatConfig;
use crate::slack::SlackAlerter;
use crate::subspace::Subspace;
use crate::uptime::push_uptime_status;
use clap::Parser;
use env_logger::{Builder, Env, Target};
use log::info;
use serde::Deserialize;
use sp_runtime::app_crypto::sp_core::crypto::set_default_ss58_version;
use std::collections::BTreeMap;
use std::io::Write;
use std::{env, fs};
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

#[derive(Debug, Deserialize, Clone)]
pub(crate) struct Account {
    pub(crate) name: String,
    pub(crate) address: String,
}

#[derive(Debug, Deserialize, Clone)]
pub(crate) struct Network {
    pub(crate) accounts: Vec<Account>,
}

#[derive(Debug, Deserialize, Clone)]
struct Networks {
    networks: BTreeMap<String, Network>,
}

fn load_networks() -> Result<Networks, Error> {
    let path = match env::var("CARGO_MANIFEST_DIR") {
        Ok(base_dir) => format!("{base_dir}/networks.toml"),
        Err(_) => "config.toml".to_string(),
    };
    info!("Loading network configuration from `{path}`",);
    let config = fs::read_to_string(path)?;
    let networks = toml::from_str(config.as_str())?;
    Ok(networks)
}

#[tokio::main]
async fn main() -> Result<(), Error> {
    init_logger();
    let cli = Config::parse();
    let subspace = Subspace::new_from_url(&cli.rpc_url).await?;
    let network_details = subspace.network_details().await?;
    set_default_ss58_version(network_details.ss58_format);
    info!("Detected network: {}", network_details.name);
    let networks = load_networks()?;
    let network_config = networks
        .networks
        .get(&network_details.name)
        .cloned()
        .ok_or(Error::Config(format!(
            "Missing network config: {}",
            network_details.name
        )))?;

    let mut join_set = JoinSet::default();
    let updater = subspace.runtime_metadata_updater();
    join_set.spawn(async move { updater.perform_runtime_updates().await.map_err(Into::into) });

    let mut slack = SlackAlerter::new(cli.slack).await?;

    if let Some(uptimekuma_url) = cli.uptimekuma.uptimekuma_url {
        join_set.spawn(push_uptime_status(
            uptimekuma_url,
            cli.uptimekuma.uptimekuma_interval,
        ));
    }

    // monitor chain stall
    join_set.spawn({
        let stream = subspace.blocks_stream();
        let alert_sink = slack.sink();
        async move {
            stall_and_reorg::watch_chain_stall_and_reorg(stream, cli.stall_and_reorg, alert_sink)
                .await
        }
    });

    // monitor slot times
    join_set.spawn({
        let stream = subspace.blocks_stream();
        let alert_sink = slack.sink();
        async move { slots::monitor_chain_slots(stream, alert_sink, cli.slot).await }
    });

    // monitor ai3 transfers
    join_set.spawn({
        let stream = subspace.blocks_stream();
        let alert_sink = slack.sink();
        async move { events::watch_events(stream, alert_sink, network_config.accounts).await }
    });

    // start slack alerter
    join_set.spawn({
        let format_config = FormatConfig {
            rpc_url: cli.rpc_url,
            token_name: network_details.token_symbol,
            token_decimals: network_details.token_decimals,
        };
        async move { slack.run(format_config).await }
    });

    join_set.spawn(async move { subspace.listen_for_all_blocks().await });

    // no task in the join set should exit
    // if exits, it is a failure
    if let Some(update) = join_set.join_next().await {
        update??;
    }

    Ok(())
}
