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

/// Whose prompt a backend cache hit is credited against.
///
/// The relay injects a system prompt in front of the caller's body, and the
/// backend's cache hit covers the whole thing. Which of the two is being billed
/// decides how much of that hit is a discount the caller earned.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum CacheCredit {
    /// The caller is billed for their own body only, so the injected prefix in
    /// front of it comes off the hit before any of it is credited to them.
    #[default]
    CallersBodyOnly,
    /// The caller is billed for the whole upstream body, injection included, so
    /// the whole hit discounts tokens they are paying for.
    WholeBody,
}

/// How a backend cache hit divides between the relay and the caller.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CacheBasis {
    pub credit: CacheCredit,
    /// The relay's own count of the prompt it injected — a known quantity,
    /// measured on the way out rather than reconstructed afterwards. It is the
    /// floor under the prefix that comes off a hit.
    pub injected: u64,
    /// Below this many tokens of the caller's own prompt, nothing is credited
    /// and the whole prompt bills as fresh input. 0 disables the floor.
    pub floor: u64,
}

impl CacheBasis {
    /// No floor and no injected prompt: the caller's body is the whole body.
    #[cfg(test)]
    pub fn callers_body() -> Self {
        Self {
            credit: CacheCredit::CallersBodyOnly,
            injected: 0,
            floor: 0,
        }
    }

    /// No floor, whole hit credited.
    #[cfg(test)]
    pub fn whole_body() -> Self {
        Self {
            credit: CacheCredit::WholeBody,
            injected: 0,
            floor: 0,
        }
    }
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
    pub fn charged_to_caller(&self, user_local: u64, basis: CacheBasis) -> Usage {
        Usage {
            prompt_tokens: user_local,
            total_tokens: user_local + self.completion_tokens,
            cached_tokens: self.caller_cached(user_local, basis),
            ..self.clone()
        }
    }

    /// How much of the backend's cache hit was the caller's own prompt.
    ///
    /// A cache hit is not, on its own, a fact about the caller's prompt.
    /// Prefix caching matches from the first token forward, and the first
    /// tokens of the upstream body are the prompt the relay injected — so the
    /// leading cached tokens are the relay's own words coming back, and only
    /// what is left over ever reached the caller's body.
    ///
    /// The prefix is measured as the backend's own prompt count less the
    /// caller's body: everything the backend charged for beyond what the
    /// caller wrote is the relay's, whether it is injected prompt, chat
    /// template overhead, or the backend's tokenizer counting richer than
    /// ours. Taking the relay's local count of the injection instead would
    /// subtract a figure in this tokenizer's units from a hit in the backend's,
    /// and under-subtract by exactly the drift between them — which is worst
    /// in the case this exists for, where the backend reports thousands of
    /// cached tokens for a body the caller contributed a handful to.
    ///
    /// Clamping without the offset is what made short requests book a loss. A
    /// caller who sends 7 tokens behind a 3.7k-token injected prompt comes back
    /// with a cache hit in the thousands; `min` alone calls all 7 of them
    /// cached, bills every one at the cache rate, and applies the fresh input
    /// rate to nothing at all. The relay still pays the backend for the real
    /// body it sent, so the request earns a rounding error and costs real
    /// money. Subtracting the prefix first leaves those 7 tokens fresh, which
    /// is what they were.
    ///
    /// The clamp stays behind the offset: a backend that reports more cached
    /// tokens than the caller has is still not evidence of a bigger discount.
    fn caller_cached(&self, user_local: u64, basis: CacheBasis) -> u64 {
        // Under the floor the split is not worth trusting: it is drawn by
        // subtracting two tokenizers' counts, and on a short prompt that drift
        // is most of the answer. Bill the lot as fresh input.
        if user_local < basis.floor {
            return 0;
        }
        let offset = match basis.credit {
            // Two readings of where the caller's body starts, and the prefix
            // is the longer of them.
            //
            // `injected` is what the relay measured itself putting in front of
            // the caller — a known number, not a reconstruction, and the one
            // the operator can point at. The subtraction is everything the
            // backend charged for beyond the caller's body: the same injected
            // prompt, plus the chat template's own per-message overhead, plus
            // whatever the backend's tokenizer counts differently from ours.
            //
            // Neither is reliably the larger. The measured count misses the
            // template overhead sitting in front of the caller's first token;
            // the subtraction misses nothing but inherits the drift on the
            // caller's body, which is unbounded and varies per request. Taking
            // the longer prefix discounts both and credits the caller only what
            // clears each — never a discount the relay did not receive.
            CacheCredit::CallersBodyOnly => basis
                .injected
                .max(self.prompt_tokens.saturating_sub(user_local)),
            CacheCredit::WholeBody => 0,
        };
        self.cached_tokens.saturating_sub(offset).min(user_local)
    }

