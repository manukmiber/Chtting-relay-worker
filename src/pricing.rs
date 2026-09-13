//! What a request costs the relay, what it costs the caller, and the gap.
//!
//! Three numbers come out of here for every request:
//!
//! * **backend** — what the upstream provider charges the relay, at the rates
//!   the relay was quoted. Tiers never touch it: a markup of ours cannot change
//!   somebody else's invoice.
//! * **proxy** — what the caller is charged, at the relay's own rates after
//!   every matching tier has had its say.
//! * **profit** — proxy minus backend.
//!
//! Tiers are a list, not a setting. Any number of them may match one request
//! and every match applies in order, so "input over 256k" and "peak hour" and
//! "thinking effort high" stack instead of competing for one slot. A tier that
//! should end the matching says so with `stop`.
//!
//! Rates are quoted per million tokens, which is how every provider publishes
//! them, and carried as `f64` because these are multiplied and summed rather
//! than compared for equality.

use serde_json::Value;

use crate::config::{Model, Pricing, PricingTier};
use crate::util::{glob_match, round};

/// How hard the caller asked the model to think.
///
/// A normalised vocabulary, because the same intent arrives spelled four
/// different ways: OpenAI's `reasoning_effort`, OpenRouter's `reasoning.effort`,
/// Anthropic's `thinking.budget_tokens`, and Qwen's `enable_thinking`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Effort {
    /// The caller said nothing about thinking at all.
    #[default]
    Unspecified,
    /// The caller explicitly turned thinking off.
    None,
    Minimal,
    Low,
    Medium,
    High,
    Max,
}

impl Effort {
    pub fn as_str(self) -> &'static str {
        match self {
            Effort::Unspecified => "default",
            Effort::None => "none",
            Effort::Minimal => "minimal",
            Effort::Low => "low",
            Effort::Medium => "medium",
            Effort::High => "high",
            Effort::Max => "max",
        }
    }

    pub fn parse(name: &str) -> Option<Effort> {
        match name.trim().to_lowercase().as_str() {
            "default" | "unspecified" | "" => Some(Effort::Unspecified),
            "none" | "off" | "disabled" | "no" | "false" => Some(Effort::None),
            "minimal" | "min" => Some(Effort::Minimal),
            "low" => Some(Effort::Low),
            "medium" | "mid" | "moderate" => Some(Effort::Medium),
            "high" => Some(Effort::High),
            "max" | "maximum" | "xhigh" | "ultra" => Some(Effort::Max),
            _ => None,
        }
    }

    /// Where this sits on the low-to-high scale, for `minEffort`/`maxEffort`
    /// comparisons. `Unspecified` is deliberately outside the scale — it is the
    /// absence of a choice, not a point on it — so a ranked comparison skips it.
    pub fn rank(self) -> Option<u8> {
        match self {
            Effort::Unspecified => None,
            Effort::None => Some(0),
            Effort::Minimal => Some(1),
            Effort::Low => Some(2),
            Effort::Medium => Some(3),
            Effort::High => Some(4),
            Effort::Max => Some(5),
        }
    }

    /// True for the efforts that actually buy reasoning tokens, which is what
    /// decides between a model's thinking and non-thinking system prompt.
    pub fn is_thinking(self) -> bool {
        matches!(
            self,
            Effort::Low | Effort::Medium | Effort::High | Effort::Max
        )
    }
}

