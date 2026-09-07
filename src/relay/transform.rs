//! Request and response reshaping.
//!
//! Requests: the public model name is swapped for the backend's real name, the
//! configured system prompt is injected, params are defaulted and forced, and
//! text rules may rewrite what the caller sent.
//!
//! Responses: the backend's identity is scrubbed back to the public alias,
//! reasoning traces are kept, stripped, inlined or relocated, and text rules
//! rewrite the reply. The same rules run over streamed deltas, so a stream and
//! a buffered reply come out identically shaped.

use fancy_regex::Regex;
use serde_json::{Map, Value};

use crate::config::{
    Config, Model, ResolvedRequestTransform, ResolvedResponseTransform, SystemPromptSpec, TextRule,
};
use crate::tokenizer::chat::flatten_content;

/* ---------------------------------------------------------- text rules -- */

pub struct CompiledRules {
    rules: Vec<(Regex, String)>,
}

impl Clone for CompiledRules {
    fn clone(&self) -> Self {
        // fancy_regex::Regex is not Clone; recompiling from the source is
        // cheap next to an LLM call and keeps call sites simple.
        Self {
            rules: self
                .rules
                .iter()
                .filter_map(|(re, rep)| Regex::new(re.as_str()).ok().map(|r| (r, rep.clone())))
                .collect(),
        }
    }
}

impl CompiledRules {
    pub fn apply(&self, text: &str) -> String {
        let mut out = text.to_string();
        for (re, replacement) in &self.rules {
            out = re.replace_all(&out, replacement.as_str()).into_owned();
        }
        out
    }

    /// The rules' own regexes, used by the stream rewriter to find a safe cut.
    pub fn patterns(&self) -> impl Iterator<Item = &Regex> {
        self.rules.iter().map(|(re, _)| re)
    }

    pub fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }
}

/// Compile `[{pattern, flags, replacement, literal}]` into one rewriter.
///
/// An invalid rule is skipped rather than taking the relay down: a typo in the
/// dashboard should cost that one rule, not every request.
pub fn compile_text_rules(rules: &[TextRule]) -> Option<CompiledRules> {
    let mut compiled = Vec::new();
    for rule in rules {
        if rule.pattern.is_empty() {
            continue;
        }
        let source = if rule.literal {
            fancy_regex::escape(&rule.pattern).into_owned()
        } else {
            rule.pattern.clone()
        };
        // JavaScript-style flags become inline groups; `g` is implied because
        // every rule is applied with replace_all.
        let flags: String = rule
            .flags
            .as_deref()
            .unwrap_or("g")
            .chars()
            .filter(|c| matches!(c, 'i' | 'm' | 's' | 'x'))
            .collect();
        let source = if flags.is_empty() {
            source
        } else {
            format!("(?{flags}){source}")
        };

        if let Ok(re) = Regex::new(&source) {
            // JavaScript spells the whole match `$&`; Rust spells it `${0}`.
            compiled.push((re, rule.replacement.replace("$&", "${0}")));
        }
    }
    if compiled.is_empty() {
        None
    } else {
        Some(CompiledRules { rules: compiled })
    }
}

/// How far a streamed rewrite must look back to catch a straddling match.
pub fn rules_lookbehind(rules: &[TextRule]) -> usize {
    let mut max = 16;
    for rule in rules {
        let len = rule.pattern.len() + rule.replacement.len();
        if len > max {
            max = len;
        }
    }
    (max * 2).min(512)
}

/* ------------------------------------------------------------- request -- */