    /// The `usage` object handed back to the caller.
    ///
    /// `cost` is what this request came to under the price list the caller is
    /// actually on, which they otherwise have no way to work out: the rates are
    /// the relay's, not the backend's, and a band, a tier or a published time
    /// window can move them per request. It goes out under two names —
    /// `usage`, and `cost` for clients that read OpenRouter's spelling — and
    /// they are the same number, never two prices.
    ///
    /// Nine decimal places, which is the ledger's own precision. Six is not
    /// enough: a short request at a tenth of a dollar per million tokens comes
    /// to a few millionths of a cent, and rounding that to six places reports
    /// every small request as free.
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
            let cost = serde_json::json!(crate::util::round(cost, 9));
            map.insert("usage".into(), cost.clone());
            map.insert("cost".into(), cost);
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

    /// The report this fix came from: `{"content":"hai"}` behind a 3.7k-token
    /// injected prompt, billed to a caller who sent seven tokens.
    #[test]
    fn an_injected_prompts_cache_hit_is_not_the_callers_discount() {
        let billed = Usage {
            prompt_tokens: 3_941,
            completion_tokens: 8,
            cached_tokens: 3_712,
            ..Usage::default()
        };
        // 3,712 cached, all of it inside a 3,934-token injected prefix: none of
        // it reached the seven tokens the caller actually wrote.
        let charged = billed.charged_to_caller(7, CacheBasis::callers_body());
        assert_eq!(charged.prompt_tokens, 7);
        assert_eq!(
            charged.cached_tokens, 0,
            "the hit was the relay's own prompt"
        );
        assert_eq!(charged.total_tokens, 15);
        // What the clamp alone used to do, and why the request lost money: all
        // seven tokens at the cache rate, nothing left at the input rate.
        assert_eq!(billed.cached_tokens.min(7), 7);
    }

    /// The worked example from the design sketch: a 3,710-token injected
    /// prompt, 109 tokens of the caller's own, and a backend hit of 3,802 —
    /// which reaches 92 tokens past the injection into what the caller wrote.
    #[test]
    fn the_known_injected_count_draws_the_line_the_hit_is_measured_from() {
        let billed = Usage {
            prompt_tokens: 3_819,
            completion_tokens: 40,
            cached_tokens: 3_802,
            ..Usage::default()
        };
        let basis = CacheBasis {
            credit: CacheCredit::CallersBodyOnly,
            injected: 3_710,
            floor: 0,
        };
        let charged = billed.charged_to_caller(109, basis);
        assert_eq!(charged.cached_tokens, 92, "3,802 - 3,710");
        assert_eq!(charged.prompt_tokens, 109);
        // So 17 of the caller's 109 tokens are fresh.
        assert_eq!(charged.prompt_tokens - charged.cached_tokens, 17);
    }

    #[test]
    fn the_floor_overrides_that_split_on_a_short_prompt() {
        // The same request under the 2048 rule: the split is correct and is
        // still not used, because 109 tokens of discount is worth less than
        // the confidence it would take to trust it.
        let billed = Usage {
            prompt_tokens: 3_819,
            cached_tokens: 3_802,
            ..Usage::default()
        };
        let basis = CacheBasis {
            credit: CacheCredit::CallersBodyOnly,
            injected: 3_710,
            floor: 2_048,
        };
        let charged = billed.charged_to_caller(109, basis);
        assert_eq!(charged.cached_tokens, 0);
        assert_eq!(charged.prompt_tokens, 109, "billed full fresh input");
    }

    #[test]
    fn the_longer_of_the_two_prefixes_wins() {
        let billed = Usage {
            prompt_tokens: 4_000,
            cached_tokens: 3_800,
            ..Usage::default()
        };
        // Measured injection 3,710, but the backend charged 4,000 for a body
        // the caller contributed 109 to — 3,891 of it was not theirs. The
        // extra 181 is chat template and tokenizer drift, and it sits in front
        // of their first token just as the injection does.
        let basis = CacheBasis {
            credit: CacheCredit::CallersBodyOnly,
            injected: 3_710,
            floor: 0,
        };
        assert_eq!(
            billed.charged_to_caller(109, basis).cached_tokens,
            0,
            "3,800 does not clear the 3,891-token prefix"
        );

        // The other way round: the backend counts the caller's body richer
        // than we do, so the subtraction reads a short prefix and the measured
        // injection is the honest one.
        let rich = Usage {
            prompt_tokens: 3_750,
            cached_tokens: 3_740,
            ..Usage::default()
        };
        assert_eq!(
            rich.charged_to_caller(109, basis).cached_tokens,
            30,
            "3,740 - 3,710, not 3,740 - 3,641"
        );
    }