/// Read the caller's thinking request out of the body they sent.
///
/// Runs over the caller's own body, before any injection or forced parameter,
/// so what it reports is what the caller asked for rather than what the relay
/// decided on their behalf.
pub fn effort_of(body: &Value) -> Effort {
    // The explicit spellings first: if the caller named an effort, that is the
    // answer and no budget heuristic should second-guess it.
    for path in [
        body.get("reasoning_effort"),
        body.get("reasoning").and_then(|r| r.get("effort")),
        body.get("thinking").and_then(|t| t.get("effort")),
        body.get("extra_body")
            .and_then(|e| e.get("reasoning"))
            .and_then(|r| r.get("effort")),
    ] {
        if let Some(name) = path.and_then(|v| v.as_str()) {
            if let Some(effort) = Effort::parse(name) {
                if effort != Effort::Unspecified {
                    return effort;
                }
            }
        }
    }

    // Switches that can only say on or off.
    for flag in [
        body.get("reasoning").and_then(|r| r.get("enabled")),
        body.get("enable_thinking"),
        body.get("thinking").and_then(|t| t.get("enabled")),
    ] {
        match flag.and_then(|v| v.as_bool()) {
            Some(false) => return Effort::None,
            // "on" with no level is a choice to think, but not which level;
            // the budget below is what can still say how much.
            Some(true) => break,
            None => {}
        }
    }
    if body
        .get("thinking")
        .and_then(|t| t.get("type"))
        .and_then(|v| v.as_str())
        .is_some_and(|t| t.eq_ignore_ascii_case("disabled"))
    {
        return Effort::None;
    }

    // A token budget, which every Anthropic-shaped client sends instead of a
    // level. The boundaries follow Anthropic's own published guidance on what
    // counts as a small, medium or large thinking budget.
    let budget = body
        .get("thinking")
        .and_then(|t| t.get("budget_tokens"))
        .or_else(|| body.get("reasoning").and_then(|r| r.get("max_tokens")))
        .or_else(|| body.get("thinking_budget"))
        .and_then(|v| v.as_u64());
    if let Some(budget) = budget {
        return match budget {
            0 => Effort::None,
            1..=2_048 => Effort::Low,
            2_049..=8_192 => Effort::Medium,
            8_193..=32_768 => Effort::High,
            _ => Effort::Max,
        };
    }

    if body
        .get("enable_thinking")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
    {
        return Effort::Medium;
    }
    Effort::Unspecified
}

/* ------------------------------------------------------------- pricing -- */

/// Everything a tier may be asked about.
#[derive(Debug, Clone, Default)]
pub struct Shape {
    pub model_id: String,
    pub input_tokens: u64,
    pub cached_input_tokens: u64,
    pub output_tokens: u64,
    pub reasoning_tokens: u64,
    pub effort: Option<Effort>,
    /// Hour of day, 0-23, in the relay's configured zone — never UTC, or a
    /// peak-hour rule would fire at the wrong time of day.
    pub hour: u32,
    /// Day of week, Monday = 0 through Sunday = 6.
    pub weekday: u32,
    pub streamed: bool,
}

/// What one request came to.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Priced {
    /// USD the upstream provider charges the relay.
    pub backend_usd: f64,
    /// USD the caller is charged.
    pub proxy_usd: f64,
    /// `proxy_usd - backend_usd`. Negative is possible and is not hidden: a
    /// tier that discounts below cost should be visible as a loss.
    pub profit_usd: f64,
    /// The names of the tiers that actually applied, for the log line and the
    /// request drawer.
    pub tiers: Vec<String>,
}

/// The four sell-side rates, per million tokens, as a tier chain leaves them.
#[derive(Debug, Clone, Copy)]
struct Rates {
    input: f64,
    cached_input: f64,
    output: f64,
    reasoning: f64,
}

/// Merge the global price list with a model's own.
///
/// Rates: a model's own non-zero rate wins, field by field, so a model can
/// reprice its output without restating its input. Tiers: the global list
/// first, then the model's, because a model's rules are the more specific and
/// should get the last word (and the chance to `stop`).
pub fn resolve(defaults: &Pricing, model: &Model) -> Pricing {
    let route = &model.pricing;
    let pick = |a: f64, b: f64| if a > 0.0 { a } else { b };
    let mut tiers = defaults.tiers.clone();
    tiers.extend(route.tiers.iter().cloned());

    Pricing {
        enabled: defaults.enabled || route.enabled,
        currency: if route.currency.is_empty() {
            defaults.currency.clone()
        } else {
            route.currency.clone()
        },
        backend_input_usd_per_m: pick(
            route.backend_input_usd_per_m,
            defaults.backend_input_usd_per_m,
        ),
        backend_cached_input_usd_per_m: pick(
            route.backend_cached_input_usd_per_m,
            defaults.backend_cached_input_usd_per_m,
        ),
        backend_output_usd_per_m: pick(
            route.backend_output_usd_per_m,
            defaults.backend_output_usd_per_m,
        ),
        backend_reasoning_usd_per_m: pick(
            route.backend_reasoning_usd_per_m,
            defaults.backend_reasoning_usd_per_m,
        ),
        input_usd_per_m: pick(route.input_usd_per_m, defaults.input_usd_per_m),
        cached_input_usd_per_m: pick(
            route.cached_input_usd_per_m,
            defaults.cached_input_usd_per_m,
        ),
        output_usd_per_m: pick(route.output_usd_per_m, defaults.output_usd_per_m),
        reasoning_usd_per_m: pick(route.reasoning_usd_per_m, defaults.reasoning_usd_per_m),
        margin_percent: pick(route.margin_percent, defaults.margin_percent),
        request_usd: pick(route.request_usd, defaults.request_usd),
        tiers,
    }
}