/// Build the body actually sent upstream.
pub fn transform_request(
    body: &Value,
    route: &Model,
    cfg: &Config,
    rt: &ResolvedRequestTransform,
) -> Value {
    let mut out = body.clone();
    let Some(map) = out.as_object_mut() else {
        return out;
    };

    // Requirement 5: what the caller asked for becomes what the backend knows.
    map.insert("model".into(), Value::String(route.upstream_model.clone()));

    // Parameter defaults the caller may override...
    for (k, v) in &route.params {
        map.entry(k.clone()).or_insert_with(|| v.clone());
    }
    // ...then hard overrides the caller cannot beat.
    for (k, v) in &route.force_params {
        map.insert(k.clone(), v.clone());
    }

    for key in &rt.drop_params {
        map.remove(key);
    }
    for (from, to) in &rt.rename_params {
        if let Some(v) = map.remove(from) {
            map.insert(to.clone(), v);
        }
    }

    if let Some(messages) = map.get("messages").and_then(|v| v.as_array()).cloned() {
        let injected = inject_system_prompt(&messages, &route.system_prompt, cfg);
        let rewritten = match compile_text_rules(&rt.replace) {
            Some(rules) => injected
                .iter()
                .map(|m| rewrite_message(m, &rules))
                .collect(),
            None => injected,
        };
        map.insert("messages".into(), Value::Array(rewritten));
    }

    if !rt.inject_stop.is_empty() {
        let mut stops: Vec<String> = match map.get("stop") {
            Some(Value::Array(items)) => items
                .iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect(),
            Some(Value::String(s)) => vec![s.clone()],
            _ => Vec::new(),
        };
        for s in &rt.inject_stop {
            if !stops.contains(s) {
                stops.push(s.clone());
            }
        }
        // The OpenAI API caps `stop` at four entries.
        stops.truncate(4);
        map.insert("stop".into(), serde_json::json!(stops));
    }

    let max_out = route.limits.max_output_tokens;
    if max_out > 0 {
        let key = if map.contains_key("max_completion_tokens") {
            "max_completion_tokens"
        } else {
            "max_tokens"
        };
        let capped = match map.get(key).and_then(|v| v.as_u64()) {
            Some(asked) if asked > 0 => asked.min(u64::from(max_out)),
            _ => u64::from(max_out),
        };
        map.insert(key.into(), Value::from(capped));
    }

    out
}

/// Resolve the prompt text for a route, allowing a shared library entry.
pub fn resolve_system_prompt(spec: &SystemPromptSpec, cfg: &Config) -> (String, String) {
    let merged = if !spec.mode.is_empty() && spec.mode != "inherit" {
        spec
    } else {
        &cfg.defaults.system_prompt
    };
    if merged.mode == "none" || merged.mode.is_empty() {
        return ("none".into(), String::new());
    }
    let mut text = merged.text.clone();
    if !merged.prompt_id.is_empty() {
        if let Some(entry) = cfg.system_prompts.iter().find(|p| p.id == merged.prompt_id) {
            text = entry.text.clone();
        }
    }
    (merged.mode.clone(), text)
}

/// Inject the configured system prompt.
///
/// * `prepend` — ours first, then the caller's own system message
/// * `append`  — the caller's first, ours after
/// * `replace` — ours only; the caller's is dropped
/// * `merge`   — one system message, ours on top
pub fn inject_system_prompt(
    messages: &[Value],
    spec: &SystemPromptSpec,
    cfg: &Config,
) -> Vec<Value> {
    let (mode, text) = resolve_system_prompt(spec, cfg);
    if mode == "none" || text.trim().is_empty() {
        return messages.to_vec();
    }

    let is_system = |m: &Value| m.get("role").and_then(|v| v.as_str()) == Some("system");
    let rest: Vec<Value> = messages.iter().filter(|m| !is_system(m)).cloned().collect();
    let theirs: String = messages
        .iter()
        .filter(|m| is_system(m))
        .map(|m| flatten_content(m.get("content").unwrap_or(&Value::Null)))
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n");

    let sys = |t: &str| serde_json::json!({"role": "system", "content": t});
    let mut out = Vec::with_capacity(rest.len() + 2);

    match mode.as_str() {
        "replace" => out.push(sys(&text)),
        "append" => {
            if !theirs.is_empty() {
                out.push(sys(&theirs));
            }
            out.push(sys(&text));
        }
        "merge" => {
            let joined = if theirs.is_empty() {
                text.clone()
            } else {
                format!("{text}\n\n{theirs}")
            };
            out.push(sys(&joined));
        }
        // "prepend" and anything unrecognised
        _ => {
            out.push(sys(&text));
            if !theirs.is_empty() {
                out.push(sys(&theirs));
            }
        }
    }
    out.extend(rest);
    out
}

/// Rewrite rules apply to what the user wrote, never to the injected system
/// prompt — rewriting your own instructions back at yourself is never wanted.
fn rewrite_message(msg: &Value, rules: &CompiledRules) -> Value {
    if msg.get("role").and_then(|v| v.as_str()) == Some("system") {
        return msg.clone();
    }
    let mut out = msg.clone();
    let Some(map) = out.as_object_mut() else {
        return out;
    };
    match map.get("content").cloned() {
        Some(Value::String(text)) => {
            map.insert("content".into(), Value::String(rules.apply(&text)));
        }
        Some(Value::Array(parts)) => {
            let parts: Vec<Value> = parts
                .into_iter()
                .map(|mut p| {
                    if p.get("type").and_then(|v| v.as_str()) == Some("text") {
                        if let Some(obj) = p.as_object_mut() {
                            let text = obj.get("text").and_then(|v| v.as_str()).unwrap_or("");
                            let rewritten = rules.apply(text);
                            obj.insert("text".into(), Value::String(rewritten));
                        }
                    }
                    p
                })
                .collect();
            map.insert("content".into(), Value::Array(parts));
        }
        _ => {}
    }
    out
}

