//! The token-accounting facade.
//!
//! It answers three questions: how many tokens went in, how many came out, and
//! how much either number can be trusted.

pub mod chat;
pub mod registry;

use serde::Serialize;
use serde_json::Value;
use std::sync::Arc;

use crate::config::{Config, ImageDefaults};
use chat::{count_chat_request, count_completion, RequestCount};
use registry::{Encoder, Registry};

/// What vocabulary and chat profile a given call should be counted with.
#[derive(Debug, Clone)]
pub struct Resolved {
    pub tokenizer: String,
    pub profile: String,
}

pub struct TokenCounter {
    pub registry: Arc<Registry>,
}

impl TokenCounter {
    pub fn new(registry: Arc<Registry>) -> Self {
        Self { registry }
    }

    /// Decide which vocabulary and profile to use.
    ///
    /// `model` must be the **backend** model name, because that is what
    /// actually bills. An override names a vocabulary directly and bypasses the
    /// glob rules, which is how a route pins an exact vocabulary — passing the
    /// pinned name back through the rules would send it to the catch-all.
    pub fn resolve(
        &self,
        cfg: &Config,
        model: &str,
        override_tokenizer: &str,
        override_profile: &str,
    ) -> Resolved {
        let (matched_tok, matched_prof) = Registry::match_rules(&cfg.tokenizer, model);
        Resolved {
            tokenizer: if override_tokenizer.is_empty() {
                matched_tok
            } else {
                override_tokenizer.to_string()
            },
            profile: if override_profile.is_empty() {
                matched_prof
            } else {
                override_profile.to_string()
            },
        }
    }

    pub async fn encoder_for(&self, resolved: &Resolved) -> Arc<Encoder> {
        self.registry.get(&resolved.tokenizer).await
    }

    /// Count a chat request. The encode itself is CPU-bound, so a large prompt
    /// runs on the blocking pool rather than stalling an async worker that
    /// other users' streams are sharing.
    pub async fn count_request(
        &self,
        body: &Value,
        resolved: &Resolved,
        images: &ImageDefaults,
    ) -> RequestCount {
        let encoder = self.encoder_for(resolved).await;
        let body = body.clone();
        let profile = resolved.profile.clone();
        let images = images.clone();
        run_maybe_blocking(move || count_chat_request(&body, &encoder, &profile, &images)).await
    }

    pub async fn count_text(&self, text: &str, resolved: &Resolved) -> (usize, bool, String) {
        let encoder = self.encoder_for(resolved).await;
        let text = text.to_string();
        run_maybe_blocking(move || {
            (
                encoder.count(&text),
                encoder.exact(),
                encoder.name().to_string(),
            )
        })
        .await
    }

    pub async fn count_output(
        &self,
        text: &str,
        resolved: &Resolved,
        reasoning: &str,
        tool_calls: Vec<Value>,
    ) -> (usize, bool, String) {
        let encoder = self.encoder_for(resolved).await;
        let text = text.to_string();
        let reasoning = reasoning.to_string();
        run_maybe_blocking(move || {
            (
                count_completion(&text, &encoder, &reasoning, &tool_calls),
                encoder.exact(),
                encoder.name().to_string(),
            )
        })
        .await
    }

    /// Split text into the pieces the model sees, for the playground.
    pub async fn pieces(&self, text: &str, resolved: &Resolved, limit: usize) -> Value {
        let encoder = self.encoder_for(resolved).await;
        let text = text.to_string();
        let (kind, name, exact) = (
            encoder.kind().to_string(),
            encoder.name().to_string(),
            encoder.exact(),
        );
        let all = run_maybe_blocking(move || encoder.pieces(&text)).await;
        let truncated = all.len() > limit;
        serde_json::json!({
            "tokenizer": name,
            "kind": kind,
            "exact": exact,
            "count": all.len(),
            "pieces": all.into_iter().take(limit).collect::<Vec<_>>(),
            "truncated": truncated,
        })
    }
}

/// Short work runs inline; anything big enough to matter goes to the blocking
/// pool. Always spawning would add scheduling overhead to the common case of a
/// few hundred tokens; never spawning would let one 200 KB prompt block a
/// worker thread that other users' streams are running on.
async fn run_maybe_blocking<T, F>(work: F) -> T
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    tokio::task::spawn_blocking(work)
        .await
        .expect("tokenizer work should not panic")
}