/// Price one request.
pub fn price(pricing: &Pricing, shape: &Shape) -> Priced {
    if !pricing.enabled {
        return Priced::default();
    }

    let backend = base_backend_rates(pricing);
    let backend_usd = charge(backend, shape) + pricing.request_usd.max(0.0) * 0.0;

    // The sell side starts either from an explicit rate or from the backend's
    // rate plus the margin, field by field, so setting one explicitly does not
    // silently drop the margin from the others.
    let margin = 1.0 + pricing.margin_percent / 100.0;
    let sell = |explicit: f64, cost: f64| {
        if explicit > 0.0 {
            explicit
        } else {
            cost * margin
        }
    };
    let mut rates = Rates {
        input: sell(pricing.input_usd_per_m, backend.input),
        cached_input: sell(pricing.cached_input_usd_per_m, backend.cached_input),
        output: sell(pricing.output_usd_per_m, backend.output),
        reasoning: sell(pricing.reasoning_usd_per_m, backend.reasoning),
    };

    let mut applied = Vec::new();
    let mut surcharge = 0.0;
    for tier in pricing.tiers.iter().filter(|t| t.enabled) {
        if !matches(tier, shape) {
            continue;
        }
        applied.push(if tier.name.is_empty() {
            tier.id.clone()
        } else {
            tier.name.clone()
        });
        apply(tier, &mut rates);
        surcharge += tier.surcharge_usd;
        if tier.stop {
            break;
        }
    }

    let proxy_usd = charge(rates, shape) + pricing.request_usd + surcharge;
    Priced {
        backend_usd: round(backend_usd.max(0.0), 9),
        proxy_usd: round(proxy_usd.max(0.0), 9),
        profit_usd: round(proxy_usd.max(0.0) - backend_usd.max(0.0), 9),
        tiers: applied,
    }
}

fn base_backend_rates(p: &Pricing) -> Rates {
    let output = p.backend_output_usd_per_m;
    Rates {
        input: p.backend_input_usd_per_m,
        // An unset cached rate means the provider does not discount cache hits,
        // not that they are free.
        cached_input: if p.backend_cached_input_usd_per_m > 0.0 {
            p.backend_cached_input_usd_per_m
        } else {
            p.backend_input_usd_per_m
        },
        output,
        // Reasoning tokens bill as output unless the provider prices them apart.
        reasoning: if p.backend_reasoning_usd_per_m > 0.0 {
            p.backend_reasoning_usd_per_m
        } else {
            output
        },
    }
}

/// Tokens times rates. Cached input and reasoning are carved out of the totals
/// they arrive inside, so nothing is charged twice.
fn charge(rates: Rates, shape: &Shape) -> f64 {
    const PER_M: f64 = 1_000_000.0;
    let cached = shape.cached_input_tokens.min(shape.input_tokens);
    let fresh_input = shape.input_tokens - cached;
    let reasoning = shape.reasoning_tokens.min(shape.output_tokens);
    let plain_output = shape.output_tokens - reasoning;

    (fresh_input as f64 * rates.input
        + cached as f64 * rates.cached_input
        + plain_output as f64 * rates.output
        + reasoning as f64 * rates.reasoning)
        / PER_M
}

