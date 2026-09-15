//! The public model listing, served at `/models` and `/v1/models`.
//!
//! One document, in the shape the wider ecosystem already reads: OpenAI's
//! envelope on the outside (`object: "list"`, `object: "model"`, `owned_by`) so
//! an unmodified OpenAI client still works, carrying what a caller needs to
//! send a request and reconcile a bill — `context_length`,
//! `max_completion_tokens`, `architecture`, `pricing`, `supported_parameters`,
//! `reasoning` — and nothing else.
//!
//! **Only that.** This document is the most widely copied thing the service
//! publishes: every client fetches it at startup, it gets pasted into issues,
//! and it is crawled. So everything describing how an answer is actually
//! produced has been taken out, because each of those fields was a sentence
//! about the internals written in a machine-readable format:
//!
//! * `architecture.tokenizer` named the vocabulary family, which is the model
//!   underneath in all but name. It was the worst of them.
//! * `architecture.instruct_type` names a prompt format, which identifies a
//!   model family just as surely.
//! * `hugging_face_id` pointed at the real weights by URL.
//! * `top_provider` said there is a provider, and implied there could be more
//!   than one. `max_completion_tokens` was the only part a caller used, so it
//!   moved up beside `context_length`, where it reads as a property of the
//!   model rather than of whatever serves it.
//! * `canonical_slug`, `knowledge_cutoff` and `expiration_date` date a
//!   snapshot, and a dated snapshot is a model anyone can look up.
//! * `per_request_limits`, `supported_voices` and `links` were constant nulls
//!   and an empty object: noise that made the rest harder to read.
//!
//! Two rules hold throughout. The real model name never appears — nor anything
//! that names it by implication. And no price is invented: a rate nobody set is
//! either derived from the service's own rate card — the same numbers in a
//! different unit, which is arithmetic rather than invention — or left out
//! entirely. A missing price is a question; a wrong one is a bill.
//!
//! Two things make the listing complete rather than approximate, and both are
//! there for the same reason: these models are not sold at one rate.
//!
//! * `pricing.bands` — the rate moves with how hard the caller asked the model
//!   to think. Thinking off is cheaper than ordinary thinking; maximum effort
//!   is dearer. All three are published side by side, whole and whether or not
//!   they differ, because a listing that quotes the middle one and leaves the
//!   other two to be discovered from an invoice is a half-truth. Which band a
//!   caller who names no effort lands on is its own question, so
//!   `pricing.default_band` answers it by name rather than leaving it inferred.
//! * `pricing.overrides` — the rate also moves with the hour and the day, and a
//!   listing that publishes only the off-peak number is one nobody can
//!   reconcile an invoice against.
//!
//! And the envelope carries no blanks. A `description` nobody wrote, a
//! `context_length` of `0`, a `max_completion_tokens` of `null`, an empty
//! `default_parameters` — every one of those is a field a client has to
//! special-case, and `0` in particular is worse than silence: it reads as a
//! model with no room in it. An unset value is left out, so what is present is
//! known.

use serde_json::{json, Map, Value};