    #[test]
    fn under_the_floor_a_hit_buys_the_caller_nothing() {
        // Even a backend calling the whole prompt cached: under the floor the
        // caller's tokens bill as fresh input, because the split between their
        // body and the injected prefix is drawn by subtracting two tokenizers'
        // counts and is worth least exactly where it is least reliable.
        let billed = Usage {
            prompt_tokens: 1_500,
            completion_tokens: 8,
            cached_tokens: 1_500,
            ..Usage::default()
        };
        let basis = CacheBasis {
            credit: CacheCredit::CallersBodyOnly,
            injected: 0,
            floor: 2_048,
        };
        assert_eq!(billed.charged_to_caller(1_500, basis).cached_tokens, 0);
        // And with the prefix out of the picture entirely.
        let whole = CacheBasis {
            credit: CacheCredit::WholeBody,
            injected: 0,
            floor: 2_048,
        };
        assert_eq!(billed.charged_to_caller(1_500, whole).cached_tokens, 0);
    }

    #[test]
    fn at_the_floor_the_offset_takes_over_again() {
        let billed = Usage {
            prompt_tokens: 12_000,
            completion_tokens: 8,
            cached_tokens: 9_000,
            ..Usage::default()
        };
        let basis = CacheBasis {
            credit: CacheCredit::CallersBodyOnly,
            injected: 0,
            floor: 2_048,
        };
        // Exactly at the floor: credited, and the 9,952-token prefix comes off
        // the hit, which leaves nothing of it for the caller.
        assert_eq!(billed.charged_to_caller(2_048, basis).cached_tokens, 0);
        // One token under, and the floor answers instead of the offset. Same
        // result here, reached a different way — the assertion that matters is
        // that neither route ever credits more than the offset would.
        assert_eq!(billed.charged_to_caller(2_047, basis).cached_tokens, 0);

        // Well past the floor, the hit clears the prefix and is credited.
        let long = Usage {
            prompt_tokens: 26_000,
            cached_tokens: 22_000,
            ..Usage::default()
        };
        assert_eq!(
            long.charged_to_caller(22_300, basis).cached_tokens,
            18_300,
            "a real conversation still earns its discount"
        );
    }

    #[test]
    fn a_zero_floor_leaves_the_offset_in_sole_charge() {
        let billed = Usage {
            prompt_tokens: 1_500,
            cached_tokens: 1_500,
            ..Usage::default()
        };
        let basis = CacheBasis {
            credit: CacheCredit::CallersBodyOnly,
            injected: 0,
            floor: 0,
        };
        // 1,500 cached over a 1,500-token prompt the caller wrote all of: no
        // prefix to subtract, so the hit is theirs. Turning the floor off is
        // what makes that reachable.
        assert_eq!(billed.charged_to_caller(1_500, basis).cached_tokens, 1_500);
    }

    #[test]
    fn a_hit_reaching_past_the_injected_prefix_is_credited_to_the_caller() {
        let billed = Usage {
            prompt_tokens: 26_000,
            completion_tokens: 600,
            cached_tokens: 22_000,
            ..Usage::default()
        };
        // A long conversation: the hit covers the 3,700-token injection and
        // 18,300 tokens of the caller's own history, which is a real discount.
        let charged = billed.charged_to_caller(22_300, CacheBasis::callers_body());
        assert_eq!(charged.cached_tokens, 18_300);
        assert_eq!(charged.prompt_tokens, 22_300);
    }

    #[test]
    fn the_callers_share_of_a_hit_never_exceeds_their_own_prompt() {
        let billed = Usage {
            prompt_tokens: 400,
            cached_tokens: 900,
            ..Usage::default()
        };
        // A backend reporting more cached tokens than it counted prompt
        // tokens: there is no prefix left to subtract, and the hit still
        // cannot be larger than the 500-token body it is part of.
        assert_eq!(
            billed
                .charged_to_caller(500, CacheBasis::callers_body())
                .cached_tokens,
            500
        );
    }

    #[test]
    fn billing_the_injected_prompt_to_the_caller_passes_the_whole_hit_on() {
        let billed = Usage {
            prompt_tokens: 3_941,
            completion_tokens: 8,
            cached_tokens: 3_712,
            ..Usage::default()
        };
        // `bill_system_prompt_to_user` charges them for the full body, so the
        // offset is zero and the cache discount over it is theirs.
        let charged = billed.charged_to_caller(3_941, CacheBasis::whole_body());
        assert_eq!(charged.cached_tokens, 3_712);
    }

