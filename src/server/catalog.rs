//! The public model listing, served at `/models` and `/v1/models`.
//!
//! One document, in the shape the wider ecosystem already reads: OpenAI's
//! envelope on the outside (`object: "list"`, `object: "model"`, `owned_by`) so
//! an unmodified OpenAI client still works, and OpenRouter's model document on
//! the inside — `architecture`, `pricing`, `top_provider`,
//! `supported_parameters`, `reasoning` — so a caller can read what a model
//! costs and what it accepts without asking anybody.
//!
//! Two rules hold throughout, the same two the provider document keeps. The
//! backend's real model name never appears. And no price is invented: a rate
//! nobody set is either derived from the relay's own rate card — the same
//! numbers in a different unit, which is arithmetic rather than invention — or
//! left out entirely. A missing price is a question; a wrong one is a bill.
//!
//! `pricing.overrides` is what makes the listing complete rather than
//! approximate: these models are not sold at one rate, they are sold at a rate
//! that moves with the hour and the day, and a listing that publishes only the
//! off-peak number is a listing nobody can reconcile an invoice against.

use serde_json::{json, Map, Value};

use crate::config::{Config, Model, OpenRouterPricing, PricingOverride, RequestTransform};
use crate::pricing::Effort;
use crate::util::per_token;

/// The whole listing, in the OpenAI envelope every client already unwraps.
pub fn document(cfg: &Config) -> Value {
    let data: Vec<Value> = cfg
        .models
        .iter()
        .filter(|m| m.enabled)
        .map(|m| model_document(m, cfg))
        .collect();
    json!({ "object": "list", "data": data })
}

/// One model's entry.
pub fn model_document(model: &Model, cfg: &Config) -> Value {
    let o = &model.openrouter;
    let mut doc = Map::new();

    doc.insert("id".into(), json!(model.id));
    // The dated name of this exact snapshot, beside the moving `id`. A caller
    // pinning a behaviour pins this one.
    doc.insert(
        "canonical_slug".into(),
        json!(if o.canonical_slug.is_empty() {
            &model.id
        } else {
            &o.canonical_slug
        }),
    );
    doc.insert("hugging_face_id".into(), or_null(&o.hugging_face_id));
    doc.insert(
        "name".into(),
        json!(if model.display_name.is_empty() {
            &model.id
        } else {
            &model.display_name
        }),
    );
    doc.insert(
        "created".into(),
        json!(if o.created > 0 {
            o.created
        } else {
            model.created_at / 1000
        }),
    );
    doc.insert("description".into(), json!(model.description));
    doc.insert("context_length".into(), json!(context_length(model)));
    doc.insert("architecture".into(), architecture(model, cfg));

    let pricing = pricing(model, cfg);
    if !pricing.is_empty() {
        doc.insert("pricing".into(), Value::Object(pricing));
    }

    doc.insert("top_provider".into(), top_provider(model));
    doc.insert("per_request_limits".into(), Value::Null);
    doc.insert(
        "supported_parameters".into(),
        json!(supported_parameters(model, cfg)),
    );
    doc.insert(
        "default_parameters".into(),
        Value::Object(default_parameters(model)),
    );
    doc.insert("supported_voices".into(), Value::Null);
    doc.insert("knowledge_cutoff".into(), or_null(&o.knowledge_cutoff));
    doc.insert("expiration_date".into(), or_null(&o.deprecation_date));
    doc.insert("links".into(), json!({}));
    if o.supports_reasoning {
        doc.insert("reasoning".into(), reasoning(model));
    }

    // The OpenAI half of the envelope, so a client that only knows `/v1/models`
    // still recognises what it is holding.
    doc.insert("object".into(), json!("model"));
    doc.insert(
        "owned_by".into(),
        json!(if model.owner.is_empty() {
            crate::config::DEFAULT_MODEL_OWNER
        } else {
            &model.owner
        }),
    );
    doc.insert(
        "display_name".into(),
        json!(if model.display_name.is_empty() {
            &model.id
        } else {
            &model.display_name
        }),
    );

    Value::Object(doc)
}

fn or_null(text: &str) -> Value {
    if text.is_empty() {
        Value::Null
    } else {
        json!(text)
    }
}

fn context_length(model: &Model) -> u32 {
    if model.context_length > 0 {
        model.context_length
    } else if model.openrouter.max_prompt_tokens > 0 {
        model.openrouter.max_prompt_tokens
    } else {
        model.limits.max_input_tokens
    }
}

fn max_output(model: &Model) -> u32 {
    if model.openrouter.max_output_tokens > 0 {
        model.openrouter.max_output_tokens
    } else {
        model.limits.max_output_tokens
    }
}

