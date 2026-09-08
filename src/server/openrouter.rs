//! The model document OpenRouter asks a provider to publish.
//!
//! OpenRouter routes on this: it decides from here what a model costs, which
//! parameters it may forward, and how much traffic to send. Everything in it
//! comes from the config, so the numbers are the operator's to set from the
//! dashboard — none of them are technical facts this program could work out on
//! its own.
//!
//! Two rules hold throughout. The backend's real model name never appears —
//! the whole point of the relay is that callers see `id`, not what it resolves
//! to. And nothing is invented: a price nobody has set is left out rather than
//! published as zero, because a wrong price is worse than a missing one.
//!
//! Shapes follow the published `schema_version` 2.4 document: modality objects
//! carrying `supported_inputs` / `supported_parameters`, `pricing` and
//! `capacity` arrays, and the capability-descriptor grammar
//! (`range` / `integer` / `boolean` / `enum` / `array`).

use serde_json::{json, Map, Value};

use crate::config::{Config, Model, OpenRouterModel, RequestTransform};

pub const SCHEMA_VERSION: &str = "2.4";

/// Build the whole listing.
pub fn document(cfg: &Config) -> Value {
    let data: Vec<Value> = cfg
        .models
        .iter()
        .filter(|m| m.enabled && m.openrouter.listed)
        .map(|m| model_document(m, cfg))
        .collect();
    json!({ "data": data })
}

/// One model, as OpenRouter's schema describes it.
pub fn model_document(model: &Model, cfg: &Config) -> Value {
    let o = &model.openrouter;
    let mut doc = Map::new();

    doc.insert("schema_version".into(), json!(SCHEMA_VERSION));
    doc.insert("id".into(), json!(model.id));
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
    if !model.description.is_empty() {
        doc.insert("description".into(), json!(model.description));
    }
    if !o.hugging_face_id.is_empty() {
        doc.insert("hugging_face_id".into(), json!(o.hugging_face_id));
    }
    // Explicitly null rather than absent: "unquantized or unspecified" is a
    // meaningful answer, and the schema has a place for it.
    doc.insert(
        "quantization".into(),
        if o.quantization.is_empty() {
            Value::Null
        } else {
            json!(o.quantization)
        },
    );
    doc.insert("tokenizer".into(), json!(tokenizer_family(model, cfg)));

    doc.insert("input_modalities".into(), json!(input_modalities(model)));
    doc.insert(
        "output_modalities".into(),
        json!(output_modalities(model, cfg)),
    );

    let root_pricing = root_pricing(o);
    if !root_pricing.is_empty() {
        doc.insert("pricing".into(), json!(root_pricing));
    }
    let root_capacity = root_capacity(cfg);
    if !root_capacity.is_empty() {
        doc.insert("capacity".into(), json!(root_capacity));
    }

    doc.insert("is_ready".into(), json!(cfg.openrouter.is_ready));
    doc.insert("is_free".into(), json!(o.is_free));
    if o.discount_to_user > 0.0 {
        doc.insert("discount_to_user".into(), json!(o.discount_to_user));
    }
    if !o.deprecation_date.is_empty() {
        doc.insert("deprecation_date".into(), json!(o.deprecation_date));
    }
    doc.insert("openrouter".into(), json!({ "slug": slug(model, cfg) }));

    if !cfg.openrouter.deployment_region.is_empty() {
        doc.insert(
            "deployment_region".into(),
            json!(cfg.openrouter.deployment_region),
        );
    }
    if !cfg.openrouter.datacenters.is_empty() {
        let centres: Vec<Value> = cfg
            .openrouter
            .datacenters
            .iter()
            .map(|d| {
                let mut entry = Map::new();
                entry.insert("country_code".into(), json!(d.country_code.to_uppercase()));
                if !d.region.is_empty() {
                    entry.insert("region".into(), json!(d.region));
                }
                Value::Object(entry)
            })
            .collect();
        doc.insert("datacenters".into(), json!(centres));
    }
    doc.insert(
        "compliance".into(),
        json!({
            "zdr": cfg.openrouter.compliance.zdr,
            "hipaa": cfg.openrouter.compliance.hipaa,
        }),
    );

    Value::Object(doc)
}

/* ---------------------------------------------------------- modalities -- */