    #[test]
    fn a_shorter_injected_prompt_leaves_nothing_to_discount() {
        let billed = Usage {
            prompt_tokens: 800,
            cached_tokens: 600,
            ..Usage::default()
        };
        // The route replaced a longer system prompt of the caller's with a
        // shorter one: `injected` was negative and the handler floors it at 0,
        // so the hit stays the caller's.
        assert_eq!(
            billed
                .charged_to_caller(1_000, CacheBasis::callers_body())
                .cached_tokens,
            600
        );
    }

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
        let charged = usage(100, 20).charged_to_caller(40, CacheBasis::callers_body());
        assert_eq!(charged.prompt_tokens, 40);
        assert_eq!(charged.completion_tokens, 20, "output is theirs entirely");
        assert_eq!(charged.total_tokens, 60);
    }

    #[test]
    fn a_richer_backend_tokenizer_never_inflates_what_the_caller_sent() {
        // The backend charged 120 for what the relay measured as 100, of which
        // 40 was the caller's. The backend's arithmetic is the backend's; the
        // caller wrote 40 tokens and 40 is what they are told.
        let charged = usage(120, 10).charged_to_caller(40, CacheBasis::callers_body());
        assert_eq!(charged.prompt_tokens, 40);
    }

    #[test]
    fn a_backend_that_undercounts_the_prompt_does_not_move_the_caller_either() {
        // A backend that counts the prompt more cheaply than the relay does is
        // a real case, and it is still not the caller's number. Theirs does not
        // move because somebody else's tokenizer disagreed.
        let charged = usage(41, 27).charged_to_caller(40, CacheBasis::callers_body());
        assert_eq!(charged.prompt_tokens, 40);
    }

    #[test]
    fn six_thousand_in_is_six_thousand_back() {
        // The whole point, at the scale it actually bites: the caller wrote 6k
        // and the relay injected 4k on top. The backend bills for all 10k (and
        // counts it as 10_500 with its own tokenizer). The caller is told 6000.
        let charged = usage(10_500, 300).charged_to_caller(6_000, CacheBasis::callers_body());
        assert_eq!(charged.prompt_tokens, 6_000);
        assert_eq!(charged.total_tokens, 6_300);
    }

    #[test]
    fn nothing_injected_means_nothing_taken_off() {
        let charged = usage(100, 20).charged_to_caller(100, CacheBasis::callers_body());
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
        let charged = usage(80, 5).charged_to_caller(200, CacheBasis::callers_body());
        assert_eq!(charged.prompt_tokens, 200);
    }

    #[test]
    fn an_empty_prompt_counts_as_nothing() {
        let charged = usage(0, 0).charged_to_caller(0, CacheBasis::callers_body());
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
        // 600 injected, so 300 of the hit is the caller's by the offset — but
        // a backend that reports 900 cached against a 400-token body is over
        // its own prompt either way, and the clamp is the backstop.
        assert_eq!(
            billed
                .charged_to_caller(400, CacheBasis::callers_body())
                .cached_tokens,
            300
        );
        let charged = billed.charged_to_caller(400, CacheBasis::whole_body());
        assert_eq!(charged.cached_tokens, 400);
    }

    #[test]
    fn the_public_usage_carries_the_cost_and_prunes_what_is_empty() {
        let plain = usage(10, 5)
            .charged_to_caller(10, CacheBasis::callers_body())
            .public(None);
        assert_eq!(plain["prompt_tokens"], 10);
        assert!(plain.get("usage").is_none(), "no cost, no field");
        assert!(plain.get("cost").is_none());
        assert!(plain.get("prompt_tokens_details").is_none());
        assert!(plain.get("completion_tokens_details").is_none());

        let priced = Usage {
            cached_tokens: 4,
            reasoning_tokens: 3,
            ..usage(10, 5)
        }
        .charged_to_caller(10, CacheBasis::callers_body())
        .public(Some(0.102_949_9));
        assert_eq!(priced["usage"], 0.102_949_9);
        // The same number under OpenRouter's spelling, never a second price.
        assert_eq!(priced["cost"], priced["usage"]);
        assert_eq!(priced["prompt_tokens_details"]["cached_tokens"], 4);
        assert_eq!(priced["completion_tokens_details"]["reasoning_tokens"], 3);

        // A short request at a tenth of a dollar per million tokens: six
        // decimal places reported this as free, which is the bug.
        let small = usage(61, 190)
            .charged_to_caller(61, CacheBasis::callers_body())
            .public(Some(0.000_009_15));
        assert_eq!(small["usage"], 0.000_009_15);
    }
}