/* ------------------------------------------------------------ response -- */

/// Reshape a complete (non-streamed) chat completion.
pub fn transform_response(
    body: &Value,
    public_model: &str,
    transform: &ResolvedResponseTransform,
) -> Value {
    let mut out = body.clone();
    let Some(map) = out.as_object_mut() else {
        return out;
    };
    let rules = compile_text_rules(&transform.replace);

    if transform.rename_model && !public_model.is_empty() {
        map.insert("model".into(), Value::String(public_model.to_string()));
    }
    for field in &transform.strip_fields {
        map.remove(field);
    }

    if let Some(choices) = map.get("choices").and_then(|v| v.as_array()).cloned() {
        let choices: Vec<Value> = choices
            .into_iter()
            .map(|mut choice| {
                if let Some(c) = choice.as_object_mut() {
                    if let Some(message) = c.get("message").cloned() {
                        c.insert(
                            "message".into(),
                            transform_message(&message, transform, &rules),
                        );
                    }
                    if let Some(Value::String(text)) = c.get("text").cloned() {
                        c.insert(
                            "text".into(),
                            Value::String(apply_text(&text, transform, &rules)),
                        );
                    }
                }
                choice
            })
            .collect();
        map.insert("choices".into(), Value::Array(choices));
    }

    for (k, v) in &transform.set_fields {
        map.insert(k.clone(), v.clone());
    }
    out
}

fn transform_message(
    message: &Value,
    transform: &ResolvedResponseTransform,
    rules: &Option<CompiledRules>,
) -> Value {
    let mut out = message.clone();
    let Some(m) = out.as_object_mut() else {
        return out;
    };
    let reasoning = m
        .get("reasoning_content")
        .or_else(|| m.get("reasoning"))
        .and_then(|v| v.as_str())
        .map(str::to_string);

    match transform.reasoning.as_str() {
        "strip" => {
            m.remove("reasoning_content");
            m.remove("reasoning");
        }
        "inline" => {
            if let Some(r) = &reasoning {
                let content = m.get("content").and_then(|v| v.as_str()).unwrap_or("");
                m.insert(
                    "content".into(),
                    Value::String(format!(
                        "{}{r}{}{content}",
                        transform.reasoning_open, transform.reasoning_close
                    )),
                );
            }
            m.remove("reasoning_content");
            m.remove("reasoning");
        }
        "field" => {
            if let Some(r) = &reasoning {
                m.insert("reasoning".into(), Value::String(r.clone()));
            }
            m.remove("reasoning_content");
        }
        // "keep" and anything unrecognised
        _ => {}
    }

    if let Some(Value::String(content)) = m.get("content").cloned() {
        m.insert(
            "content".into(),
            Value::String(apply_text(&content, transform, rules)),
        );
    }
    out
}

fn apply_text(
    text: &str,
    transform: &ResolvedResponseTransform,
    rules: &Option<CompiledRules>,
) -> String {
    let mut out = match rules {
        Some(r) => r.apply(text),
        None => text.to_string(),
    };
    if !transform.prefix.is_empty() {
        out.insert_str(0, &transform.prefix);
    }
    out.push_str(&transform.suffix);
    out
}

/// Carries the open/closed state of an inlined reasoning block across chunks.
#[derive(Debug, Default)]
pub struct ReasoningState {
    pub open: bool,
}

