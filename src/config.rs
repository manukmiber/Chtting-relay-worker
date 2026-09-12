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
use rand::Rng;

pub fn generate_client_key() -> String {
    // 32 karakter kombinasi: Huruf besar, huruf kecil, angka, dan tanda/simbol
    const CHARSET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789!@#$%^&*()_+-=[]{}|;:,.<>?";
    let mut rng = rand::rng();
    let random_part: String = (0..32)
        .map(|_| {
            let idx = rng.random_range(0..CHARSET.len());
            CHARSET[idx] as char
        })
        .collect();
    
    format!("Kunci-Zeiko-{}", random_part)
}
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
#[serde(rename_all = "camelCase")]
pub struct KeyConfig {
    pub id: String,
    pub label: String, // Sekarang bebas diubah namanya
    pub key: String,
    pub enabled: bool,
    pub models: Vec<String>,
    pub quota: KeyQuota,
}
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
    pub openrouter: OpenRouterConfig,
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
            openrouter: OpenRouterConfig::default(),
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
    /// How many requests may wait for a slot. Past this the relay answers 503
    /// straight away: a queue nobody will reach the front of is worse than an
    /// honest refusal, because the caller has already committed a retry budget
    /// to waiting.
    pub queue_capacity: usize,
    /// How long a queued request waits before giving up. Should stay well
    /// under the client's own timeout.
    pub queue_timeout_ms: u64,
    /// Hold Android's wake lock while the relay runs, so the phone does not
    /// suspend it the moment the screen goes off. Ignored off Termux.
    pub wake_lock: bool,
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
            queue_capacity: 2048,
            queue_timeout_ms: 30_000,
            wake_lock: true,
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
    pub openrouter: OpenRouterModel,
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
            openrouter: OpenRouterModel::default(),
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
    #[serde(
        deserialize_with = "double_option::deserialize",
        skip_serializing_if = "Option::is_none"
    )]
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
            force_stream: route.force_stream.or(defaults.force_stream).flatten(),
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
    /// Charge the caller for the system prompt the relay injects on their
    /// behalf. Off by default: they did not write it and cannot see it, so
    /// billing them for it would be indefensible. The relay still records what
    /// the backend charged, so the margin stays visible.
    pub bill_system_prompt_to_user: bool,
}

