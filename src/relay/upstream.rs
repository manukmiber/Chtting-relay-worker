//! Talks to the configured backends.
//!
//! One `reqwest::Client` is shared by every request in the process. That is the
//! single most important detail for concurrency here: the client owns a
//! connection pool, so a few hundred simultaneous callers reuse a handful of
//! warm TLS connections per backend instead of paying a fresh handshake each.
//!
//! Mobile networks drop connections constantly, so a request retries on
//! connection-level failures and on 429/5xx, then falls through to the route's
//! fallback backends before giving up.

use anyhow::Result;
use serde_json::Value;
use std::sync::Arc;
use std::time::Duration;

use crate::config::{Backend, Config, Model};
use crate::logging::Logger;

pub struct Upstream {
    client: reqwest::Client,
    logger: Arc<Logger>,
}

/// The outcome of a call, once every backend on the route has had its turn.
pub enum Sent {
    /// A 2xx whose body is still unread, so it can be streamed.
    Ok {
        response: reqwest::Response,
        backend: Backend,
        /// Total attempts made across all backends, for the metrics row.
        attempts: u32,
    },
    /// A non-2xx whose body has already been drained into `body`.
    Failed {
        status: u16,
        body: String,
        backend: Backend,
        attempts: u32,
    },
}

impl Sent {
    pub fn backend(&self) -> &Backend {
        match self {
            Sent::Ok { backend, .. } | Sent::Failed { backend, .. } => backend,
        }
    }

    pub fn attempts(&self) -> u32 {
        match self {
            Sent::Ok { attempts, .. } | Sent::Failed { attempts, .. } => *attempts,
        }
    }
}

/// A failure that carries the status the caller should see.
#[derive(Debug)]
pub struct UpstreamError {
    pub status: u16,
    pub message: String,
    pub attempts: u32,
}

impl std::fmt::Display for UpstreamError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl Upstream {
    pub fn new(logger: Arc<Logger>) -> Result<Self> {
        let client = reqwest::Client::builder()
            // Keep connections warm across requests; this is what makes a few
            // hundred concurrent callers cheap rather than handshake-bound.
            .pool_max_idle_per_host(64)
            .pool_idle_timeout(Duration::from_secs(90))
            .tcp_keepalive(Duration::from_secs(60))
            // No global timeout: a long generation must not be cut off. The
            // per-attempt timeout below is what bounds a hung backend.
            .user_agent("chtting-relay/2.0")
            .build()?;
        Ok(Self { client, logger })
    }

    pub fn client(&self) -> &reqwest::Client {
        &self.client
    }

    /// `https://host/v1` + `v1/chat/completions` must not become `/v1/v1/...`.
    pub fn endpoint_url(backend: &Backend, endpoint: &str) -> String {
        let base = backend.base_url.trim_end_matches('/');
        let path = endpoint.trim_start_matches('/');
        let already_versioned = base
            .rsplit('/')
            .next()
            .is_some_and(|last| {
                let mut chars = last.chars();
                chars.next() == Some('v') && chars.clone().count() > 0 && chars.all(|c| c.is_ascii_digit())
            });
        if already_versioned {
            // Drop a leading version segment from the endpoint.
            let trimmed = match path.split_once('/') {
                Some((first, rest))
                    if first.starts_with('v') && first[1..].chars().all(|c| c.is_ascii_digit()) =>
                {
                    rest
                }
                _ => path,
            };
            format!("{base}/{trimmed}")
        } else {
            format!("{base}/{path}")
        }
    }

    fn request(&self, backend: &Backend, endpoint: &str, stream: bool) -> reqwest::RequestBuilder {
        let url = Self::endpoint_url(backend, endpoint);
        let mut req = self
            .client
            .post(&url)
            .timeout(Duration::from_millis(backend.timeout_ms.max(1_000)))
            .header("content-type", "application/json")
            .header(
                "accept",
                if stream { "text/event-stream" } else { "application/json" },
            );

        for (k, v) in &backend.headers {
            req = req.header(k.as_str(), v.as_str());
        }
        if !backend.api_key.is_empty() {
            if backend.kind == "anthropic" {
                req = req.header("x-api-key", &backend.api_key);
                if !backend.headers.contains_key("anthropic-version") {
                    req = req.header("anthropic-version", "2023-06-01");
                }
            } else {
                req = req.header("authorization", format!("Bearer {}", backend.api_key));
            }
        }
        req
    }

    /// Send to one backend, retrying transient failures.
    pub async fn send(
        &self,
        backend: &Backend,
        endpoint: &str,
        body: &Value,
        stream: bool,
    ) -> Result<(reqwest::Response, u32), UpstreamError> {
        let max_attempts = backend.max_retries.saturating_add(1).max(1);
        let mut last_error = String::new();

        for attempt in 1..=max_attempts {
            let result = self
                .request(backend, endpoint, stream)
                .json(body)
                .send()
                .await;

            match result {
                Ok(res) if res.status().is_success() => return Ok((res, attempt)),
                Ok(res) => {
                    let status = res.status().as_u16();
                    let retryable = status == 429 || status >= 500;
                    if attempt < max_attempts && retryable {
                        let wait = retry_after(&res)
                            .unwrap_or_else(|| backoff(attempt));
                        self.logger.warn(format!(
                            "upstream {} returned {status}, retry {attempt}/{} in {}ms",
                            backend.id,
                            max_attempts - 1,
                            wait.as_millis()
                        ));
                        drop(res);
                        tokio::time::sleep(wait).await;
                        continue;
                    }
                    return Ok((res, attempt));
                }
                Err(err) => {
                    last_error = err.to_string();
                    self.logger.warn(format!(
                        "upstream {} attempt {attempt} failed: {last_error}",
                        backend.id
                    ));
                    if attempt < max_attempts {
                        tokio::time::sleep(backoff(attempt)).await;
                        continue;
                    }
                }
            }
        }

        Err(UpstreamError {
            status: 502,
            message: format!(
                "backend \"{}\" unreachable: {}",
                backend.name,
                if last_error.is_empty() { "unknown error".into() } else { last_error }
            ),
            attempts: max_attempts,
        })
    }

