//! Slack connection and message code.

use crate::subspace::BlockInfo;
use hyper_rustls::HttpsConnector;
use hyper_util::client::legacy::connect::HttpConnector;
use slack_morphism::api::{
    SlackApiChatPostMessageRequest, SlackApiChatPostMessageResponse,
    SlackApiConversationsListRequest,
};
use slack_morphism::prelude::{
    SlackApiRateControlConfig, SlackApiResponseScrollerExt, SlackClientHyperConnector,
};
use slack_morphism::{
    SlackApiScrollableRequest, SlackApiToken, SlackChannelId, SlackChannelInfo, SlackClient,
    SlackClientSession, SlackMessageContent,
};
use std::io;
use std::ops::Deref;
use std::time::Duration;
use tokio::fs;
use tracing::info;
use zeroize::{Zeroize, ZeroizeOnDrop};

/// The path of the file that stores the Slack OAuth secret.
pub const SLACK_OAUTH_SECRET_PATH: &str = "slack-secret";

/// The Slack channel to post alerts to.
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

/// The maximum number of retries for Slack API requests.
/// We set this quite high, so important messages aren't lost due to rate limits.
const MAX_SLACK_API_RETRIES: usize = 30;

/// The Autonomys Slack Team ID. This is not a secret.
/// TODO: if we ever operate the bot in multiple workspaces, make this a configurable env/CLI
/// parameter
const AUTONOMYS_TEAM_ID: &str = "T03LJ85UR5G";

/// The connector to use for the Slack client.
/// TODO: type-erase or generic this if possible/needed
pub type SlackConnector = SlackClientHyperConnector<HttpsConnector<HttpConnector>>;

/// A Slack Client with the info it needs to post to Slack as the chain alerts bot.
#[derive(Debug)]
pub struct SlackClientInfo {
    /// The Slack HTTPS client, used to create new Slack sessions.
    client: SlackClient<SlackConnector>,

    /// The channel ID to post to.
    /// Some Slack APIs accept channel names and IDs, but some only accept IDs, so we convert this
    /// to an ID at startup.
    ///
    /// TODO: add test and prod channel IDs in a hashmap
    pub channel_id: SlackChannelId,

    /// The secret required to post to Slack.
    secret: SlackSecret,
}

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
    async fn new(path: &str) -> Result<Self, io::Error> {
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
            if secret_perms & 0o777 != USER_READ_ONLY && secret_perms & 0o777 != USER_READ_WRITE {
                panic!(
                    "slack-oauth-secret must be readable only by the user running this process \
                    ({USER_READ_ONLY:?} or {USER_READ_WRITE:?}), but it is {secret_perms:?}"
                );
            }
        }

        let secret = fs::read_to_string(path).await?;

        Ok(Self(
            SlackApiToken::new(secret.into()).with_team_id(AUTONOMYS_TEAM_ID.into()),
        ))
    }
}

impl SlackClientInfo {
    /// Load the Slack OAuth secret from a file, and find the channel ID for the test channel.
    pub async fn new(path: &str) -> Result<Self, anyhow::Error> {
        let secret = SlackSecret::new(path).await?;

        info!("setting up Slack client...");
        let client = SlackClient::new(SlackClientHyperConnector::new()?.with_rate_control(
            SlackApiRateControlConfig::new().with_max_retries(MAX_SLACK_API_RETRIES),
        ));
        let session = client.open_session(&secret);
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

        Ok(Self {
            client,
            channel_id,
            secret,
        })
    }

    /// Post a message to Slack.
    /// TODO:
    /// - take a channel here and look up the channel ID
    /// - split channel ID lookups into their own function
    /// - spawn this to a background task, so that any retries don't block the main task.
    pub async fn post_message(
        &self,
        message: impl AsRef<str>,
        block_info: &BlockInfo,
    ) -> Result<SlackApiChatPostMessageResponse, anyhow::Error> {
        let slack_session = self.open_session().await?;

        let message = format!(
            "{}\n\n\
            {block_info}",
            message.as_ref(),
        );
        info!(
            "posting message to '{TEST_CHANNEL_NAME}' channel id: {:?}...\n\
            {message}",
            self.channel_id,
        );
        let post_chat_req = SlackApiChatPostMessageRequest::new(
            self.channel_id.clone(),
            SlackMessageContent::new().with_text(message),
        )
        .with_icon_emoji(ALERTS_BOT_ICON_EMOJI.into())
        .with_username(ALERTS_BOT_NAME.into());
        let post_chat_resp = slack_session.chat_post_message(&post_chat_req).await?;

        info!("message posted: {post_chat_req:?} response: {post_chat_resp:?}");

        Ok(post_chat_resp)
    }

    /// Open a new Slack session.
    async fn open_session<'this, 'session>(
        &'this self,
    ) -> Result<SlackClientSession<'session, SlackConnector>, anyhow::Error>
    where
        'this: 'session,
    {
        let session = self.client.open_session(&self.secret);
        Ok(session)
    }
}
