//! Slack connection and message code.

use crate::alerts::Alert;
use anyhow::Context;
use hyper_rustls::HttpsConnector;
use hyper_util::client::legacy::connect::HttpConnector;
use slack_morphism::api::{
    SlackApiChatPostMessageRequest, SlackApiChatPostMessageResponse,
    SlackApiConversationsListRequest,
};
use slack_morphism::blocks::{
    SlackBlock, SlackBlockMarkDownText, SlackContextBlock, SlackDividerBlock, SlackMarkdownBlock,
};
use slack_morphism::prelude::{
    SlackApiRateControlConfig, SlackApiResponseScrollerExt, SlackClientHyperConnector,
};
use slack_morphism::{
    SlackApiScrollableRequest, SlackApiToken, SlackChannelId, SlackChannelInfo, SlackClient,
    SlackClientSession, SlackMessageContent,
};
use std::collections::{HashMap, HashSet};
use std::ops::Deref;
use std::time::Duration;
use tokio::fs;
use tokio::time::timeout;
use tracing::{info, trace};
use zeroize::{Zeroize, ZeroizeOnDrop};

/// The path of the file that stores the Slack OAuth secret.
pub const SLACK_OAUTH_SECRET_PATH: &str = "slack-secret";

/// The test Slack channel, used for alert testing, and to post all startup alerts.
const TEST_CHANNEL_NAME: &str = "chain-alerts-test";

/// The Slack channel to post production alerts to.
const PROD_CHANNEL_NAME: &str = "chain-alerts";

/// The name the bot uses to post alerts to Slack.
/// TODO: take "test" out of this name once we're production-ready
const ALERTS_BOT_NAME_SUFFIX: &str = "Chain Alerts Tester";

/// The default emoji the bot uses to post alerts to Slack, if there is no icon provided on the
/// command line, and no flag emoji can be found for the instance's external IP.
const DEFAULT_BOT_ICON: &str = "robot_face";

/// The GeoIP server to use for country flag emoji lookups.
const GEOIP_SERVER: &str = "https://api.geoip.rs";

/// The timeout for the GeoIP server.
const GEOIP_TIMEOUT: Duration = Duration::from_secs(10);

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

    /// The channel ID to post startup alerts to. This is always the test channel, because startup
    /// alerts aren't actionable.
    ///
    /// Some Slack APIs accept channel names and IDs, but some only accept IDs, so we convert this
    /// to an ID at startup.
    pub startup_channel_id: SlackChannelId,

    /// The channel ID to post alerts to. This is the test channel by default, unless the
    /// production flag is set on the command line.
    pub alert_channel_id: SlackChannelId,

    /// The name the bot uses to post alerts to Slack.
    pub bot_name: String,

    /// The emoji the bot uses to post alerts to Slack.
    pub bot_icon: String,

    // Context used by the bot instance.
    /// The IP address and country code used to identify the bot instance.
    pub bot_ip_cc: Option<String>,

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
    ///
    /// Any returned errors are fatal and require a restart.
    async fn new(path: &str) -> anyhow::Result<Self> {
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
            anyhow::bail!("{path} is an empty file (or only contains whitespace)");
        }

        Ok(Self(
            SlackApiToken::new(secret.into()).with_team_id(AUTONOMYS_TEAM_ID.into()),
        ))
    }
}

