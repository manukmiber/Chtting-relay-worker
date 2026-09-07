//! Vocabulary loading and caching.
//!
//! Two families of tokenizer, both the *reference* implementation rather than a
//! reimplementation of it:
//!
//! * OpenAI vocabularies come from `tiktoken-rs`, which embeds OpenAI's own
//!   rank files in the binary. Nothing to download, and the ids are identical
//!   to the Python `tiktoken` package by construction.
//! * Everything else loads a HuggingFace `tokenizer.json` through the
//!   `tokenizers` crate — the same Rust code the Python `tokenizers` package is
//!   a binding for, so BPE, Unigram and WordPiece are all exact.
//!
//! Nothing is *required* on disk: a missing vocabulary degrades to a
//! script-aware estimator, and every count carries an `exact` flag so an
//! estimate is never silently treated as fact.

use anyhow::{anyhow, Result};
use parking_lot::RwLock;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tiktoken_rs::CoreBPE;

use crate::config::{TokenizerConfig, TokenizerRule};
use crate::util::glob_match;

/// OpenAI vocabularies compiled into the binary.
pub const BUILTIN_TIKTOKEN: [&str; 6] = [
    "o200k_base",
    "cl100k_base",
    "p50k_base",
    "p50k_edit",
    "r50k_base",
    "o200k_harmony",
];

/// Popular open models and the repo that carries their `tokenizer.json`.
pub const HF_PRESETS: [(&str, &str); 8] = [
    ("deepseek", "deepseek-ai/DeepSeek-V3"),
    ("deepseek-r1", "deepseek-ai/DeepSeek-R1"),
    ("qwen", "Qwen/Qwen2.5-7B-Instruct"),
    ("qwen3", "Qwen/Qwen3-8B"),
    ("llama3", "meta-llama/Meta-Llama-3-8B-Instruct"),
    ("mistral", "mistralai/Mistral-7B-Instruct-v0.3"),
    ("gemma", "google/gemma-2-9b-it"),
    ("glm", "THUDM/glm-4-9b-chat"),
];

pub fn hf_url(repo: &str) -> String {
    format!("https://huggingface.co/{repo}/resolve/main/tokenizer.json")
}

/// The cl100k pre-tokenizer split pattern, needed to rebuild a `CoreBPE` from a
/// rank file on disk.
const CL100K_PAT: &str = "(?i:'s|'t|'re|'ve|'m|'ll|'d)|[^\\r\\n\\p{L}\\p{N}]?\\p{L}+|\\p{N}{1,3}| ?[^\\s\\p{L}\\p{N}]+[\\r\\n]*|\\s*[\\r\\n]+|\\s+(?!\\S)|\\s+";

/* ------------------------------------------------------------- encoder -- */

/// Where a tiktoken rank table came from. Kept private: callers get an
/// [`Encoder`], never a handle on the tables behind it.
enum Inner {
    /// A built-in vocabulary: a shared `'static` reference, so every thread
    /// uses the same parsed tables with no locking and no per-call cost.
    Builtin(&'static CoreBPE),
    /// An OpenAI rank file loaded from disk.
    Loaded(Box<CoreBPE>),
    Hf(Box<tokenizers::Tokenizer>),
    /// No vocabulary installed: a script-aware approximation.
    Estimate,
}

impl Inner {
    fn bpe(&self) -> Option<&CoreBPE> {
        match self {
            Inner::Builtin(b) => Some(b),
            Inner::Loaded(b) => Some(b),
            _ => None,
        }
    }
}

pub struct Encoder {
    name: String,
    inner: Inner,
}

impl Encoder {
    pub fn estimator() -> Self {
        Self {
            name: "estimate".into(),
            inner: Inner::Estimate,
        }
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn kind(&self) -> &'static str {
        match self.inner {
            Inner::Builtin(_) | Inner::Loaded(_) => "tiktoken",
            Inner::Hf(_) => "huggingface",
            Inner::Estimate => "estimate",
        }
    }

    /// Whether this count is the real thing or an approximation.
    pub fn exact(&self) -> bool {
        !matches!(self.inner, Inner::Estimate)
    }