/* -------------------------------------------------------- architecture -- */

fn architecture(model: &Model, cfg: &Config) -> Value {
    let o = &model.openrouter;
    let inputs: Vec<String> = if o.input_modalities.is_empty() {
        vec!["text".into()]
    } else {
        o.input_modalities.clone()
    };
    let outputs: Vec<String> = if o.output_modalities.is_empty() {
        vec!["text".into()]
    } else {
        o.output_modalities.clone()
    };
    json!({
        "modality": format!("{}->{}", inputs.join("+"), outputs.join("+")),
        "input_modalities": inputs,
        "output_modalities": outputs,
        "tokenizer": super::openrouter::tokenizer_family(model, cfg),
        "instruct_type": or_null(&o.instruct_type),
    })
}

fn top_provider(model: &Model) -> Value {
    let max_out = max_output(model);
    json!({
        "context_length": context_length(model),
        "max_completion_tokens": if max_out > 0 { json!(max_out) } else { Value::Null },
        "is_moderated": model.openrouter.is_moderated,
    })
}

/* ------------------------------------------------------------- pricing -- */

/// The per-token prices this model is published at, as decimal strings.
///
/// Strings rather than numbers all the way through, because a rate like
/// `0.000000003` loses its last digits through an `f64` and a caller checking
/// an invoice is comparing decimals, not floats.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Card {
    pub prompt: String,
    pub completion: String,
    pub cached_prompt: String,
    pub cache_write: String,
    pub internal_reasoning: String,
    pub request: String,
}

impl Card {
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }

    /// Lay another card over this one. A price the other leaves empty keeps the
    /// one already here — an override that only moves output is one number, and
    /// an unmentioned rate is never silently zeroed.
    fn over(&self, other: &Card) -> Card {
        let pick = |a: &str, b: &String| {
            if a.is_empty() {
                b.clone()
            } else {
                a.to_string()
            }
        };
        Card {
            prompt: pick(&other.prompt, &self.prompt),
            completion: pick(&other.completion, &self.completion),
            cached_prompt: pick(&other.cached_prompt, &self.cached_prompt),
            cache_write: pick(&other.cache_write, &self.cache_write),
            internal_reasoning: pick(&other.internal_reasoning, &self.internal_reasoning),
            request: pick(&other.request, &self.request),
        }
    }

    fn to_json(&self) -> Map<String, Value> {
        let mut out = Map::new();
        let mut put = |key: &str, value: &str| {
            if !value.is_empty() {
                out.insert(key.into(), json!(value));
            }
        };
        put("prompt", &self.prompt);
        put("completion", &self.completion);
        put("input_cache_read", &self.cached_prompt);
        put("input_cache_write", &self.cache_write);
        put("internal_reasoning", &self.internal_reasoning);
        put("request", &self.request);
        out
    }
}

fn card_of(p: &OpenRouterPricing) -> Card {
    Card {
        prompt: p.prompt_usd.trim().to_string(),
        completion: p.completion_usd.trim().to_string(),
        cached_prompt: p.cached_prompt_usd.trim().to_string(),
        cache_write: p.cache_write_usd.trim().to_string(),
        internal_reasoning: p.internal_reasoning_usd.trim().to_string(),
        request: p.request_usd.trim().to_string(),
    }
}

fn card_of_override(o: &PricingOverride) -> Card {
    Card {
        prompt: o.prompt_usd.trim().to_string(),
        completion: o.completion_usd.trim().to_string(),
        cached_prompt: o.cached_prompt_usd.trim().to_string(),
        cache_write: o.cache_write_usd.trim().to_string(),
        internal_reasoning: o.internal_reasoning_usd.trim().to_string(),
        request: o.request_usd.trim().to_string(),
    }
}

/// The standing price of one model, before any time window is read.
///
/// An operator who already priced the model on the rate card does not have to
/// type the same numbers again per token: a rate left unpublished is derived
/// from the sell side of that card, which is the same figure divided by a
/// million. Nothing is derived from a rate card that was never switched on.
pub fn base_card(model: &Model, cfg: &Config) -> Card {
    let explicit = card_of(&model.openrouter.pricing);
    let resolved = crate::pricing::resolve(&cfg.pricing, model);
    if !resolved.enabled {
        return explicit;
    }
    // The standard band is what the listing quotes: it is the rate a caller who
    // asked for ordinary thinking pays, and the one the other bands move from.
    let derived = Card {
        prompt: per_token(resolved.input_usd_per_m),
        completion: per_token(resolved.output_usd_per_m),
        cached_prompt: per_token(resolved.cached_input_usd_per_m),
        cache_write: String::new(),
        internal_reasoning: per_token(resolved.reasoning_usd_per_m),
        request: per_token(resolved.request_usd),
    };
    derived.over(&explicit)
}