use crate::config::{
    BandRates, Config, Model, OpenRouterPricing, Pricing, PricingOverride, RequestTransform,
};
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
    doc.insert("object".into(), json!("model"));
    doc.insert(
        "name".into(),
        json!(if model.display_name.is_empty() {
            &model.id
        } else {
            &model.display_name
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
    doc.insert(
        "owned_by".into(),
        json!(if model.owner.is_empty() {
            crate::config::DEFAULT_MODEL_OWNER
        } else {
            &model.owner
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
    if !model.description.is_empty() {
        doc.insert("description".into(), json!(model.description));
    }

    // The two numbers that bound a request, side by side. `max_completion_tokens`
    // used to sit inside `top_provider`, which said there was a provider and
    // implied there could be several; as a property of the model it reads as
    // what it is, and a caller reaching for it has one place to look.
    //
    // A bound nobody set is left out rather than published as `0` or `null`. A
    // client that finds no `context_length` falls back to its own default; one
    // that finds `0` believes the model has no room in it and clamps every
    // request to nothing.
    let context = context_length(model);
    if context > 0 {
        doc.insert("context_length".into(), json!(context));
    }
    let max_out = max_output(model);
    if max_out > 0 {
        doc.insert("max_completion_tokens".into(), json!(max_out));
    }

    doc.insert("architecture".into(), architecture(model));

    let pricing = pricing(model, cfg);
    if !pricing.is_empty() {
        doc.insert("pricing".into(), Value::Object(pricing));
    }

    doc.insert(
        "supported_parameters".into(),
        json!(supported_parameters(model, cfg)),
    );
    let defaults = default_parameters(model);
    if !defaults.is_empty() {
        doc.insert("default_parameters".into(), Value::Object(defaults));
    }
    doc.insert("reasoning".into(), reasoning(model, cfg));

    Value::Object(doc)
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

fn architecture(model: &Model) -> Value {
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
        // Reasoning tokens are output tokens until somebody prices them apart,
        // and that is exactly how they are charged — so the rate they are
        // charged at is published under its own name rather than left for a
        // caller to infer from a missing field. Absent, `internal_reasoning`
        // read as "reasoning is not billed" while every invoice billed it at
        // the output rate: the one hole in the formula a caller reconciles
        // against. Taken from this card's own `completion`, so a band or a
        // window that moves output moves this with it.
        put(
            "internal_reasoning",
            if self.internal_reasoning.is_empty() {
                &self.completion
            } else {
                &self.internal_reasoning
            },
        );
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
        // Not per-token: `requestUsd` is what one request costs, flat. Dividing
        // it by a million published the one fee on the card at a millionth of
        // its size — the one number a caller could not reconcile.
        request: flat_usd(resolved.request_usd),
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

    // The rates above are the standard band. Which of the three a caller who
    // named no effort actually pays is a separate question with its own answer,
    // so it is named rather than inferred.
    out.insert("default_band".into(), json!(default_band(cfg)));
    out.insert("bands".into(), Value::Object(bands(model, cfg, &base)));

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

/* --------------------------------------------------------------- bands -- */

/// The three prices one model is actually sold at, cheapest first.
///
/// Nobody buys "the model"; they buy the model at an effort, and the rate moves
/// with it. Each entry is the band's id — what a client keys off — the name a
/// human reads, and the `reasoning_effort` values that land a request on it.
const BANDS: [(&str, &str, &[&str]); 3] = [
    ("non_thinking", "Non-thinking", &["none", "minimal"]),
    ("default", "Default", &["low", "medium", "high"]),
    ("max", "Max", &["max"]),
];

/// The rate card this band moves, or `None` for the standard band, which is the
/// card the listing already quotes at the top level.
fn band_rates<'a>(id: &str, pricing: &'a Pricing) -> Option<&'a BandRates> {
    match id {
        "non_thinking" => Some(&pricing.non_thinking),
        "max" => Some(&pricing.max_thinking),
        _ => None,
    }
}

/// Each band's whole price, keyed by band id.
///
/// Whole, like an override is whole: a caller reading the max band should not
/// have to walk back up to learn what its input rate is. And published for all
/// three even when two of them are the same number, because "thinking off costs
/// the same here" is an answer, and an absent band is a question.
fn bands(model: &Model, cfg: &Config, base: &Card) -> Map<String, Value> {
    let resolved = crate::pricing::resolve(&cfg.pricing, model);
    let mut out = Map::new();
    for (id, name, efforts) in BANDS {
        // A relay billing off per-token strings alone has no bands at all, and
        // neither does one whose rate card is switched off: then all three are
        // the one price, said three times rather than left to be guessed at.
        let card = match band_rates(id, &resolved) {
            Some(band) if resolved.enabled => over_band(base, band),
            _ => base.clone(),
        };
        let mut entry = Map::new();
        entry.insert("name".into(), json!(name));
        entry.insert("efforts".into(), json!(efforts));
        entry.extend(card.to_json());
        out.insert(id.into(), Value::Object(entry));
    }
    out
}

/// Lay a band's per-million rates over the standing card, exactly as the biller
/// lays them over the standard rates.
///
/// Exactly, including that a band which moves output moves reasoning with it,
/// because reasoning tokens are output tokens until somebody prices them apart.
/// A published price arrived at differently from the charged one is not a
/// published price.
fn over_band(base: &Card, band: &BandRates) -> Card {
    let mut card = base.clone();
    if band.input_usd_per_m > 0.0 {
        card.prompt = per_token(band.input_usd_per_m);
    }
    if band.cached_input_usd_per_m > 0.0 {
        card.cached_prompt = per_token(band.cached_input_usd_per_m);
    }
    if band.output_usd_per_m > 0.0 {
        card.completion = per_token(band.output_usd_per_m);
        // Only restated where the standing card states it: a card that leaves
        // reasoning to follow output has a band that leaves it to follow too.
        if !card.internal_reasoning.is_empty() {
            card.internal_reasoning = per_token(band.output_usd_per_m);
        }
    }
    if band.reasoning_usd_per_m > 0.0 {
        card.internal_reasoning = per_token(band.reasoning_usd_per_m);
    }
    card
}

/// The band a request that names no effort is billed at.
///
/// Not always the standard one: it is whatever `defaults.effort` resolves
/// silence into, and an operator who resolves silence to `none` puts every
/// unadorned request on the non-thinking band. The caller sending that request
/// is the one who needs to know.
fn default_band(cfg: &Config) -> &'static str {
    match crate::pricing::default_effort(cfg) {
        Effort::Max => "max",
        Effort::None | Effort::Minimal | Effort::Unspecified => "non_thinking",
        Effort::Low | Effort::Medium | Effort::High => "default",
    }
}

/// A flat USD amount as the same kind of decimal string the per-token rates are
/// published as — decimal because a caller checking an invoice compares
/// decimals, not floats.
fn flat_usd(usd: f64) -> String {
    if !usd.is_finite() || usd <= 0.0 {
        return String::new();
    }
    let text = format!("{usd:.12}");
    let trimmed = text.trim_end_matches('0');
    if trimmed.ends_with('.') {
        return String::new();
    }
    trimmed.to_string()
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
    // The thinking controls belong to the relay, not to whatever serves the
    // model: the effort a caller names is what picks the system prompt that
    // goes out and the price band the request is billed on, and that happens on
    // every route whether or not the model publishes its working. So they are
    // advertised everywhere — a listing that quotes three thinking bands while
    // advertising no way to ask for one leaves a caller to guess at the
    // parameter that moves their bill.
    names.push("reasoning".into());
    names.push("reasoning_effort".into());
    if o.supports_reasoning {
        // Only a model that shows its working has a trace to leave out.
        names.push("include_reasoning".into());
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

/// The thinking controls this model takes, and what silence means.
///
/// Published for every model rather than only the ones that show their working,
/// because the price moves with the effort on all of them: `pricing.bands`
/// quotes three rates chosen by a parameter, and a document that publishes the
/// rates without the vocabulary that selects them is half a price list. That
/// was the gap a client reading this document actually fell into — three bands,
/// no `reasoning_effort` in `supported_parameters`, and no block here saying
/// which levels exist.
///
/// What it says is what the relay does: the levels it parses, and the level a
/// request that named none is treated as having asked for — the operator's
/// `defaults.effort`, read through the same function the biller and the prompt
/// picker read, rather than a constant that was right only while nobody
/// changed the setting.
fn reasoning(model: &Model, cfg: &Config) -> Value {
    let r = &model.openrouter.reasoning;
    let silence = crate::pricing::default_effort(cfg);
    let efforts: Vec<String> = if r.supported_efforts.is_empty() {
        // Every level the relay parses, dearest first, which is the order a
        // caller reads a menu in. `minimal` is one of them — the documentation
        // and the biller both took it while this list left it out, so a client
        // generating its options from here could not ask for the band it would
        // have been charged at.
        ["max", "high", "medium", "low", "minimal", "none"]
            .iter()
            .map(|s| (*s).to_string())
            .collect()
    } else {
        r.supported_efforts.clone()
    };
    let default_effort = if r.default_effort.is_empty() {
        silence.as_str().to_string()
    } else {
        r.default_effort.clone()
    };
    json!({
        "mandatory": r.mandatory,
        // Thinking is on for a caller who said nothing exactly when silence
        // resolves to a thinking level, which is the same question
        // `pricing.default_band` answers in money.
        "default_enabled": r.default_enabled || silence.is_thinking(),
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
    fn the_shape_carries_what_a_caller_needs_and_no_more() {
        let cfg = setup();
        let doc = document(&cfg);
        let m = &doc["data"][0];
        assert_eq!(doc["object"], "list");
        // The OpenAI envelope, so an unmodified client still recognises it.
        assert_eq!(m["object"], "model");
        assert_eq!(m["owned_by"], crate::config::DEFAULT_MODEL_OWNER);
        assert_eq!(m["architecture"]["modality"], "text->text");
        // The two bounds a caller writes code against, side by side.
        assert_eq!(m["context_length"], 1_048_576);
        assert_eq!(m["max_completion_tokens"], 384_000);
    }

    /// Every field here described how the answer gets produced rather than how
    /// to ask for one, and each named the model underneath or dated it well
    /// enough to be looked up. The listing is the most-copied thing published,
    /// so this test is the guard on what may go into it.
    #[test]
    fn the_listing_describes_the_model_not_what_runs_it() {
        let cfg = setup();
        let doc = document(&cfg);
        let m = &doc["data"][0];

        for leaky in [
            "canonical_slug",
            "hugging_face_id",
            "top_provider",
            "knowledge_cutoff",
            "expiration_date",
            "per_request_limits",
            "supported_voices",
            "links",
        ] {
            assert!(
                m.get(leaky).is_none(),
                "{leaky} is back in the public listing"
            );
        }
        for leaky in ["tokenizer", "instruct_type"] {
            assert!(
                m["architecture"].get(leaky).is_none(),
                "architecture.{leaky} names the model family"
            );
        }
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
    fn the_listing_publishes_all_three_prices_not_just_the_middle_one() {
        // The complaint this answers: a price list that quotes one number for a
        // model sold at three, leaving the other two to be discovered from an
        // invoice.
        let mut cfg = setup();
        cfg.models[0].openrouter.pricing = OpenRouterPricing::default();
        cfg.models[0].pricing = Pricing {
            enabled: true,
            input_usd_per_m: 0.8,
            cached_input_usd_per_m: 0.2,
            output_usd_per_m: 4.0,
            max_thinking: BandRates {
                output_usd_per_m: 6.0,
                ..Default::default()
            },
            non_thinking: BandRates {
                output_usd_per_m: 3.5,
                ..Default::default()
            },
            request_usd: 0.002,
            ..Default::default()
        };

        let pricing = &document(&cfg)["data"][0]["pricing"];
        // The top level is still the standard band, so a client that only knows
        // `pricing.prompt` reads exactly what it always read.
        assert_eq!(pricing["completion"], "0.000004");
        // A flat per-request fee is a flat fee, not a per-token rate.
        assert_eq!(pricing["request"], "0.002");

        let bands = &pricing["bands"];
        assert_eq!(bands["non_thinking"]["completion"], "0.0000035");
        assert_eq!(bands["default"]["completion"], "0.000004");
        assert_eq!(bands["max"]["completion"], "0.000006");
        // Whole prices: a band that moved only its output still says what its
        // input costs, so nobody has to walk back up the document.
        for band in ["non_thinking", "default", "max"] {
            assert_eq!(bands[band]["prompt"], "0.0000008", "{band}");
            assert_eq!(bands[band]["input_cache_read"], "0.0000002", "{band}");
        }
        assert_eq!(bands["max"]["name"], "Max");
        assert_eq!(bands["max"]["efforts"][0], "max");
        assert_eq!(bands["non_thinking"]["efforts"][0], "none");
        // Cheapest first, which is the order a menu is read in.
        let order: Vec<&String> = bands.as_object().unwrap().keys().collect();
        assert_eq!(order, ["non_thinking", "default", "max"]);
    }

    #[test]
    fn the_listing_names_the_band_a_caller_who_says_nothing_lands_on() {
        let mut cfg = setup();
        let band_of = |cfg: &Config| document(cfg)["data"][0]["pricing"]["default_band"].clone();
        // Silence ships resolving to `high`, which is the standard band.
        assert_eq!(band_of(&cfg), "default");
        cfg.defaults.effort = "none".into();
        assert_eq!(band_of(&cfg), "non_thinking");
        cfg.defaults.effort = "max".into();
        assert_eq!(band_of(&cfg), "max");
    }

    #[test]
    fn a_relay_with_no_rate_card_still_publishes_three_bands() {
        // Priced by per-token strings alone, so there are no bands to read: the
        // one price is said three times rather than left to be guessed at.
        let cfg = setup();
        let bands = &document(&cfg)["data"][0]["pricing"]["bands"];
        for band in ["non_thinking", "default", "max"] {
            assert_eq!(bands[band]["prompt"], "0.00000015", "{band}");
            assert_eq!(bands[band]["completion"], "0.0000006", "{band}");
        }
    }

    #[test]
    fn a_field_nobody_filled_in_is_left_out_rather_than_published_blank() {
        let mut cfg = setup();
        cfg.models[0].description = String::new();
        cfg.models[0].context_length = 0;
        cfg.models[0].limits.max_output_tokens = 0;

        let m = &document(&cfg)["data"][0];
        for blank in [
            "description",
            "context_length",
            "max_completion_tokens",
            "default_parameters",
        ] {
            assert!(
                m.get(blank).is_none(),
                "{blank} was published as a blank a client has to special-case"
            );
        }
        // What is there is still there.
        assert_eq!(m["id"], "zeikoai/wissangeni-flash");
        assert_eq!(m["object"], "model");
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

    /// The gap a customer integration reported: the document quoted three
    /// thinking bands while advertising no parameter that selects one, and
    /// carried no `reasoning` block to say which levels exist — so the bands
    /// read as prices for something a caller could not ask for.
    #[test]
    fn a_document_that_prices_thinking_says_how_to_ask_for_it() {
        let mut cfg = setup();
        // Even with nothing declared about traces: the effort still moves the
        // price on this route, so the controls are still real.
        cfg.models[0].openrouter.supports_reasoning = false;

        let m = &document(&cfg)["data"][0];
        let params: Vec<String> =
            serde_json::from_value(m["supported_parameters"].clone()).unwrap();
        assert!(params.contains(&"reasoning_effort".to_string()));
        assert!(params.contains(&"reasoning".to_string()));
        // A trace is the one part a model without one cannot offer.
        assert!(!params.contains(&"include_reasoning".to_string()));

        let r = &m["reasoning"];
        assert_eq!(r["default_effort"], "high");
        assert_eq!(r["default_enabled"], true);
        let efforts: Vec<String> = serde_json::from_value(r["supported_efforts"].clone()).unwrap();
        // Every level the biller prices, `minimal` included — it was missing
        // here while the price list and the documentation both took it.
        for level in ["none", "minimal", "low", "medium", "high", "max"] {
            assert!(efforts.contains(&level.to_string()), "{level}");
        }

        // A model that does show its working says so.
        cfg.models[0].openrouter.supports_reasoning = true;
        let params: Vec<String> =
            serde_json::from_value(document(&cfg)["data"][0]["supported_parameters"].clone())
                .unwrap();
        assert!(params.contains(&"include_reasoning".to_string()));
    }

    /// `default_effort` is the operator's setting, read through the same
    /// function the biller and the prompt picker read. Published as a constant
    /// it was right only until somebody changed the setting, and then it
    /// disagreed with `pricing.default_band` in the same document.
    #[test]
    fn the_effort_silence_resolves_to_is_the_one_the_relay_actually_uses() {
        let mut cfg = setup();
        cfg.defaults.effort = "none".into();
        let m = &document(&cfg)["data"][0];
        assert_eq!(m["reasoning"]["default_effort"], "none");
        assert_eq!(m["reasoning"]["default_enabled"], false);
        assert_eq!(m["pricing"]["default_band"], "non_thinking");

        cfg.defaults.effort = "max".into();
        let m = &document(&cfg)["data"][0];
        assert_eq!(m["reasoning"]["default_effort"], "max");
        assert_eq!(m["reasoning"]["default_enabled"], true);
    }

    /// Reasoning tokens are charged at the output rate of whichever band the
    /// request is on. Leaving `internal_reasoning` out said the opposite — that
    /// they are not charged — and it was the one term missing from the formula
    /// a caller reconciles an invoice with.
    #[test]
    fn the_rate_reasoning_tokens_are_billed_at_is_published() {
        let mut cfg = setup();
        cfg.models[0].openrouter.pricing = OpenRouterPricing::default();
        cfg.models[0].pricing = Pricing {
            enabled: true,
            input_usd_per_m: 0.8,
            cached_input_usd_per_m: 0.2,
            output_usd_per_m: 4.0,
            max_thinking: BandRates {
                output_usd_per_m: 6.0,
                ..Default::default()
            },
            non_thinking: BandRates {
                output_usd_per_m: 3.5,
                ..Default::default()
            },
            ..Default::default()
        };

        let pricing = &document(&cfg)["data"][0]["pricing"];
        assert_eq!(pricing["internal_reasoning"], "0.000004");
        // Every band, at that band's own output rate, exactly as `apply_band`
        // charges it.
        assert_eq!(
            pricing["bands"]["non_thinking"]["internal_reasoning"],
            "0.0000035"
        );
        assert_eq!(
            pricing["bands"]["default"]["internal_reasoning"],
            "0.000004"
        );
        assert_eq!(pricing["bands"]["max"]["internal_reasoning"], "0.000006");

        // Priced apart, the price that was set is the price that is published.
        cfg.models[0].pricing.reasoning_usd_per_m = 1.2;
        let pricing = &document(&cfg)["data"][0]["pricing"];
        assert_eq!(pricing["internal_reasoning"], "0.0000012");

        // And a window that moves output moves what reasoning costs with it,
        // rather than quoting the standing rate inside an hour that does not
        // charge it.
        let windowed = setup();
        let window = &document(&windowed)["data"][0]["pricing"]["overrides"][1];
        assert_eq!(window["completion"], "0.0000012");
        assert_eq!(window["internal_reasoning"], "0.0000012");

        // Nothing priced still publishes nothing: a rate nobody set is not
        // invented out of a blank.
        let mut bare = setup();
        bare.models[0].openrouter.pricing = OpenRouterPricing::default();
        assert!(document(&bare)["data"][0].get("pricing").is_none());
    }
}