    pub fn vocab_size(&self) -> usize {
        match &self.inner {
            // The rank table is not exposed; report the well-known sizes.
            Inner::Builtin(_) | Inner::Loaded(_) => match self.name.as_str() {
                n if n.starts_with("o200k") => 200_019,
                n if n.starts_with("cl100k") => 100_277,
                n if n.starts_with("p50k") || n.starts_with("r50k") => 50_281,
                _ => 0,
            },
            Inner::Hf(tk) => tk.get_vocab_size(true),
            Inner::Estimate => 0,
        }
    }

    /// Token count for a piece of raw text.
    ///
    /// Special tokens are deliberately *not* honoured: text arriving from a
    /// caller must never be able to inject a control token by spelling one out.
    pub fn count(&self, text: &str) -> usize {
        if text.is_empty() {
            return 0;
        }
        match &self.inner {
            Inner::Builtin(bpe) => bpe.encode_ordinary(text).len(),
            Inner::Loaded(bpe) => bpe.encode_ordinary(text).len(),
            Inner::Hf(tk) => match tk.encode_fast(text, false) {
                Ok(enc) => enc.len(),
                Err(_) => estimate(text),
            },
            Inner::Estimate => estimate(text),
        }
    }

    /// Split text into the pieces the model actually sees.
    ///
    /// A multi-byte character can straddle two tokens; rather than emitting a
    /// replacement character for each half, the halves are joined into one
    /// piece, so the playground never shows U+FFFD for CJK or emoji. A joined
    /// piece has no single id of its own and reports `-1`.
    pub fn pieces(&self, text: &str) -> Vec<Piece> {
        match &self.inner {
            _ if self.inner.bpe().is_some() => {
                let bpe = self.inner.bpe().expect("just checked");
                let ids = bpe.encode_ordinary(text);
                let mut out = Vec::with_capacity(ids.len());
                let mut pending: Vec<u8> = Vec::new();
                let mut pending_ids: Vec<u32> = Vec::new();

                for (id, bytes) in ids.iter().zip(bpe._decode_native_and_split(ids.clone())) {
                    pending.extend_from_slice(&bytes);
                    pending_ids.push(*id);
                    match std::str::from_utf8(&pending) {
                        Ok(s) => {
                            out.push(Piece {
                                text: s.to_string(),
                                id: if pending_ids.len() == 1 {
                                    i64::from(pending_ids[0])
                                } else {
                                    -1
                                },
                                special: false,
                            });
                            pending.clear();
                            pending_ids.clear();
                        }
                        // Incomplete character: hold it and join with the next.
                        Err(_) => continue,
                    }
                }
                if !pending.is_empty() {
                    out.push(Piece {
                        text: String::from_utf8_lossy(&pending).into_owned(),
                        id: -1,
                        special: false,
                    });
                }
                out
            }
            Inner::Hf(tk) => match tk.encode(text, false) {
                Ok(enc) => {
                    let specials = enc.get_special_tokens_mask();
                    enc.get_offsets()
                        .iter()
                        .zip(enc.get_tokens())
                        .zip(enc.get_ids())
                        .enumerate()
                        .map(|(i, ((&(start, end), token), id))| Piece {
                            // Offsets index the original text, so this shows the
                            // real characters rather than the byte-level alphabet.
                            text: text
                                .get(start..end)
                                .filter(|s| !s.is_empty())
                                .map(str::to_string)
                                .unwrap_or_else(|| token.clone()),
                            id: i64::from(*id),
                            special: specials.get(i).is_some_and(|m| *m == 1),
                        })
                        .collect()
                }
                Err(_) => Vec::new(),
            },
            _ => text
                .split_inclusive(' ')
                .map(|s| Piece {
                    text: s.to_string(),
                    id: -1,
                    special: false,
                })
                .collect(),
        }
    }
}

/// One token as the playground shows it.
#[derive(Debug, Clone, serde::Serialize)]
pub struct Piece {
    pub text: String,
    /// The vocabulary id, or `-1` when this piece spans several tokens.
    pub id: i64,
    pub special: bool,
}

/// Script-aware fallback for when no vocabulary is installed.
///
/// Latin text runs about 4 characters per token; CJK is far denser, closer to
/// one token per character, and counting them the same way is wrong by 3x.
pub fn estimate(text: &str) -> usize {
    let mut dense = 0usize; // CJK, kana, hangul
    let mut other = 0usize;
    for ch in text.chars() {
        let c = ch as u32;
        let is_dense = (0x1100..=0x11FF).contains(&c)
            || (0x2E80..=0xA4CF).contains(&c)
            || (0xAC00..=0xD7AF).contains(&c)
            || (0xF900..=0xFAFF).contains(&c)
            || (0xFF00..=0xFF9F).contains(&c)
            || (0x20000..=0x2FA1F).contains(&c);
        if is_dense {
            dense += 1;
        } else {
            other += 1;
        }
    }
    let approx = dense as f64 * 1.05 + other as f64 / 3.6;
    (approx.ceil() as usize).max(if text.is_empty() { 0 } else { 1 })
}

/* ------------------------------------------------------------ registry -- */

pub struct Registry {
    dir: PathBuf,
    /// Loaded vocabularies. Reads take a shared lock and are uncontended in
    /// practice, since everything is loaded within the first few requests.
    cache: RwLock<HashMap<String, Arc<Encoder>>>,
    /// Serialises loads so a burst of concurrent first-requests parses a
    /// 3 MB vocabulary once rather than once per request.
    load_lock: tokio::sync::Mutex<()>,
    estimate: Arc<Encoder>,
    misses: RwLock<Vec<String>>,
    logger: Arc<crate::logging::Logger>,
}

impl Registry {
    pub fn new(dir: PathBuf, logger: Arc<crate::logging::Logger>) -> Self {
        Self {
            dir,
            cache: RwLock::new(HashMap::new()),
            load_lock: tokio::sync::Mutex::new(()),
            estimate: Arc::new(Encoder::estimator()),
            misses: RwLock::new(Vec::new()),
            logger,
        }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Which vocabulary and chat profile a backend model should use.
    pub fn match_rules(cfg: &TokenizerConfig, model: &str) -> (String, String) {
        for rule in &cfg.rules {
            if rule.pattern.is_empty() {
                continue;
            }
            if glob_match(model, &rule.pattern) {
                let tokenizer = if rule.tokenizer.is_empty() {
                    cfg.fallback.clone()
                } else {
                    rule.tokenizer.clone()
                };
                let profile = if rule.profile.is_empty() {
                    "openai".to_string()
                } else {
                    rule.profile.clone()
                };
                return (tokenizer, profile);
            }
        }
        (cfg.fallback.clone(), "openai".into())
    }

    /// Fetch a vocabulary, loading it on first use.
    ///
    /// Loading is CPU- and IO-bound (a multi-megabyte parse), so it happens on
    /// the blocking pool and never occupies an async worker thread.
    pub async fn get(&self, name: &str) -> Arc<Encoder> {
        if name.is_empty() {
            return self.estimate.clone();
        }
        if let Some(hit) = self.cache.read().get(name) {
            return hit.clone();
        }

        let _guard = self.load_lock.lock().await;
        // Another task may have loaded it while we waited for the lock.
        if let Some(hit) = self.cache.read().get(name) {
            return hit.clone();
        }

        let name_owned = name.to_string();
        let dir = self.dir.clone();
        let loaded = tokio::task::spawn_blocking(move || load(&dir, &name_owned))
            .await
            .unwrap_or_else(|e| Err(anyhow!("tokenizer load panicked: {e}")));

        let encoder = match loaded {
            Ok(enc) => Arc::new(enc),
            Err(err) => {
                let mut misses = self.misses.write();
                if !misses.iter().any(|m| m == name) {
                    misses.push(name.to_string());
                    self.logger.warn(format!(
                        "tokenizer \"{name}\" unavailable ({err}); counting with the estimator"
                    ));
                }
                self.estimate.clone()
            }
        };
        self.cache.write().insert(name.to_string(), encoder.clone());
        encoder
    }

    /// Drop cached vocabularies so a freshly downloaded file is picked up.
    pub fn invalidate(&self, name: Option<&str>) {
        match name {
            Some(n) => {
                self.cache.write().remove(n);
                self.misses.write().retain(|m| m != n);
            }
            None => {
                self.cache.write().clear();
                self.misses.write().clear();
            }
        }
    }

    /// Everything usable right now, plus what could still be installed.
    pub async fn inventory(&self) -> serde_json::Value {
        let mut installed = Vec::new();
        for name in BUILTIN_TIKTOKEN {
            installed.push(serde_json::json!({
                "name": name,
                "kind": "tiktoken",
                "size": 0,
                "file": "(built in)",
                "builtin": true,
            }));
        }

        if let Ok(mut entries) = tokio::fs::read_dir(&self.dir).await {
            let mut files = Vec::new();
            while let Ok(Some(entry)) = entries.next_entry().await {
                let file = entry.file_name().to_string_lossy().to_string();
                let size = entry.metadata().await.map(|m| m.len()).unwrap_or(0);
                if let Some(stem) = file.strip_suffix(".tokenizer.json") {
                    files.push((stem.to_string(), "huggingface", size, file));
                } else if let Some(stem) = file.strip_suffix(".tiktoken") {
                    files.push((stem.to_string(), "tiktoken", size, file));
                }
            }
            files.sort_by(|a, b| a.0.cmp(&b.0));
            for (name, kind, size, file) in files {
                installed.push(serde_json::json!({
                    "name": name, "kind": kind, "size": size, "file": file, "builtin": false,
                }));
            }
        }

        let installed_names: Vec<String> = installed
            .iter()
            .filter_map(|i| i["name"].as_str().map(str::to_string))
            .collect();

        let available: Vec<serde_json::Value> = HF_PRESETS
            .iter()
            .map(|(name, repo)| {
                serde_json::json!({
                    "name": name,
                    "kind": "huggingface",
                    "repo": repo,
                    "installed": installed_names.iter().any(|i| i == name),
                    "url": hf_url(repo),
                })
            })
            .collect();

        let loaded: Vec<serde_json::Value> = self
            .cache
            .read()
            .iter()
            .map(|(name, enc)| {
                serde_json::json!({
                    "name": name,
                    "kind": enc.kind(),
                    "exact": enc.exact(),
                    "vocabSize": enc.vocab_size(),
                })
            })
            .collect();

        serde_json::json!({
            "dir": self.dir.to_string_lossy(),
            "installed": installed,
            "available": available,
            "loaded": loaded,
        })
    }
}

/// Blocking load of one vocabulary. Built-ins first, then disk.
fn load(dir: &Path, name: &str) -> Result<Encoder> {
    if let Some(enc) = builtin_encoder(name) {
        return Ok(enc);
    }

    let tiktoken_path = dir.join(format!("{name}.tiktoken"));
    if tiktoken_path.exists() {
        return load_tiktoken_file(name, &tiktoken_path);
    }

    for candidate in [
        format!("{name}.tokenizer.json"),
        format!("{name}.json"),
        format!("{name}/tokenizer.json"),
    ] {
        let path = dir.join(candidate);
        if path.exists() {
            let tk = tokenizers::Tokenizer::from_file(&path)
                .map_err(|e| anyhow!("{}: {e}", path.display()))?;
            return Ok(Encoder {
                name: name.to_string(),
                inner: Inner::Hf(Box::new(tk)),
            });
        }
    }

    Err(anyhow!("no vocabulary file found in {}", dir.display()))
}

/// An encoder for a vocabulary compiled into the binary, without touching disk.
pub fn builtin_encoder(name: &str) -> Option<Encoder> {
    builtin(name).map(|bpe| Encoder {
        name: name.to_string(),
        inner: Inner::Builtin(bpe),
    })
}

fn builtin(name: &str) -> Option<&'static CoreBPE> {
    match name {
        "cl100k_base" => Some(tiktoken_rs::cl100k_base_singleton()),
        "o200k_base" => Some(tiktoken_rs::o200k_base_singleton()),
        "p50k_base" => Some(tiktoken_rs::p50k_base_singleton()),
        "p50k_edit" => Some(tiktoken_rs::p50k_edit_singleton()),
        "r50k_base" => Some(tiktoken_rs::r50k_base_singleton()),
        "o200k_harmony" => Some(tiktoken_rs::o200k_harmony_singleton()),
        _ => None,
    }
}

/// Rebuild a `CoreBPE` from an OpenAI rank file on disk.
///
/// The rank file carries no pre-tokenizer pattern, so it is chosen by name:
/// o200k-style vocabularies use the o200k split, everything else the cl100k
/// one. A custom vocabulary built on a different split needs its own build.
fn load_tiktoken_file(name: &str, path: &Path) -> Result<Encoder> {
    use base64::Engine;
    let raw = std::fs::read_to_string(path)?;
    let mut ranks: rustc_hash::FxHashMap<Vec<u8>, u32> = rustc_hash::FxHashMap::default();
    for (lineno, line) in raw.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let mut parts = line.split(' ');
        let (token, rank) = match (parts.next(), parts.next()) {
            (Some(t), Some(r)) => (t, r),
            _ => {
                return Err(anyhow!(
                    "{}: malformed rank on line {}",
                    path.display(),
                    lineno + 1
                ))
            }
        };
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(token)
            .map_err(|e| anyhow!("{}: bad base64 on line {}: {e}", path.display(), lineno + 1))?;
        let rank: u32 = rank
            .trim()
            .parse()
            .map_err(|e| anyhow!("{}: bad rank on line {}: {e}", path.display(), lineno + 1))?;
        ranks.insert(bytes, rank);
    }
    if ranks.is_empty() {
        return Err(anyhow!("{} contains no ranks", path.display()));
    }

