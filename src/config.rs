//! The single source of truth for the relay.
//!
//! The on-disk shape is byte-for-byte the same JSON the Node version used, so
//! an existing `config/config.json` keeps working across the port. Everything
//! the dashboard edits lives here, is validated on write, and is saved
//! atomically.
//!
//! Reads happen on every single request, so the live config is published
//! through an [`ArcSwap`]: readers never take a lock and never block a writer.

use anyhow::{bail, Result};
use arc_swap::ArcSwap;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::util::{deep_merge, mask_secret, new_id, write_atomic};

const SECRET_KEYS: [&str; 4] = ["apiKey", "key", "password", "token"];

/* ------------------------------------------------------------- helpers -- */

/// Distinguishes "absent" from "explicitly null" so that a model can opt back
/// out of a default that was switched on globally.
mod double_option {
    use serde::{Deserialize, Deserializer};
    pub fn deserialize<'de, D, T>(d: D) -> Result<Option<Option<T>>, D::Error>
    where
        D: Deserializer<'de>,
        T: Deserialize<'de>,
    {
        Option::<T>::deserialize(d).map(Some)
    }
}

/* -------------------------------------------------------------- schema -- */

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Config {
    pub version: u32,
    pub timezone: String,
    pub server: ServerConfig,
    pub dashboard: DashboardConfig,
    pub security: SecurityConfig,
    pub backends: Vec<Backend>,
    pub models: Vec<Model>,
    pub keys: Vec<ClientKey>,
    pub system_prompts: Vec<SystemPrompt>,
    pub defaults: Defaults,
    pub tokenizer: TokenizerConfig,
    pub logging: LoggingConfig,
    pub tunnel: TunnelConfig,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            version: 2,
            timezone: "Asia/Jakarta".into(),
            server: ServerConfig::default(),
            dashboard: DashboardConfig::default(),
            security: SecurityConfig::default(),
            backends: Vec::new(),
            models: Vec::new(),
            keys: Vec::new(),
            system_prompts: Vec::new(),
            defaults: Defaults::default(),
            tokenizer: TokenizerConfig::default(),
            logging: LoggingConfig::default(),
            tunnel: TunnelConfig::default(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ServerConfig {
    pub host: String,
    pub port: u16,
    pub max_body_bytes: usize,
    pub keep_alive_timeout_ms: u64,
    /// Ceiling on requests in flight upstream at once. Past this the relay
    /// answers 503 immediately instead of queueing until the phone dies.
    pub max_concurrent_requests: usize,
    /// Tokio worker threads. 0 means "one per core", which is what a phone
    /// wants; pin it lower to leave headroom for other Termux processes.
    pub worker_threads: usize,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            host: "0.0.0.0".into(),
            port: 8787,
            max_body_bytes: 20 * 1024 * 1024,
            keep_alive_timeout_ms: 75_000,
            max_concurrent_requests: 512,
            worker_threads: 0,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct DashboardConfig {
    pub enabled: bool,
    /// Keep this on loopback: the dashboard is never routed through the tunnel.
    pub host: String,
    pub port: u16,
    pub password: String,
    pub session_ttl_ms: i64,
}

impl Default for DashboardConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            host: "127.0.0.1".into(),
            port: 8788,
            password: String::new(),
            session_ttl_ms: 7 * 24 * 60 * 60 * 1000,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct SecurityConfig {
    pub require_client_key: bool,
    pub cors_origins: Vec<String>,
    pub trust_proxy_headers: bool,
    pub blocked_ips: Vec<String>,
}

impl Default for SecurityConfig {
    fn default() -> Self {
        Self {
            require_client_key: true,
            cors_origins: vec!["*".into()],
            trust_proxy_headers: true,
            blocked_ips: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Backend {
    pub id: String,
    pub name: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub base_url: String,
    pub api_key: String,
    pub enabled: bool,
    pub timeout_ms: u64,
    pub headers: BTreeMap<String, String>,
    pub max_retries: u32,
    /// Ask for `stream_options.include_usage` when streaming.
    pub stream_options: bool,
    pub note: String,
}

impl Default for Backend {
    fn default() -> Self {
        Self {
            id: String::new(),
            name: String::new(),
            kind: "openai".into(),
            base_url: String::new(),
            api_key: String::new(),
            enabled: true,
            timeout_ms: 600_000,
            headers: BTreeMap::new(),
            max_retries: 1,
            stream_options: true,
            note: String::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Model {
    pub id: String,
    pub aliases: Vec<String>,
    pub enabled: bool,
    pub display_name: String,
    pub description: String,
    pub backend: String,
    /// The name actually sent upstream. Never exposed to callers.
    pub upstream_model: String,
    pub fallbacks: Vec<String>,
    pub system_prompt: SystemPromptSpec,
    pub params: Map<String, Value>,
    pub force_params: Map<String, Value>,
    pub limits: Limits,
    pub tokenizer: String,
    pub chat_profile: String,
    pub request_transform: RequestTransform,
    pub response_transform: ResponseTransform,
    pub context_length: u32,
    pub created_at: i64,
}

impl Default for Model {
    fn default() -> Self {
        Self {
            id: String::new(),
            aliases: Vec::new(),
            enabled: true,
            display_name: String::new(),
            description: String::new(),
            backend: String::new(),
            upstream_model: String::new(),
            fallbacks: Vec::new(),
            system_prompt: SystemPromptSpec::default(),
            params: Map::new(),
            force_params: Map::new(),
            limits: Limits::default(),
            tokenizer: String::new(),
            chat_profile: String::new(),
            request_transform: RequestTransform::default(),
            response_transform: ResponseTransform::default(),
            context_length: 0,
            created_at: 0,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Limits {
    pub max_input_tokens: u32,
    pub max_output_tokens: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct SystemPromptSpec {
    /// `none` | `prepend` | `append` | `replace` | `merge` | `inherit`
    pub mode: String,
    pub text: String,
    /// Points at a `systemPrompts[]` entry; wins over `text`.
    pub prompt_id: String,
}

impl Default for SystemPromptSpec {
    fn default() -> Self {
        Self {
            mode: "none".into(),
            text: String::new(),
            prompt_id: String::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct SystemPrompt {
    pub id: String,
    pub name: String,
    pub text: String,
    pub updated_at: i64,
}

impl Default for SystemPrompt {
    fn default() -> Self {
        Self {
            id: String::new(),
            name: "prompt".into(),
            text: String::new(),
            updated_at: 0,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ClientKey {
    pub id: String,
    pub label: String,
    pub key: String,
    pub enabled: bool,
    /// `["*"]` or an explicit list of public model ids.
    pub models: Vec<String>,
    pub quota: Quota,
    pub note: String,
    pub created_at: i64,
}

impl Default for ClientKey {
    fn default() -> Self {
        Self {
            id: String::new(),
            label: String::new(),
            key: String::new(),
            enabled: true,
            models: vec!["*".into()],
            quota: Quota::default(),
            note: String::new(),
            created_at: 0,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Quota {
    pub requests_per_day: u64,
    pub tokens_per_day: u64,
    pub requests_per_minute: u32,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Defaults {
    pub system_prompt: SystemPromptSpec,
    pub params: Map<String, Value>,
    pub request_transform: RequestTransform,
    pub response_transform: ResponseTransform,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct RequestTransform {
    /// `Some(Some(true))` always streams upstream, `Some(Some(false))` never
    /// does, `Some(None)` explicitly follows the caller, `None` inherits.
    #[serde(deserialize_with = "double_option::deserialize", skip_serializing_if = "Option::is_none")]
    pub force_stream: Option<Option<bool>>,
    pub drop_params: Option<Vec<String>>,
    pub rename_params: Option<BTreeMap<String, String>>,
    pub inject_stop: Option<Vec<String>>,
    pub replace: Option<Vec<TextRule>>,
}

impl RequestTransform {
    /// Field-by-field override of the global defaults.
    pub fn merged(defaults: &Self, route: &Self) -> ResolvedRequestTransform {
        ResolvedRequestTransform {
            force_stream: route
                .force_stream
                .or(defaults.force_stream)
                .flatten(),
            drop_params: route
                .drop_params
                .clone()
                .or_else(|| defaults.drop_params.clone())
                .unwrap_or_default(),
            rename_params: route
                .rename_params
                .clone()
                .or_else(|| defaults.rename_params.clone())
                .unwrap_or_default(),
            inject_stop: route
                .inject_stop
                .clone()
                .or_else(|| defaults.inject_stop.clone())
                .unwrap_or_default(),
            replace: route
                .replace
                .clone()
                .or_else(|| defaults.replace.clone())
                .unwrap_or_default(),
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct ResolvedRequestTransform {
    pub force_stream: Option<bool>,
    pub drop_params: Vec<String>,
    pub rename_params: BTreeMap<String, String>,
    pub inject_stop: Vec<String>,
    pub replace: Vec<TextRule>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ResponseTransform {
    pub rename_model: Option<bool>,
    /// `keep` | `strip` | `inline` | `field`
    pub reasoning: Option<String>,
    pub reasoning_tags: Option<Vec<String>>,
    pub strip_fields: Option<Vec<String>>,
    pub set_fields: Option<Map<String, Value>>,
    pub prefix: Option<String>,
    pub suffix: Option<String>,
    pub replace: Option<Vec<TextRule>>,
}

impl ResponseTransform {
    pub fn merged(defaults: &Self, route: &Self) -> ResolvedResponseTransform {
        let tags = route
            .reasoning_tags
            .clone()
            .or_else(|| defaults.reasoning_tags.clone())
            .unwrap_or_else(|| vec!["<think>".into(), "</think>".into()]);
        ResolvedResponseTransform {
            rename_model: route.rename_model.or(defaults.rename_model).unwrap_or(true),
            reasoning: route
                .reasoning
                .clone()
                .or_else(|| defaults.reasoning.clone())
                .unwrap_or_else(|| "keep".into()),
            reasoning_open: tags.first().cloned().unwrap_or_default(),
            reasoning_close: tags.get(1).cloned().unwrap_or_default(),
            strip_fields: route
                .strip_fields
                .clone()
                .or_else(|| defaults.strip_fields.clone())
                .unwrap_or_default(),
            set_fields: route
                .set_fields
                .clone()
                .or_else(|| defaults.set_fields.clone())
                .unwrap_or_default(),
            prefix: route
                .prefix
                .clone()
                .or_else(|| defaults.prefix.clone())
                .unwrap_or_default(),
            suffix: route
                .suffix
                .clone()
                .or_else(|| defaults.suffix.clone())
                .unwrap_or_default(),
            replace: route
                .replace
                .clone()
                .or_else(|| defaults.replace.clone())
                .unwrap_or_default(),
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct ResolvedResponseTransform {
    pub rename_model: bool,
    pub reasoning: String,
    pub reasoning_open: String,
    pub reasoning_close: String,
    pub strip_fields: Vec<String>,
    pub set_fields: Map<String, Value>,
    pub prefix: String,
    pub suffix: String,
    pub replace: Vec<TextRule>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", default)]
pub struct TextRule {
    pub pattern: String,
    pub flags: Option<String>,
    pub replacement: String,
    /// Escape the pattern instead of treating it as a regex.
    pub literal: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct TokenizerConfig {
    pub fallback: String,
    /// Trust the backend's own `usage` when it reports one.
    pub prefer_upstream_usage: bool,
    pub rules: Vec<TokenizerRule>,
    pub image_defaults: ImageDefaults,
}

impl Default for TokenizerConfig {
    fn default() -> Self {
        Self {
            fallback: "o200k_base".into(),
            prefer_upstream_usage: true,
            rules: default_tokenizer_rules(),
            image_defaults: ImageDefaults::default(),
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct TokenizerRule {
    /// Case-insensitive glob against the **backend** model name.
    #[serde(rename = "match")]
    pub pattern: String,
    pub tokenizer: String,
    pub profile: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ImageDefaults {
    pub default_detail: String,
    pub default_width: u32,
    pub default_height: u32,
}

impl Default for ImageDefaults {
    fn default() -> Self {
        Self {
            default_detail: "auto".into(),
            default_width: 1024,
            default_height: 1024,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct LoggingConfig {
    pub level: String,
    pub retention_days: i64,
    /// `none` | `preview` | `full`
    pub store_bodies: String,
    pub preview_chars: usize,
    pub file_enabled: bool,
}

impl Default for LoggingConfig {
    fn default() -> Self {
        Self {
            level: "info".into(),
            retention_days: 30,
            store_bodies: "preview".into(),
            preview_chars: 800,
            file_enabled: true,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct TunnelConfig {
    /// `quick` | `named` | `off`
    pub mode: String,
    pub binary: String,
    pub auto_start: bool,
    pub token: String,
    pub hostname: String,
    pub config_file: String,
    pub extra_args: Vec<String>,
}

impl Default for TunnelConfig {
    fn default() -> Self {
        Self {
            mode: "quick".into(),
            binary: "cloudflared".into(),
            auto_start: false,
            token: String::new(),
            hostname: String::new(),
            config_file: String::new(),
            extra_args: Vec::new(),
        }
    }
}

/// Sensible defaults covering the model families a relay usually fronts.
pub fn default_tokenizer_rules() -> Vec<TokenizerRule> {
    let r = |p: &str, t: &str, prof: &str| TokenizerRule {
        pattern: p.into(),
        tokenizer: t.into(),
        profile: prof.into(),
    };
    vec![
        r("gpt-4o*", "o200k_base", "openai"),
        r("gpt-5*", "o200k_base", "openai"),
        r("o1*", "o200k_base", "openai"),
        r("o3*", "o200k_base", "openai"),
        r("gpt-4*", "cl100k_base", "openai"),
        r("gpt-3.5*", "cl100k_base", "openai"),
        r("text-embedding-*", "cl100k_base", "raw"),
        r("deepseek*", "deepseek", "deepseek"),
        r("qwen*", "qwen", "chatml"),
        r("llama-3*", "llama3", "llama3"),
        r("mistral*", "mistral", "mistral"),
        r("gemma*", "gemma", "gemma"),
        r("claude*", "cl100k_base", "openai"),
        r("*", "o200k_base", "openai"),
    ]
}

/* ------------------------------------------------------------ accessors -- */

impl Config {
    pub fn find_model(&self, public_name: &str) -> Option<&Model> {
        let want = public_name.trim();
        if want.is_empty() {
            return None;
        }
        self.models
            .iter()
            .find(|m| m.id == want)
            .or_else(|| self.models.iter().find(|m| m.aliases.iter().any(|a| a == want)))
    }

    pub fn find_backend(&self, id: &str) -> Option<&Backend> {
        self.backends.iter().find(|b| b.id == id)
    }

    pub fn find_key_by_secret(&self, secret: &str) -> Option<&ClientKey> {
        if secret.is_empty() {
            return None;
        }
        // Constant-time compare so a timing oracle cannot walk the key out.
        self.keys
            .iter()
            .find(|k| crate::util::safe_equal(&k.key, secret))
    }

    pub fn tz(&self) -> chrono_tz::Tz {
        crate::util::parse_tz(&self.timezone)
    }
}

/* --------------------------------------------------------------- store -- */

/// Live config plus the file it came from.
///
/// `load()` reads once; every later read goes through `current()`, which is a
/// lock-free atomic pointer read — that is what lets hundreds of concurrent
/// requests consult the routing table without contending on anything.
pub struct ConfigStore {
    file: PathBuf,
    current: ArcSwap<Config>,
    /// Serialises writers only. Readers never touch it.
    write_lock: tokio::sync::Mutex<()>,
}

impl ConfigStore {
    pub async fn load(file: &Path) -> Result<Self> {
        let cfg = if tokio::fs::try_exists(file).await.unwrap_or(false) {
            let raw = tokio::fs::read_to_string(file).await?;
            let parsed: Value = serde_json::from_str(&raw)
                .map_err(|e| anyhow::anyhow!("config at {} is not valid JSON: {e}", file.display()))?;
            let merged = deep_merge(&serde_json::to_value(Config::default())?, &parsed);
            normalize(serde_json::from_value(merged)?)
        } else {
            let cfg = normalize(Config::default());
            write_atomic(file, &to_pretty(&cfg)?).await?;
            cfg
        };

        let errors = validate(&cfg);
        if !errors.is_empty() {
            bail!("config is not usable: {}", errors.join("; "));
        }

        Ok(Self {
            file: file.to_path_buf(),
            current: ArcSwap::from_pointee(cfg),
            write_lock: tokio::sync::Mutex::new(()),
        })
    }

    /// Lock-free read of the live config.
    pub fn current(&self) -> Arc<Config> {
        self.current.load_full()
    }

    pub fn file(&self) -> &Path {
        &self.file
    }

    /// Merge a JSON patch, validate, persist atomically, publish.
    pub async fn update(&self, patch: Value) -> Result<Arc<Config>> {
        let _guard = self.write_lock.lock().await;
        let base = serde_json::to_value(&*self.current())?;
        let merged: Config = serde_json::from_value(deep_merge(&base, &patch))?;
        let next = normalize(merged);

        let errors = validate(&next);
        if !errors.is_empty() {
            bail!("{}", errors.join("; "));
        }

        write_atomic(&self.file, &to_pretty(&next)?).await?;
        let next = Arc::new(next);
        self.current.store(next.clone());
        Ok(next)
    }

    /// Replace a whole collection (`models` / `backends` / `keys` / `systemPrompts`).
    pub async fn replace_list(&self, name: &str, items: Value) -> Result<Arc<Config>> {
        if !items.is_array() {
            bail!("{name} must be an array");
        }
        // A list replacement must not deep-merge with what is already there,
        // otherwise deleting an item would be impossible.
        let _guard = self.write_lock.lock().await;
        let mut base = serde_json::to_value(&*self.current())?;
        base.as_object_mut()
            .expect("config serialises to an object")
            .insert(name.to_string(), items);
        let next = normalize(serde_json::from_value(base)?);

        let errors = validate(&next);
        if !errors.is_empty() {
            bail!("{}", errors.join("; "));
        }
        write_atomic(&self.file, &to_pretty(&next)?).await?;
        let next = Arc::new(next);
        self.current.store(next.clone());
        Ok(next)
    }

    /// Insert or update one item of a collection by id.
    pub async fn upsert(&self, name: &str, item: Value) -> Result<Value> {
        let current = self.current();
        let base = serde_json::to_value(&*current)?;
        let list = base
            .get(name)
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();

        let id = item
            .get("id")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| new_id(&name.chars().take(3).collect::<String>()));

        let mut next = list.clone();
        let mut merged = item.clone();
        merged
            .as_object_mut()
            .ok_or_else(|| anyhow::anyhow!("{name} item must be an object"))?
            .insert("id".into(), Value::String(id.clone()));

        match next
            .iter()
            .position(|x| x.get("id").and_then(|v| v.as_str()) == Some(id.as_str()))
        {
            Some(idx) => {
                merged = deep_merge(&next[idx], &merged);
                next[idx] = merged.clone();
            }
            None => next.push(merged.clone()),
        }

        self.replace_list(name, Value::Array(next)).await?;

        // Return the item as it was actually stored, after normalisation.
        let stored = serde_json::to_value(&*self.current())?;
        Ok(stored
            .get(name)
            .and_then(|v| v.as_array())
            .and_then(|a| {
                a.iter()
                    .find(|x| x.get("id").and_then(|v| v.as_str()) == Some(id.as_str()))
            })
            .cloned()
            .unwrap_or(merged))
    }

    pub async fn remove(&self, name: &str, id: &str) -> Result<()> {
        let base = serde_json::to_value(&*self.current())?;
        let kept: Vec<Value> = base
            .get(name)
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter(|x| x.get("id").and_then(|v| v.as_str()) != Some(id))
                    .cloned()
                    .collect()
            })
            .unwrap_or_default();
        self.replace_list(name, Value::Array(kept)).await?;
        Ok(())
    }

    /// The config with every secret masked, for the dashboard.
    pub fn redacted(&self) -> Value {
        let raw = serde_json::to_value(&*self.current()).unwrap_or(Value::Null);
        redact(&raw, "")
    }
}

fn to_pretty(cfg: &Config) -> Result<String> {
    Ok(format!("{}\n", serde_json::to_string_pretty(cfg)?))
}

/* ---------------------------------------------------------- normalize -- */

/// Fill in ids, trim URLs, drop unusable entries. Runs on load and every save,
/// so the file on disk is always in canonical form.
pub fn normalize(mut cfg: Config) -> Config {
    let now = crate::util::now_ms();

    for b in &mut cfg.backends {
        if b.id.is_empty() {
            b.id = new_id("be");
        }
        if b.name.is_empty() {
            b.name = if b.id.is_empty() { "backend".into() } else { b.id.clone() };
        }
        if b.kind.is_empty() {
            b.kind = "openai".into();
        }
        b.base_url = b.base_url.trim_end_matches('/').to_string();
    }

    for m in &mut cfg.models {
        m.id = m.id.trim().to_string();
        m.aliases.retain(|a| !a.trim().is_empty());
        if m.display_name.is_empty() {
            m.display_name = m.id.clone();
        }
        if m.created_at == 0 {
            m.created_at = now;
        }
    }
    cfg.models.retain(|m| !m.id.is_empty());

    for k in &mut cfg.keys {
        if k.id.is_empty() {
            k.id = new_id("key");
        }
        if k.models.is_empty() {
            k.models = vec!["*".into()];
        }
        if k.created_at == 0 {
            k.created_at = now;
        }
    }

    for p in &mut cfg.system_prompts {
        if p.id.is_empty() {
            p.id = new_id("sp");
        }
        if p.updated_at == 0 {
            p.updated_at = now;
        }
    }

    if cfg.timezone.trim().is_empty() {
        cfg.timezone = "Asia/Jakarta".into();
    }
    cfg
}

/* ----------------------------------------------------------- validate -- */

pub fn validate(cfg: &Config) -> Vec<String> {
    let mut errors = Vec::new();
    let backend_ids: Vec<&str> = cfg.backends.iter().map(|b| b.id.as_str()).collect();

    for b in &cfg.backends {
        if b.base_url.is_empty() {
            errors.push(format!("backend \"{}\" needs a baseUrl", b.name));
        } else {
            let lower = b.base_url.to_lowercase();
            if !lower.starts_with("http://") && !lower.starts_with("https://") {
                errors.push(format!(
                    "backend \"{}\" baseUrl must start with http:// or https://",
                    b.name
                ));
            }
        }
    }

    let mut seen: Vec<&str> = Vec::new();
    for m in &cfg.models {
        if seen.contains(&m.id.as_str()) {
            errors.push(format!("duplicate model id \"{}\"", m.id));
        }
        seen.push(&m.id);

        if m.upstream_model.is_empty() {
            errors.push(format!(
                "model \"{}\" needs an upstreamModel (the name sent to the backend)",
                m.id
            ));
        }
        if m.backend.is_empty() {
            errors.push(format!("model \"{}\" needs a backend", m.id));
        } else if !backend_ids.contains(&m.backend.as_str()) {
            errors.push(format!(
                "model \"{}\" points at unknown backend \"{}\"",
                m.id, m.backend
            ));
        }
        for fb in &m.fallbacks {
            if !backend_ids.contains(&fb.as_str()) {
                errors.push(format!(
                    "model \"{}\" has unknown fallback backend \"{}\"",
                    m.id, fb
                ));
            }
        }
    }

    let mut secrets: Vec<&str> = Vec::new();
    for k in &cfg.keys {
        if k.key.is_empty() {
            let label = if k.label.is_empty() { &k.id } else { &k.label };
            errors.push(format!("key \"{label}\" is empty"));
        } else if secrets.contains(&k.key.as_str()) {
            errors.push("two client keys share the same secret".into());
        }
        secrets.push(&k.key);
    }

    // Port 0 means "pick any free port", so two zeroes are not a conflict.
    if cfg.server.port != 0 && cfg.server.port == cfg.dashboard.port {
        errors.push("server.port and dashboard.port must differ".into());
    }
    errors
}

/* ------------------------------------------------------------- redact -- */

pub fn redact(value: &Value, key: &str) -> Value {
    match value {
        Value::Array(items) => Value::Array(items.iter().map(|v| redact(v, "")).collect()),
        Value::Object(map) => {
            let mut out = Map::new();
            for (k, v) in map {
                out.insert(k.clone(), redact(v, k));
            }
            Value::Object(out)
        }
        Value::String(s) if SECRET_KEYS.contains(&key) && !s.is_empty() => {
            Value::String(mask_secret(s))
        }
        other => other.clone(),
    }
}

/// The dashboard only ever sees masked secrets, so a save that echoes a mask
/// back must keep the stored value rather than overwrite it with the mask.
pub fn unmask_secrets(patch: &Value, current: &Value) -> Value {
    match patch {
        Value::Array(items) => Value::Array(
            items
                .iter()
                .map(|item| {
                    let id = item.get("id").and_then(|v| v.as_str());
                    let matched = id.and_then(|id| {
                        current.as_array().and_then(|arr| {
                            arr.iter()
                                .find(|x| x.get("id").and_then(|v| v.as_str()) == Some(id))
                        })
                    });
                    unmask_secrets(item, matched.unwrap_or(&Value::Null))
                })
                .collect(),
        ),
        Value::Object(map) => {
            let mut out = Map::new();
            for (k, v) in map {
                let cur = current.get(k).unwrap_or(&Value::Null);
                match (v, cur) {
                    (Value::String(new), Value::String(old))
                        if SECRET_KEYS.contains(&k.as_str())
                            && !old.is_empty()
                            && *new == mask_secret(old) =>
                    {
                        // Unchanged mask: keep the real secret.
                        out.insert(k.clone(), Value::String(old.clone()));
                    }
                    (Value::Object(_) | Value::Array(_), _) => {
                        out.insert(k.clone(), unmask_secrets(v, cur));
                    }
                    _ => {
                        out.insert(k.clone(), v.clone());
                    }
                }
            }
            Value::Object(out)
        }
        other => other.clone(),
    }
}

/// Strip secrets from an item the dashboard is about to receive back.
pub fn mask_item(item: &Value) -> Value {
    redact(item, "")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg_with_backend() -> Config {
        let mut cfg = Config::default();
        cfg.backends.push(Backend {
            id: "be1".into(),
            name: "primary".into(),
            base_url: "https://api.example.com/v1".into(),
            ..Default::default()
        });
        cfg
    }

    #[test]
    fn a_model_must_name_a_real_backend_and_upstream_name() {
        let mut cfg = cfg_with_backend();
        cfg.models.push(Model {
            id: "manukmiberai/creative-writer".into(),
            backend: "nope".into(),
            upstream_model: String::new(),
            ..Default::default()
        });
        let errors = validate(&cfg);
        assert!(errors.iter().any(|e| e.contains("needs an upstreamModel")));
        assert!(errors.iter().any(|e| e.contains("unknown backend")));
    }

    #[test]
    fn two_zero_ports_are_not_a_conflict() {
        let mut cfg = cfg_with_backend();
        cfg.server.port = 0;
        cfg.dashboard.port = 0;
        assert!(validate(&cfg).is_empty());
        cfg.server.port = 8787;
        cfg.dashboard.port = 8787;
        assert!(validate(&cfg).iter().any(|e| e.contains("must differ")));
    }

    #[test]
    fn force_stream_distinguishes_absent_from_null() {
        let absent: RequestTransform = serde_json::from_str("{}").unwrap();
        let explicit: RequestTransform = serde_json::from_str(r#"{"forceStream":null}"#).unwrap();
        let on: RequestTransform = serde_json::from_str(r#"{"forceStream":true}"#).unwrap();
        assert_eq!(absent.force_stream, None);
        assert_eq!(explicit.force_stream, Some(None));
        assert_eq!(on.force_stream, Some(Some(true)));

        let defaults = on.clone();
        // absent inherits the default; explicit null opts back out of it
        assert_eq!(
            RequestTransform::merged(&defaults, &absent).force_stream,
            Some(true)
        );
        assert_eq!(
            RequestTransform::merged(&defaults, &explicit).force_stream,
            None
        );
    }

    #[test]
    fn a_mask_echoed_back_does_not_destroy_the_secret() {
        let current = serde_json::json!({"apiKey": "sk-super-secret-value-here"});
        let masked = mask_secret("sk-super-secret-value-here");
        let patch = serde_json::json!({ "apiKey": masked, "name": "renamed" });
        let out = unmask_secrets(&patch, &current);
        assert_eq!(out["apiKey"], "sk-super-secret-value-here");
        assert_eq!(out["name"], "renamed");

        // ...but a genuinely new value still replaces it
        let patch = serde_json::json!({"apiKey": "sk-brand-new"});
        assert_eq!(unmask_secrets(&patch, &current)["apiKey"], "sk-brand-new");
    }

    #[test]
    fn redaction_covers_nested_lists() {
        let raw = serde_json::json!({
            "backends": [{"apiKey": "sk-abcdefghijklmnop"}],
            "dashboard": {"password": "hunter2hunter2"},
        });
        let out = redact(&raw, "");
        assert_eq!(out["backends"][0]["apiKey"], "sk-abc…mnop");
        assert_ne!(out["dashboard"]["password"], "hunter2hunter2");
    }

    #[test]
    fn aliases_resolve_to_the_same_route() {
        let mut cfg = cfg_with_backend();
        cfg.models.push(Model {
            id: "manukmiberai/creative-writer".into(),
            aliases: vec!["creative".into()],
            backend: "be1".into(),
            upstream_model: "Deepseek-v4-flash-0731".into(),
            ..Default::default()
        });
        assert_eq!(
            cfg.find_model("creative").unwrap().upstream_model,
            "Deepseek-v4-flash-0731"
        );
        assert!(cfg.find_model("Deepseek-v4-flash-0731").is_none());
    }
}