fn apply(tier: &PricingTier, rates: &mut Rates) {
    let one = |m: f64| if m > 0.0 { m } else { 1.0 };
    match tier.input_usd_per_m {
        Some(rate) if rate >= 0.0 => {
            rates.input = rate;
            // A cached rate of its own, or the same override: a tier that
            // repriced input without mentioning the cache meant both.
            rates.cached_input = tier.cached_input_usd_per_m.unwrap_or(rate);
        }
        _ => {
            rates.input *= one(tier.input_multiplier);
            rates.cached_input *= one(tier.input_multiplier);
            if let Some(rate) = tier.cached_input_usd_per_m {
                rates.cached_input = rate;
            }
        }
    }
    match tier.output_usd_per_m {
        Some(rate) if rate >= 0.0 => rates.output = rate,
        _ => rates.output *= one(tier.output_multiplier),
    }
    // Reasoning follows output unless the tier speaks about it, which is what
    // makes "thinking costs 1.5x" one field rather than two.
    match tier.reasoning_usd_per_m {
        Some(rate) if rate >= 0.0 => rates.reasoning = rate,
        _ => {
            rates.reasoning *= one(tier.output_multiplier) * one(tier.reasoning_multiplier);
        }
    }
}

/// Does this tier describe this request?
///
/// Every condition that is set must hold. An unset condition is not a
/// condition — an empty `efforts` list matches every effort rather than none,
/// which is what makes a tier with one condition readable.
fn matches(tier: &PricingTier, shape: &Shape) -> bool {
    let w = &tier.when;

    if !w.models.is_empty() && !w.models.iter().any(|p| glob_match(&shape.model_id, p)) {
        return false;
    }

    if !w.efforts.is_empty() {
        let effort = shape.effort.unwrap_or_default();
        if !w
            .efforts
            .iter()
            .filter_map(|name| Effort::parse(name))
            .any(|e| e == effort)
        {
            return false;
        }
    }
    // Ranked bounds skip a request that never named an effort: "high and above"
    // is a statement about callers who chose, and silence is not a choice.
    if !w.min_effort.is_empty() || !w.max_effort.is_empty() {
        let Some(rank) = shape.effort.unwrap_or_default().rank() else {
            return false;
        };
        if let Some(min) = Effort::parse(&w.min_effort).and_then(Effort::rank) {
            if rank < min {
                return false;
            }
        }
        if let Some(max) = Effort::parse(&w.max_effort).and_then(Effort::rank) {
            if rank > max {
                return false;
            }
        }
    }

    if !w.weekdays.is_empty() && !w.weekdays.contains(&shape.weekday) {
        return false;
    }
    if !w.hours.is_empty() && !w.hours.iter().any(|r| hour_in(r.from, r.to, shape.hour)) {
        return false;
    }

    if shape.input_tokens < w.min_input_tokens {
        return false;
    }
    if w.max_input_tokens > 0 && shape.input_tokens > w.max_input_tokens {
        return false;
    }
    if shape.output_tokens < w.min_output_tokens {
        return false;
    }
    if w.max_output_tokens > 0 && shape.output_tokens > w.max_output_tokens {
        return false;
    }
    let total = shape.input_tokens + shape.output_tokens;
    if total < w.min_total_tokens {
        return false;
    }
    if w.max_total_tokens > 0 && total > w.max_total_tokens {
        return false;
    }

    if let Some(want) = w.streamed {
        if want != shape.streamed {
            return false;
        }
    }
    if let Some(want) = w.cache_hit {
        if want != (shape.cached_input_tokens > 0) {
            return false;
        }
    }
    true
}