impl Default for TokenizerConfig {
    fn default() -> Self {
        Self {
            fallback: "o200k_base".into(),
            prefer_upstream_usage: true,
            rules: default_tokenizer_rules(),
            image_defaults: ImageDefaults::default(),
            bill_system_prompt_to_user: false,
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
        self.models.iter().find(|m| m.id == want).or_else(|| {
            self.models
                .iter()
                .find(|m| m.aliases.iter().any(|a| a == want))
        })
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

/* -------------------------------------------------------- openrouter -- */

/// What OpenRouter asks a provider to publish about itself.
///
/// OpenRouter routes on this document: it decides pricing, which parameters it
/// may forward, and — through the uptime, TTFT and throughput it measures — how
/// much traffic a provider gets. It is deliberately editable from the
/// dashboard rather than hard-coded, because every number here is a commercial
/// decision, not a technical one.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct OpenRouterConfig {
    /// Serve the model document at all.
    pub enabled: bool,
    /// Where the document is served from. OpenRouter polls this URL.
    pub path: String,
    /// Optional bearer token OpenRouter must present. Empty means public,
    /// which is fine — the document contains no secrets.
    pub token: String,
    /// Prefixes `openrouter.slug` for models that do not set their own.
    pub provider_slug: String,
    /// ISO country code the traffic is actually served from.
    pub deployment_region: String,
    pub datacenters: Vec<Datacenter>,
    pub compliance: Compliance,
    /// Clear this while a model list is still being set up: OpenRouter will
    /// list the models but send no traffic.
    pub is_ready: bool,
    /// Root-scope capacity, across every model.
    pub max_concurrent_requests: u32,
    pub requests_per_minute: u64,
}

impl Default for OpenRouterConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            path: "/provider/models".into(),
            token: String::new(),
            provider_slug: "chtting".into(),
            deployment_region: String::new(),
            datacenters: Vec::new(),
            compliance: Compliance::default(),
            is_ready: true,
            max_concurrent_requests: 0,
            requests_per_minute: 0,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Datacenter {
    pub country_code: String,
    pub region: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Compliance {
    /// Zero data retention: true only if nothing of the prompt is kept. Turning
    /// on request body storage in the logging section makes this a lie, so the
    /// relay refuses that combination rather than publishing a false claim.
    pub zdr: bool,
    pub hipaa: bool,
}

/// Per-model OpenRouter metadata.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct OpenRouterModel {
    /// Offer this model to OpenRouter. Off by default: a model is only listed
    /// once someone has decided what it costs.
    pub listed: bool,
    /// `openrouter.slug`. Empty derives one from the provider slug and model id.
    pub slug: String,
    /// Required by OpenRouter when the model exists on HuggingFace.
    pub hugging_face_id: String,
    /// One of int4, int8, fp4, mxfp4, nvfp4, fp6, fp8, mxfp8, fp16, bf16, fp32.
    /// Empty is published as null, which OpenRouter reads as "unspecified".
    pub quantization: String,
    /// Tokenizer family, e.g. "GPT" or "Claude". Empty falls back to the
    /// tokenizer the relay actually counts with.
    pub tokenizer_family: String,
    pub input_modalities: Vec<String>,
    pub max_prompt_tokens: u32,
    pub max_output_tokens: u32,
    pub streaming: bool,
    pub supports_tools: bool,
    pub supports_structured_outputs: bool,
    pub supports_reasoning: bool,
    pub temperature_max: f64,
    pub pricing: OpenRouterPricing,
    pub capacity: OpenRouterCapacity,
    pub is_free: bool,
    /// 0 to just under 1. OpenRouter applies it as a discount to the end user.
    pub discount_to_user: f64,
    /// `YYYY-MM-DD`, or empty for none.
    pub deprecation_date: String,
    pub created: i64,
}

impl Default for OpenRouterModel {
    fn default() -> Self {
        Self {
            listed: false,
            slug: String::new(),
            hugging_face_id: String::new(),
            quantization: String::new(),
            tokenizer_family: String::new(),
            input_modalities: vec!["text".into()],
            max_prompt_tokens: 0,
            max_output_tokens: 0,
            streaming: true,
            supports_tools: true,
            supports_structured_outputs: false,
            supports_reasoning: false,
            temperature_max: 2.0,
            pricing: OpenRouterPricing::default(),
            capacity: OpenRouterCapacity::default(),
            is_free: false,
            discount_to_user: 0.0,
            deprecation_date: String::new(),
            created: 0,
        }
    }
}

/// Prices in USD, per single token — the unit OpenRouter's `cost_usd` uses.
///
/// Kept as strings all the way through: a price like 0.0000006 loses its last
/// digit in an f64 round trip, and OpenRouter compares these as decimals.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct OpenRouterPricing {
    pub prompt_usd: String,
    pub cached_prompt_usd: String,
    pub cache_write_usd: String,
    pub completion_usd: String,
    pub internal_reasoning_usd: String,
    /// A flat per-request fee, on top of the token prices.
    pub request_usd: String,
    /// How long a prompt cache entry lives, for the cached-prompt price.
    pub cache_ttl_seconds: u32,
    /// True when caching happens without the caller asking for it.
    pub cache_implicit: bool,
}

/// Throughput this model can actually sustain. Publishing an honest number is
/// what keeps OpenRouter from routing more traffic at a phone than it can take.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct OpenRouterCapacity {
    pub prompt_tokens_per_minute: u64,
    pub completion_tokens_per_minute: u64,
    pub requests_per_minute: u64,
    pub concurrency: u32,
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
            let parsed: Value = serde_json::from_str(&raw).map_err(|e| {
                anyhow::anyhow!("config at {} is not valid JSON: {e}", file.display())
            })?;
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
            b.name = if b.id.is_empty() {
                "backend".into()
            } else {
                b.id.clone()
            };
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