fn input_modalities(model: &Model) -> Vec<Value> {
    let o = &model.openrouter;
    let context = if o.max_prompt_tokens > 0 {
        o.max_prompt_tokens
    } else if model.limits.max_input_tokens > 0 {
        model.limits.max_input_tokens
    } else {
        model.context_length
    };

    let mut out = Vec::new();
    for kind in &o.input_modalities {
        let mut entry = Map::new();
        entry.insert("type".into(), json!(kind));

        let mut supported = Map::new();
        match kind.as_str() {
            "text" => {
                if model.context_length > 0 {
                    supported.insert(
                        "max_context_length".into(),
                        json!({"value": model.context_length, "unit": "token"}),
                    );
                }
                if context > 0 {
                    supported.insert(
                        "max_prompt_length".into(),
                        json!({"value": context, "unit": "token"}),
                    );
                }
            }
            "image" => {
                supported.insert("sources".into(), enum_of(&["url", "base64"]));
                supported.insert(
                    "formats".into(),
                    enum_of(&["image/png", "image/jpeg", "image/webp", "image/gif"]),
                );
                supported.insert("detail_levels".into(), enum_of(&["auto", "low", "high"]));
            }
            "audio" => {
                supported.insert("sources".into(), enum_of(&["url", "base64"]));
                supported.insert("formats".into(), enum_of(&["audio/wav", "audio/mpeg"]));
            }
            "video" => {
                supported.insert("sources".into(), enum_of(&["url", "base64"]));
                supported.insert("formats".into(), enum_of(&["video/mp4", "video/webm"]));
            }
            "file" => {
                supported.insert("sources".into(), enum_of(&["url", "base64"]));
                supported.insert(
                    "formats".into(),
                    enum_of(&["application/pdf", "text/plain", "text/csv"]),
                );
            }
            _ => {}
        }
        entry.insert("supported_inputs".into(), Value::Object(supported));

        // Prompt pricing belongs to the text input, which is what the token
        // prices are quoted in; the other modalities carry the same request.
        if kind == "text" {
            let pricing = input_pricing(&model.openrouter);
            if !pricing.is_empty() {
                entry.insert("pricing".into(), json!(pricing));
            }
            let capacity = input_capacity(&model.openrouter);
            if !capacity.is_empty() {
                entry.insert("capacity".into(), json!(capacity));
            }
        }
        out.push(Value::Object(entry));
    }
    out
}

fn output_modalities(model: &Model, cfg: &Config) -> Vec<Value> {
    let o = &model.openrouter;
    let max_out = if o.max_output_tokens > 0 {
        o.max_output_tokens
    } else {
        model.limits.max_output_tokens
    };

    let mut params = Map::new();
    params.insert(
        "temperature".into(),
        json!({"type": "range", "min": 0, "max": o.temperature_max}),
    );
    params.insert("top_p".into(), json!({"type": "range", "min": 0, "max": 1}));
    if max_out > 0 {
        params.insert(
            "max_tokens".into(),
            json!({"type": "integer", "min": 1, "max": max_out, "unit": "token"}),
        );
    }
    // The relay caps `stop` at four entries on the way through, so it says so
    // rather than letting OpenRouter forward a fifth that would be dropped.
    params.insert(
        "stop".into(),
        json!({"type": "array", "items": {"type": "unknown"}, "max_items": 4}),
    );
    params.insert("tools".into(), json!({"type": "boolean"}));
    params.insert("structured_outputs".into(), json!({"type": "boolean"}));
    if o.supports_reasoning {
        params.insert("reasoning".into(), json!({"type": "boolean"}));
    }
    // A parameter the relay is configured to drop is not one it supports, so
    // it is not advertised — including one dropped by the shared defaults.
    let rt = RequestTransform::merged(&cfg.defaults.request_transform, &model.request_transform);
    for dropped in &rt.drop_params {
        params.remove(dropped.as_str());
    }
    for from in rt.rename_params.keys() {
        params.remove(from.as_str());
    }
    if !o.supports_tools {
        params.remove("tools");
    }
    if !o.supports_structured_outputs {
        params.remove("structured_outputs");
    }

    let mut entry = Map::new();
    entry.insert("type".into(), json!("text"));
    entry.insert("supported_parameters".into(), Value::Object(params));
    entry.insert("streaming".into(), json!(o.streaming));
    if max_out > 0 {
        entry.insert(
            "max_length".into(),
            json!({"value": max_out, "unit": "token"}),
        );
    }
    let pricing = output_pricing(o);
    if !pricing.is_empty() {
        entry.insert("pricing".into(), json!(pricing));
    }
    let capacity = output_capacity(o, cfg);
    if !capacity.is_empty() {
        entry.insert("capacity".into(), json!(capacity));
    }
    vec![Value::Object(entry)]
}

/* ------------------------------------------------------------- pricing -- */

fn price(kind: &str, unit: &str, cost: &str) -> Option<Value> {
    if cost.trim().is_empty() {
        return None;
    }
    Some(json!({"type": kind, "unit": unit, "cost_usd": cost}))
}