/// Inclusive on both ends, and wrapping: `from: 22, to: 5` is the night shift.
fn hour_in(from: u32, to: u32, hour: u32) -> bool {
    if from <= to {
        hour >= from && hour <= to
    } else {
        hour >= from || hour <= to
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{HourRange, TierWhen};
    use serde_json::json;

    fn pricing() -> Pricing {
        Pricing {
            enabled: true,
            backend_input_usd_per_m: 0.28,
            backend_output_usd_per_m: 0.42,
            margin_percent: 100.0,
            ..Default::default()
        }
    }

    fn tier(name: &str, when: TierWhen) -> PricingTier {
        PricingTier {
            id: name.into(),
            name: name.into(),
            enabled: true,
            when,
            ..Default::default()
        }
    }

    fn shape(input: u64, output: u64) -> Shape {
        Shape {
            model_id: "wissanggeni-512B-V1".into(),
            input_tokens: input,
            output_tokens: output,
            ..Default::default()
        }
    }

    #[test]
    fn the_margin_is_what_separates_proxy_from_backend() {
        let out = price(&pricing(), &shape(1_000_000, 1_000_000));
        assert_eq!(out.backend_usd, 0.7);
        // 100% margin on both rates.
        assert_eq!(out.proxy_usd, 1.4);
        assert_eq!(out.profit_usd, 0.7);
        assert!(out.tiers.is_empty());
    }

    #[test]
    fn pricing_switched_off_charges_nothing_rather_than_guessing() {
        let mut p = pricing();
        p.enabled = false;
        assert_eq!(price(&p, &shape(1_000_000, 1_000_000)), Priced::default());
    }

    #[test]
    fn every_matching_tier_applies_and_they_stack() {
        let mut p = pricing();
        p.tiers = vec![
            PricingTier {
                input_multiplier: 2.0,
                ..tier(
                    "long input",
                    TierWhen {
                        min_input_tokens: 256_000,
                        ..Default::default()
                    },
                )
            },
            PricingTier {
                input_multiplier: 1.5,
                output_multiplier: 1.5,
                ..tier(
                    "peak hour",
                    TierWhen {
                        hours: vec![HourRange { from: 19, to: 23 }],
                        ..Default::default()
                    },
                )
            },
            PricingTier {
                reasoning_multiplier: 3.0,
                ..tier(
                    "hard thinking",
                    TierWhen {
                        min_effort: "high".into(),
                        ..Default::default()
                    },
                )
            },
        ];

        let shape = Shape {
            model_id: "wissanggeni-512B-V1".into(),
            input_tokens: 300_000,
            output_tokens: 1_000,
            reasoning_tokens: 1_000,
            effort: Some(Effort::Max),
            hour: 20,
            ..Default::default()
        };
        let out = price(&p, &shape);
        assert_eq!(
            out.tiers,
            vec!["long input", "peak hour", "hard thinking"],
            "a request can sit in more than one category at once"
        );

        // input: 0.56 base * 2 * 1.5 = 1.68/M over 300k tokens
        // reasoning: 0.84 base * 1.5 (output mult) * 3 = 3.78/M over 1k tokens
        let expected = 300_000.0 * 1.68 / 1e6 + 1_000.0 * 3.78 / 1e6;
        assert!((out.proxy_usd - expected).abs() < 1e-9, "{out:?}");
        // The backend's invoice is untouched by our own markup.
        let backend = (300_000.0 * 0.28 + 1_000.0 * 0.42) / 1e6;
        assert!((out.backend_usd - backend).abs() < 1e-9, "{out:?}");
    }

    #[test]
    fn a_tier_that_stops_hides_the_ones_after_it() {
        let mut p = pricing();
        p.tiers = vec![
            PricingTier {
                input_multiplier: 2.0,
                stop: true,
                ..tier("flat deal", TierWhen::default())
            },
            PricingTier {
                input_multiplier: 10.0,
                ..tier("never reached", TierWhen::default())
            },
        ];
        let out = price(&p, &shape(1_000_000, 0));
        assert_eq!(out.tiers, vec!["flat deal"]);
        assert!((out.proxy_usd - 1.12).abs() < 1e-9, "{out:?}");
    }

    #[test]
    fn an_absolute_override_beats_the_multiplier_chain() {
        let mut p = pricing();
        p.tiers = vec![PricingTier {
            input_usd_per_m: Some(9.0),
            ..tier("premium input", TierWhen::default())
        }];
        let out = price(&p, &shape(1_000_000, 0));
        assert_eq!(out.proxy_usd, 9.0);
    }

    #[test]
    fn a_disabled_tier_is_not_consulted() {
        let mut p = pricing();
        p.tiers = vec![PricingTier {
            input_multiplier: 5.0,
            enabled: false,
            ..tier("off", TierWhen::default())
        }];
        let out = price(&p, &shape(1_000_000, 0));
        assert_eq!(out.proxy_usd, 0.56);
        assert!(out.tiers.is_empty());
    }

    #[test]
    fn a_night_shift_window_wraps_past_midnight() {
        let when = TierWhen {
            hours: vec![HourRange { from: 22, to: 5 }],
            ..Default::default()
        };
        let t = tier("night", when);
        for hour in [22, 23, 0, 3, 5] {
            assert!(
                matches(
                    &t,
                    &Shape {
                        hour,
                        ..shape(1, 1)
                    }
                ),
                "hour {hour}"
            );
        }
        for hour in [6, 12, 21] {
            assert!(
                !matches(
                    &t,
                    &Shape {
                        hour,
                        ..shape(1, 1)
                    }
                ),
                "hour {hour}"
            );
        }
    }

    #[test]
    fn cached_input_is_charged_once_at_its_own_rate() {
        let mut p = pricing();
        p.backend_cached_input_usd_per_m = 0.028;
        let out = price(
            &p,
            &Shape {
                input_tokens: 1_000_000,
                cached_input_tokens: 750_000,
                ..shape(1_000_000, 0)
            },
        );
        let expected = (250_000.0 * 0.28 + 750_000.0 * 0.028) / 1e6;
        assert!((out.backend_usd - expected).abs() < 1e-9, "{out:?}");
    }

    #[test]
    fn a_models_own_rates_and_tiers_win_over_the_global_ones() {
        let defaults = Pricing {
            enabled: true,
            backend_input_usd_per_m: 0.28,
            backend_output_usd_per_m: 0.42,
            margin_percent: 50.0,
            tiers: vec![tier("global", TierWhen::default())],
            ..Default::default()
        };
        let model = Model {
            pricing: Pricing {
                backend_output_usd_per_m: 1.0,
                tiers: vec![tier("model", TierWhen::default())],
                ..Default::default()
            },
            ..Default::default()
        };
        let merged = resolve(&defaults, &model);
        assert_eq!(merged.backend_input_usd_per_m, 0.28, "inherited");
        assert_eq!(merged.backend_output_usd_per_m, 1.0, "overridden");
        assert_eq!(merged.margin_percent, 50.0);
        assert_eq!(
            merged
                .tiers
                .iter()
                .map(|t| t.id.as_str())
                .collect::<Vec<_>>(),
            vec!["global", "model"],
            "the model's rules get the last word"
        );
    }

    #[test]
    fn every_spelling_of_an_effort_is_understood() {
        assert_eq!(
            effort_of(&json!({"reasoning_effort": "high"})),
            Effort::High
        );
        assert_eq!(
            effort_of(&json!({"reasoning_effort": "XHIGH"})),
            Effort::Max
        );
        assert_eq!(
            effort_of(&json!({"reasoning": {"effort": "low"}})),
            Effort::Low
        );
        assert_eq!(
            effort_of(&json!({"reasoning": {"enabled": false}})),
            Effort::None
        );
        assert_eq!(
            effort_of(&json!({"thinking": {"type": "disabled"}})),
            Effort::None
        );
        assert_eq!(
            effort_of(&json!({"thinking": {"budget_tokens": 1024}})),
            Effort::Low
        );
        assert_eq!(
            effort_of(&json!({"thinking": {"budget_tokens": 24000}})),
            Effort::High
        );
        assert_eq!(
            effort_of(&json!({"thinking": {"budget_tokens": 60000}})),
            Effort::Max
        );
        assert_eq!(effort_of(&json!({"enable_thinking": false})), Effort::None);
        assert_eq!(effort_of(&json!({"enable_thinking": true})), Effort::Medium);
        assert_eq!(effort_of(&json!({"messages": []})), Effort::Unspecified);
        // An explicit level is never overridden by a budget sitting beside it.
        assert_eq!(
            effort_of(&json!({"reasoning_effort": "low", "thinking": {"budget_tokens": 60000}})),
            Effort::Low
        );
    }

    #[test]
    fn an_unspecified_effort_matches_no_ranked_tier_but_still_matches_an_open_one() {
        let ranked = tier(
            "thinkers",
            TierWhen {
                min_effort: "low".into(),
                ..Default::default()
            },
        );
        let open = tier("everyone", TierWhen::default());
        let unspecified = shape(10, 10);
        assert!(!matches(&ranked, &unspecified));
        assert!(matches(&open, &unspecified));
    }
}
