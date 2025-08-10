//! Chain alerter process-specific code.

use chrono::DateTime;
use hyper_rustls::HttpsConnector;
use hyper_util::client::legacy::connect::HttpConnector;
use scale_value::Composite;
use slack_morphism::prelude::*;
use std::io;
use std::ops::Deref;
use std::time::Duration;
use subspace_process::{AsyncJoinOnDrop, init_logger, shutdown_signal};
use subxt::blocks::{Block, Extrinsics};
use subxt::client::OnlineClientT;
use subxt::utils::H256;
use subxt::{OnlineClient, SubstrateConfig};
use tokio::fs::{metadata, read_to_string};
use tokio::select;
use tracing::{error, info, warn};
use zeroize::{Zeroize, ZeroizeOnDrop};

// TODO: move these to the slack crate

/// The path of the file that stores the Slack OAuth secret.
const SLACK_OAUTH_SECRET_PATH: &str = "slack-secret";

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

/// The maximum length for debug-formatted extrinsic fields.
/// This includes whitespace indentation.
const MAX_EXTRINSIC_DEBUG_LENGTH: usize = 300;

/// One Subspace Credit, copied from subspace-runtime-primitives.
const AI3: u128 = 10_u128.pow(18);

/// The minimum balance change to alert on.
const MIN_BALANCE_CHANGE: u128 = 1_000_000 * AI3;

/// The connector to use for the Slack client.
/// TODO: type-erase or generic this if possible/needed
pub type SlackConnector = SlackClientHyperConnector<HttpsConnector<HttpConnector>>;

/// The config for basic Subspace block and extrinsic types.
/// TODO: create a custom SubspaceConfig type
pub type SubspaceConfig = SubstrateConfig;

/// A Slack Client with the info it needs to post to Slack as the chain alerts bot.
#[derive(Debug)]
struct SlackClientInfo {
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

    /// Open a new Slack session.
    pub async fn open_session<'this, 'session>(
        &'this self,
    ) -> Result<SlackClientSession<'session, SlackConnector>, anyhow::Error>
    where
        'this: 'session,
    {
        let session = self.client.open_session(&self.secret);
        Ok(session)
    }