/// The price in force at one UTC moment: the standing card with the first
/// window that covers that moment laid over it.
pub fn card_at(model: &Model, cfg: &Config, weekday: u32, hhmm: u32) -> Card {
    let base = base_card(model, cfg);
    match model
        .openrouter
        .pricing
        .overrides
        .iter()
        .find(|o| o.covers(weekday, hhmm))
    {
        Some(window) => base.over(&card_of_override(window)),
        None => base,
    }
}

/// What one request comes to at the price this model is *published* at.
///
/// The listing is a promise, and this is the only thing that keeps it one: a
/// caller who read `/models` can multiply the same rates by the same token
/// counts and land on the same number. `None` when nothing was ever priced —
/// a relay nobody set a price on reports no money rather than a zero that
/// reads like free service.
///
/// Used when the relay's own rate card is switched off. With the card on, the
/// card is what the caller is charged and this is not consulted: two price
/// lists cannot both be the bill.
pub fn published_cost(
    model: &Model,
    cfg: &Config,
    usage: &crate::tokenizer::Usage,
    ts_ms: i64,
) -> Option<f64> {
    let (weekday, hhmm) = crate::util::utc_parts(ts_ms);
    let card = card_at(model, cfg, weekday, hhmm);
    if card.is_empty() {
        return None;
    }

    let rate = |text: &str| text.parse::<f64>().unwrap_or(0.0);
    let prompt = rate(&card.prompt);
    // An unpublished cache rate means the provider does not discount a cache
    // hit, not that it is free.
    let cached_rate = if card.cached_prompt.is_empty() {
        prompt
    } else {
        rate(&card.cached_prompt)
    };
    let completion = rate(&card.completion);
    // Reasoning tokens are output tokens until somebody prices them apart.
    let reasoning_rate = if card.internal_reasoning.is_empty() {
        completion
    } else {
        rate(&card.internal_reasoning)
    };

    // Carved out of the totals they arrive inside, so nothing is billed twice.
    let cached = usage.cached_tokens.min(usage.prompt_tokens);
    let fresh = usage.prompt_tokens - cached;
    let reasoned = usage.reasoning_tokens.min(usage.completion_tokens);
    let plain = usage.completion_tokens - reasoned;

    Some(
        fresh as f64 * prompt
            + cached as f64 * cached_rate
            + plain as f64 * completion
            + reasoned as f64 * reasoning_rate
            + rate(&card.request),
    )
}

fn pricing(model: &Model, cfg: &Config) -> Map<String, Value> {
    let base = base_card(model, cfg);
    let mut out = base.to_json();
    if out.is_empty() {
        return out;
    }

    // Every window publishes a whole price, not the one field it moved: a
    // caller reading the third override should not have to walk back up the
    // list to learn what the other two rates are during that hour.
    let windows: Vec<Value> = model
        .openrouter
        .pricing
        .overrides
        .iter()
        .map(|window| {
            let mut entry = Map::new();
            if !window.utc_days.is_empty() {
                entry.insert("utc_days".into(), json!(window.utc_days));
            }
            if window.utc_start != window.utc_end {
                entry.insert("utc_start".into(), json!(window.utc_start));
                entry.insert("utc_end".into(), json!(window.utc_end));
            }
            entry.extend(base.over(&card_of_override(window)).to_json());
            Value::Object(entry)
        })
        .collect();
    if !windows.is_empty() {
        out.insert("overrides".into(), Value::Array(windows));
    }
    out
}

/* ---------------------------------------------------------- parameters -- */

/// Every parameter the OpenAI-shaped surface forwards, in the vocabulary the
/// listing publishes them under.
const BASE_PARAMETERS: [&str; 14] = [
    "frequency_penalty",
    "logit_bias",
    "logprobs",
    "max_tokens",
    "min_p",
    "presence_penalty",
    "repetition_penalty",
    "response_format",
    "seed",
    "stop",
    "temperature",
    "top_k",
    "top_logprobs",
    "top_p",
];

