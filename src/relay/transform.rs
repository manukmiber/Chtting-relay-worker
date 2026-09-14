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
use std::borrow::Cow;
use std::sync::Arc;

use crate::config::{
    Config, Model, ResolvedRequestTransform, ResolvedResponseTransform, SystemPromptRule,
    SystemPromptSpec, TextRule,
};
use crate::pricing::Effort;
use crate::tokenizer::chat::flatten_content;

/* ---------------------------------------------------------- text rules -- */

pub struct CompiledRules {
    rules: Vec<(Regex, String)>,
}

impl CompiledRules {
    /// Rewrite `text`, borrowing it unchanged when no rule matches.
    ///
    /// The old shape allocated a `String` per rule *and* one for the input,
    /// whether or not anything matched — so a route with three rules that
    /// never fire still copied every reply three times. `replace_all` already
    /// hands back a `Cow::Borrowed` when it changed nothing; passing that
    /// through means the common case allocates nothing at all.
    pub fn apply<'a>(&self, text: &'a str) -> Cow<'a, str> {
        let mut out: Cow<'a, str> = Cow::Borrowed(text);
        for (re, replacement) in &self.rules {
            // Each rule runs over what the last one produced, not over `text`,
            // so the rules still compose exactly as they did.
            let next = match re.replace_all(out.as_ref(), replacement.as_str()) {
                Cow::Borrowed(_) => continue,
                Cow::Owned(next) => next,
            };
            out = Cow::Owned(next);
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

/// Rule sets already compiled, so a regex is built once rather than per call.
///
/// The relay was calling [`compile_text_rules`] two or three times on every
/// request — once on the way out, once for the stream rewriter, once more when
/// the reply was reassembled — for a rule set that only ever changes when
/// somebody edits the config. `Regex::new` on a handful of patterns is tens of
/// microseconds of pure repetition, on a phone, per request.
///
/// Keyed by the rules themselves: a config change misses once and then the
/// steady state compiles nothing at all. The hash is only the bucket — the
/// rules stored beside it are compared for real, so a collision cannot hand
/// back somebody else's patterns.
type RuleCache = parking_lot::RwLock<
    std::collections::HashMap<u64, Vec<(Vec<TextRule>, Option<Arc<CompiledRules>>)>>,
>;

fn rule_cache() -> &'static RuleCache {
    static CACHE: std::sync::OnceLock<RuleCache> = std::sync::OnceLock::new();
    CACHE.get_or_init(Default::default)
}

/// Distinct rule sets to remember before starting over.
///
/// A relay serves one config at a time, so the live set is one or two entries;
/// the rest are what editing left behind. Clearing rather than evicting the
/// oldest keeps this to a few lines — the cost of a miss is one compile.
const RULE_CACHE_MAX: usize = 64;

fn rules_hash(rules: &[TextRule]) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    rules.len().hash(&mut hasher);
    for rule in rules {
        rule.pattern.hash(&mut hasher);
        rule.flags.hash(&mut hasher);
        rule.replacement.hash(&mut hasher);
        rule.literal.hash(&mut hasher);
    }
    hasher.finish()
}

/// Compile `[{pattern, flags, replacement, literal}]` into one rewriter.
///
/// An invalid rule is skipped rather than taking the relay down: a typo in the
/// dashboard should cost that one rule, not every request.
///
/// The result is shared and cached; [`compile_text_rules_uncached`] is the
/// compile itself, for the one caller that wants to measure it.
pub fn compile_text_rules(rules: &[TextRule]) -> Option<Arc<CompiledRules>> {
    // An empty rule set is the overwhelmingly common case and needs no cache
    // entry, no hash and no lock.
    if rules.iter().all(|r| r.pattern.is_empty()) {
        return None;
    }
    let key = rules_hash(rules);

    if let Some(bucket) = rule_cache().read().get(&key) {
        if let Some((_, compiled)) = bucket.iter().find(|(stored, _)| stored == rules) {
            return compiled.clone();
        }
    }

    let compiled = compile_text_rules_uncached(rules).map(Arc::new);
    let mut cache = rule_cache().write();
    if cache.len() >= RULE_CACHE_MAX {
        cache.clear();
    }
    let bucket = cache.entry(key).or_default();
    if !bucket.iter().any(|(stored, _)| stored == rules) {
        bucket.push((rules.to_vec(), compiled.clone()));
    }
    compiled
}

