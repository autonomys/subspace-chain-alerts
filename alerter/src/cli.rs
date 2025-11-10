use clap::Parser;
use humantime::Duration;
use rust_decimal::Decimal;

/// Cli config for alerter.
#[derive(Debug, Parser)]
pub(crate) struct Config {
    /// Node RPC Url.
    #[arg(long, required = true)]
    pub(crate) rpc_url: String,
    #[clap(flatten)]
    pub(crate) uptimekuma: UptimekumaConfig,
    #[clap(flatten)]
    pub(crate) stall_and_reorg: StallAndReorgConfig,
    #[clap(flatten)]
    pub(crate) slot: SlotConfig,
}

/// Cli config for uptimekuma.
#[derive(Debug, Parser)]
pub(crate) struct UptimekumaConfig {
    /// Uptimekuma url.
    #[arg(long)]
    pub(crate) uptimekuma_url: Option<String>,
    /// Time interval to push health check.
    #[arg(long, default_value = "60s")]
    pub(crate) uptimekuma_interval: Duration,
}

/// Cli config for Chain stall and re-orgs.
#[derive(Debug, Parser)]
pub(crate) struct StallAndReorgConfig {
    /// Time interval to push alerts if no blocks are imported.
    #[arg(long, default_value = "60s")]
    pub(crate) non_block_import_threshold: Duration,
    /// Reorg depth threshold
    #[arg(long, default_value = "6")]
    pub(crate) reorg_depth_threshold: usize,
}

/// Cli config for block slots.
#[derive(Debug, Parser)]
pub(crate) struct SlotConfig {
    /// Fast slot time threshold.
    #[arg(long, default_value = "0.93")]
    pub(crate) fast_slot_threshold: Decimal,
    /// Slow slot time threshold.
    #[arg(long, default_value = "1.10")]
    pub(crate) slow_slot_threshold: Decimal,
}