/* ------------------------------------------------------------- usage -- */

#[derive(Debug, Clone, Default, Serialize)]
pub struct NormalizedUsage {
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub total_tokens: u64,
    pub cached_tokens: u64,
    pub reasoning_tokens: u64,
}

#[derive(Debug, Clone, Default)]
pub struct LocalCount {
    pub prompt: u64,
    pub completion: u64,
    pub exact: bool,
}

#[derive(Debug, Clone, Default)]
pub struct Usage {
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub total_tokens: u64,
    /// `upstream` when the backend's own number won, `local` otherwise.
    pub source: &'static str,
    pub exact: bool,
    pub local_prompt: u64,
    pub local_completion: u64,
    /// Local minus upstream, so the dashboard can show how far the relay's
    /// tokenizer is from what the provider actually billed.
    pub drift_prompt: i64,
    pub drift_completion: i64,
    pub cached_tokens: u64,
    pub reasoning_tokens: u64,
}

impl Usage {
    /// The `usage` object handed back to the caller.
    pub fn public(&self) -> Value {
        let mut out = serde_json::json!({
            "prompt_tokens": self.prompt_tokens,
            "completion_tokens": self.completion_tokens,
            "total_tokens": self.total_tokens,
        });
        let map = out.as_object_mut().expect("just built an object");
        if self.cached_tokens > 0 {
            map.insert(
                "prompt_tokens_details".into(),
                serde_json::json!({ "cached_tokens": self.cached_tokens }),
            );
        }
        if self.reasoning_tokens > 0 {
            map.insert(
                "completion_tokens_details".into(),
                serde_json::json!({ "reasoning_tokens": self.reasoning_tokens }),
            );
        }
        out
    }
}

/// Merge the relay's own count with whatever the backend reported.
pub fn reconcile_usage(
    local: LocalCount,
    upstream: Option<&Value>,
    prefer_upstream: bool,
) -> Usage {
    let up = upstream.and_then(normalize_usage);
    let has_up = up
        .as_ref()
        .is_some_and(|u| u.prompt_tokens > 0 || u.completion_tokens > 0);
    let use_up = prefer_upstream && has_up;

    let (prompt, completion) = match (&up, use_up) {
        (Some(u), true) => (
            if u.prompt_tokens > 0 {
                u.prompt_tokens
            } else {
                local.prompt
            },
            if u.completion_tokens > 0 {
                u.completion_tokens
            } else {
                local.completion
            },
        ),
        _ => (local.prompt, local.completion),
    };

    Usage {
        prompt_tokens: prompt,
        completion_tokens: completion,
        total_tokens: prompt + completion,
        source: if use_up { "upstream" } else { "local" },
        exact: if use_up { true } else { local.exact },
        local_prompt: local.prompt,
        local_completion: local.completion,
        drift_prompt: up
            .as_ref()
            .filter(|_| has_up)
            .map_or(0, |u| local.prompt as i64 - u.prompt_tokens as i64),
        drift_completion: up
            .as_ref()
            .filter(|_| has_up)
            .map_or(0, |u| local.completion as i64 - u.completion_tokens as i64),
        cached_tokens: up.as_ref().map_or(0, |u| u.cached_tokens),
        reasoning_tokens: up.as_ref().map_or(0, |u| u.reasoning_tokens),
    }
}

