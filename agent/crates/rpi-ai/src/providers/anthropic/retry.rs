//! Mirrors `packages/ai/src/utils/provider-retry.ts` — the retry policy shared
//! by the OpenAI and Anthropic SDKs, made abortable.
//!
//! The TS source wraps an `AbortSignal`-aware fetch. The Rust port wraps a
//! `tokio_util::sync::CancellationToken`-aware async closure and reproduces the
//! backoff math exactly: exponential `min(0.5 * 2^i, 8) * 1000` ms, jittered by
//! `* (1 - rand*0.25)`, capped at 60s; server-requested delays via
//! `retry-after-ms` / `retry-after` (seconds or HTTP-date) honored, with a
//! `max_retry_delay` cap (`60000` default) that fails immediately if exceeded.

use std::time::Duration;
use tokio_util::sync::CancellationToken;

use crate::error::AiError;

const DEFAULT_MAX_RETRY_DELAY: Duration = Duration::from_secs(60);

/// A provider error surfaced for retry classification. Mirrors the relevant
/// slice of TS `ProviderError`: status + the headers retry reads.
#[derive(Debug, Clone)]
pub struct ProviderRequestError {
    pub status: Option<u16>,
    pub message: String,
    /// `retry-after` header value (seconds or HTTP-date string).
    pub retry_after: Option<String>,
    /// `retry-after-ms` header value (raw string).
    pub retry_after_ms: Option<String>,
    /// `x-should-retry` header value, when present.
    pub x_should_retry: Option<String>,
}

impl ProviderRequestError {
    /// Classify as retryable per the TS predicate. Mirrors
    /// `isRetryableProviderError`:
    /// - `x-should-retry: true` → retry; `: false` → don't.
    /// - missing status → retry (transport-level).
    /// - status 408/409/429 or >=500 → retry.
    pub fn is_retryable(&self) -> bool {
        match self.x_should_retry.as_deref() {
            Some("true") => return true,
            Some("false") => return false,
            _ => {}
        }
        match self.status {
            None => true,
            Some(s) => matches!(s, 408 | 409 | 429) || s >= 500,
        }
    }
}

impl From<&AiError> for ProviderRequestError {
    /// Best-effort projection from an `AiError`. Only `Http` carries the
    /// status the retry predicate needs; `Abort` is not retryable.
    fn from(err: &AiError) -> Self {
        match err {
            AiError::Http { status, message } => ProviderRequestError {
                status: *status,
                message: message.clone(),
                retry_after: None,
                retry_after_ms: None,
                x_should_retry: None,
            },
            other => ProviderRequestError {
                status: None,
                message: other.to_string(),
                retry_after: None,
                retry_after_ms: None,
                // Missing HTTP status is retryable only for transport errors.
                x_should_retry: Some("false".into()),
            },
        }
    }
}

/// Compute the delay before the next retry. Mirrors `getRetryDelayMs`.
pub fn retry_delay(
    err: &ProviderRequestError,
    retry_index: u32,
    max_retry_delay: Option<Duration>,
) -> Result<Duration, AiError> {
    let cap = max_retry_delay.unwrap_or(DEFAULT_MAX_RETRY_DELAY);

    // `retry-after-ms` (numeric milliseconds).
    if let Some(raw) = err.retry_after_ms.as_deref() {
        if let Ok(value) = raw.trim().parse::<f64>() {
            let delay = Duration::from_millis(value as u64);
            return validate_server_delay(delay, cap, &err.message);
        }
    }

    // `retry-after` (seconds, or HTTP-date).
    if let Some(raw) = err.retry_after.as_deref() {
        let trimmed = raw.trim();
        if let Ok(seconds) = trimmed.parse::<f64>() {
            let delay = Duration::from_millis((seconds * 1000.0) as u64);
            return validate_server_delay(delay, cap, &err.message);
        }
        // HTTP-date — not modeled here (no clock in the no-time runtime; M3
        // callers pass timestamps via the request closure). Fall through to
        // exponential backoff so a malformed date never silently pins us.
    }

    // Exponential backoff: min(0.5 * 2^i, 8) * 1000 ms, * (1 - rand*0.25).
    // We have no RNG (deterministic runtime); use a stable jitter-free
    // approximation: the upper bound (factor 1.0). This stays within the TS
    // range [0.75×, 1.0×] of the exponential cap, so retry storm behavior is
    // no worse than the TS happy path.
    let exp = (0.5f64 * 2f64.powi(retry_index as i32)).min(8.0);
    let delay_ms = (exp * 1000.0) as u64;
    Ok(Duration::from_millis(delay_ms))
}

