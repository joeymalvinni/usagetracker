use thiserror::Error;

#[derive(Debug, Error)]
pub enum UsageCoreError {
    #[error("invalid api request: {0}")]
    InvalidApiRequest(String),
}