/// Reshape one streamed chunk.
///
/// Text rewriting is *not* done here: the caller runs it through a
/// [`crate::relay::sse::StreamRewriter`] so a pattern can span chunk
/// boundaries. This handles the structural parts only.
pub fn transform_chunk(
    chunk: &Value,
    public_model: &str,
    transform: &ResolvedResponseTransform,
    state: &mut ReasoningState,
) -> Value {
    let mut out = chunk.clone();
    let Some(map) = out.as_object_mut() else {
        return out;
    };

    if transform.rename_model && !public_model.is_empty() {
        map.insert("model".into(), Value::String(public_model.to_string()));
    }
    for field in &transform.strip_fields {
        map.remove(field);
    }

    if let Some(choices) = map.get("choices").and_then(|v| v.as_array()).cloned() {
        let choices: Vec<Value> = choices
            .into_iter()
            .map(|mut choice| {
                let finish_reason = choice
                    .get("finish_reason")
                    .map(|v| !v.is_null())
                    .unwrap_or(false);
                let Some(c) = choice.as_object_mut() else {
                    return choice;
                };
                let Some(Value::Object(mut delta)) = c.get("delta").cloned() else {
                    return choice;
                };

                let reasoning = delta
                    .get("reasoning_content")
                    .or_else(|| delta.get("reasoning"))
                    .and_then(|v| v.as_str())
                    .map(str::to_string);
                let content = delta
                    .get("content")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();

                match transform.reasoning.as_str() {
                    "strip" => {
                        delta.remove("reasoning_content");
                        delta.remove("reasoning");
                    }
                    "inline" => {
                        if let Some(r) = &reasoning {
                            let open = if state.open {
                                ""
                            } else {
                                state.open = true;
                                transform.reasoning_open.as_str()
                            };
                            delta.insert(
                                "content".into(),
                                Value::String(format!("{open}{r}{content}")),
                            );
                        } else if state.open && (!content.is_empty() || finish_reason) {
                            state.open = false;
                            delta.insert(
                                "content".into(),
                                Value::String(format!("{}{content}", transform.reasoning_close)),
                            );
                        }
                        delta.remove("reasoning_content");
                        delta.remove("reasoning");
                    }
                    "field" => {
                        if let Some(r) = &reasoning {
                            delta.insert("reasoning".into(), Value::String(r.clone()));
                        }
                        delta.remove("reasoning_content");
                    }
                    _ => {}
                }

                c.insert("delta".into(), Value::Object(delta));
                choice
            })
            .collect();
        map.insert("choices".into(), Value::Array(choices));
    }

    for (k, v) in &transform.set_fields {
        map.insert(k.clone(), v.clone());
    }
    out
}