/// Read a `usage` object in any of the dialects backends actually send.
pub fn normalize_usage(usage: &Value) -> Option<NormalizedUsage> {
    let map = usage.as_object()?;
    let num = |v: Option<&Value>| -> u64 {
        v.and_then(|v| v.as_f64())
            .filter(|n| n.is_finite() && *n > 0.0)
            .map_or(0, |n| n.round() as u64)
    };

    let prompt = num(map
        .get("prompt_tokens")
        // Anthropic calls it input_tokens
        .or_else(|| map.get("input_tokens"))
        .or_else(|| map.get("promptTokens")));
    let completion = num(map
        .get("completion_tokens")
        .or_else(|| map.get("output_tokens"))
        .or_else(|| map.get("completionTokens")));
    let cached = num(map
        .get("prompt_tokens_details")
        .and_then(|d| d.get("cached_tokens"))
        .or_else(|| map.get("prompt_cache_hit_tokens"))
        .or_else(|| map.get("cache_read_input_tokens")));
    let reasoning = num(map
        .get("completion_tokens_details")
        .and_then(|d| d.get("reasoning_tokens"))
        .or_else(|| map.get("reasoning_tokens")));

    let total = num(map.get("total_tokens"));
    Some(NormalizedUsage {
        prompt_tokens: prompt,
        completion_tokens: completion,
        total_tokens: if total > 0 {
            total
        } else {
            prompt + completion
        },
        cached_tokens: cached,
        reasoning_tokens: reasoning,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_backend_number_wins_and_the_difference_is_kept_as_drift() {
        let local = LocalCount {
            prompt: 100,
            completion: 50,
            exact: true,
        };
        let upstream = serde_json::json!({"prompt_tokens": 98, "completion_tokens": 52});
        let usage = reconcile_usage(local, Some(&upstream), true);
        assert_eq!(usage.prompt_tokens, 98);
        assert_eq!(usage.completion_tokens, 52);
        assert_eq!(usage.source, "upstream");
        assert_eq!(usage.drift_prompt, 2);
        assert_eq!(usage.drift_completion, -2);
        assert_eq!(usage.local_prompt, 100);
    }

    #[test]
    fn a_silent_backend_leaves_the_local_count_in_charge() {
        let local = LocalCount {
            prompt: 100,
            completion: 50,
            exact: false,
        };
        let usage = reconcile_usage(local, None, true);
        assert_eq!(usage.prompt_tokens, 100);
        assert_eq!(usage.source, "local");
        // an estimate must never be reported as exact
        assert!(!usage.exact);
        assert_eq!(usage.drift_prompt, 0);
    }

    #[test]
    fn preferring_local_ignores_the_backend_but_still_records_drift() {
        let local = LocalCount {
            prompt: 100,
            completion: 50,
            exact: true,
        };
        let upstream = serde_json::json!({"prompt_tokens": 98, "completion_tokens": 52});
        let usage = reconcile_usage(local, Some(&upstream), false);
        assert_eq!(usage.prompt_tokens, 100);
        assert_eq!(usage.source, "local");
        assert_eq!(usage.drift_prompt, 2);
    }

    #[test]
    fn anthropic_and_openai_usage_dialects_both_parse() {
        let anthropic = serde_json::json!({
            "input_tokens": 10, "output_tokens": 20, "cache_read_input_tokens": 5
        });
        let u = normalize_usage(&anthropic).unwrap();
        assert_eq!(
            (u.prompt_tokens, u.completion_tokens, u.cached_tokens),
            (10, 20, 5)
        );

        let openai = serde_json::json!({
            "prompt_tokens": 10, "completion_tokens": 20, "total_tokens": 30,
            "prompt_tokens_details": {"cached_tokens": 4},
            "completion_tokens_details": {"reasoning_tokens": 7}
        });
        let u = normalize_usage(&openai).unwrap();
        assert_eq!(
            (u.total_tokens, u.cached_tokens, u.reasoning_tokens),
            (30, 4, 7)
        );
    }

    #[test]
    fn a_pinned_vocabulary_is_not_run_back_through_the_glob_rules() {
        let cfg = Config::default();
        let counter = TokenCounter::new(Arc::new(Registry::new(
            std::path::PathBuf::from("/nonexistent"),
            crate::logging::Logger::console(crate::logging::Level::Silent),
        )));
        // Without a pin, the backend name drives the rules.
        assert_eq!(
            counter
                .resolve(&cfg, "Deepseek-v4-flash-0731", "", "")
                .tokenizer,
            "deepseek"
        );
        // With a pin, the pinned name is used verbatim — the bug this guards
        // against was feeding "cl100k_base" back in as if it were a model name,
        // which fell through to the catch-all rule and silently used o200k.
        assert_eq!(
            counter
                .resolve(&cfg, "Deepseek-v4-flash-0731", "cl100k_base", "")
                .tokenizer,
            "cl100k_base"
        );
    }
}
