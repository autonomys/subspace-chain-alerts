//! Chain alerter process-specific code.

use slack_morphism::prelude::*;
use std::io;
use std::ops::Deref;
use std::time::Duration;
use subspace_process::{init_logger, shutdown_signal};
use tokio::fs::{metadata, read_to_string};
use tokio::select;
use tracing::{error, info};
use zeroize::{Zeroize, ZeroizeOnDrop};

// TODO: move these to the slack crate

/// The Slack channel to post alerts to.
/// Some APIs accept channel names and IDs, but some only accept IDs, so we convert this to an ID
/// at startup.
///
/// TODO: add the production channel and switch to it after startup on mainnet prod instances
const TEST_CHANNEL_NAME: &str = "chain-alerts-test";

/// The name the bot uses to post alerts to Slack.
/// TODO: make this configurable per instance, default to the instance external IP
const ALERTS_BOT_NAME: &str = "Teor's Chain Alerts Tester";

/// The emoji the bot uses to post alerts to Slack.
/// TODO: make this configurable per instance, default to the external IP country flag
const ALERTS_BOT_ICON_EMOJI: &str = "flag-au";

/// The maximum number of channels to list when searching for the channel ID.
/// The API might return fewer than this number even if there are more channels.
const MAX_CHANNEL_LIST_LIMIT: u16 = 1000;

/// The duration to wait between some repeated API requests.
const REQUEST_THROTTLE: Duration = Duration::from_secs(2);

/// A secret used to post to Slack as the chain alerts bot.
#[derive(Clone)]
struct SlackSecret(SlackApiToken);

impl Deref for SlackSecret {
    type Target = SlackApiToken;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

// Unfortunately, upstream does not implement Zeroize for SlackApiToken, so we have to do it ourselves.
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
    /// Load the Slack OAuth secret from a file, which should only be readable by the user running this process.
    pub async fn new(path: &str) -> Result<Self, io::Error> {
        // It is not secure to provide secrets on the command line or in environment variables,
        // because those secrets can be visible to other users of the system via `ps` or `top`.

        // Permissions checks are much tricker to do on Windows.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;

            const USER_READ_ONLY: u32 = 0o400;
            const USER_READ_WRITE: u32 = 0o600;

            let secret_perms = metadata(path).await?.permissions().mode();
            if secret_perms & 0o777 != USER_READ_ONLY && secret_perms & 0o777 != USER_READ_WRITE {
                panic!(
                    "slack-oauth-secret must be readable only by the user running this process \
                    ({USER_READ_ONLY:?} or {USER_READ_WRITE:?}), but it is {secret_perms:?}"
                );
            }
        }

        let secret = read_to_string(path).await?;

        Ok(Self(SlackApiToken::new(secret.into())))
    }
}

/// Run the chain alerter process.
async fn run() -> anyhow::Result<()> {
    info!("setting up Slack client...");
    let slack_client = SlackClient::new(SlackClientHyperConnector::new()?);
    let slack_secret = SlackSecret::new("slack-secret").await?;
    let session = slack_client.open_session(&slack_secret);
    info!("opened Slack session");

    info!("finding channel ID for '{TEST_CHANNEL_NAME}'...");
    let list_req = SlackApiConversationsListRequest::new()
        .with_limit(MAX_CHANNEL_LIST_LIMIT)
        .with_exclude_archived(true);
    let list_scroller = list_req.scroller();
    let collected_channels: Vec<SlackChannelInfo> = list_scroller
        .collect_items_stream(&session, REQUEST_THROTTLE)
        .await?;
    info!("got {} channels", collected_channels.len());

    let mut channel_id = None;
    for channel in collected_channels {
        if channel.name == Some(TEST_CHANNEL_NAME.into()) {
            channel_id = Some(channel.id);
            break;
        }
    }
    let Some(channel_id) = channel_id else {
        anyhow::bail!("channel '{TEST_CHANNEL_NAME}' ID not found");
    };
    info!("channel ID: {channel_id:?}");

    info!("posting message to '{TEST_CHANNEL_NAME}' channel id: {channel_id:?}...");
    let post_chat_req = SlackApiChatPostMessageRequest::new(
        channel_id,
        SlackMessageContent::new().with_text("Hello from Rust via channel name!".into()),
    )
    .with_icon_emoji(ALERTS_BOT_ICON_EMOJI.into())
    .with_username(ALERTS_BOT_NAME.into());
    let post_chat_resp = session.chat_post_message(&post_chat_req).await?;

    info!("message posted: {post_chat_req:?} response: {post_chat_resp:?}");

    Ok(())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    init_logger();
    let shutdown_fut = shutdown_signal("chain-alerter");

    select! {
        _ = shutdown_fut => {
            info!("chain-alerter exited due to user shutdown");
        }

        result = run() => {
            if let Err(e) = result {
                error!("chain-alerter exited with error: {e}");
            } else {
                info!("chain-alerter exited");
            }
        }
    }

    Ok(())
}
