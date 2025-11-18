use clap::Parser;
use humantime::Duration;

/// Cli config for alerter.
#[derive(Debug, Parser)]
pub(crate) struct Config {
    /// Node RPC Url.
    #[arg(long, required = true)]
    pub(crate) rpc_url: String,
    #[arg(long, default_value = "/networks.toml")]
    pub(crate) network_config_path: String,
    #[clap(flatten)]
    pub(crate) uptimekuma: UptimekumaConfig,
    #[clap(flatten)]
    pub(crate) stall_and_reorg: StallAndReorgConfig,
    #[clap(flatten)]
    pub(crate) slack: SlackConfig,
    #[clap(flatten)]
    pub(crate) slots: SlotsConfig,
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

/// Cli config for slack.
#[derive(Debug, Parser)]
pub(crate) struct SlackConfig {
    #[arg(long, default_value = "T03LJ85UR5G")]
    pub(crate) slack_team_id: String,
    #[arg(long, required = true)]
    pub(crate) slack_bot_name: String,
    #[arg(long, default_value = "robot_face")]
    pub(crate) slack_bot_icon: String,
    #[arg(long, required = true)]
    pub(crate) slack_channel_name: String,
    #[arg(long, default_value = "/slack-secret")]
    pub(crate) slack_secret_path: String,
}

/// Cli config for slots.
#[derive(Debug, Parser)]
pub(crate) struct SlotsConfig {
    /// Per slot threshold
    #[arg(long, default_value = "1.2s")]
    pub(crate) per_slot_threshold: Duration,
    /// Avg slot threshold
    #[arg(long, default_value = "1.1s")]
    pub(crate) avg_slot_threshold: Duration,
}
