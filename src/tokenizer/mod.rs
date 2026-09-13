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
use chat::{
    count_chat_request, count_completion, count_system_messages, same_but_system, Breakdown,
    RequestCount,
};
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

    /// Count a prompt and split it between the caller and the relay.
    ///
    /// The caller's figure is a plain count of the body that arrived, taken
    /// from the untouched request. Nothing the relay does afterwards can move
    /// it — not the injected system prompt, not a rewrite rule. Send 6k tokens
    /// and the answer says 6k, because 6k is what was sent.
    ///
    /// The body the backend receives is counted too, but for the operator
    /// rather than the caller: it is what the backend will bill, and the gap
    /// between the two is the relay's own cost of doing business. Injection
    /// only ever touches system turns, so that figure is usually the caller's
    /// count plus the difference between the two bodies' system turns, which
    /// are short. Only a rewrite rule editing the caller's own text forces a
    /// second full pass, and a rule that rewrites every message has earned one.
    ///
    /// One tokenizer over one conversation, so adding and subtracting here is
    /// exact.
    pub async fn count_prompt(
        &self,
        original: &Value,
        upstream: &Value,
        resolved: &Resolved,
        images: &ImageDefaults,
    ) -> PromptSplit {
        let encoder = self.encoder_for(resolved).await;
        let original = original.clone();
        let upstream = upstream.clone();
        let profile = resolved.profile.clone();
        let images = images.clone();

        run_maybe_blocking(move || {
            let theirs = count_chat_request(&original, &encoder, &profile, &images);
            let injected = count_system_messages(&upstream, &encoder, &profile, &images) as i64
                - count_system_messages(&original, &encoder, &profile, &images) as i64;
            let billed = if same_but_system(&original, &upstream) {
                (theirs.total as i64 + injected).max(0) as usize
            } else {
                count_chat_request(&upstream, &encoder, &profile, &images).total
            };
            PromptSplit {
                user: theirs.total,
                billed,
                injected,
                breakdown: theirs.breakdown,
                exact: theirs.exact,
                tokenizer: theirs.tokenizer,
            }
        })
        .await
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

/// How a prompt's cost divides between the caller and the relay.
#[derive(Debug, Clone, Default)]
pub struct PromptSplit {
    /// The body as actually sent upstream: what the backend charges for.
    pub billed: usize,
    /// The request as it arrived, counted before the relay touched it.
    pub user: usize,
    /// The relay's contribution. Negative when the route replaces a longer
    /// system prompt of the caller's with a shorter one of its own.
    pub injected: i64,
    /// Where the *caller's* budget goes, matching `user` rather than `billed`.
    pub breakdown: Breakdown,
    pub exact: bool,
    pub tokenizer: String,
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
    /// The numbers the caller is shown and accounted for.
    ///
    /// `prompt_tokens` is the relay's own count of the caller's own body,
    /// measured before a word of system prompt was injected and before any
    /// rewrite rule ran. It is never the backend's figure, for two reasons that
    /// point the same way.
    ///
    /// The first is billing. The backend charges for everything it received,
    /// injected prompt included; the caller neither wrote that prompt nor can
    /// see it, so passing that number on would be charging them for the relay's
    /// own words. A caller who sends 6k tokens and is told 10k has no way to
    /// tell an injected prompt from a markup, and is right not to trust the
    /// difference.
    ///
    /// The second is that the backend's number is the backend's business. Its
    /// tokenizer, its template overhead, its caching — none of that is
    /// something a caller of this relay should be able to read off a usage
    /// block. What they get is what this relay measured.
    ///
    /// What the backend billed is kept beside it on the request row, so the
    /// margin stays visible to the operator and to nobody else.
    pub fn charged_to_caller(&self, user_local: u64) -> Usage {
        Usage {
            prompt_tokens: user_local,
            total_tokens: user_local + self.completion_tokens,
            // A cache hit is a fact about the caller's own prompt, so it
            // travels — but it cannot be larger than the prompt it is part of.
            cached_tokens: self.cached_tokens.min(user_local),
            ..self.clone()
        }
    }

    /// The `usage` object handed back to the caller.
    ///
    /// `cost` is what this request came to under the relay's own price list,
    /// which callers otherwise have no way to work out: the rates are the
    /// relay's, not the backend's, and tiers can move them per request.
    pub fn public(&self, cost: Option<f64>) -> Value {
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
        if let Some(cost) = cost {
            map.insert(
                "usage".into(),
                serde_json::json!(crate::util::round(cost, 6)),
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

#[cfg(test)]
mod usage_tests {
    use super::*;

    fn usage(prompt: u64, completion: u64) -> Usage {
        Usage {
            prompt_tokens: prompt,
            completion_tokens: completion,
            total_tokens: prompt + completion,
            ..Default::default()
        }
    }

    #[test]
    fn the_caller_is_charged_the_prompt_they_actually_wrote() {
        // 100 tokens went upstream, 40 of them the caller's. They pay for 40.
        let charged = usage(100, 20).charged_to_caller(40);
        assert_eq!(charged.prompt_tokens, 40);
        assert_eq!(charged.completion_tokens, 20, "output is theirs entirely");
        assert_eq!(charged.total_tokens, 60);
    }

    #[test]
    fn a_richer_backend_tokenizer_never_inflates_what_the_caller_sent() {
        // The backend charged 120 for what the relay measured as 100, of which
        // 40 was the caller's. The backend's arithmetic is the backend's; the
        // caller wrote 40 tokens and 40 is what they are told.
        let charged = usage(120, 10).charged_to_caller(40);
        assert_eq!(charged.prompt_tokens, 40);
    }

    #[test]
    fn a_backend_that_undercounts_the_prompt_does_not_move_the_caller_either() {
        // A backend that counts the prompt more cheaply than the relay does is
        // a real case, and it is still not the caller's number. Theirs does not
        // move because somebody else's tokenizer disagreed.
        let charged = usage(41, 27).charged_to_caller(40);
        assert_eq!(charged.prompt_tokens, 40);
    }

    #[test]
    fn six_thousand_in_is_six_thousand_back() {
        // The whole point, at the scale it actually bites: the caller wrote 6k
        // and the relay injected 4k on top. The backend bills for all 10k (and
        // counts it as 10_500 with its own tokenizer). The caller is told 6000.
        let charged = usage(10_500, 300).charged_to_caller(6_000);
        assert_eq!(charged.prompt_tokens, 6_000);
        assert_eq!(charged.total_tokens, 6_300);
    }

    #[test]
    fn nothing_injected_means_nothing_taken_off() {
        let charged = usage(100, 20).charged_to_caller(100);
        assert_eq!(charged.prompt_tokens, 100);
        assert_eq!(charged.total_tokens, 120);
    }

    #[test]
    fn a_shorter_replacement_prompt_still_charges_what_the_caller_sent() {
        // The route replaced the caller's long system prompt with a short one,
        // so less went upstream than came in. The caller is still accounted for
        // the 200 tokens they sent: what the relay chose to drop on the way out
        // is the relay's decision, not a discount it owes them. The margin is
        // the operator's to set in the price list, not something to smuggle
        // into a token count.
        let charged = usage(80, 5).charged_to_caller(200);
        assert_eq!(charged.prompt_tokens, 200);
    }

    #[test]
    fn an_empty_prompt_counts_as_nothing() {
        let charged = usage(0, 0).charged_to_caller(0);
        assert_eq!(charged.prompt_tokens, 0);
        assert_eq!(charged.total_tokens, 0);
    }

    #[test]
    fn a_cache_hit_can_never_be_larger_than_the_prompt_it_is_part_of() {
        // The cached figure is the backend's, over the whole injected body; the
        // prompt is the relay's, over the caller's half of it. Carried across
        // unclamped, a caller could be shown more cached tokens than tokens.
        let billed = Usage {
            cached_tokens: 900,
            ..usage(1_000, 10)
        };
        let charged = billed.charged_to_caller(400);
        assert_eq!(charged.cached_tokens, 400);
    }

    #[test]
    fn the_public_usage_carries_the_cost_and_prunes_what_is_empty() {
        let plain = usage(10, 5).charged_to_caller(10).public(None);
        assert_eq!(plain["prompt_tokens"], 10);
        assert!(plain.get("usage").is_none(), "no cost, no field");
        assert!(plain.get("prompt_tokens_details").is_none());
        assert!(plain.get("completion_tokens_details").is_none());

        let priced = Usage {
            cached_tokens: 4,
            reasoning_tokens: 3,
            ..usage(10, 5)
        }
        .charged_to_caller(10)
        .public(Some(0.102_949_9));
        assert_eq!(priced["usage"], 0.10295);
        assert_eq!(priced["prompt_tokens_details"]["cached_tokens"], 4);
        assert_eq!(priced["completion_tokens_details"]["reasoning_tokens"], 3);
    }
}
