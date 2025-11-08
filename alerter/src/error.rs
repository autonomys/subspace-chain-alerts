use subxt::utils::H256;
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