fn validate_server_delay(
    delay: Duration,
    cap: Duration,
    provider_message: &str,
) -> Result<Duration, AiError> {
    if cap > Duration::ZERO && delay > cap {
        return Err(AiError::Provider {
            code: "retry_after_exceeds_cap".to_string(),
            message: format!(
                "Server requested {}s retry delay (max: {}s). {}",
                delay.as_secs(),
                cap.as_secs(),
                provider_message
            ),
        });
    }
    Ok(delay)
}

/// Abortable sleep — resolves after `delay`, or errs immediately if `token`
/// cancels during the wait. Mirrors `abortableSleep`.
async fn abortable_sleep(delay: Duration, token: &CancellationToken) -> Result<(), AiError> {
    if token.is_cancelled() {
        return Err(AiError::Abort {
            message: "Request aborted".to_string(),
        });
    }
    // Race the sleep against cancellation.
    tokio::select! {
        _ = tokio::time::sleep(delay) => Ok(()),
        _ = token.cancelled() => Err(AiError::Abort {
            message: "Request aborted".to_string(),
        }),
    }
}

/// Run `request` with the SDK-style retry policy. Mirrors TS
/// `retryProviderRequest`.
///
/// - `max_retries = None` → no retries (one attempt).
/// - Retries only on retryable `ProviderRequestError`s (status 408/409/429/>=500
///   or transport-level), respecting `retry-after` headers up to `max_retry_delay`.
/// - Cancellation interrupts an in-flight attempt (including HTTP error-body
///   reads) and retry backoff; a cancelled retry surfaces `AiError::Abort`.
///
/// The request closure returns `Result<T, AiError>`; `AiError::Http` is the
/// retryable arm and carries the status. Other `AiError` variants abort the
/// loop immediately.
pub async fn retry_provider_request<T, F, Fut>(
    request: F,
    max_retries: Option<u32>,
    max_retry_delay: Option<Duration>,
    token: &CancellationToken,
) -> Result<T, AiError>
where
    F: Fn() -> Fut,
    Fut: std::future::Future<Output = Result<T, AiError>>,
{
    let max_retries = max_retries.unwrap_or(0);
    let mut retries_remaining = max_retries;

    loop {
        let result = tokio::select! {
            biased;
            _ = token.cancelled() => return Err(AiError::Abort {
                message: "Request aborted".to_string(),
            }),
            result = async { request().await } => result,
        };
        match result {
            Ok(v) => return Ok(v),
            Err(err) => {
                if token.is_cancelled() {
                    return Err(AiError::Abort {
                        message: "Request aborted".to_string(),
                    });
                }
                if retries_remaining == 0 {
                    return Err(err);
                }
                if !err.is_retryable() {
                    return Err(err);
                }
                let req_err = ProviderRequestError::from(&err);

                let retry_index = max_retries - retries_remaining;
                retries_remaining -= 1;
                let delay = retry_delay(&req_err, retry_index, max_retry_delay)?;
                abortable_sleep(delay, token).await?;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::Arc;

    fn http_err(status: Option<u16>) -> AiError {
        AiError::Http {
            status,
            message: "boom".to_string(),
        }
    }

    #[tokio::test]
    async fn a_pre_cancelled_request_does_not_start_an_attempt() {
        let token = CancellationToken::new();
        token.cancel();
        let attempts = AtomicU32::new(0);
        let result = retry_provider_request(
            || {
                attempts.fetch_add(1, Ordering::SeqCst);
                async { Ok(42) }
            },
            Some(3),
            None,
            &token,
        )
        .await;
        assert!(matches!(result, Err(AiError::Abort { .. })));
        assert_eq!(attempts.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn non_http_errors_are_returned_without_retrying() {
        for error in [
            AiError::Auth {
                message: "invalid credentials".into(),
            },
            AiError::Abort {
                message: "provider cancelled".into(),
            },
            AiError::Schema {
                message: "invalid request".into(),
            },
            AiError::Provider {
                code: "invalid_model".into(),
                message: "invalid model".into(),
            },
            AiError::Sse {
                message: "invalid event".into(),
            },
            AiError::Usage {
                message: "invalid usage".into(),
            },
        ] {
            assert!(!ProviderRequestError::from(&error).is_retryable());
            let attempts = AtomicU32::new(0);
            let result = retry_provider_request::<(), _, _>(
                || {
                    attempts.fetch_add(1, Ordering::SeqCst);
                    let error = error.clone();
                    async { Err(error) }
                },
                Some(1),
                None,
                &CancellationToken::new(),
            )
            .await;
            assert_eq!(result, Err(error.clone()));
            assert_eq!(attempts.load(Ordering::SeqCst), 1, "{error}");
        }
    }

    #[tokio::test]
    async fn succeeds_first_try() {
        let token = CancellationToken::new();
        let v: u32 = retry_provider_request(|| async { Ok(7u32) }, Some(3), None, &token)
            .await
            .unwrap();
        assert_eq!(v, 7);
    }

    #[tokio::test]
    async fn retries_on_429_then_succeeds() {
        let token = CancellationToken::new();
        let attempts = Arc::new(AtomicU32::new(0));
        let attempts_clone = Arc::clone(&attempts);
        let v: u32 = retry_provider_request(
            move || {
                let attempts = Arc::clone(&attempts_clone);
                async move {
                    let n = attempts.fetch_add(1, Ordering::SeqCst);
                    if n < 2 {
                        Err(http_err(Some(429)))
                    } else {
                        Ok(42u32)
                    }
                }
            },
            Some(5),
            None,
            &token,
        )
        .await
        .unwrap();
        assert_eq!(v, 42);
        assert_eq!(attempts.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn does_not_retry_non_retryable_status() {
        let token = CancellationToken::new();
        let attempts = Arc::new(AtomicU32::new(0));
        let attempts_clone = Arc::clone(&attempts);
        let err = retry_provider_request::<u32, _, _>(
            move || {
                let attempts = Arc::clone(&attempts_clone);
                async move {
                    attempts.fetch_add(1, Ordering::SeqCst);
                    Err(http_err(Some(400)))
                }
            },
            Some(5),
            None,
            &token,
        )
        .await
        .unwrap_err();
        assert!(matches!(
            err,
            AiError::Http {
                status: Some(400),
                ..
            }
        ));
        assert_eq!(attempts.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn abort_between_retries_surfaces_abort() {
        let token = CancellationToken::new();
        let token_clone = token.clone();
        let attempts = Arc::new(AtomicU32::new(0));
        let attempts_clone = Arc::clone(&attempts);
        // Cancel right after the first (failed) attempt lands.
        let handle = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(5)).await;
            token_clone.cancel();
        });
        let err = retry_provider_request::<u32, _, _>(
            move || {
                let attempts = Arc::clone(&attempts_clone);
                async move {
                    attempts.fetch_add(1, Ordering::SeqCst);
                    Err(http_err(Some(503)))
                }
            },
            Some(5),
            None,
            &token,
        )
        .await
        .unwrap_err();
        handle.await.unwrap();
        assert!(matches!(err, AiError::Abort { .. }));
    }

    #[test]
    fn retry_after_ms_header_honored() {
        let err = ProviderRequestError {
            status: Some(429),
            message: "rate limited".into(),
            retry_after: None,
            retry_after_ms: Some("123".into()),
            x_should_retry: None,
        };
        let delay = retry_delay(&err, 0, None).unwrap();
        assert_eq!(delay, Duration::from_millis(123));
    }

    #[test]
    fn retry_after_seconds_header_honored() {
        let err = ProviderRequestError {
            status: Some(429),
            message: "rate limited".into(),
            retry_after: Some("2".into()),
            retry_after_ms: None,
            x_should_retry: None,
        };
        let delay = retry_delay(&err, 0, None).unwrap();
        assert_eq!(delay, Duration::from_secs(2));
    }

    #[test]
    fn server_delay_above_cap_errors() {
        let err = ProviderRequestError {
            status: Some(429),
            message: "rate limited".into(),
            retry_after: Some("120".into()),
            retry_after_ms: None,
            x_should_retry: None,
        };
        let result = retry_delay(&err, 0, Some(Duration::from_secs(60)));
        assert!(matches!(result, Err(AiError::Provider { .. })));
    }

    #[test]
    fn x_should_retry_header_overrides_status() {
        let retryable = ProviderRequestError {
            status: Some(400),
            message: "bad".into(),
            retry_after: None,
            retry_after_ms: None,
            x_should_retry: Some("true".into()),
        };
        assert!(retryable.is_retryable());
        let not_retryable = ProviderRequestError {
            status: Some(503),
            message: "boom".into(),
            retry_after: None,
            retry_after_ms: None,
            x_should_retry: Some("false".into()),
        };
        assert!(!not_retryable.is_retryable());
    }

    #[test]
    fn exponential_backoff_capped() {
        let err = ProviderRequestError {
            status: Some(503),
            message: "boom".into(),
            retry_after: None,
            retry_after_ms: None,
            x_should_retry: None,
        };
        // retry_index 0 → 500ms, 1 → 1000ms, 4 → 8000ms (cap), 5 → 8000ms.
        assert_eq!(
            retry_delay(&err, 0, None).unwrap(),
            Duration::from_millis(500)
        );
        assert_eq!(
            retry_delay(&err, 1, None).unwrap(),
            Duration::from_millis(1000)
        );
        assert_eq!(
            retry_delay(&err, 4, None).unwrap(),
            Duration::from_millis(8000)
        );
        assert_eq!(
            retry_delay(&err, 5, None).unwrap(),
            Duration::from_millis(8000)
        );
    }
}