impl SlackClientInfo {
    /// Load the Slack OAuth secret from a file, and find the channel ID for the test channel.
    /// Keeps track of the supplied bot name and icon for use when posting alerts.
    ///
    /// Any returned errors are fatal and require a restart.
    pub async fn new(
        is_production: bool,
        bot_name: impl AsRef<str>,
        bot_icon: Option<String>,
        secret_path: impl AsRef<str>,
    ) -> Result<Self, anyhow::Error> {
        let secret = SlackSecret::new(secret_path.as_ref()).await?;

        info!("setting up Slack client...");
        let client = SlackClient::new(SlackClientHyperConnector::new()?.with_rate_control(
            SlackApiRateControlConfig::new().with_max_retries(MAX_SLACK_API_RETRIES),
        ));

        let alert_channel_name = if is_production {
            PROD_CHANNEL_NAME
        } else {
            TEST_CHANNEL_NAME
        };
        let channel_names = if is_production {
            HashSet::from([TEST_CHANNEL_NAME, alert_channel_name])
        } else {
            HashSet::from([TEST_CHANNEL_NAME])
        };
        let channel_ids = Self::find_channel_ids(channel_names, &client, &secret).await?;
        info!("channel IDs: {channel_ids:?}");

        let geoip = match Self::find_external_geoip().await {
            Ok(geoip) => geoip,
            Err(e) => {
                info!("error finding instance geoip: {e}");
                (None, None)
            }
        };

        let bot_icon = match bot_icon {
            Some(icon) => icon,
            None => match geoip.1 {
                Some(bot_country_flag) => bot_country_flag,
                None => {
                    info!("no country in geoip response");
                    DEFAULT_BOT_ICON.to_string()
                }
            },
        };

        Ok(Self {
            client,
            startup_channel_id: channel_ids[TEST_CHANNEL_NAME].clone(),
            alert_channel_id: channel_ids[alert_channel_name].clone(),
            bot_name: bot_name.as_ref().to_string(),
            bot_ip_cc: geoip.0,
            bot_icon,
            secret,
        })
    }

    /// Find the channel ID for the supplied channel names.
    ///
    /// Any returned errors are fatal and require a restart.
    async fn find_channel_ids(
        channel_names: HashSet<&str>,
        client: &SlackClient<SlackConnector>,
        secret: &SlackSecret,
    ) -> Result<HashMap<String, SlackChannelId>, anyhow::Error> {
        let session = client.open_session(secret);
        info!("opened Slack session");

        info!("finding channel IDs for '{channel_names:?}'...");
        let list_req = SlackApiConversationsListRequest::new()
            .with_limit(MAX_CHANNEL_LIST_LIMIT)
            .with_exclude_archived(true);
        let list_scroller = list_req.scroller();
        let collected_channels: Vec<SlackChannelInfo> = list_scroller
            .collect_items_stream(&session, REQUEST_THROTTLE)
            .await
            .context("Is the Slack secret correct?")?;
        info!("got {} channels", collected_channels.len());

        let mut channel_ids = HashMap::new();
        for channel_name in channel_names.iter() {
            for channel in collected_channels.iter() {
                if channel.name == Some(channel_name.to_string()) {
                    channel_ids.insert(channel_name.to_string(), channel.id.clone());
                }
            }
        }

        for channel_name in channel_names {
            if !channel_ids.contains_key(channel_name) {
                anyhow::bail!("channel '{channel_name}' ID not found");
            };
        }

        Ok(channel_ids)
    }

    /// Finds the external IP and country flag emoji for this instance.
    ///
    /// GeoIP databases are notoriously unreliable, particularly for data centres, so this flag
    /// could be from a completely different country. There's also no guarantee that Slack has a
    /// flag emoji for the country.
    ///
    /// Returned errors should be ignored, because this is an optional feature with a default
    /// fallback.
    async fn find_external_geoip() -> Result<(Option<String>, Option<String>), anyhow::Error> {
        info!("finding instance external IP...");
        // Timeout if the server takes too long, showing the bot location is non-essential.
        let geoip_resp = timeout(GEOIP_TIMEOUT, reqwest::get(GEOIP_SERVER)).await??;
        if !geoip_resp.status().is_success() {
            anyhow::bail!(
                "GeoIP server returned non-success status: {}",
                geoip_resp.status()
            );
        }
        let geoip_body = timeout(GEOIP_TIMEOUT, geoip_resp.text()).await??;
        info!("geoip response: {geoip_body}");

        let geoip_json: serde_json::Value = serde_json::from_str(&geoip_body)?;
        let ip = geoip_json["ip_address"].as_str().map(|ip| ip.to_string());
        let country_code = geoip_json["country_code"].as_str().map(|cc| cc.to_string());
        info!("instance external IP: {ip:?}, country code: {country_code:?}");

        let flag_emoji = country_code
            .as_ref()
            .map(|c| format!("flag-{}", c.to_lowercase()));

        let ip_cc = if let (Some(country_code), Some(ip)) = (&country_code, &ip) {
            Some(format!("{country_code} ({ip})"))
        } else {
            country_code.or(ip)
        };

        Ok((ip_cc, flag_emoji))
    }

