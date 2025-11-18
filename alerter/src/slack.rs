//! Slack integration to send alerts

use crate::cli::SlackConfig;
use crate::error::Error;
use crate::event_types::Event;
use crate::md_format::{FormatConfig, MdFormat};
use crate::slots::{AvgSlowSlot, SlowSlot, TimekeeperRecovery, TimekeeperStall};
use crate::stall_and_reorg::{ChainRecovery, ChainReorg, ChainStall};
use log::{debug, error, info};
use slack_morphism::api::SlackApiChatPostMessageRequest;
use slack_morphism::blocks::{SlackBlock, SlackMarkdownBlock};
use slack_morphism::hyper_tokio::SlackClientHyperConnector;
use slack_morphism::prelude::SlackApiRateControlConfig;
use slack_morphism::{SlackApiToken, SlackClient, SlackMessageContent};
use sp_runtime::app_crypto::sp_core::crypto::Zeroize;
use std::ops::Deref;
use tokio::fs;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};
use zeroize::ZeroizeOnDrop;

/// Slack alert message
#[derive(Debug)]
pub(crate) enum Alert {
    Event(Event),
    ChainStall(ChainStall),
    ChainRecovery(ChainRecovery),
    Reorg(ChainReorg),
    TimekeeperStall(TimekeeperStall),
    TimekeeperRecovery(TimekeeperRecovery),
    SlowSlot(SlowSlot),
    AvgSlowSlots(AvgSlowSlot),
}

type AlertStream = UnboundedReceiver<Alert>;

/// Sink channel for sending alerts.
pub(crate) type AlertSink = UnboundedSender<Alert>;

/// The maximum number of retries for Slack API requests.
/// We set this quite high, so important messages aren't lost due to rate limits.
const MAX_SLACK_API_RETRIES: usize = 30;

/// A secret used to post to Slack as the chain alerts bot.
#[derive(Clone, Debug, PartialEq, Eq)]
struct SlackSecret(SlackApiToken);

impl Deref for SlackSecret {
    type Target = SlackApiToken;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

// Unfortunately, upstream does not implement Zeroize for SlackApiToken, so we have to do it
// ourselves.
impl Zeroize for SlackSecret {
    fn zeroize(&mut self) {
        self.0.token_value.0.zeroize();
    }
}

impl Drop for SlackSecret {
    fn drop(&mut self) {
        self.zeroize();
    }
}

impl ZeroizeOnDrop for SlackSecret {}

impl SlackSecret {
    /// Load the Slack OAuth secret from a file, which should only be readable by the user running
    /// this process.
    ///
    /// Any returned errors are fatal and require a restart.
    async fn new(path: &str, team_id: &str) -> Result<Self, Error> {
        // It is not secure to provide secrets on the command line or in environment variables,
        // because those secrets can be visible to other users of the system via `ps` or `top`.

        // Permissions checks are much tricker to do on Windows.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            use tokio::fs;

            const USER_READ_ONLY: u32 = 0o400;
            const USER_READ_WRITE: u32 = 0o600;

            let secret_perms = fs::metadata(path).await?.permissions().mode();
            assert!(
                secret_perms & 0o777 == USER_READ_ONLY || secret_perms & 0o777 == USER_READ_WRITE,
                "{path} must be readable only by the user running this process \
                ({USER_READ_ONLY:?} or {USER_READ_WRITE:?}), but it is {secret_perms:?}"
            );
        }

        let secret = fs::read_to_string(path).await?;
        let secret = secret.trim().to_string();

        if secret.is_empty() {
            return Err(Error::Config("Slack Secret cannot be empty".into()));
        }

        Ok(Self(
            SlackApiToken::new(secret.into()).with_team_id(team_id.into()),
        ))
    }
}

pub(crate) struct SlackAlerter {
    bot_name: String,
    bot_icon: String,
    channel_name: String,
    secret: SlackSecret,
    stream: AlertStream,
    sink: AlertSink,
}

impl SlackAlerter {
    pub(crate) async fn new(config: SlackConfig) -> Result<Self, Error> {
        let (sink, stream) = unbounded_channel();
        let SlackConfig {
            slack_team_id,
            slack_bot_name,
            slack_bot_icon,
            slack_channel_name,
            slack_secret_path,
        } = config;
        let secret = SlackSecret::new(slack_secret_path.as_ref(), &slack_team_id).await?;
        Ok(SlackAlerter {
            bot_name: slack_bot_name,
            bot_icon: slack_bot_icon,
            channel_name: format!(
                "#{}",
                slack_channel_name
                    .strip_prefix("#")
                    .unwrap_or(&slack_channel_name)
            ),
            secret,
            stream,
            sink,
        })
    }

    pub(crate) fn sink(&self) -> AlertSink {
        self.sink.clone()
    }

    pub(crate) async fn run(&mut self, format_config: FormatConfig) -> Result<(), Error> {
        info!("Starting Slack Alerter {}...", self.bot_name);
        let client = SlackClient::new(SlackClientHyperConnector::new()?.with_rate_control(
            SlackApiRateControlConfig::new().with_max_retries(MAX_SLACK_API_RETRIES),
        ));

        let session = client.open_session(&self.secret);
        session.auth_test().await?;
        debug!("Slack session opened successfully.");
        let formatter = MdFormat::new(format_config);

        loop {
            let Some(alert) = self.stream.recv().await else {
                return Err(Error::App("Slack stream closed".into()));
            };

            debug!("Slack alert received: {alert:?}");

            // Format the message as Slack message blocks:
            // <https://api.slack.com/reference/block-kit/blocks>
            let message_blocks: Vec<SlackBlock> =
                vec![SlackMarkdownBlock::new(formatter.format_alert(alert)).into()];

            let post_chat_req = SlackApiChatPostMessageRequest::new(
                self.channel_name.clone().into(),
                SlackMessageContent::new().with_blocks(message_blocks),
            )
            .with_icon_emoji(self.bot_icon.clone())
            .with_username(format!(
                "{}({})",
                self.bot_name.clone(),
                env!("CARGO_PKG_VERSION")
            ))
            .with_unfurl_links(false);

            if let Err(err) = session.chat_post_message(&post_chat_req).await {
                error!("⛔️ failed to send Slack alert: {err}");
            }
        }
    }
}
