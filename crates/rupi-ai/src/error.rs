use thiserror::Error;

#[derive(Debug, Error)]
pub enum AiError {
    #[error("HTTP error {status}: {body}")]
    Http { status: u16, body: String },
    #[error("network error: {0}")]
    Network(String),
    #[error("provider returned an empty or malformed stream: {0}")]
    Stream(String),
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("missing API key for provider {0}")]
    MissingApiKey(String),
    #[error("aborted")]
    Aborted,
    #[error("{0}")]
    Other(String),
}

impl AiError {
    pub fn is_retryable(&self) -> bool {
        match self {
            AiError::Http { status, .. } => matches!(*status, 429 | 500 | 502 | 503 | 529),
            AiError::Network(_) => true,
            AiError::Stream(_) => true,
            _ => false,
        }
    }
}
