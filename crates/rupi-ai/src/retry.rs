use crate::AiError;
use std::time::Duration;
use tokio::time::sleep;

#[derive(Debug, Clone)]
pub struct RetryPolicy {
    pub max_retries: u32,
    pub initial_delay: Duration,
    pub max_delay: Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_retries: 3,
            initial_delay: Duration::from_millis(400),
            max_delay: Duration::from_secs(8),
        }
    }
}

pub async fn retry_with_backoff<T, F, Fut>(policy: &RetryPolicy, mut op: F) -> Result<T, AiError>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T, AiError>>,
{
    let mut attempt = 0u32;
    let mut delay = policy.initial_delay;
    loop {
        match op().await {
            Ok(v) => return Ok(v),
            Err(err) if err.is_retryable() && attempt < policy.max_retries => {
                tracing::warn!(attempt, error = %err, "retrying provider request");
                sleep(delay).await;
                delay = (delay * 2).min(policy.max_delay);
                attempt += 1;
            }
            Err(err) => return Err(err),
        }
    }
}