pub fn compile_text_rules_uncached(rules: &[TextRule]) -> Option<CompiledRules> {
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

/// The request fields OpenRouter's chat API defines for routing and reporting
/// rather than for the model: which provider to pick, which models to fall
/// back through, what to report alongside the answer.
///
/// This relay has settled all of that by the time a body is built — it *is* the
/// routing — so these stop at the door. Forwarding one is how a strict backend
/// answers 400 to a request that was perfectly valid when it arrived.
///
/// Dropped before the model's own `params` and `forceParams` are laid on, so a
/// route pointed at OpenRouter itself can still set any of them deliberately.
const ROUTING_ONLY_KEYS: [&str; 7] = [
    "usage",
    "transforms",
    "route",
    "provider",
    "models",
    "plugins",
    "preset",
];

/// Build the body actually sent upstream.
pub fn transform_request(
    body: &Value,
    route: &Model,
    cfg: &Config,
    rt: &ResolvedRequestTransform,
    prompt: &SystemPromptSpec,
) -> Value {
    let mut out = body.clone();
    let Some(map) = out.as_object_mut() else {
        return out;
    };

    // Requirement 5: what the caller asked for becomes what the backend knows.
    map.insert("model".into(), Value::String(route.upstream_model.clone()));

    for key in ROUTING_ONLY_KEYS {
        map.remove(key);
    }

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
        let injected = inject_system_prompt(&messages, prompt, cfg);
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

/// Pick the system prompt this request should get.
///
/// A model may carry several: one for callers who asked for no reasoning, one
/// for the ones who asked for all of it, and any number in between. The first
/// enabled rule that answers for this effort wins; when none does, the model's
/// plain `systemPrompt` is what gets injected, exactly as before.
///
/// Returns the rule's id alongside the spec, so the request row can record
/// which prompt a given answer was produced under.
pub fn select_system_prompt(route: &Model, effort: Effort) -> (&SystemPromptSpec, String) {
    for rule in route.system_prompts.iter().filter(|r| r.enabled) {
        if rule_answers(rule, effort) {
            return (&rule.prompt, rule.id.clone());
        }
    }
    (&route.system_prompt, String::new())
}

fn rule_answers(rule: &SystemPromptRule, effort: Effort) -> bool {
    if !rule.efforts.is_empty() {
        return rule
            .efforts
            .iter()
            .filter_map(|name| Effort::parse(name))
            .any(|e| e == effort);
    }
    if rule.min_effort.is_empty() && rule.max_effort.is_empty() {
        // No condition at all: an unconditional override, which is a legitimate
        // way to say "this model always uses this prompt".
        return true;
    }
    // Ranked bounds are a statement about callers who chose an effort. Silence
    // is not a choice, so it falls through to the model's default prompt.
    let Some(rank) = effort.rank() else {
        return false;
    };
    if let Some(min) = Effort::parse(&rule.min_effort).and_then(Effort::rank) {
        if rank < min {
            return false;
        }
    }
    if let Some(max) = Effort::parse(&rule.max_effort).and_then(Effort::rank) {
        if rank > max {
            return false;
        }
    }
    true
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
            map.insert(
                "content".into(),
                Value::String(rules.apply(&text).into_owned()),
            );
        }
        Some(Value::Array(parts)) => {
            let parts: Vec<Value> = parts
                .into_iter()
                .map(|mut p| {
                    if p.get("type").and_then(|v| v.as_str()) == Some("text") {
                        if let Some(obj) = p.as_object_mut() {
                            let text = obj.get("text").and_then(|v| v.as_str()).unwrap_or("");
                            let rewritten = rules.apply(text);
                            obj.insert("text".into(), Value::String(rewritten.into_owned()));
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

/// Who the answer says it is.
///
/// Every field here is the relay's own: a uuid it minted, the moment it took
/// the request, and the public model name. None of it comes from the backend,
/// which is the whole point — see [`rebuild`].
#[derive(Debug, Clone)]
pub struct Identity {
    pub id: String,
    pub created: i64,
    pub model: String,
    /// Who served this, in OpenRouter's vocabulary — the relay's own provider
    /// slug, never the backend that actually ran the prompt. Empty publishes
    /// no `provider` field at all.
    pub provider: String,
}

impl Identity {
    /// Start a reply envelope from this identity and nothing else.
    ///
    /// Every envelope the relay emits comes from here — the buffered reply, a
    /// streamed chunk, the closing usage frame, a replayed stream — so there is
    /// one place where "what a reply looks like" is decided and no path can
    /// quietly drift from the others.
    pub fn envelope(&self, object: &str) -> Map<String, Value> {
        let mut out = Map::with_capacity(5);
        out.insert("id".into(), Value::String(self.id.clone()));
        out.insert("object".into(), Value::String(object.to_string()));
        out.insert("created".into(), Value::from(self.created));
        out.insert("model".into(), Value::String(self.model.clone()));
        if !self.provider.is_empty() {
            out.insert("provider".into(), Value::String(self.provider.clone()));
        }
        out
    }
}

/// The keys a delta may carry outward. Everything else the backend puts in one
/// is dropped.
const DELTA_KEYS: [&str; 5] = [
    "role",
    "content",
    "reasoning_content",
    "reasoning",
    "tool_calls",
];

/// The keys an assembled message may carry outward.
const MESSAGE_KEYS: [&str; 5] = [
    "role",
    "content",
    "reasoning_content",
    "reasoning",
    "tool_calls",
];

/// Start a reply envelope from nothing.
///
/// Reshaping by deletion is the wrong way round: it only removes what someone
/// thought to name, so the day a backend adds a field, that field ships. This
/// builds the envelope from the relay's own values and copies across only the
/// handful of keys below, so a new field upstream is not a leak here — it is
/// simply not copied. `system_fingerprint`, the backend's request id, its
/// `created`, its model name and its `service_tier` all stop at this line.
fn rebuild(identity: &Identity, object: &str) -> Map<String, Value> {
    identity.envelope(object)
}

/// Copy the allowed keys of `from` into a fresh map.
fn only(from: &Map<String, Value>, keys: &[&str]) -> Map<String, Value> {
    let mut out = Map::with_capacity(keys.len());
    for key in keys {
        if let Some(value) = from.get(*key) {
            if !value.is_null() {
                out.insert((*key).to_string(), value.clone());
            }
        }
    }
    out
}

fn finish_reason_of(choice: &Value) -> Value {
    match choice.get("finish_reason") {
        Some(v) if !v.is_null() => v.clone(),
        _ => Value::Null,
    }
}

fn index_of(choice: &Value) -> u64 {
    choice.get("index").and_then(|v| v.as_u64()).unwrap_or(0)
}

/// Reshape a complete (non-streamed) chat completion.
pub fn transform_response(
    body: &Value,
    identity: &Identity,
    transform: &ResolvedResponseTransform,
) -> Value {
    let rules = compile_text_rules(&transform.replace);
    let mut map = rebuild(identity, "chat.completion");
    // A route may opt out of the rename, in which case the backend's own name
    // is what goes out — a deliberate choice, and the only way its name ever
    // leaves this process.
    if !transform.rename_model {
        if let Some(model) = body.get("model").filter(|v| v.is_string()) {
            map.insert("model".into(), model.clone());
        }
    }

    let choices: Vec<Value> = body
        .get("choices")
        .and_then(|v| v.as_array())
        .map(|choices| {
            choices
                .iter()
                .map(|choice| {
                    let mut out = Map::with_capacity(4);
                    out.insert("index".into(), Value::from(index_of(choice)));
                    if let Some(message) = choice.get("message").and_then(|m| m.as_object()) {
                        out.insert(
                            "message".into(),
                            transform_message(message, transform, &rules),
                        );
                    }
                    // The legacy completions shape, which has text where chat
                    // has a message.
                    if let Some(Value::String(text)) = choice.get("text") {
                        out.insert(
                            "text".into(),
                            Value::String(apply_text(text, transform, &rules).into_owned()),
                        );
                    }
                    let finish_reason = finish_reason_of(choice);
                    // OpenRouter's pair: the normalised reason and the one the
                    // provider itself gave. The relay normalises nothing here,
                    // so they are the same value — said twice rather than left
                    // for a client to guess at.
                    out.insert("native_finish_reason".into(), finish_reason.clone());
                    out.insert("finish_reason".into(), finish_reason);
                    Value::Object(out)
                })
                .collect()
        })
        .unwrap_or_default();
    map.insert("choices".into(), Value::Array(choices));

    // Applied last so an operator can still strip or add whatever they want on
    // top of the rebuilt envelope.
    for field in &transform.strip_fields {
        map.remove(field);
    }
    for (k, v) in &transform.set_fields {
        map.insert(k.clone(), v.clone());
    }
    Value::Object(map)
}

fn transform_message(
    message: &Map<String, Value>,
    transform: &ResolvedResponseTransform,
    rules: &Option<Arc<CompiledRules>>,
) -> Value {
    let mut m = only(message, &MESSAGE_KEYS);
    m.entry("role".to_string())
        .or_insert_with(|| Value::String("assistant".into()));

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
            Value::String(apply_text(&content, transform, rules).into_owned()),
        );
    }
    Value::Object(m)
}

/// Rewrite one piece of text and wrap it in the route's prefix and suffix.
///
/// Borrowed all the way through when there is nothing to do — no rules, no
/// prefix, no suffix — which is what the great majority of routes look like.
fn apply_text<'a>(
    text: &'a str,
    transform: &ResolvedResponseTransform,
    rules: &Option<Arc<CompiledRules>>,
) -> Cow<'a, str> {
    let rewritten = match rules {
        Some(r) => r.apply(text),
        None => Cow::Borrowed(text),
    };
    if transform.prefix.is_empty() && transform.suffix.is_empty() {
        return rewritten;
    }
    Cow::Owned(format!(
        "{}{rewritten}{}",
        transform.prefix, transform.suffix
    ))
}

/// Carries the open/closed state of an inlined reasoning block across chunks.
#[derive(Debug, Default)]
pub struct ReasoningState {
    pub open: bool,
}

/// Reshape one streamed chunk, rebuilt from nothing exactly as
/// [`transform_response`] is.
///
/// The backend's `usage` is deliberately not carried over: the relay reports
/// the input it counted itself, not the one the backend billed, and it sends
/// that in a closing chunk of its own.
///
/// Text rewriting is *not* done here: the caller runs it through a
/// [`crate::relay::sse::StreamRewriter`] so a pattern can span chunk
/// boundaries. This handles the structural parts only.
pub fn transform_chunk(
    chunk: &Value,
    identity: &Identity,
    transform: &ResolvedResponseTransform,
    state: &mut ReasoningState,
) -> Value {
    let mut map = rebuild(identity, "chat.completion.chunk");
    if !transform.rename_model {
        if let Some(model) = chunk.get("model").filter(|v| v.is_string()) {
            map.insert("model".into(), model.clone());
        }
    }

    let choices: Vec<Value> = chunk
        .get("choices")
        .and_then(|v| v.as_array())
        .map(|choices| {
            choices
                .iter()
                .map(|choice| {
                    let finish_reason = finish_reason_of(choice);
                    let mut out = Map::with_capacity(3);
                    out.insert("index".into(), Value::from(index_of(choice)));
                    let delta = choice
                        .get("delta")
                        .and_then(|d| d.as_object())
                        .map(|d| only(d, &DELTA_KEYS))
                        .unwrap_or_default();
                    out.insert(
                        "delta".into(),
                        Value::Object(transform_delta(
                            delta,
                            transform,
                            state,
                            !finish_reason.is_null(),
                        )),
                    );
                    out.insert("native_finish_reason".into(), finish_reason.clone());
                    out.insert("finish_reason".into(), finish_reason);
                    Value::Object(out)
                })
                .collect()
        })
        .unwrap_or_default();
    map.insert("choices".into(), Value::Array(choices));

    for field in &transform.strip_fields {
        map.remove(field);
    }
    for (k, v) in &transform.set_fields {
        map.insert(k.clone(), v.clone());
    }
    Value::Object(map)
}

fn transform_delta(
    mut delta: Map<String, Value>,
    transform: &ResolvedResponseTransform,
    state: &mut ReasoningState,
    finished: bool,
) -> Map<String, Value> {
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
            } else if state.open && (!content.is_empty() || finished) {
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
    delta
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

    /// Compiling the same rules twice must hand back the same object, because
    /// the whole point is that a request does not pay for a regex build that
    /// has already happened. Different rules must not collide with them.
    #[test]
    fn a_rule_set_is_compiled_once_and_then_shared() {
        let rules = |pattern: &str| {
            vec![TextRule {
                pattern: pattern.into(),
                flags: Some("gi".into()),
                replacement: "Writer".into(),
                literal: false,
            }]
        };

        let first = compile_text_rules(&rules("DeepSeek")).expect("compiles");
        let again = compile_text_rules(&rules("DeepSeek")).expect("compiles");
        assert!(
            Arc::ptr_eq(&first, &again),
            "the second call rebuilt the same rules"
        );

        let other = compile_text_rules(&rules("Qwen")).expect("compiles");
        assert!(!Arc::ptr_eq(&first, &other));
        assert_eq!(first.apply("ask DeepSeek"), "ask Writer");
        assert_eq!(other.apply("ask DeepSeek"), "ask DeepSeek");

        // A rule set that compiles to nothing is remembered as nothing rather
        // than retried on every request.
        assert!(compile_text_rules(&[]).is_none());
        assert!(compile_text_rules(&rules("")).is_none());
    }

    /// Text no rule touches comes back borrowed, not copied.
    #[test]
    fn text_that_matches_nothing_is_not_copied() {
        let rules = compile_text_rules(&[TextRule {
            pattern: "DeepSeek".into(),
            replacement: "Writer".into(),
            ..Default::default()
        }])
        .expect("compiles");

        let untouched = "nothing here matches";
        assert!(matches!(rules.apply(untouched), Cow::Borrowed(_)));
        assert!(matches!(rules.apply("ask DeepSeek"), Cow::Owned(_)));
    }
    use crate::config::{Backend, ResponseTransform, SystemPrompt};

    fn identity(model: &str) -> Identity {
        Identity {
            id: "7f3a11d2-9b0c-4f6e-8a21-5c7d9e0b1a34".into(),
            created: 1_789_263_746,
            model: model.into(),
            provider: "chtting".into(),
        }
    }

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
        let out = transform_request(
            &body,
            &route,
            &cfg,
            &ResolvedRequestTransform::default(),
            &route.system_prompt,
        );
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
        let out = transform_request(
            &body,
            &route,
            &cfg,
            &ResolvedRequestTransform::default(),
            &route.system_prompt,
        );
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
            transform_request(&asked_more, &route, &cfg, &rt, &route.system_prompt)["max_tokens"],
            100
        );

        let asked_less = serde_json::json!({"max_tokens": 50, "messages": []});
        assert_eq!(
            transform_request(&asked_less, &route, &cfg, &rt, &route.system_prompt)["max_tokens"],
            50
        );

        let asked_nothing = serde_json::json!({"messages": []});
        assert_eq!(
            transform_request(&asked_nothing, &route, &cfg, &rt, &route.system_prompt)
                ["max_tokens"],
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
        let out = transform_request(&body, &route, &cfg, &rt, &route.system_prompt);
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
        let out = transform_response(&body, &identity("manukmiberai/creative-writer"), &transform);
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
            transform_response(&body, &identity("alias"), &t)
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
        let out = transform_response(&body, &identity("alias"), &t);
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
        let out = transform_response(&body, &identity("alias"), &t);
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
