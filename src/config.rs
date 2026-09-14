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
    pub pricing: Pricing,
    pub tokenizer: TokenizerConfig,
    pub logging: LoggingConfig,
    pub tunnel: TunnelConfig,
    pub openrouter: OpenRouterConfig,
    pub billing: BillingConfig,

    /// `timezone`, already parsed, worked out once when the config is
    /// published.
    ///
    /// `Tz::from_str` is a lookup over the six hundred-odd IANA names, and a
    /// single chat request asks for it three times — the day bucket, the hour
    /// bucket, and the local parts a price rule reads — plus once more for the
    /// daily quota. Parsing the same unchanging string four times per request,
    /// on a phone, is work the config already knows the answer to.
    ///
    /// Never serialised: it is a derived value, and writing it into
    /// `config.json` would invite somebody to edit it out of step with the
    /// name beside it. `None` means nobody has normalised this `Config` — only
    /// ever one built by hand in a test — and [`Config::tz`] parses in that
    /// case, exactly as it always did.
    ///
    /// Public only so a `Config` can still be built with `..Default::default()`
    /// from outside this module. Do not set it: [`normalize`] owns it, and
    /// every path that publishes a config goes through there. Read it through
    /// [`Config::tz`], which falls back to parsing the name when it is unset.
    #[serde(skip)]
    pub parsed_tz: Option<chrono_tz::Tz>,
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
            pricing: Pricing::default(),
            tokenizer: TokenizerConfig::default(),
            logging: LoggingConfig::default(),
            tunnel: TunnelConfig::default(),
            openrouter: OpenRouterConfig::default(),
            billing: BillingConfig::default(),
            parsed_tz: None,
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
    /// How often a streaming response emits a keep-alive comment of its own
    /// while the backend is quiet. 0 switches it off.
    ///
    /// The relay never forwards the backend's keep-alives: whatever shape they
    /// have is the backend's business, and passing them through would say
    /// something about it. This is ours.
    pub sse_keepalive_ms: u64,
    /// The text of that comment, after the `: ` an SSE comment starts with.
    pub sse_keepalive_text: String,
    /// Replace this process with a fresh one every N hours, handing the port
    /// over without dropping a connection. 0 switches it off.
    ///
    /// Android's low-memory killer goes after whatever has been resident
    /// longest, so a process that never ages never reaches the top of its list.
    pub rotate_hours: u32,
    /// How long a retiring instance waits for its in-flight requests before it
    /// exits anyway.
    pub rotate_drain_timeout_ms: u64,
    /// The rotation interval in minutes, when it needs to be finer than hours.
    /// Set, it wins over `rotateHours`; it exists so the handover can be
    /// exercised for real rather than trusted for an hour at a time.
    pub rotate_minutes: u32,
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
            sse_keepalive_ms: 15_000,
            sse_keepalive_text: "Zeiko is still here, Just be patience".into(),
            rotate_hours: 1,
            rotate_drain_timeout_ms: 10 * 60 * 1000,
            rotate_minutes: 0,
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
    /// Believe `CF-Connecting-IP` and `X-Forwarded-For` about who the caller
    /// is. Only ever honoured from a loopback peer — see
    /// [`crate::server::client_ip`] — because a header anyone can set is not
    /// an address, and `blockedIps` is enforced against whatever this returns.
    pub trust_proxy_headers: bool,
    pub blocked_ips: Vec<String>,
    /// What a private key's user id is, as the backend sees it.
    ///
    /// * `fingerprint` — a SHA-256 of the key, truncated. Stable, unique per
    ///   key, and reveals nothing: the default, and the only one of the three
    ///   that does not hand a credential or an internal id to a third party.
    /// * `keyId` — the key's own `key_...` id.
    /// * `secret` — the key itself. Only for a backend that genuinely needs
    ///   it; it writes your client key into somebody else's request log.
    pub private_user_id: String,
    /// Reject a dashboard request that arrives with a cross-origin `Origin`,
    /// or for a host that is not this machine.
    ///
    /// The dashboard is on loopback, but on Android loopback is not private:
    /// every app on the phone can reach `127.0.0.1:8788`, and so can a page
    /// the browser is pointed at. Leave this on.
    pub dashboard_origin_guard: bool,
}

impl Default for SecurityConfig {
    fn default() -> Self {
        Self {
            require_client_key: true,
            cors_origins: vec!["*".into()],
            trust_proxy_headers: true,
            blocked_ips: Vec::new(),
            private_user_id: PRIVATE_ID_FINGERPRINT.into(),
            dashboard_origin_guard: true,
        }
    }
}

/// The three spellings [`SecurityConfig::private_user_id`] accepts.
pub const PRIVATE_ID_FINGERPRINT: &str = "fingerprint";
pub const PRIVATE_ID_KEY_ID: &str = "keyId";
pub const PRIVATE_ID_SECRET: &str = "secret";
pub const PRIVATE_ID_MODES: [&str; 3] =
    [PRIVATE_ID_FINGERPRINT, PRIVATE_ID_KEY_ID, PRIVATE_ID_SECRET];

/// How usage is turned into invoices, and what those invoices say.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct BillingConfig {
    /// Issue invoices at all. Off leaves the ledger accumulating, which is
    /// exactly what it did before there was any of this.
    pub enabled: bool,
    /// Printed on the invoice. The relay prices in USD throughout; this only
    /// changes the symbol, never the arithmetic.
    pub currency: String,
    /// `INV` gives `INV-2026-0001`.
    pub number_prefix: String,
    /// Added to the subtotal unless the key overrides it.
    pub tax_percent: f64,
    /// An invoice under this much is not worth issuing; the period rolls on
    /// instead. 0 issues whatever is there, including nothing.
    pub minimum_usd: f64,
    /// Day of the month the automatic cycle runs, 1-28. Days past 28 are not
    /// offered because February would silently skip them.
    pub cycle_day: u32,
    /// Run that cycle. Only keys with `billing.autoInvoice` are billed by it.
    pub auto_issue: bool,
    /// Who the invoice is from.
    pub issuer: Issuer,
}