    /// Post a message to Slack.
    /// TODO:
    /// - take a channel here and look up the channel ID
    /// - split channel ID lookups into their own function
    /// - spawn this to a background task, so that any retries don't block the main task.
    pub async fn post_message<Client>(
        &self,
        message: &str,
        block: &Block<SubspaceConfig, Client>,
        extrinsics: &Extrinsics<SubspaceConfig, Client>,
        genesis_hash: &H256,
    ) -> Result<SlackApiChatPostMessageResponse, anyhow::Error>
    where
        Client: OnlineClientT<SubspaceConfig>,
    {
        let slack_session = self.open_session().await?;

        let block_height = block.header().number;
        let block_hash = block.hash();

        // TODO:
        // - check its the right extrinsic by checking the metadata is Timestamp Set
        // - proper error handling, log/message with error rather than panic
        let block_time = extrinsics
            .iter()
            .next()
            .expect("timestamp is always the first extrinsic")
            .field_values()?
            .into_values()
            .next()
            .expect("timestamp has exactly one field")
            .as_u128()
            .expect("timestamp is an integer");

        let human_time = DateTime::from_timestamp_millis(block_time as i64);
        let human_time = if let Some(human_time) = human_time {
            human_time.format("%Y-%m-%d %H:%M:%S UTC").to_string()
        } else {
            "invalid timestamp".to_string()
        };

        let message = format!(
            "{message}\n\n\
            Block height: {block_height}\n\
            Block time: {human_time} ({block_time})\n\
            Block hash: {block_hash:?}\n\
            Genesis hash: {genesis_hash}"
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
}

/// Truncate a string to a maximum number of characters, respecting UTF-8 character boundaries.
fn truncate(s: &mut String, max_chars: usize) {
    match s.char_indices().nth(max_chars) {
        None => {}
        Some((idx, _)) => {
            s.truncate(idx);
        }
    }
}

/// Format an amount in AI3, accepting `u128` or `Option<u128>`.
/// If `None`, return "unknown".
fn fmt_amount(val: impl Into<Option<u128>>) -> String {
    if let Some(val) = val.into() {
        format!("{} AI3", val / AI3)
    } else {
        "unknown".to_string()
    }
}

/// Run the chain alerter process.
async fn run() -> anyhow::Result<()> {
    // Avoid a crypto provider conflict: jsonrpsee activates ring, and hyper-rustls activates
    // aws-lc, but there can only be one per process. We use the library with more formal
    // verification.
    //
    // TODO: remove ring to reduce compile time/size
    rustls::crypto::aws_lc_rs::default_provider()
        .install_default()
        .map_err(|_| anyhow::anyhow!("Selecting default TLS crypto provider failed"))?;

    // Connect to Slack and get basic info.
    let slack_client_info = SlackClientInfo::new(SLACK_OAUTH_SECRET_PATH).await?;

    // Create a client that subscribes to a local Substrate node.
    // TODO: make URL configurable
    let chain_client = OnlineClient::<SubspaceConfig>::from_url("ws://127.0.0.1:9944").await?;
    let mut first_block = true;

    info!("spawning runtime metadata update task...");
    // Spawn a background task to keep the runtime metadata up to date.
    // TODO: proper error handling, if an update fails we should restart the process
    // TODO: do we need to abort the process if the update task fails?
    let update_task = chain_client.updater();
    let _update_task = AsyncJoinOnDrop::new(
        tokio::spawn(async move { update_task.perform_runtime_updates().await }),
        true,
    );

    // TODO: add a network name table and look up the network name by genesis hash
    let genesis_hash = chain_client.genesis_hash();

    // Subscribe to best blocks (before they are finalized).
    // TODO: do we need to subscribe to all blocks from all forks here?
    let mut blocks_sub = chain_client.blocks().subscribe_best().await?;

    while let Some(block) = blocks_sub.next().await {
        let block = block?;
        let extrinsics = block.extrinsics().await?;
        let block_height = block.header().number;
        let block_hash = block.hash();

        if first_block {
            // TODO: always post this to the test channel, because it's not an alert.
            slack_client_info
                .post_message(
                    "Launched and connected to the local node",
                    &block,
                    &extrinsics,
                    &genesis_hash,
                )
                .await?;
            first_block = false;
        }

        // Extrinsic parsing should never fail, if it does, the runtime metdata is likely wrong.
        // But we don't want to panic or exit when that happens, instead we warn, and hope to
        // recover after we pick up the runtime upgrade in the next block.
        for extrinsic in extrinsics.iter() {
            let Ok(meta) = extrinsic.extrinsic_metadata() else {
                // If we can't get the extrinsic pallet and call name, there's nothing we can do.
                // Just log it and move on.
                warn!(
                    "extrinsic {} pallet/name unavailable in block {block_height} {block_hash:?}",
                    extrinsic.index(),
                );
                continue;
            };

            // We can always hex-print the extrinsic bytes.
            let bytes = {
                let mut bytes = hex::encode(extrinsic.bytes());
                truncate(&mut bytes, MAX_EXTRINSIC_DEBUG_LENGTH);
                bytes
            };

            // We can usually get the extrinsic fields, but we don't need the fields for some
            // extrinsic alerts. So we just warn and substitute empty fields.
            let fields = extrinsic.field_values().unwrap_or_else(|_| {
                warn!(
                    "extrinsic {} fields unavailable in block {block_height} {block_hash:?}\n\
                    {bytes}",
                    extrinsic.index(),
                );
                Composite::unnamed(Vec::new())
            });
            let fields_str = {
                // The decoded value debug format is extremely verbose, display seems a bit better.
                let mut fields_str = format!("{fields}");
                truncate(&mut fields_str, MAX_EXTRINSIC_DEBUG_LENGTH);
                fields_str
            };

            // TODO:
            // - extract each alert into a pallet-specific function or trait object
            // - add tests to make sure we can parse the extrinsics for each alert

            // All sudo calls are alerts.
            // TODO:
            // - check if the call is from the sudo account
            // - decode the inner call
            if meta.pallet.name() == "Sudo" {
                slack_client_info
                    .post_message(
                        format!(
                            "Sudo::{} call detected at extrinsic {}\n\
                            {bytes}\n\
                            {fields_str}",
                            meta.variant.name,
                            extrinsic.index()
                        )
                        .as_str(),
                        &block,
                        &extrinsics,
                        &genesis_hash,
                    )
                    .await?;
            } else if meta.pallet.name() == "Balances" {
                // "force*" calls and large balance changes are alerts.

                // subxt knows these field names, so we can search for the transfer value by
                // name.
                // TODO:
                // - track the total of recent transfers, so the threshold can't be bypassed by
                //   splitting the transfer into multiple calls
                // - split this field search into a function which takes a field name,
                //   and another function which does the numeric conversion and range check
                let transfer_value = if let Composite::Named(named_fields) = fields
                    && let Some((_, transfer_value)) = named_fields.iter().find(|(name, _)| {
                        ["value", "amount", "new_free", "delta"].contains(&name.as_str())
                    }) {
                    transfer_value.as_u128()
                } else {
                    None
                };

                if meta.variant.name.starts_with("force") {
                    slack_client_info
                        .post_message(
                            format!(
                                "Force Balances::{} call detected at extrinsic {}\n\
                                    Transfer value: {}\n\
                                    {bytes}\n\
                                    {fields_str}",
                                meta.variant.name,
                                extrinsic.index(),
                                fmt_amount(transfer_value),
                            )
                            .as_str(),
                            &block,
                            &extrinsics,
                            &genesis_hash,
                        )
                        .await?;
                } else if let Some(transfer_value) = transfer_value
                    && transfer_value >= MIN_BALANCE_CHANGE
                {
                    slack_client_info
                        .post_message(
                            format!(
                                "Large Balances::{} call detected at extrinsic {}\n\
                                    Transfer value: {} (above {})\n\
                                    {bytes}\n\
                                    {fields_str}",
                                meta.variant.name,
                                extrinsic.index(),
                                fmt_amount(transfer_value),
                                fmt_amount(MIN_BALANCE_CHANGE),
                            )
                            .as_str(),
                            &block,
                            &extrinsics,
                            &genesis_hash,
                        )
                        .await?;
                } else if !["transfer_all", "upgrade_accounts"]
                    .contains(&meta.variant.name.as_str())
                {
                    // Every other Balances extrinsic should have an amount.
                    // TODO: check transfer_all by accessing account storage to get the value
                    warn!(
                        "Balance extrinsic {} amount unavailable in block \
                        {block_height} {block_hash:?}\n\
                        {bytes}\n\
                        {fields_str}",
                        extrinsic.index()
                    );
                }
            }
        }
    }

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