    if cfg.server.max_concurrent_requests == 0 {
        errors.push("server.maxConcurrentRequests must be at least 1".into());
    }

    errors.extend(validate_openrouter(cfg));
    errors
}

/// Quantization values OpenRouter accepts. Anything else is rejected rather
/// than published, because an unknown value invalidates the whole document.
pub const QUANTIZATIONS: [&str; 11] = [
    "int4", "int8", "fp4", "mxfp4", "nvfp4", "fp6", "fp8", "mxfp8", "fp16", "bf16", "fp32",
];

pub const INPUT_MODALITIES: [&str; 5] = ["text", "image", "video", "audio", "file"];

fn validate_openrouter(cfg: &Config) -> Vec<String> {
    let or = &cfg.openrouter;
    let mut errors = Vec::new();
    if !or.enabled {
        return errors;
    }

    if !or.path.starts_with('/') {
        errors.push("openrouter.path must start with \"/\"".into());
    }
    // Publishing zero data retention while the relay is keeping prompt text
    // would be a false claim to OpenRouter's users, so it is refused outright
    // rather than quietly published.
    if or.compliance.zdr && cfg.logging.store_bodies != "none" {
        errors.push(
            "openrouter.compliance.zdr claims nothing is retained, but logging.storeBodies is \
             keeping request text — set storeBodies to \"none\" or drop the ZDR claim"
                .into(),
        );
    }
    for dc in &or.datacenters {
        if dc.country_code.len() != 2 {
            errors.push(format!(
                "openrouter datacenter country code \"{}\" must be two letters",
                dc.country_code
            ));
        }
    }

    for m in cfg.models.iter().filter(|m| m.openrouter.listed) {
        let o = &m.openrouter;
        if !o.quantization.is_empty() && !QUANTIZATIONS.contains(&o.quantization.as_str()) {
            errors.push(format!(
                "model \"{}\" has quantization \"{}\"; OpenRouter accepts only: {}",
                m.id,
                o.quantization,
                QUANTIZATIONS.join(", ")
            ));
        }
        for modality in &o.input_modalities {
            if !INPUT_MODALITIES.contains(&modality.as_str()) {
                errors.push(format!(
                    "model \"{}\" lists input modality \"{modality}\"; OpenRouter accepts only: {}",
                    m.id,
                    INPUT_MODALITIES.join(", ")
                ));
            }
        }
        if o.input_modalities.is_empty() {
            errors.push(format!(
                "model \"{}\" must declare at least one input modality",
                m.id
            ));
        }
        if !(0.0..1.0).contains(&o.discount_to_user) {
            errors.push(format!(
                "model \"{}\" discountToUser must be at least 0 and below 1",
                m.id
            ));
        }
        for (label, price) in [
            ("prompt", &o.pricing.prompt_usd),
            ("cachedPrompt", &o.pricing.cached_prompt_usd),
            ("cacheWrite", &o.pricing.cache_write_usd),
            ("completion", &o.pricing.completion_usd),
            ("internalReasoning", &o.pricing.internal_reasoning_usd),
            ("request", &o.pricing.request_usd),
        ] {
            if price.is_empty() {
                continue;
            }
            match price.parse::<f64>() {
                Ok(n) if n >= 0.0 && n.is_finite() => {}
                _ => errors.push(format!(
                    "model \"{}\" {label} price \"{price}\" is not a USD amount",
                    m.id
                )),
            }
        }
        if !o.is_free && o.pricing.prompt_usd.is_empty() && o.pricing.completion_usd.is_empty() {
            errors.push(format!(
                "model \"{}\" is offered to OpenRouter with no price and is not marked free",
                m.id
            ));
        }
        if !o.deprecation_date.is_empty()
            && chrono::NaiveDate::parse_from_str(&o.deprecation_date, "%Y-%m-%d").is_err()
        {
            errors.push(format!(
                "model \"{}\" deprecationDate \"{}\" must be YYYY-MM-DD",
                m.id, o.deprecation_date
            ));
        }
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

use std::sync::atomic::{AtomicUsize, Ordering};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BackendConfig {
    pub id: String,
    pub name: String,
    #[serde(default = "default_backend_type")]
    pub r#type: String,
    pub base_url: String,
    #[serde(default)]
    pub api_key: String,          // Key tunggal (backward compatibility)
    #[serde(default)]
    pub api_keys: Vec<String>,    // Multi-key pool
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_timeout_ms")]
    pub timeout_ms: u64,
    #[serde(default = "default_max_retries")]
    pub max_retries: u32,
    #[serde(default = "default_true")]
    pub stream_options: bool,
    #[serde(default)]
    pub headers: std::collections::HashMap<String, String>,

    #[serde(skip)]
    pub key_counter: std::sync::Arc<AtomicUsize>,
}

impl BackendConfig {
    /// Mengambil API key berikutnya secara round-robin dari pool multi-key
    pub fn get_active_api_key(&self) -> String {
        if !self.api_keys.is_empty() {
            let idx = self.key_counter.fetch_add(1, Ordering::Relaxed) % self.api_keys.len();
            self.api_keys[idx].clone()
        } else {
            self.api_key.clone()
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct TieredPricingConfig {
    pub base_input_usd_per_m: f64,      // Harga backend per 1M token input
    pub base_output_usd_per_m: f64,     // Harga backend per 1M token output
    pub reasoning_multiplier: f64,      // Pengali untuk thinking token (cth: 1.5x)
    pub proxy_margin_percent: f64,      // Margin proxy (cth: 30.0 untuk profit 30%)
    pub peak_hours: Option<Vec<u32>>,   // Jam-jam sibuk (0-23)
    pub peak_multiplier: Option<f64>,   // Pengali jam sibuk (cth: 1.25x)
    pub volume_threshold: Option<u64>,  // Ambang token (cth: 32000 token)
    pub high_volume_multiplier: Option<f64>, // Diskon/perubahan volume tinggi
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelConfig {
    pub id: String,
    pub backend: String,
    pub upstream_model: String,
    // ... field lainnya ...
    #[serde(default)]
    pub target_tps: Option<f64>,        // Batasi speed keluar tunnel (cth: 35.0 TPs)
    #[serde(default)]
    pub pricing: TieredPricingConfig,   // Tiered pricing
}

pub struct PriceResult {
    pub backend_cost: f64,
    pub proxy_price: f64,
    pub profit: f64,
}

pub fn calculate_pricing(
    pricing: &TieredPricingConfig,
    prompt_tokens: u64,
    completion_tokens: u64,
    reasoning_tokens: u64,
    hour: u32,
) -> PriceResult {
    let mut input_rate = pricing.base_input_usd_per_m;
    let mut output_rate = pricing.base_output_usd_per_m;

    // 1. Cek jam sibuk (Time of day)
    if let (Some(hours), Some(mult)) = (&pricing.peak_hours, pricing.peak_multiplier) {
        if hours.contains(&hour) {
            input_rate *= mult;
            output_rate *= mult;
        }
    }

    // 2. Cek volume input token
    if let (Some(thresh), Some(mult)) = (pricing.volume_threshold, pricing.high_volume_multiplier) {
        if prompt_tokens > thresh {
            input_rate *= mult;
        }
    }

    // 3. Hitung biaya backend
    let input_cost = (prompt_tokens as f64 / 1_000_000.0) * input_rate;
    let standard_output = completion_tokens.saturating_sub(reasoning_tokens);
    let reasoning_mult = if pricing.reasoning_multiplier > 0.0 { pricing.reasoning_multiplier } else { 1.0 };
    let output_cost = ((standard_output as f64 + (reasoning_tokens as f64 * reasoning_mult)) / 1_000_000.0) * output_rate;
    
    let backend_cost = input_cost + output_cost;

    // 4. Hitung harga jual proxy & profit
    let margin = if pricing.proxy_margin_percent > 0.0 { pricing.proxy_margin_percent / 100.0 } else { 0.20 };
    let proxy_price = backend_cost * (1.0 + margin);
    let profit = proxy_price - backend_cost;

    PriceResult {
        backend_cost,
        proxy_price,
        profit,
    }
}