impl Default for BillingConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            currency: "USD".into(),
            number_prefix: "INV".into(),
            tax_percent: 0.0,
            minimum_usd: 0.0,
            cycle_day: 1,
            auto_issue: false,
            issuer: Issuer::default(),
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Issuer {
    pub name: String,
    pub email: String,
    pub address: String,
    pub tax_id: String,
    /// Free text under the totals: bank details, payment terms, a thank you.
    pub payment_terms: String,
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
    /// A pool of keys to spread requests over, round-robin. `apiKey` stays the
    /// single-key form and is used when this is empty.
    pub api_keys: Vec<String>,
    /// Pass the caller's user id upstream, so a backend that isolates prompt
    /// cache per user keeps one caller's cache out of another's.
    pub forward_user_id: bool,
    /// Header the user id travels in. Empty sends no header; the `user` field
    /// in the body goes either way.
    pub user_id_header: String,
    /// A second body field the caller's id is copied into, beside OpenAI's
    /// `user`. Backends disagree on the spelling and several read only this
    /// one, so it is named rather than assumed. Clear it for a backend that
    /// rejects body fields it does not recognise.
    pub user_id_field: String,
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
            api_keys: Vec::new(),
            forward_user_id: true,
            user_id_header: "x-user-id".into(),
            user_id_field: "user_id".into(),
            note: String::new(),
        }
    }
}

/// The name every model is published under unless one of them says otherwise.
pub const DEFAULT_MODEL_OWNER: &str = "ZeikoAI";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Model {
    pub id: String,
    pub aliases: Vec<String>,
    pub enabled: bool,
    pub display_name: String,
    pub description: String,
    /// Who the model is published as, in `owned_by` on the OpenAI surface.
    /// Left blank it reads as [`DEFAULT_MODEL_OWNER`]: a model served here is
    /// the house's own, whatever hardware answers it.
    pub owner: String,
    pub backend: String,
    /// The name actually sent upstream. Never exposed to callers.
    pub upstream_model: String,
    pub fallbacks: Vec<String>,
    pub system_prompt: SystemPromptSpec,
    /// Extra prompts chosen by how hard the caller asked the model to think.
    /// The first rule that matches wins; `systemPrompt` is the fallback.
    pub system_prompts: Vec<SystemPromptRule>,
    pub params: Map<String, Value>,
    pub force_params: Map<String, Value>,
    pub limits: Limits,
    pub tokenizer: String,
    pub chat_profile: String,
    pub request_transform: RequestTransform,
    pub response_transform: ResponseTransform,
    pub context_length: u32,
    pub created_at: i64,
    /// Hold the stream out to the caller at this many tokens a second, however
    /// fast the backend produced them. 0 means full speed.
    pub max_tokens_per_second: f64,
    /// This model's own price list, layered over the global one.
    pub pricing: Pricing,
    pub openrouter: OpenRouterModel,

    /// Which vocabulary and chat profile this route counts with, decided once
    /// when the config is published.
    ///
    /// The answer is a pure function of the config — `upstreamModel` against
    /// `tokenizer.rules`, with this model's own `tokenizer`/`chatProfile`
    /// overriding — and yet it was being worked out again on every request:
    /// a walk of the rule list, and `glob_match` lower-casing and collecting
    /// both the model name *and* the pattern into a `Vec<char>` at each rule.
    /// Fourteen rules in the default config, so tens of allocations per
    /// request to re-answer a question whose inputs had not moved.
    ///
    /// `Arc<str>`, so handing it to the counter — which has to own it to read
    /// it on the blocking pool — is a refcount bump rather than two `String`
    /// allocations of its own.
    ///
    /// Set by [`normalize`] and never serialised. `None` means nobody
    /// normalised this `Model`, which is only ever one built by hand in a
    /// test; the counter then works it out live, exactly as it always did.
    #[serde(skip)]
    pub resolved_tokenizer: Option<Arc<str>>,
    #[serde(skip)]
    pub resolved_profile: Option<Arc<str>>,
}

