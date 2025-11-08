mod block_importer;
mod cli;
mod error;
mod uptime;

use crate::block_importer::listen_for_all_blocks;
use crate::cli::Cli;
use crate::error::Error;
use crate::uptime::push_uptime_status;
use clap::Parser;
use env_logger::{Builder, Env, Target};
use std::io::Write;
use subxt::backend::legacy::LegacyRpcMethods;
use subxt::ext::subxt_rpcs::RpcClient;
use subxt::{OnlineClient, SubstrateConfig};
use tokio::task::JoinSet;

pub(crate) type SubspaceClient = OnlineClient<SubstrateConfig>;
pub(crate) type SubspaceRpcClient = LegacyRpcMethods<SubstrateConfig>;

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
#[allow(clippy::result_large_err)]
async fn main() -> Result<(), Error> {
    init_logger();
    let cli = Cli::parse();
    let rpc_client = RpcClient::from_url(&cli.rpc_url).await?;
    let rpc = LegacyRpcMethods::<SubstrateConfig>::new(rpc_client.clone());
    let client = SubspaceClient::from_url(&cli.rpc_url).await?;
    let mut join_set = JoinSet::default();

    let updater = client.updater();
    join_set.spawn(async move { updater.perform_runtime_updates().await.map_err(Into::into) });

    if let Some(uptimekuma_url) = cli.uptimekuma.uptimekuma_url {
        join_set.spawn(push_uptime_status(
            uptimekuma_url,
            cli.uptimekuma.uptimekuma_interval,
        ));
    }

    join_set.spawn(listen_for_all_blocks(
        rpc,
        client,
        cli.non_block_import_threshold.into(),
    ));

    // no task in the join set should exit
    // if exits, it is a failure
    if let Some(update) = join_set.join_next().await {
        update??;
    }

    Ok(())
}