    let pattern = if name.contains("o200k") {
        tiktoken_rs::O200K_BASE_PAT_STR
    } else {
        CL100K_PAT
    };
    let bpe = CoreBPE::new(ranks, rustc_hash::FxHashMap::default(), pattern)
        .map_err(|e| anyhow!("{}: {e}", path.display()))?;
    Ok(Encoder {
        name: name.to_string(),
        inner: Inner::Loaded(Box::new(bpe)),
    })
}

/// Rules a route may override; kept next to the rule matching it drives.
pub fn rule_for(rules: &[TokenizerRule], model: &str) -> Option<TokenizerRule> {
    rules
        .iter()
        .find(|r| !r.pattern.is_empty() && glob_match(model, &r.pattern))
        .cloned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::TokenizerConfig;

    #[test]
    fn builtin_vocabularies_need_no_files() {
        for name in BUILTIN_TIKTOKEN {
            assert!(builtin(name).is_some(), "{name} should be compiled in");
        }
        assert!(builtin("deepseek").is_none());
    }

    #[test]
    fn rules_match_the_backend_name_not_the_alias() {
        let cfg = TokenizerConfig::default();
        assert_eq!(
            Registry::match_rules(&cfg, "Deepseek-v4-flash-0731").0,
            "deepseek"
        );
        assert_eq!(Registry::match_rules(&cfg, "gpt-4o-mini").0, "o200k_base");
        assert_eq!(Registry::match_rules(&cfg, "gpt-4-turbo").0, "cl100k_base");
        // unknown names fall through to the catch-all rule
        assert_eq!(Registry::match_rules(&cfg, "something-new").0, "o200k_base");
    }

    #[test]
    fn the_estimator_knows_cjk_is_denser_than_latin() {
        let latin = estimate("the quick brown fox jumps over the lazy dog");
        let cjk = estimate("你好世界这是一个中文测试句子");
        assert!(latin < 15, "latin over-counted: {latin}");
        assert!(cjk >= 14, "cjk under-counted: {cjk}");
        assert_eq!(estimate(""), 0);
    }

    #[test]
    fn pieces_never_emit_replacement_characters() {
        let enc = builtin_encoder("cl100k_base").expect("cl100k is built in");
        let text = "你好，世界！🚀 emoji";
        let pieces = enc.pieces(text);
        assert!(!pieces.is_empty());
        assert!(
            !pieces.iter().any(|p| p.text.contains('\u{FFFD}')),
            "got replacement characters: {pieces:?}"
        );
        // The pieces must still reconstruct the original text exactly.
        let joined: String = pieces.iter().map(|p| p.text.as_str()).collect();
        assert_eq!(joined, text);
        // A piece that is exactly one token carries that token's id.
        assert!(pieces.iter().any(|p| p.id >= 0), "no ids were reported");
    }
}