fn input_pricing(o: &OpenRouterModel) -> Vec<Value> {
    let mut out = Vec::new();
    out.extend(price("prompt", "token", &o.pricing.prompt_usd));
    if let Some(mut cached) = price("cached_prompt", "token", &o.pricing.cached_prompt_usd) {
        let map = cached.as_object_mut().expect("built as an object");
        if o.pricing.cache_ttl_seconds > 0 {
            map.insert("ttl_seconds".into(), json!(o.pricing.cache_ttl_seconds));
        }
        if o.pricing.cache_implicit {
            map.insert("implicit".into(), json!(true));
        }
        out.push(cached);
    }
    out.extend(price("cache_write", "token", &o.pricing.cache_write_usd));
    out
}

fn output_pricing(o: &OpenRouterModel) -> Vec<Value> {
    let mut out = Vec::new();
    out.extend(price("completion", "token", &o.pricing.completion_usd));
    out.extend(price(
        "internal_reasoning",
        "token",
        &o.pricing.internal_reasoning_usd,
    ));
    out
}

fn root_pricing(o: &OpenRouterModel) -> Vec<Value> {
    price("request", "request", &o.pricing.request_usd)
        .into_iter()
        .collect()
}

/* ------------------------------------------------------------ capacity -- */

fn per_minute(kind: &str, unit: &str, value: u64) -> Option<Value> {
    (value > 0).then(|| json!({"type": kind, "unit": unit, "per": "minute", "value": value}))
}

fn input_capacity(o: &OpenRouterModel) -> Vec<Value> {
    per_minute("prompt", "token", o.capacity.prompt_tokens_per_minute)
        .into_iter()
        .collect()
}

fn output_capacity(o: &OpenRouterModel, cfg: &Config) -> Vec<Value> {
    let mut out = Vec::new();
    out.extend(per_minute(
        "completion",
        "token",
        o.capacity.completion_tokens_per_minute,
    ));
    out.extend(per_minute(
        "completion",
        "request",
        o.capacity.requests_per_minute,
    ));
    // An unset per-model concurrency reports the relay's real ceiling rather
    // than nothing: OpenRouter throttling to a number the phone can serve is
    // the entire point of publishing it.
    let concurrency = if o.capacity.concurrency > 0 {
        u64::from(o.capacity.concurrency)
    } else {
        cfg.server.max_concurrent_requests as u64
    };
    if concurrency > 0 {
        out.push(json!({"type": "concurrency", "unit": "request", "value": concurrency}));
    }
    out
}

fn root_capacity(cfg: &Config) -> Vec<Value> {
    let mut out = Vec::new();
    out.extend(per_minute(
        "request",
        "request",
        cfg.openrouter.requests_per_minute,
    ));
    let concurrency = if cfg.openrouter.max_concurrent_requests > 0 {
        u64::from(cfg.openrouter.max_concurrent_requests)
    } else {
        cfg.server.max_concurrent_requests as u64
    };
    if concurrency > 0 {
        out.push(json!({"type": "concurrency", "unit": "request", "value": concurrency}));
    }
    out
}

/* ------------------------------------------------------------- helpers -- */

fn enum_of(values: &[&str]) -> Value {
    json!({"type": "enum", "values": values})
}

fn slug(model: &Model, cfg: &Config) -> String {
    if !model.openrouter.slug.is_empty() {
        return model.openrouter.slug.clone();
    }
    // A model id is often already `vendor/name`; keep only the name so the
    // slug does not come out as `provider/vendor/name`.
    let name = model.id.rsplit('/').next().unwrap_or(&model.id);
    let provider = if cfg.openrouter.provider_slug.is_empty() {
        "chtting"
    } else {
        &cfg.openrouter.provider_slug
    };
    format!("{provider}/{name}")
}