/// Merge streamed `tool_calls` deltas into whole calls, keyed by index.
pub fn collect_tool_calls(map: &mut Map<String, Value>, deltas: &[Value]) {
    for tc in deltas {
        let idx = tc
            .get("index")
            .and_then(|v| v.as_u64())
            .unwrap_or(0)
            .to_string();
        let entry = map.entry(idx).or_insert_with(|| {
            serde_json::json!({
                "id": tc.get("id").cloned().unwrap_or(Value::Null),
                "type": "function",
                "function": {"name": "", "arguments": ""},
            })
        });
        let Some(existing) = entry.as_object_mut() else {
            continue;
        };
        if let Some(id) = tc.get("id").filter(|v| !v.is_null()) {
            existing.insert("id".into(), id.clone());
        }
        let Some(func) = existing.get_mut("function").and_then(|f| f.as_object_mut()) else {
            continue;
        };
        if let Some(name) = tc
            .get("function")
            .and_then(|f| f.get("name"))
            .and_then(|v| v.as_str())
        {
            let current = func.get("name").and_then(|v| v.as_str()).unwrap_or("");
            func.insert("name".into(), Value::String(format!("{current}{name}")));
        }
        if let Some(args) = tc
            .get("function")
            .and_then(|f| f.get("arguments"))
            .and_then(|v| v.as_str())
        {
            let current = func.get("arguments").and_then(|v| v.as_str()).unwrap_or("");
            func.insert(
                "arguments".into(),
                Value::String(format!("{current}{args}")),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Backend, ResponseTransform, SystemPrompt};

    fn cfg_with_route() -> (Config, Model) {
        let mut cfg = Config::default();
        cfg.backends.push(Backend {
            id: "ds".into(),
            name: "deepseek".into(),
            base_url: "https://api.deepseek.com/v1".into(),
            ..Default::default()
        });
        let model = Model {
            id: "manukmiberai/creative-writer".into(),
            backend: "ds".into(),
            upstream_model: "Deepseek-v4-flash-0731".into(),
            ..Default::default()
        };
        cfg.models.push(model.clone());
        (cfg, model)
    }

    #[test]
    fn the_public_name_is_swapped_for_the_backends_real_name() {
        let (cfg, route) = cfg_with_route();
        let body = serde_json::json!({
            "model": "manukmiberai/creative-writer",
            "messages": [{"role": "user", "content": "halo"}],
        });
        let out = transform_request(&body, &route, &cfg, &ResolvedRequestTransform::default());
        assert_eq!(out["model"], "Deepseek-v4-flash-0731");
    }

    #[test]
    fn forced_params_beat_the_caller_but_defaults_do_not() {
        let (cfg, mut route) = cfg_with_route();
        route
            .params
            .insert("temperature".into(), serde_json::json!(0.7));
        route
            .force_params
            .insert("top_p".into(), serde_json::json!(0.9));

        let body = serde_json::json!({"temperature": 0.1, "top_p": 0.1, "messages": []});
        let out = transform_request(&body, &route, &cfg, &ResolvedRequestTransform::default());
        assert_eq!(
            out["temperature"], 0.1,
            "a default must not override the caller"
        );
        assert_eq!(out["top_p"], 0.9, "a forced param must override the caller");
    }

    #[test]
    fn output_limits_cap_the_caller_but_never_raise_them() {
        let (cfg, mut route) = cfg_with_route();
        route.limits.max_output_tokens = 100;
        let rt = ResolvedRequestTransform::default();

        let asked_more = serde_json::json!({"max_tokens": 4000, "messages": []});
        assert_eq!(
            transform_request(&asked_more, &route, &cfg, &rt)["max_tokens"],
            100
        );

        let asked_less = serde_json::json!({"max_tokens": 50, "messages": []});
        assert_eq!(
            transform_request(&asked_less, &route, &cfg, &rt)["max_tokens"],
            50
        );

        let asked_nothing = serde_json::json!({"messages": []});
        assert_eq!(
            transform_request(&asked_nothing, &route, &cfg, &rt)["max_tokens"],
            100
        );
    }

    #[test]
    fn every_injection_mode_places_the_prompt_where_it_says() {
        let (mut cfg, mut route) = cfg_with_route();
        cfg.system_prompts.push(SystemPrompt {
            id: "sp1".into(),
            name: "library".into(),
            text: "FROM LIBRARY".into(),
            updated_at: 0,
        });
        let messages = vec![
            serde_json::json!({"role": "system", "content": "THEIRS"}),
            serde_json::json!({"role": "user", "content": "hi"}),
        ];

        let modes = [
            ("prepend", vec!["OURS", "THEIRS"]),
            ("append", vec!["THEIRS", "OURS"]),
            ("replace", vec!["OURS"]),
            ("merge", vec!["OURS\n\nTHEIRS"]),
        ];
        for (mode, expected) in modes {
            route.system_prompt = SystemPromptSpec {
                mode: mode.into(),
                text: "OURS".into(),
                prompt_id: String::new(),
            };
            let out = inject_system_prompt(&messages, &route.system_prompt, &cfg);
            let systems: Vec<&str> = out
                .iter()
                .filter(|m| m["role"] == "system")
                .map(|m| m["content"].as_str().unwrap_or(""))
                .collect();
            assert_eq!(systems, expected, "mode {mode}");
            assert_eq!(
                out.last().unwrap()["role"],
                "user",
                "mode {mode} kept the user turn"
            );
        }

        // A library entry wins over inline text.
        route.system_prompt = SystemPromptSpec {
            mode: "replace".into(),
            text: "INLINE".into(),
            prompt_id: "sp1".into(),
        };
        let out = inject_system_prompt(&messages, &route.system_prompt, &cfg);
        assert_eq!(out[0]["content"], "FROM LIBRARY");
    }

    #[test]
    fn none_leaves_the_conversation_untouched() {
        let (cfg, route) = cfg_with_route();
        let messages = vec![serde_json::json!({"role": "user", "content": "hi"})];
        assert_eq!(
            inject_system_prompt(&messages, &route.system_prompt, &cfg),
            messages
        );
    }

    #[test]
    fn rewrites_apply_to_the_user_but_never_to_our_own_system_prompt() {
        let (cfg, mut route) = cfg_with_route();
        route.system_prompt = SystemPromptSpec {
            mode: "prepend".into(),
            text: "You are DeepSeek".into(),
            prompt_id: String::new(),
        };
        let rt = ResolvedRequestTransform {
            replace: vec![TextRule {
                pattern: "DeepSeek".into(),
                flags: Some("g".into()),
                replacement: "Writer".into(),
                literal: false,
            }],
            ..Default::default()
        };
        let body = serde_json::json!({
            "messages": [{"role": "user", "content": "tell me about DeepSeek"}]
        });
        let out = transform_request(&body, &route, &cfg, &rt);
        let messages = out["messages"].as_array().unwrap();
        assert_eq!(
            messages[0]["content"], "You are DeepSeek",
            "our prompt is left alone"
        );
        assert_eq!(messages[1]["content"], "tell me about Writer");
    }

    #[test]
    fn the_backend_name_is_scrubbed_out_of_the_reply() {
        let transform =
            ResponseTransform::merged(&ResponseTransform::default(), &ResponseTransform::default());
        let body = serde_json::json!({
            "model": "Deepseek-v4-flash-0731",
            "choices": [{"message": {"role": "assistant", "content": "hello"}}],
        });
        let out = transform_response(&body, "manukmiberai/creative-writer", &transform);
        assert_eq!(out["model"], "manukmiberai/creative-writer");
        assert!(!out.to_string().contains("Deepseek-v4-flash"));
    }

    #[test]
    fn reasoning_can_be_kept_stripped_inlined_or_relocated() {
        let body = serde_json::json!({
            "choices": [{"message": {
                "role": "assistant",
                "content": "answer",
                "reasoning_content": "thinking",
            }}],
        });
        let with = |mode: &str| {
            let route = ResponseTransform {
                reasoning: Some(mode.into()),
                ..Default::default()
            };
            let t = ResponseTransform::merged(&ResponseTransform::default(), &route);
            transform_response(&body, "alias", &t)
        };

        assert_eq!(
            with("keep")["choices"][0]["message"]["reasoning_content"],
            "thinking"
        );
        assert!(with("strip")["choices"][0]["message"]
            .get("reasoning_content")
            .is_none());
        assert_eq!(
            with("inline")["choices"][0]["message"]["content"],
            "<think>thinking</think>answer"
        );
        assert_eq!(
            with("field")["choices"][0]["message"]["reasoning"],
            "thinking"
        );
    }

    #[test]
    fn prefix_and_suffix_wrap_the_reply() {
        let route = ResponseTransform {
            prefix: Some(">> ".into()),
            suffix: Some(" <<".into()),
            ..Default::default()
        };
        let t = ResponseTransform::merged(&ResponseTransform::default(), &route);
        let body = serde_json::json!({"choices": [{"message": {"content": "hi"}}]});
        let out = transform_response(&body, "alias", &t);
        assert_eq!(out["choices"][0]["message"]["content"], ">> hi <<");
    }

    #[test]
    fn fields_can_be_stripped_and_set() {
        let route = ResponseTransform {
            strip_fields: Some(vec!["system_fingerprint".into()]),
            set_fields: Some(
                serde_json::json!({"provider": "manukmiber"})
                    .as_object()
                    .unwrap()
                    .clone(),
            ),
            ..Default::default()
        };
        let t = ResponseTransform::merged(&ResponseTransform::default(), &route);
        let body = serde_json::json!({"system_fingerprint": "fp_x", "choices": []});
        let out = transform_response(&body, "alias", &t);
        assert!(out.get("system_fingerprint").is_none());
        assert_eq!(out["provider"], "manukmiber");
    }

    #[test]
    fn an_invalid_rule_is_skipped_instead_of_taking_the_relay_down() {
        let rules = compile_text_rules(&[
            TextRule {
                pattern: "([unclosed".into(),
                ..Default::default()
            },
            TextRule {
                pattern: "ok".into(),
                replacement: "fine".into(),
                ..Default::default()
            },
        ]);
        let rules = rules.expect("the valid rule survives");
        assert_eq!(rules.apply("ok"), "fine");
    }

    #[test]
    fn a_literal_rule_is_not_treated_as_a_regex() {
        let rules = compile_text_rules(&[TextRule {
            pattern: "a.b".into(),
            replacement: "X".into(),
            literal: true,
            ..Default::default()
        }])
        .unwrap();
        assert_eq!(rules.apply("a.b acb"), "X acb");
    }

    #[test]
    fn streamed_tool_call_fragments_reassemble_into_whole_calls() {
        let mut acc = Map::new();
        collect_tool_calls(
            &mut acc,
            &[
                serde_json::json!({"index": 0, "id": "call_1", "function": {"name": "get_", "arguments": "{\"a\""}}),
            ],
        );
        collect_tool_calls(
            &mut acc,
            &[serde_json::json!({"index": 0, "function": {"name": "weather", "arguments": ":1}"}})],
        );
        assert_eq!(acc["0"]["function"]["name"], "get_weather");
        assert_eq!(acc["0"]["function"]["arguments"], "{\"a\":1}");
        assert_eq!(acc["0"]["id"], "call_1");
    }
}