impl Default for Model {
    fn default() -> Self {
        Self {
            id: String::new(),
            aliases: Vec::new(),
            enabled: true,
            display_name: String::new(),
            description: String::new(),
            owner: DEFAULT_MODEL_OWNER.into(),
            backend: String::new(),
            upstream_model: String::new(),
            fallbacks: Vec::new(),
            system_prompt: SystemPromptSpec::default(),
            system_prompts: Vec::new(),
            params: Map::new(),
            force_params: Map::new(),
            limits: Limits::default(),
            tokenizer: String::new(),
            chat_profile: String::new(),
            request_transform: RequestTransform::default(),
            response_transform: ResponseTransform::default(),
            context_length: 0,
            created_at: 0,
            max_tokens_per_second: 0.0,
            pricing: Pricing::default(),
            openrouter: OpenRouterModel::default(),
            resolved_tokenizer: None,
            resolved_profile: None,
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

/// One model, several system prompts, picked by how hard the caller asked the
/// model to think.
///
/// A non-reasoning call and a maximum-effort call want different instructions:
/// the first needs the answer shaped for it directly, the second needs room to
/// work. Rules are tried in order and the first match wins, so the narrow ones
/// belong first.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct SystemPromptRule {
    pub id: String,
    pub name: String,
    pub enabled: bool,
    /// Efforts this rule answers for: `none`, `minimal`, `low`, `medium`,
    /// `high`, `max`, or `default` for a caller who named none. Empty matches
    /// every effort, which makes the rule an unconditional override.
    pub efforts: Vec<String>,
    /// Inclusive ranked bounds, as an alternative to listing every effort.
    /// A caller who named no effort matches neither.
    pub min_effort: String,
    pub max_effort: String,
    /// What to inject when this rule wins. Shaped exactly like the model's own
    /// `systemPrompt`, and nested rather than flattened into the rule so that a
    /// rule reads as a condition and a prompt rather than a bag of both.
    pub prompt: SystemPromptSpec,
}

/// The id the dashboard's "No thinking" box owns.
///
/// Reserved: a rule carrying it *is* that box, which is what lets the box be
/// edited as a plain prompt while the relay still reads it as one entry in an
/// ordered list. Because the relay owns the meaning of the id, it also owns the
/// efforts the rule answers for — see [`NON_THINKING_EFFORTS`].
pub const NON_THINKING_RULE: &str = "sp-non-thinking";

/// What the "No thinking" box covers: thinking off, thinking barely on, and the
/// lowest level that counts as thinking at all. Medium, high and max get the
/// model's Default prompt instead.
///
/// Canonical, and rewritten onto the reserved rule on every load and save. A
/// model saved when this list was spelled differently would otherwise keep
/// answering to the old spelling until somebody opened it in the dashboard and
/// pressed Save, which is a migration nobody would know they owed.
pub const NON_THINKING_EFFORTS: [&str; 3] = ["none", "minimal", "low"];

impl Default for SystemPromptRule {
    fn default() -> Self {
        Self {
            id: String::new(),
            name: String::new(),
            enabled: true,
            efforts: Vec::new(),
            min_effort: String::new(),
            max_effort: String::new(),
            prompt: SystemPromptSpec::default(),
        }
    }
}

/* ------------------------------------------------------------- pricing -- */

/// What the models say when they will not answer. One sentence, fixed wording,
/// so a refusal can be recognised from the completion alone.
pub const DEFAULT_REFUSAL_PHRASES: [&str; 1] = ["I cannot do that. I only provide AI roleplay."];

/// One band of a rate card: what a million tokens costs while the model is
/// thinking that hard.
///
/// A field left at 0 is not free — it means "same as the standard band", which
/// is what lets a model that only charges more for output say so in one number
/// instead of restating its input and cache rates.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", default)]
pub struct BandRates {
    pub input_usd_per_m: f64,
    pub cached_input_usd_per_m: f64,
    pub output_usd_per_m: f64,
    /// Unset bills reasoning tokens at this band's output rate.
    pub reasoning_usd_per_m: f64,
}

impl BandRates {
    /// True when this band says nothing at all, and the standard rates stand.
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}

/// What a request costs and what it sells for.
///
/// Rates are USD per million tokens, the unit every provider publishes. The
/// `backend*` rates are what the relay is charged; the rest are what the relay
/// charges, and when one of those is left at 0 it is derived from the backend
/// rate plus `marginPercent` instead of being free.
///
/// The sell side is a rate card of three bands rather than one row of numbers,
/// because that is how these models are actually sold: a standard rate, a
/// higher one when the caller asks for maximum thinking, a lower one when
/// thinking is off. The band is chosen by the effort the caller asked for and
/// nothing else, so it is decided before any tier is read.
///
/// `tiers` is what is left for the conditions a rate card cannot express — the
/// hour, the size of the prompt, a weekend deal. It is a list with no ceiling
/// on its length, and every tier that matches a request applies on top of the
/// band, rather than the relay having to pick one reason to charge more.

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Pricing {
    /// Off by default: a relay with no prices set should report no money rather
    /// than a column of zeroes that looks like free service.
    pub enabled: bool,
    /// Display only. Every figure here is USD; this labels it.
    pub currency: String,
    pub backend_input_usd_per_m: f64,
    pub backend_cached_input_usd_per_m: f64,
    pub backend_output_usd_per_m: f64,
    /// Unset bills reasoning tokens at the output rate.
    pub backend_reasoning_usd_per_m: f64,
    /// The standard band: what a caller pays when they asked the model to think
    /// at low, medium or high effort, and the fallback for every band below.
    pub input_usd_per_m: f64,
    pub cached_input_usd_per_m: f64,
    pub output_usd_per_m: f64,
    pub reasoning_usd_per_m: f64,
    /// The band for `reasoning_effort: "max"` and the budgets that large.
    pub max_thinking: BandRates,
    /// The band for thinking turned off and thinking set to minimal — and for a
    /// caller who said nothing about thinking at all, if and only if
    /// [`Defaults::effort`] leaves silence unresolved. It ships resolving to
    /// `high`, which puts those callers on the standard band instead.
    pub non_thinking: BandRates,
    /// Markup over the backend rate, in percent, for every sell-side rate left
    /// at 0.
    pub margin_percent: f64,
    /// A flat fee per request, on top of the token charges.
    pub request_usd: f64,
    /// What a refused answer costs instead of its tokens. The model still had
    /// to read the prompt to decide it would not answer, so the request is not
    /// free; it is also not worth the full price of an answer. 0 bills a
    /// refusal like any other reply.
    pub refusal_usd: f64,
    /// The wording that marks a reply as a refusal, matched case-insensitively
    /// anywhere in the completion. Empty, with a refusal price set, falls back
    /// to [`DEFAULT_REFUSAL_PHRASES`].
    pub refusal_phrases: Vec<String>,
    pub tiers: Vec<PricingTier>,
}

/// One conditional price change. Unlimited in number, and they stack.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct PricingTier {
    pub id: String,
    pub name: String,
    pub enabled: bool,
    pub when: TierWhen,
    /// Multipliers on the sell-side rates. 0 is read as "leave it alone".
    pub input_multiplier: f64,
    pub output_multiplier: f64,
    /// Stacks on top of `outputMultiplier`, so "thinking costs half again as
    /// much" is one number.
    pub reasoning_multiplier: f64,
    /// Absolute rates, in USD per million tokens. Set, they replace the rate
    /// outright rather than scaling it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input_usd_per_m: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cached_input_usd_per_m: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_usd_per_m: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_usd_per_m: Option<f64>,
    /// A flat amount added to this request when the tier matches.
    pub surcharge_usd: f64,
    /// Apply this tier and stop, leaving the rest of the list unread.
    pub stop: bool,
    pub note: String,
}

impl Default for PricingTier {
    fn default() -> Self {
        Self {
            id: String::new(),
            name: String::new(),
            enabled: true,
            when: TierWhen::default(),
            input_multiplier: 1.0,
            output_multiplier: 1.0,
            reasoning_multiplier: 1.0,
            input_usd_per_m: None,
            cached_input_usd_per_m: None,
            output_usd_per_m: None,
            reasoning_usd_per_m: None,
            surcharge_usd: 0.0,
            stop: false,
            note: String::new(),
        }
    }
}

/// When a tier applies. Every field that is set must hold; a field left unset
/// is not a condition at all.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct TierWhen {
    /// Public model ids, globbed. Empty means every model.
    pub models: Vec<String>,
    pub efforts: Vec<String>,
    pub min_effort: String,
    pub max_effort: String,
    /// Local-clock hour windows, inclusive, wrapping past midnight.
    pub hours: Vec<HourRange>,
    /// Monday is 0, Sunday is 6.
    pub weekdays: Vec<u32>,
    pub min_input_tokens: u64,
    /// 0 means no ceiling.
    pub max_input_tokens: u64,
    pub min_output_tokens: u64,
    pub max_output_tokens: u64,
    pub min_total_tokens: u64,
    pub max_total_tokens: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub streamed: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_hit: Option<bool>,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct HourRange {
    pub from: u32,
    pub to: u32,
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

/// Who is behind a client key, which decides three things: whose id goes
/// upstream, whose name the usage is filed under, and whether the route's
/// tokens-a-second throttle applies.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum KeyKind {
    /// A company reselling this relay to its own users. Every call carries the
    /// end user's id, and that id is what travels upstream and what the usage
    /// is broken down by — one key, many people behind it.
    #[default]
    Company,
    /// One holder, who *is* the user. Nothing they send can say otherwise: the
    /// key's own identity is the user id, and their replies are not paced.
    Private,
}

impl KeyKind {
    pub fn as_str(self) -> &'static str {
        match self {
            KeyKind::Company => "company",
            KeyKind::Private => "private",
        }
    }