    /// Try the primary backend, then each fallback, returning the first success.
    pub async fn send_with_fallback(
        &self,
        cfg: &Config,
        route: &Model,
        endpoint: &str,
        body: &Value,
        stream: bool,
    ) -> Result<Sent, UpstreamError> {
        let mut ids = vec![route.backend.clone()];
        ids.extend(route.fallbacks.iter().cloned());

        let mut errors: Vec<String> = Vec::new();
        let mut attempts = 0u32;
        // Remembered so a single-backend route reports the backend's own status
        // rather than a generic 502 that hides what actually went wrong.
        let mut last_http_failure: Option<Sent> = None;

        for id in ids {
            let Some(backend) = cfg.find_backend(&id) else {
                errors.push(format!("unknown backend \"{id}\""));
                continue;
            };
            if !backend.enabled {
                errors.push(format!("backend \"{}\" is disabled", backend.name));
                continue;
            }

            match self.send(backend, endpoint, body, stream).await {
                Ok((res, used)) => {
                    attempts += used;
                    if res.status().is_success() {
                        return Ok(Sent::Ok {
                            response: res,
                            backend: backend.clone(),
                            attempts,
                        });
                    }

                    let status = res.status().as_u16();
                    let detail = read_error_body(res).await;
                    errors.push(format!("{}: {status} {detail}", backend.name));

                    // A 4xx from the first backend usually means a bad request,
                    // not a bad backend, so only fall through on auth, rate and
                    // server problems.
                    let worth_failing_over =
                        matches!(status, 401 | 402 | 403 | 408 | 409 | 429) || status >= 500;

                    let sent = Sent::Failed {
                        status,
                        body: detail,
                        backend: backend.clone(),
                        attempts,
                    };
                    if !worth_failing_over {
                        return Ok(sent);
                    }
                    last_http_failure = Some(sent);
                }
                Err(err) => {
                    attempts += err.attempts;
                    errors.push(err.message);
                }
            }
        }

        if let Some(Sent::Failed { status, body, backend, .. }) = last_http_failure {
            // Report the backend's own status, not a generic 502 over the top.
            return Ok(Sent::Failed { status, body, backend, attempts });
        }
        Err(UpstreamError {
            status: 502,
            message: if errors.is_empty() {
                "no usable backend".into()
            } else {
                errors.join(" | ")
            },
            attempts,
        })
    }
}

async fn read_error_body(res: reqwest::Response) -> String {
    let text = match res.text().await {
        Ok(t) => t,
        Err(_) => return "<unreadable>".into(),
    };
    match serde_json::from_str::<Value>(&text) {
        Ok(json) => json
            .get("error")
            .and_then(|e| e.get("message"))
            .or_else(|| json.get("message"))
            .and_then(|v| v.as_str())
            .map(str::to_string)
            .unwrap_or_else(|| text.chars().take(400).collect()),
        Err(_) => text.chars().take(400).collect(),
    }
}

fn retry_after(res: &reqwest::Response) -> Option<Duration> {
    let raw = res.headers().get("retry-after")?.to_str().ok()?;
    let secs: f64 = raw.trim().parse().ok()?;
    if secs <= 0.0 {
        return None;
    }
    Some(Duration::from_millis(((secs * 1000.0) as u64).min(10_000)))
}

fn backoff(attempt: u32) -> Duration {
    Duration::from_millis((500u64 << (attempt.min(5) - 1)).min(8_000))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn backend(base: &str) -> Backend {
        Backend {
            base_url: base.into(),
            ..Default::default()
        }
    }

    #[test]
    fn a_base_url_that_already_carries_a_version_is_not_doubled() {
        assert_eq!(
            Upstream::endpoint_url(&backend("https://api.deepseek.com/v1"), "v1/chat/completions"),
            "https://api.deepseek.com/v1/chat/completions"
        );
        assert_eq!(
            Upstream::endpoint_url(&backend("https://api.example.com"), "v1/chat/completions"),
            "https://api.example.com/v1/chat/completions"
        );
        assert_eq!(
            Upstream::endpoint_url(&backend("https://api.example.com/openai/"), "v1/embeddings"),
            "https://api.example.com/openai/v1/embeddings"
        );
    }

    #[test]
    fn backoff_grows_but_stays_bounded() {
        assert_eq!(backoff(1), Duration::from_millis(500));
        assert_eq!(backoff(2), Duration::from_millis(1000));
        assert!(backoff(9) <= Duration::from_millis(8_000));
    }
}