/// The tokenizer family, in OpenRouter's vocabulary.
///
/// Falls back to the vocabulary the relay actually counts with, which is the
/// honest answer even when it is not one of the familiar names.
fn tokenizer_family(model: &Model, cfg: &Config) -> String {
    if !model.openrouter.tokenizer_family.is_empty() {
        return model.openrouter.tokenizer_family.clone();
    }
    let (resolved, _) =
        crate::tokenizer::registry::Registry::match_rules(&cfg.tokenizer, &model.upstream_model);
    let name = if model.tokenizer.is_empty() {
        resolved
    } else {
        model.tokenizer.clone()
    };
    match name.as_str() {
        "cl100k_base" | "o200k_base" | "p50k_base" | "r50k_base" => "GPT".into(),
        other if other.starts_with("deepseek") => "DeepSeek".into(),
        other if other.starts_with("qwen") => "Qwen".into(),
        other if other.starts_with("llama") => "Llama".into(),
        other if other.starts_with("mistral") => "Mistral".into(),
        other if other.starts_with("gemma") => "Gemma".into(),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Backend, OpenRouterPricing};

    fn setup() -> Config {
        let mut cfg = Config::default();
        cfg.openrouter.enabled = true;
        cfg.openrouter.provider_slug = "chtting".into();
        cfg.backends.push(Backend {
            id: "b1".into(),
            base_url: "https://api.example.com/v1".into(),
            ..Default::default()
        });
        cfg.models.push(Model {
            id: "manukmiberai/creative-writer".into(),
            display_name: "Creative Writer".into(),
            backend: "b1".into(),
            upstream_model: "Deepseek-v4-flash-0731".into(),
            enabled: true,
            context_length: 128_000,
            openrouter: OpenRouterModel {
                listed: true,
                pricing: OpenRouterPricing {
                    prompt_usd: "0.0000006".into(),
                    completion_usd: "0.0000018".into(),
                    ..Default::default()
                },
                ..Default::default()
            },
            ..Default::default()
        });
        cfg
    }

    #[test]
    fn the_document_never_mentions_the_backend_model() {
        let cfg = setup();
        let text = document(&cfg).to_string();
        assert!(text.contains("manukmiberai/creative-writer"));
        assert!(
            !text.contains("Deepseek-v4-flash-0731"),
            "the backend's model name leaked into the provider document"
        );
    }

    #[test]
    fn only_models_offered_to_openrouter_are_listed() {
        let mut cfg = setup();
        cfg.models.push(Model {
            id: "internal-only".into(),
            backend: "b1".into(),
            upstream_model: "secret".into(),
            enabled: true,
            ..Default::default()
        });

        let doc = document(&cfg);
        let ids: Vec<&str> = doc["data"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["id"].as_str().unwrap())
            .collect();
        assert_eq!(ids, vec!["manukmiberai/creative-writer"]);
    }

    #[test]
    fn a_price_nobody_set_is_left_out_rather_than_published_as_zero() {
        let cfg = setup();
        let doc = document(&cfg);
        let model = &doc["data"][0];

        let input = &model["input_modalities"][0];
        let kinds: Vec<&str> = input["pricing"]
            .as_array()
            .unwrap()
            .iter()
            .map(|p| p["type"].as_str().unwrap())
            .collect();
        assert_eq!(kinds, vec!["prompt"], "only the price that was set");
        assert_eq!(input["pricing"][0]["cost_usd"], "0.0000006");
        assert!(
            model.get("pricing").is_none(),
            "no per-request fee was set, so none should be published"
        );
    }

    #[test]
    fn prices_keep_every_digit_they_were_given() {
        let mut cfg = setup();
        // A price like this loses its last digits through an f64.
        cfg.models[0].openrouter.pricing.prompt_usd = "0.000000123456789".into();
        let doc = document(&cfg);
        assert_eq!(
            doc["data"][0]["input_modalities"][0]["pricing"][0]["cost_usd"],
            "0.000000123456789"
        );
    }

    #[test]
    fn the_published_concurrency_is_what_the_relay_will_actually_serve() {
        let mut cfg = setup();
        cfg.server.max_concurrent_requests = 24;
        let doc = document(&cfg);
        let capacity = doc["data"][0]["output_modalities"][0]["capacity"]
            .as_array()
            .unwrap()
            .clone();
        let concurrency = capacity
            .iter()
            .find(|c| c["type"] == "concurrency")
            .expect("concurrency should be published");
        assert_eq!(concurrency["value"], 24);
    }

    #[test]
    fn a_dropped_parameter_is_not_advertised_as_supported() {
        let mut cfg = setup();
        cfg.models[0].request_transform.drop_params = Some(vec!["top_p".into()]);
        cfg.models[0].openrouter.supports_tools = false;

        let doc = document(&cfg);
        let params = &doc["data"][0]["output_modalities"][0]["supported_parameters"];
        assert!(params.get("top_p").is_none());
        assert!(params.get("tools").is_none());
        assert!(params.get("temperature").is_some());
    }

    #[test]
    fn the_slug_does_not_repeat_the_vendor_prefix() {
        let cfg = setup();
        assert_eq!(
            document(&cfg)["data"][0]["openrouter"]["slug"],
            "chtting/creative-writer"
        );
    }

    #[test]
    fn quantization_is_published_as_null_when_it_was_not_set() {
        let cfg = setup();
        let doc = document(&cfg);
        assert!(doc["data"][0]["quantization"].is_null());
        assert_eq!(doc["data"][0]["schema_version"], SCHEMA_VERSION);
    }
}
