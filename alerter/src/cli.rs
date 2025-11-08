use clap::Parser;
use humantime::Duration;

/// Cli config for alerter.
#[derive(Debug, Parser)]
pub(crate) struct Cli {
    /// Node RPC Url.
    #[arg(long, required = true)]
    pub(crate) rpc_url: String,
    #[clap(flatten)]
    pub(crate) uptimekuma: UptimekumaCli,
    /// Time interval to push alerts if no blocks are imported.
    #[arg(long, default_value = "60s")]
    pub(crate) non_block_import_threshold: Duration,
}

/// Cli config for uptimekuma.
#[derive(Debug, Parser)]
pub(crate) struct UptimekumaCli {
    /// Uptimekuma url.
    #[arg(long)]
    pub(crate) uptimekuma_url: Option<String>,
    /// Time interval to push health check.
    #[arg(long, default_value = "60s")]
    pub(crate) uptimekuma_interval: Duration,
}