    pub fn parse(name: &str) -> Option<Self> {
        match name.trim().to_lowercase().as_str() {
            "company" | "" => Some(KeyKind::Company),
            "private" | "personal" => Some(KeyKind::Private),
            _ => None,
        }
    }

    pub fn is_private(self) -> bool {
        matches!(self, KeyKind::Private)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ClientKey {
    pub id: String,
    pub label: String,
    pub key: String,
    pub enabled: bool,
    /// Company or private. See [`KeyKind`].
    pub kind: KeyKind,
    /// `["*"]` or an explicit list of public model ids.
    pub models: Vec<String>,
    pub quota: Quota,
    pub note: String,
    pub created_at: i64,
    /// Who this key is billed to, on the invoice. The label is used when it is
    /// blank, which is the common case for a key nobody has invoiced yet.
    pub billing: KeyBilling,
}

impl Default for ClientKey {
    fn default() -> Self {
        Self {
            id: String::new(),
            label: String::new(),
            key: String::new(),
            enabled: true,
            kind: KeyKind::default(),
            models: vec!["*".into()],
            quota: Quota::default(),
            note: String::new(),
            created_at: 0,
            billing: KeyBilling::default(),
        }
    }
}

impl ClientKey {
    /// The name to put on an invoice and in the usage tables.
    pub fn display_name(&self) -> &str {
        for candidate in [&self.billing.name, &self.label, &self.id] {
            if !candidate.is_empty() {
                return candidate;
            }
        }
        ""
    }
}

/// The parts of a key an invoice needs that are about the customer rather than
/// about the traffic.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct KeyBilling {
    /// Who the invoice is addressed to. Blank falls back to the key's label.
    pub name: String,
    pub email: String,
    pub address: String,
    /// Tax number, VAT id, NPWP — whatever the jurisdiction calls it.
    pub tax_id: String,
    /// Overrides `billing.taxPercent` for this customer. Absent inherits the
    /// global rate; an explicit 0 is a real answer, and means tax-exempt.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tax_percent: Option<f64>,
    /// Bill this key automatically on the global cycle. Off by default: an
    /// invoice resets the period, and that should be somebody's decision until
    /// they say otherwise.
    pub auto_invoice: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Quota {
    pub requests_per_day: u64,
    pub tokens_per_day: u64,
    pub requests_per_minute: u32,
}

/// What a caller who never mentioned thinking is taken to have asked for.
///
/// High rather than nothing, because "nothing" is not a level this relay
/// publishes: the four a caller can pick from are off, low, high and max, and
/// the two prompts a model carries divide them in half. Silence has to land on
/// one side of that line, and the useful side is the thinking one — a client
/// that simply never learned to send `reasoning_effort` should get the model at
/// its best, not the answer written for callers who asked it not to think.
pub const DEFAULT_EFFORT: &str = "high";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Defaults {
    pub system_prompt: SystemPromptSpec,
    pub params: Map<String, Value>,
    pub request_transform: RequestTransform,
    pub response_transform: ResponseTransform,
    /// The effort a request that named none is treated as having asked for:
    /// `none`, `minimal`, `low`, `medium`, `high` or `max`. It decides which of
    /// the model's prompts is injected, which price band the request is on, and
    /// what the request row records — one answer, used everywhere, so a caller
    /// is never told one thing and billed as another.
    ///
    /// `default` puts silence back outside the scale, which is what the relay
    /// did before this setting existed.
    pub effort: String,
}

impl Default for Defaults {
    fn default() -> Self {
        Self {
            system_prompt: SystemPromptSpec::default(),
            params: Map::new(),
            request_transform: RequestTransform::default(),
            response_transform: ResponseTransform::default(),
            effort: DEFAULT_EFFORT.into(),
        }
    }
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
    /// Below this many tokens of the caller's own prompt, a backend cache hit
    /// buys them nothing: their whole prompt bills as fresh input however much
    /// of it the backend calls cached.
    ///
    /// The hit a backend reports covers the injected prefix as well as the
    /// caller's body, and the two are told apart by subtraction, which carries
    /// the drift between the backend's tokenizer and ours. On a short prompt
    /// that drift is most of the answer, so the split is least trustworthy
    /// exactly where the discount is worth least. Rounding those to fresh input
    /// costs the caller a rounding error and takes the guesswork out of the
    /// operator's margin.
    ///
    /// 2048 by default, which is at or above the minimum cacheable prefix the
    /// major providers document. 0 turns the floor off and credits every hit
    /// the offset leaves.
    pub cache_credit_min_tokens: u64,
}

impl Default for TokenizerConfig {
    fn default() -> Self {
        Self {
            fallback: "o200k_base".into(),
            prefer_upstream_usage: true,
            rules: default_tokenizer_rules(),
            image_defaults: ImageDefaults::default(),
            bill_system_prompt_to_user: false,
            cache_credit_min_tokens: 2048,
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
    /// Log every phase of every request — the uid as it arrives, how long
    /// tokenizing and injection took, time to first token, and one summary
    /// line at the end. Goes to the terminal and the log file alike, so the
    /// dashboard's Logs tab shows the same thing Termux does.
    pub verbose_requests: bool,
}

impl Default for LoggingConfig {
    fn default() -> Self {
        Self {
            level: "info".into(),
            retention_days: 30,
            store_bodies: "preview".into(),
            preview_chars: 800,
            file_enabled: true,
            verbose_requests: true,
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
            // On by default. A relay whose tunnel has to be started by hand is
            // a relay that is down every time the phone reboots.
            auto_start: true,
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
        // The request path does not come through here — it uses the O(1)
        // [`KeyIndex`] the store publishes — but a scan is the right answer
        // for the handful of callers that hold a `Config` and nothing else.
        self.keys
            .iter()
            .find(|k| crate::util::safe_equal(&k.key, secret))
    }

    pub fn tz(&self) -> chrono_tz::Tz {
        match self.parsed_tz {
            Some(tz) => tz,
            None => crate::util::parse_tz(&self.timezone),
        }
    }
}

/* ----------------------------------------------------------- key index -- */

/// Authenticated key lookup in one hash rather than a scan.
///
/// The scan it replaces was O(number of keys) *constant-time compares* per
/// request, which at a few hundred keys is the most expensive thing that
/// happens before any work is done. Here the presented secret is hashed once
/// and looked up; the compare that follows is still constant-time, so the
/// timing story is unchanged — a lookup that misses does the same work as one
/// that hits, because a wrong secret hashes to a digest that is simply not in
/// the map.
///
/// Rebuilt whenever the config is published, and published with it, so the
/// request path never rebuilds anything.
pub struct KeyIndex {
    by_digest: std::collections::HashMap<[u8; 32], Arc<ClientKey>>,
}

impl KeyIndex {
    pub fn build(keys: &[ClientKey]) -> Self {
        let mut by_digest = std::collections::HashMap::with_capacity(keys.len());
        for key in keys {
            if key.key.is_empty() {
                continue;
            }
            by_digest.insert(
                crate::util::digest(key.key.as_bytes()),
                Arc::new(key.clone()),
            );
        }
        Self { by_digest }
    }

    pub fn get(&self, secret: &str) -> Option<&Arc<ClientKey>> {
        if secret.is_empty() {
            return None;
        }
        let found = self
            .by_digest
            .get(&crate::util::digest(secret.as_bytes()))?;
        // A digest collision would be news, but the compare costs nanoseconds
        // and means authentication never rests on the hash alone.
        crate::util::safe_equal(&found.key, secret).then_some(found)
    }

    pub fn len(&self) -> usize {
        self.by_digest.len()
    }

    pub fn is_empty(&self) -> bool {
        self.by_digest.is_empty()
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
    /// `canonicalSlug` on the public listing: the dated, never-reused name of
    /// this exact snapshot, next to the moving `id`. Empty publishes the id.
    pub canonical_slug: String,
    /// What the model answers in. Chat models say text; the field exists
    /// because the listing publishes input and output modalities apart.
    pub output_modalities: Vec<String>,
    /// A base model's prompt format — `chatml`, `alpaca`, and so on. Empty is
    /// published as null, which is what an instruct-tuned model reports.
    pub instruct_type: String,
    /// Whether a moderation pass sits in front of this model.
    pub is_moderated: bool,
    /// `YYYY-MM-DD`, or empty for none. Published as `knowledge_cutoff`.
    pub knowledge_cutoff: String,
    /// The parameter names the listing advertises. Empty derives the list from
    /// what this model is actually configured to accept, which is the answer
    /// that stays true when a transform starts dropping one.
    pub supported_parameters: Vec<String>,
    /// Parameter values a caller gets without asking. Published verbatim.
    pub default_parameters: Map<String, Value>,
    pub reasoning: OpenRouterReasoning,
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
            canonical_slug: String::new(),
            output_modalities: vec!["text".into()],
            instruct_type: String::new(),
            is_moderated: false,
            knowledge_cutoff: String::new(),
            supported_parameters: Vec::new(),
            default_parameters: Map::new(),
            reasoning: OpenRouterReasoning::default(),
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
    /// What the price becomes at certain hours of certain days — a peak-hour
    /// surcharge, a weekend rate. Read in order; the first window that covers
    /// the moment wins, so the narrow ones belong first.
    pub overrides: Vec<PricingOverride>,
}

/// One time-boxed replacement for the prices above.
///
/// The window is UTC, because that is the clock the published listing is read
/// on and the only one a caller on the other side of the world can check the
/// bill against. Times are `HHMM` integers, the spelling OpenRouter's own
/// listing uses: `0` is midnight, `100` is 01:00, `1730` is 17:30. The start is
/// inclusive and the end exclusive; an end below the start wraps past midnight,
/// and a window of `0` to `0` is the whole day.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", default)]
pub struct PricingOverride {
    /// Lower-case weekday names — `monday` through `sunday`. Empty is every day.
    pub utc_days: Vec<String>,
    pub utc_start: u32,
    pub utc_end: u32,
    /// Prices for this window, in USD per single token, as strings. A price
    /// left empty keeps the one above it rather than becoming free.
    pub prompt_usd: String,
    pub completion_usd: String,
    pub cached_prompt_usd: String,
    pub cache_write_usd: String,
    pub internal_reasoning_usd: String,
    pub request_usd: String,
}

impl PricingOverride {
    /// Does this window cover that UTC weekday and `HHMM` time?
    ///
    /// `weekday` is Monday = 0, the same scale the request row records.
    pub fn covers(&self, weekday: u32, hhmm: u32) -> bool {
        if !self.utc_days.is_empty() {
            let name = WEEKDAY_NAMES.get(weekday as usize).copied().unwrap_or("");
            if !self
                .utc_days
                .iter()
                .any(|d| d.trim().eq_ignore_ascii_case(name))
            {
                return false;
            }
        }
        match (self.utc_start, self.utc_end) {
            // No window at all, or one that starts where it ends: all day.
            (a, b) if a == b => true,
            (from, to) if from < to => hhmm >= from && hhmm < to,
            // Wraps midnight: 22:00 to 02:00 is one window, not two.
            (from, to) => hhmm >= from || hhmm < to,
        }
    }
}

pub const WEEKDAY_NAMES: [&str; 7] = [
    "monday",
    "tuesday",
    "wednesday",
    "thursday",
    "friday",
    "saturday",
    "sunday",
];

/// How this model handles reasoning, as the public listing describes it.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct OpenRouterReasoning {
    /// The model always reasons and cannot be asked not to.
    pub mandatory: bool,
    /// Reasoning happens for a caller who never mentioned it.
    pub default_enabled: bool,
    /// The effort levels this model answers to, in the order they are
    /// published. Empty falls back to the levels the relay prices.
    pub supported_efforts: Vec<String>,
    /// What a caller who asked for reasoning without naming a level gets.
    pub default_effort: String,
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
    /// Published alongside the config it was built from, so the request path
    /// authenticates in one hash instead of scanning the key list.
    keys: ArcSwap<KeyIndex>,
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
            keys: ArcSwap::from_pointee(KeyIndex::build(&cfg.keys)),
            current: ArcSwap::from_pointee(cfg),
            write_lock: tokio::sync::Mutex::new(()),
        })
    }

    /// Lock-free read of the live config.
    pub fn current(&self) -> Arc<Config> {
        self.current.load_full()
    }

    /// The key index for the config as it stands. Lock-free, like the config.
    pub fn keys(&self) -> Arc<KeyIndex> {
        self.keys.load_full()
    }

    /// Publish a new config and the index that goes with it.
    ///
    /// The index goes first: a request that lands between the two stores then
    /// sees a key that is about to exist rather than one that has just stopped
    /// existing, and every other check it goes on to make reads the new config
    /// anyway.
    fn publish(&self, next: Arc<Config>) {
        self.keys.store(Arc::new(KeyIndex::build(&next.keys)));
        self.current.store(next);
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
        self.publish(next.clone());
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
        self.publish(next.clone());
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
        for rule in &mut m.system_prompts {
            if rule.id.is_empty() {
                rule.id = new_id("spr");
            }
            if rule.id == NON_THINKING_RULE {
                // The dashboard's own box: the relay decides which efforts it
                // answers for, so changing that list here is enough to change
                // every model that carries one.
                rule.efforts = NON_THINKING_EFFORTS.iter().map(|e| (*e).into()).collect();
                rule.min_effort.clear();
                rule.max_effort.clear();
            }
            if rule.name.is_empty() {
                rule.name = describe_efforts(&rule.efforts, &rule.min_effort, &rule.max_effort);
            }
            if rule.prompt.mode.is_empty() {
                rule.prompt.mode = "prepend".into();
            }
        }
        if m.max_tokens_per_second < 0.0 {
            m.max_tokens_per_second = 0.0;
        }
        if m.owner.trim().is_empty() {
            m.owner = DEFAULT_MODEL_OWNER.into();
        }
        normalize_pricing(&mut m.pricing);
    }
    cfg.models.retain(|m| !m.id.is_empty());
    normalize_pricing(&mut cfg.pricing);

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
        // A tax rate that is not a number is a typo, and a negative one would
        // pay the customer to buy tokens. Dropping it inherits the global rate,
        // which is the answer that was true before somebody mistyped.
        k.billing.tax_percent = k.billing.tax_percent.filter(|t| t.is_finite() && *t >= 0.0);
    }

    // Anything unrecognised becomes the fingerprint, which is the mode that
    // hands nothing away. Spelled case-insensitively because `keyid` and
    // `keyId` are the same intention.
    let mode = cfg.security.private_user_id.trim();
    cfg.security.private_user_id = PRIVATE_ID_MODES
        .iter()
        .find(|known| known.eq_ignore_ascii_case(mode))
        .map_or(PRIVATE_ID_FINGERPRINT, |known| known)
        .to_string();

    normalize_billing(&mut cfg.billing);

    for p in &mut cfg.system_prompts {
        if p.id.is_empty() {
            p.id = new_id("sp");
        }
        if p.updated_at == 0 {
            p.updated_at = now;
        }
    }

    // A misspelled effort would silently mean "no default at all", which is the
    // one answer nobody sets this field to ask for.
    let effort = cfg.defaults.effort.trim();
    cfg.defaults.effort = if effort.is_empty() {
        DEFAULT_EFFORT.into()
    } else {
        crate::pricing::Effort::parse(effort)
            .map_or_else(|| DEFAULT_EFFORT.to_string(), |e| e.as_str().to_string())
    };

    if cfg.timezone.trim().is_empty() {
        cfg.timezone = "Asia/Jakarta".into();
    }
    // Last, so it is parsed from the name as it finally stands. Every path that
    // publishes a config comes through here, so the request path never has to
    // look an IANA name up again.
    cfg.parsed_tz = Some(crate::util::parse_tz(&cfg.timezone));
    resolve_tokenizers(&mut cfg);
    cfg
}

/// Decide each route's vocabulary and chat profile once, here, rather than on
/// every request that uses it.
///
/// Worked out in two passes because the answer depends on `cfg.tokenizer`
/// while the place it is written is `cfg.models`, and the borrow checker is
/// right to object to holding both at once.
fn resolve_tokenizers(cfg: &mut Config) {
    let decided: Vec<(Arc<str>, Arc<str>)> = cfg
        .models
        .iter()
        .map(|m| {
            let (rule_tokenizer, rule_profile) = crate::tokenizer::registry::Registry::match_rules(
                &cfg.tokenizer,
                &m.upstream_model,
            );
            // A route that names a vocabulary outright means it; feeding that
            // name back through the rules would send it to the catch-all.
            let tokenizer = if m.tokenizer.is_empty() {
                rule_tokenizer
            } else {
                m.tokenizer.clone()
            };
            let profile = if m.chat_profile.is_empty() {
                rule_profile
            } else {
                m.chat_profile.clone()
            };
            (Arc::from(tokenizer.as_str()), Arc::from(profile.as_str()))
        })
        .collect();

    for (model, (tokenizer, profile)) in cfg.models.iter_mut().zip(decided) {
        model.resolved_tokenizer = Some(tokenizer);
        model.resolved_profile = Some(profile);
    }
}

fn normalize_billing(billing: &mut BillingConfig) {
    if billing.currency.trim().is_empty() {
        billing.currency = "USD".into();
    }
    if billing.number_prefix.trim().is_empty() {
        billing.number_prefix = "INV".into();
    }
    billing.currency = billing.currency.trim().to_string();
    billing.number_prefix = billing.number_prefix.trim().to_string();
    for amount in [&mut billing.tax_percent, &mut billing.minimum_usd] {
        if !amount.is_finite() || *amount < 0.0 {
            *amount = 0.0;
        }
    }
    // The 29th, 30th and 31st are not offered: February would skip them and a
    // customer would be billed eleven times a year without anyone noticing.
    billing.cycle_day = billing.cycle_day.clamp(1, 28);
}

/// A readable name for a prompt rule that was saved without one.
fn describe_efforts(efforts: &[String], min: &str, max: &str) -> String {
    if !efforts.is_empty() {
        return format!("effort {}", efforts.join(", "));
    }
    match (min.is_empty(), max.is_empty()) {
        (false, false) => format!("effort {min} to {max}"),
        (false, true) => format!("effort {min} and up"),
        (true, false) => format!("effort up to {max}"),
        (true, true) => "every effort".into(),
    }
}

fn normalize_pricing(pricing: &mut Pricing) {
    if pricing.currency.is_empty() {
        pricing.currency = "USD".into();
    }
    if !pricing.refusal_usd.is_finite() || pricing.refusal_usd < 0.0 {
        pricing.refusal_usd = 0.0;
    }
    // A rate that is not a number is a typo, and a negative one would pay the
    // caller to send tokens. Either way the standard band is the safer answer.
    for rate in [
        &mut pricing.input_usd_per_m,
        &mut pricing.cached_input_usd_per_m,
        &mut pricing.output_usd_per_m,
        &mut pricing.reasoning_usd_per_m,
    ] {
        if !rate.is_finite() || *rate < 0.0 {
            *rate = 0.0;
        }
    }
    for band in [&mut pricing.max_thinking, &mut pricing.non_thinking] {
        for rate in [
            &mut band.input_usd_per_m,
            &mut band.cached_input_usd_per_m,
            &mut band.output_usd_per_m,
            &mut band.reasoning_usd_per_m,
        ] {
            if !rate.is_finite() || *rate < 0.0 {
                *rate = 0.0;
            }
        }
    }
    for phrase in &mut pricing.refusal_phrases {
        *phrase = phrase.trim().to_string();
    }
    pricing.refusal_phrases.retain(|p| !p.is_empty());
    // A refusal price with nothing to recognise a refusal by would never be
    // charged, which looks like a bug in the price list rather than a choice.
    if pricing.refusal_usd > 0.0 && pricing.refusal_phrases.is_empty() {
        pricing.refusal_phrases = DEFAULT_REFUSAL_PHRASES
            .iter()
            .map(|p| (*p).into())
            .collect();
    }
    for tier in &mut pricing.tiers {
        if tier.id.is_empty() {
            tier.id = new_id("tier");
        }
        if tier.name.is_empty() {
            tier.name = tier.id.clone();
        }
        // A multiplier of 0 means "unset" everywhere it is read, so a tier that
        // really wants to zero a rate has to say so with an absolute 0.
        for m in [
            &mut tier.input_multiplier,
            &mut tier.output_multiplier,
            &mut tier.reasoning_multiplier,
        ] {
            if !m.is_finite() || *m < 0.0 {
                *m = 1.0;
            }
        }
        for hour in &mut tier.when.hours {
            hour.from = hour.from.min(23);
            hour.to = hour.to.min(23);
        }
        tier.when.weekdays.retain(|d| *d <= 6);
    }
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

    errors.extend(validate_catalog(cfg));
    errors.extend(validate_openrouter(cfg));
    errors
}

/// The part of a model's OpenRouter block that is published at `/v1/models`
/// whether or not the relay is listed on OpenRouter at all.
///
/// Checked separately from [`validate_openrouter`] for exactly that reason: a
/// price window nobody can reach, or one whose hours are not hours, is wrong
/// for every caller reading the listing, not only for OpenRouter.
fn validate_catalog(cfg: &Config) -> Vec<String> {
    let mut errors = Vec::new();
    for m in cfg.models.iter().filter(|m| m.enabled) {
        let o = &m.openrouter;
        if !o.knowledge_cutoff.is_empty()
            && chrono::NaiveDate::parse_from_str(&o.knowledge_cutoff, "%Y-%m-%d").is_err()
        {
            errors.push(format!(
                "model \"{}\" knowledgeCutoff \"{}\" must be YYYY-MM-DD",
                m.id, o.knowledge_cutoff
            ));
        }
        for (n, window) in o.pricing.overrides.iter().enumerate() {
            for day in &window.utc_days {
                if !WEEKDAY_NAMES.contains(&day.trim().to_lowercase().as_str()) {
                    errors.push(format!(
                        "model \"{}\" price override {n} names day \"{day}\"; use one of: {}",
                        m.id,
                        WEEKDAY_NAMES.join(", ")
                    ));
                }
            }
            // A window is a clock time written HHMM, so 1360 and 2500 are not
            // late — they are unreachable, and a price that never applies is
            // worse than no price at all because it reads like one that does.
            for (label, value) in [("utcStart", window.utc_start), ("utcEnd", window.utc_end)] {
                if value > 2359 || value % 100 > 59 {
                    errors.push(format!(
                        "model \"{}\" price override {n} has {label} {value}; it is a clock \
                         time written HHMM, so 0 to 2359 with minutes under 60",
                        m.id
                    ));
                }
            }
        }
    }
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

    /// `tz()` is asked for several times on every request, so the answer is
    /// worked out once when the config is published. It has to be the same
    /// answer parsing the name would give, and it has to follow the name when
    /// the name changes.
    #[test]
    fn the_timezone_is_parsed_once_and_stays_in_step_with_its_name() {
        let jakarta = normalize(Config::default());
        assert_eq!(jakarta.parsed_tz, Some(chrono_tz::Asia::Jakarta));
        assert_eq!(jakarta.tz(), crate::util::parse_tz(&jakarta.timezone));

        let moved = normalize(Config {
            timezone: "Europe/Berlin".into(),
            ..Config::default()
        });
        assert_eq!(moved.tz(), chrono_tz::Europe::Berlin);

        // A name nothing recognises still lands on UTC rather than failing.
        let nonsense = normalize(Config {
            timezone: "Mars/Olympus_Mons".into(),
            ..Config::default()
        });
        assert_eq!(nonsense.tz(), chrono_tz::UTC);

        // A config nobody normalised has no cached answer, and parses instead
        // of quietly reporting the wrong zone.
        let raw = Config {
            timezone: "Europe/Berlin".into(),
            ..Config::default()
        };
        assert_eq!(raw.parsed_tz, None);
        assert_eq!(raw.tz(), chrono_tz::Europe::Berlin);
    }

    /// The cached zone is derived, so it must not be written into config.json
    /// where somebody could edit it out of step with the name beside it.
    #[test]
    fn the_parsed_timezone_is_never_written_to_disk() {
        let cfg = normalize(Config::default());
        let written = serde_json::to_value(&cfg).unwrap();
        assert!(written.get("parsedTz").is_none(), "{written}");
        assert!(written.get("parsed_tz").is_none(), "{written}");
        assert_eq!(written["timezone"], "Asia/Jakarta");
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
    fn a_price_window_nobody_can_reach_is_refused_rather_than_published() {
        let mut cfg = cfg_with_backend();
        cfg.models.push(Model {
            id: "priced".into(),
            backend: "be1".into(),
            upstream_model: "upstream".into(),
            openrouter: OpenRouterModel {
                knowledge_cutoff: "last tuesday".into(),
                pricing: OpenRouterPricing {
                    overrides: vec![PricingOverride {
                        utc_days: vec!["caturday".into()],
                        utc_start: 1360,
                        utc_end: 2500,
                        ..Default::default()
                    }],
                    ..Default::default()
                },
                ..Default::default()
            },
            ..Default::default()
        });

        let errors = validate(&cfg);
        assert!(errors.iter().any(|e| e.contains("caturday")), "{errors:?}");
        assert!(
            errors.iter().any(|e| e.contains("utcStart 1360")),
            "{errors:?}"
        );
        assert!(
            errors.iter().any(|e| e.contains("utcEnd 2500")),
            "{errors:?}"
        );
        assert!(
            errors.iter().any(|e| e.contains("knowledgeCutoff")),
            "{errors:?}"
        );

        // And these are checked whether or not the relay is on OpenRouter at
        // all: the listing they break is the one every caller reads.
        assert!(!cfg.openrouter.enabled);
    }

    #[test]
    fn a_price_window_is_matched_on_the_utc_clock() {
        let peak = PricingOverride {
            utc_days: vec!["Monday".into(), "friday".into()],
            utc_start: 100,
            utc_end: 400,
            ..Default::default()
        };
        assert!(peak.covers(0, 100), "the start is inclusive");
        assert!(peak.covers(0, 359));
        assert!(!peak.covers(0, 400), "the end is exclusive");
        assert!(!peak.covers(1, 200), "Tuesday is not in the list");

        let night = PricingOverride {
            utc_start: 2200,
            utc_end: 200,
            ..Default::default()
        };
        assert!(night.covers(3, 2300), "a window may wrap past midnight");
        assert!(night.covers(3, 100));
        assert!(!night.covers(3, 1200));

        let all_day = PricingOverride {
            utc_days: vec!["sunday".into()],
            ..Default::default()
        };
        assert!(all_day.covers(6, 0));
        assert!(all_day.covers(6, 2359));
        assert!(!all_day.covers(5, 1200));
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
    fn the_reserved_no_thinking_rule_answers_for_the_efforts_the_relay_says_it_does() {
        // The dashboard's "No thinking" box is one rule with a reserved id, so
        // which efforts it covers is the relay's decision, not a copy saved into
        // every model. A config written when the split was somewhere else is
        // brought forward on load rather than needing every model re-saved.
        let mut cfg = cfg_with_backend();
        cfg.models.push(Model {
            id: "writer".into(),
            backend: "be1".into(),
            upstream_model: "deepseek".into(),
            system_prompts: vec![SystemPromptRule {
                id: NON_THINKING_RULE.into(),
                efforts: vec!["none".into(), "minimal".into(), "default".into()],
                min_effort: "none".into(),
                max_effort: "minimal".into(),
                ..Default::default()
            }],
            ..Default::default()
        });

        let cfg = normalize(cfg);
        let rule = &cfg.models[0].system_prompts[0];
        assert_eq!(rule.efforts, NON_THINKING_EFFORTS);
        assert!(
            rule.min_effort.is_empty() && rule.max_effort.is_empty(),
            "a stale range would still be matched before the list"
        );
    }

    #[test]
    fn a_rule_of_the_operators_own_is_left_exactly_as_they_wrote_it() {
        let mut cfg = cfg_with_backend();
        cfg.models.push(Model {
            id: "writer".into(),
            backend: "be1".into(),
            upstream_model: "deepseek".into(),
            system_prompts: vec![SystemPromptRule {
                id: "mine".into(),
                efforts: vec!["max".into()],
                ..Default::default()
            }],
            ..Default::default()
        });

        let cfg = normalize(cfg);
        assert_eq!(cfg.models[0].system_prompts[0].efforts, vec!["max"]);
    }

    #[test]
    fn an_unreadable_default_effort_falls_back_to_high_rather_than_to_nothing() {
        // A typo here would quietly mean "silence is not an effort at all",
        // which is the one answer nobody sets this field to ask for.
        let mut cfg = Config::default();
        cfg.defaults.effort = "hgih".into();
        assert_eq!(normalize(cfg).defaults.effort, DEFAULT_EFFORT);

        let mut cfg = Config::default();
        cfg.defaults.effort = String::new();
        assert_eq!(normalize(cfg).defaults.effort, DEFAULT_EFFORT);

        // Spelled any of the ways an effort can be spelled, it survives.
        let mut cfg = Config::default();
        cfg.defaults.effort = "  OFF ".into();
        assert_eq!(normalize(cfg).defaults.effort, "none");

        // And it can be turned off, which puts silence back outside the scale.
        let mut cfg = Config::default();
        cfg.defaults.effort = "default".into();
        assert_eq!(normalize(cfg).defaults.effort, "default");
    }

    #[test]
    fn a_relay_nobody_configured_reads_silence_as_high() {
        let cfg = normalize(Config::default());
        assert_eq!(
            crate::pricing::default_effort(&cfg),
            crate::pricing::Effort::High
        );
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