/// What this model actually accepts.
///
/// Derived rather than declared, so a parameter the relay is configured to drop
/// on the way through stops being advertised the moment somebody drops it —
/// advertising one the backend will never see is how a caller ends up debugging
/// a setting that was discarded two hops earlier.
fn supported_parameters(model: &Model, cfg: &Config) -> Vec<String> {
    let o = &model.openrouter;
    if !o.supported_parameters.is_empty() {
        return o.supported_parameters.clone();
    }

    let mut names: Vec<String> = BASE_PARAMETERS.iter().map(|s| (*s).to_string()).collect();
    if o.supports_tools {
        names.push("tool_choice".into());
        names.push("tools".into());
    }
    if o.supports_structured_outputs {
        names.push("structured_outputs".into());
    }
    if o.supports_reasoning {
        names.push("include_reasoning".into());
        names.push("reasoning".into());
        names.push("reasoning_effort".into());
    }

    let rt = RequestTransform::merged(&cfg.defaults.request_transform, &model.request_transform);
    names.retain(|name| !rt.drop_params.contains(name) && !rt.rename_params.contains_key(name));
    names.sort();
    names.dedup();
    names
}

fn default_parameters(model: &Model) -> Map<String, Value> {
    if !model.openrouter.default_parameters.is_empty() {
        return model.openrouter.default_parameters.clone();
    }
    model.params.clone()
}