    /// Post an alert to Slack.
    ///
    /// Any returned errors are fatal and require a restart.
    ///
    /// TODO:
    /// - take a channel here and look up the channel ID
    /// - split channel ID lookups into their own function
    /// - spawn this to a background task, so that any retries don't block the main task.
    pub async fn post_message(
        &self,
        alert: &Alert,
    ) -> Result<SlackApiChatPostMessageResponse, anyhow::Error> {
        let slack_session = self.open_session();

        let node_rpc_urls = alert.node_rpc_urls();
        let Alert {
            alert,
            block_info,
            mode,
            node_rpc_url: _,
        } = alert;

        // Format the message as Slack message blocks:
        // <https://api.slack.com/reference/block-kit/blocks>
        let mut message_blocks: Vec<SlackBlock> = vec![];
        message_blocks.push(SlackMarkdownBlock::new(format!("{alert}")).into());
        message_blocks.push(SlackDividerBlock::new().into());

        // "verbatim" is documented as "process markdown correctly", but it actually means "don't
        // process markdown at all" in a context block. So we can't use a context block here.
        // TODO: make the font smaller and text greyer anyway
        let block_context = SlackMarkdownBlock::new(format!("{mode:?} {block_info}"));
        message_blocks.push(block_context.into());

        // Add the alerter location, RPC instance, and version as context.
        let mut context = if let Some(ip_cc) = self.bot_ip_cc.as_ref() {
            format!("🌐 {ip_cc} ")
        } else {
            String::new()
        };

        context.push_str(&format!("📞 {} ", node_rpc_urls.join(", ")));
        // TODO: add git commit hash here
        context.push_str(&format!("🔗 {}", env!("CARGO_PKG_VERSION")));

        let mut context_block = SlackBlockMarkDownText::from(context);
        context_block.verbatim = Some(true);
        message_blocks.push(SlackContextBlock::new(vec![context_block.into()]).into());

        let channel_id = if alert.is_test_alert() {
            info!(
                ?alert,
                ?mode,
                block = ?block_info.position(),
                ?node_rpc_urls,
                is_duplicate = %alert.is_duplicate(),
                channel_id = ?self.startup_channel_id,
                "posting startup message to test channel",
            );

            self.startup_channel_id.clone()
        } else {
            info!(
                ?alert,
                ?mode,
                block = ?block_info.position(),
                ?node_rpc_urls,
                is_duplicate = %alert.is_duplicate(),
                channel_id = ?self.alert_channel_id,
                "posting alert to alerts channel",
            );

            self.alert_channel_id.clone()
        };

        let post_chat_req = SlackApiChatPostMessageRequest::new(
            channel_id,
            SlackMessageContent::new().with_blocks(message_blocks),
        )
        .with_icon_emoji(self.bot_icon.clone())
        .with_username(format!("{} {ALERTS_BOT_NAME_SUFFIX}", self.bot_name));
        let post_chat_resp = slack_session.chat_post_message(&post_chat_req).await?;

        trace!("message posted: {post_chat_req:?} response: {post_chat_resp:?}");

        Ok(post_chat_resp)
    }

    /// Open a new Slack session.
    /// Sessions are cheap, so they don't need to be re-used, and should not be stored.
    fn open_session<'this, 'session>(&'this self) -> SlackClientSession<'session, SlackConnector>
    where
        'this: 'session,
    {
        self.client.open_session(&self.secret)
    }
}
