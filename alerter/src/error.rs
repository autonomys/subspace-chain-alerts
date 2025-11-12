use slack_morphism::errors::SlackClientError;
use subxt::utils::H256;
use tokio::sync::broadcast::error::RecvError;
use tokio::task::JoinError;

/// Overarching Error type for Alerter.
#[derive(thiserror::Error, Debug)]
pub(crate) enum Error {
    #[error("Subxt error: {0}")]
    Subxt(subxt::Error),
    #[error("Join error: {0}")]
    Join(JoinError),
    #[error("Reqwest error: {0}")]
    Reqwest(reqwest::Error),
    #[error("Block missing from backend")]
    MissingBlock,
    #[error("RPC error: {0}")]
    Rpc(subxt_rpcs::Error),
    #[error("Block Hash missing from Cache")]
    MissingBlockHashFromCache(H256),
    #[error("Block body missing: {0}")]
    MissingBlockBody(H256),
    #[error("Storage error: {0}")]
    Storage(String),
    #[error("Scale error: {0}")]
    Scale(sp_runtime::codec::Error),
    #[error("Broadcast Receive error: {0}")]
    BroadRecvErr(RecvError),
    #[error("Io error: {0}")]
    Io(std::io::Error),
    #[error("Toml error: {0}")]
    Toml(toml::de::Error),
    #[error("Config error: {0}")]
    Config(String),
    #[error("Event decode error: {0}")]
    EventDecode(String),
    #[error("Slack error: {0}")]
    Slack(SlackClientError),
    #[error("Application error: {0}")]
    App(String),
}

impl From<subxt::Error> for Error {
    fn from(err: subxt::Error) -> Self {
        Self::Subxt(err)
    }
}

impl From<JoinError> for Error {
    fn from(err: JoinError) -> Self {
        Self::Join(err)
    }
}

impl From<reqwest::Error> for Error {
    fn from(err: reqwest::Error) -> Self {
        Self::Reqwest(err)
    }
}

impl From<subxt_rpcs::Error> for Error {
    fn from(err: subxt_rpcs::Error) -> Self {
        Self::Rpc(err)
    }
}

impl From<sp_runtime::codec::Error> for Error {
    fn from(err: sp_runtime::codec::Error) -> Self {
        Self::Scale(err)
    }
}

impl From<RecvError> for Error {
    fn from(err: RecvError) -> Self {
        Self::BroadRecvErr(err)
    }
}

impl From<std::io::Error> for Error {
    fn from(err: std::io::Error) -> Self {
        Self::Io(err)
    }
}

impl From<toml::de::Error> for Error {
    fn from(err: toml::de::Error) -> Self {
        Self::Toml(err)
    }
}

impl From<SlackClientError> for Error {
    fn from(err: SlackClientError) -> Self {
        Self::Slack(err)
    }
}