fn reasoning(model: &Model) -> Value {
    let r = &model.openrouter.reasoning;
    let efforts: Vec<String> = if r.supported_efforts.is_empty() {
        // The levels the relay itself prices, dearest first, which is the order
        // a caller reads a menu in.
        ["max", "high", "medium", "low", "none"]
            .iter()
            .map(|s| (*s).to_string())
            .collect()
    } else {
        r.supported_efforts.clone()
    };
    let default_effort = if r.default_effort.is_empty() {
        Effort::High.as_str().to_string()
    } else {
        r.default_effort.clone()
    };
    json!({
        "mandatory": r.mandatory,
        "default_enabled": r.default_enabled,
        "supported_efforts": efforts,
        "default_effort": default_effort,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Backend, OpenRouterModel, OpenRouterPricing, Pricing};

    fn setup() -> Config {
        let mut cfg = Config::default();
        cfg.backends.push(Backend {
            id: "b1".into(),
            base_url: "https://api.example.com/v1".into(),
            ..Default::default()
        });
        cfg.models.push(Model {
            id: "zeikoai/wissangeni-flash".into(),
            display_name: "Wissangeni Flash".into(),
            description: "A sparse mixture-of-experts model.".into(),
            backend: "b1".into(),
            upstream_model: "Deepseek-v4-flash-0731".into(),
            enabled: true,
            context_length: 1_048_576,
            limits: crate::config::Limits {
                max_output_tokens: 384_000,
                ..Default::default()
            },
            openrouter: OpenRouterModel {
                supports_reasoning: true,
                pricing: OpenRouterPricing {
                    prompt_usd: "0.00000015".into(),
                    completion_usd: "0.0000006".into(),
                    cached_prompt_usd: "0.000000003".into(),
                    overrides: vec![
                        PricingOverride {
                            utc_days: vec!["saturday".into(), "sunday".into()],
                            ..Default::default()
                        },
                        PricingOverride {
                            utc_days: vec!["monday".into(), "friday".into()],
                            utc_start: 100,
                            utc_end: 400,
                            prompt_usd: "0.0000003".into(),
                            completion_usd: "0.0000012".into(),
                            cached_prompt_usd: "0.000000006".into(),
                            ..Default::default()
                        },
                    ],
                    ..Default::default()
                },
                ..Default::default()
            },
            ..Default::default()
        });
        cfg
    }

    #[test]
    fn the_listing_never_mentions_the_backend_model() {
        let cfg = setup();
        let text = document(&cfg).to_string();
        assert!(text.contains("zeikoai/wissangeni-flash"));
        assert!(
            !text.contains("Deepseek-v4-flash-0731"),
            "the backend's model name leaked into the public listing"
        );
    }

    #[test]
    fn the_shape_carries_both_envelopes() {
        let cfg = setup();
        let doc = document(&cfg);
        let m = &doc["data"][0];
        assert_eq!(doc["object"], "list");
        // OpenAI's half, so an unmodified client still recognises it.
        assert_eq!(m["object"], "model");
        assert_eq!(m["owned_by"], crate::config::DEFAULT_MODEL_OWNER);
        // OpenRouter's half.
        assert_eq!(m["architecture"]["modality"], "text->text");
        assert_eq!(m["architecture"]["instruct_type"], Value::Null);
        assert_eq!(m["context_length"], 1_048_576);
        assert_eq!(m["top_provider"]["max_completion_tokens"], 384_000);
        assert_eq!(m["per_request_limits"], Value::Null);
        assert_eq!(m["canonical_slug"], "zeikoai/wissangeni-flash");
    }

    #[test]
    fn every_override_publishes_a_whole_price_not_the_field_it_moved() {
        let cfg = setup();
        let pricing = &document(&cfg)["data"][0]["pricing"];
        assert_eq!(pricing["prompt"], "0.00000015");
        assert_eq!(pricing["input_cache_read"], "0.000000003");

        let weekend = &pricing["overrides"][0];
        assert_eq!(weekend["utc_days"][0], "saturday");
        assert!(
            weekend.get("utc_start").is_none(),
            "an all-day window should not publish an empty one"
        );
        // Inherited from the standing card rather than left out.
        assert_eq!(weekend["completion"], "0.0000006");

        let peak = &pricing["overrides"][1];
        assert_eq!(peak["utc_start"], 100);
        assert_eq!(peak["utc_end"], 400);
        assert_eq!(peak["prompt"], "0.0000003");
    }

    #[test]
    fn the_price_in_force_follows_the_clock() {
        let cfg = setup();
        let m = &cfg.models[0];
        // Monday 02:00 UTC is inside the peak window.
        assert_eq!(card_at(m, &cfg, 0, 200).prompt, "0.0000003");
        // Monday 05:00 is outside it, so the standing rate stands.
        assert_eq!(card_at(m, &cfg, 0, 500).prompt, "0.00000015");
        // Saturday matches the all-day weekend window.
        assert_eq!(card_at(m, &cfg, 5, 1400).completion, "0.0000006");
    }

    #[test]
    fn a_rate_card_prices_the_listing_when_nobody_typed_per_token_strings() {
        let mut cfg = setup();
        cfg.models[0].openrouter.pricing = OpenRouterPricing::default();
        cfg.models[0].pricing = Pricing {
            enabled: true,
            input_usd_per_m: 0.15,
            output_usd_per_m: 0.6,
            cached_input_usd_per_m: 0.003,
            ..Default::default()
        };
        let pricing = &document(&cfg)["data"][0]["pricing"];
        assert_eq!(pricing["prompt"], "0.00000015");
        assert_eq!(pricing["completion"], "0.0000006");
        assert_eq!(pricing["input_cache_read"], "0.000000003");
    }

    #[test]
    fn a_model_nobody_priced_publishes_no_price_at_all() {
        let mut cfg = setup();
        cfg.models[0].openrouter.pricing = OpenRouterPricing::default();
        assert!(
            document(&cfg)["data"][0].get("pricing").is_none(),
            "a price nobody set must not be published as zero"
        );
    }

    #[test]
    fn a_dropped_parameter_is_not_advertised_as_supported() {
        let mut cfg = setup();
        cfg.models[0].request_transform.drop_params = Some(vec!["top_p".into()]);
        cfg.models[0].openrouter.supports_tools = false;

        let params: Vec<String> =
            serde_json::from_value(document(&cfg)["data"][0]["supported_parameters"].clone())
                .unwrap();
        assert!(!params.contains(&"top_p".to_string()));
        assert!(!params.contains(&"tools".to_string()));
        assert!(params.contains(&"temperature".to_string()));
        assert!(params.contains(&"reasoning_effort".to_string()));
        let mut sorted = params.clone();
        sorted.sort();
        assert_eq!(params, sorted, "the list is published in a stable order");
    }

    #[test]
    fn the_charged_cost_is_the_published_one_multiplied_out() {
        let cfg = setup();
        let usage = crate::tokenizer::Usage {
            prompt_tokens: 61,
            completion_tokens: 190,
            cached_tokens: 61,
            reasoning_tokens: 177,
            ..Default::default()
        };
        // A Wednesday well outside the peak window: the standing rate.
        let quiet = chrono::DateTime::parse_from_rfc3339("2026-09-09T12:00:00Z")
            .unwrap()
            .timestamp_millis();
        let cost = published_cost(&cfg.models[0], &cfg, &usage, quiet).expect("a published price");
        // 61 cached at 0.000000003 plus 190 completion at 0.0000006.
        assert!(
            (cost - (61.0 * 0.000000003 + 190.0 * 0.0000006)).abs() < 1e-15,
            "{cost}"
        );

        // A model nobody priced yields no cost rather than a zero.
        let mut bare = cfg.clone();
        bare.models[0].openrouter.pricing = OpenRouterPricing::default();
        assert!(published_cost(&bare.models[0], &bare, &usage, quiet).is_none());
    }

    #[test]
    fn reasoning_is_described_only_by_a_model_that_reasons() {
        let mut cfg = setup();
        assert_eq!(
            document(&cfg)["data"][0]["reasoning"]["default_effort"],
            "high"
        );
        cfg.models[0].openrouter.supports_reasoning = false;
        assert!(document(&cfg)["data"][0].get("reasoning").is_none());
    }
}
