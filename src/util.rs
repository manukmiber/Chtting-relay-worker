//! Small shared helpers: ids, masking, time buckets, JSON merging.

use chrono::{DateTime, TimeZone, Utc};
use chrono_tz::Tz;
use rand::Rng;
use serde_json::{Map, Value};
use std::path::Path;

/// Short, URL-safe, roughly sortable id.
pub fn new_id(prefix: &str) -> String {
    let millis = chrono::Utc::now().timestamp_millis().max(0) as u64;
    let time = to_base36(millis);
    let mut bytes = [0u8; 6];
    rand::rng().fill(&mut bytes);
    let rand_hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    if prefix.is_empty() {
        format!("{time}{rand_hex}")
    } else {
        format!("{prefix}_{time}{rand_hex}")
    }
}

fn to_base36(mut n: u64) -> String {
    const ALPHABET: &[u8] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    if n == 0 {
        return "0".into();
    }
    let mut out = Vec::new();
    while n > 0 {
        out.push(ALPHABET[(n % 36) as usize]);
        n /= 36;
    }
    out.reverse();
    String::from_utf8(out).expect("base36 alphabet is ascii")
}

/// A relay client key: long enough that guessing is hopeless.
pub fn new_client_key() -> String {
    let mut bytes = [0u8; 24];
    rand::rng().fill(&mut bytes);
    format!("sk-relay-{}", base64_url(&bytes))
}

pub fn random_hex(len: usize) -> String {
    let mut bytes = vec![0u8; len];
    rand::rng().fill(&mut bytes[..]);
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn base64_url(bytes: &[u8]) -> String {
    const A: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = (b[0] as u32) << 16 | (b[1] as u32) << 8 | b[2] as u32;
        let take = chunk.len() + 1;
        for i in 0..take {
            out.push(A[((n >> (18 - 6 * i)) & 0x3f) as usize] as char);
        }
    }
    out
}

/// Constant-time compare that does not leak length through early return.
pub fn safe_equal(a: &str, b: &str) -> bool {
    use subtle::ConstantTimeEq;
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() {
        // still do the work so timing does not distinguish "wrong length"
        let _ = a.ct_eq(a);
        return false;
    }
    a.ct_eq(b).into()
}

/// Mask a secret for display: `sk-rel…wxyz`.
pub fn mask_secret(s: &str) -> String {
    let chars: Vec<char> = s.chars().collect();
    if chars.is_empty() {
        return String::new();
    }
    if chars.len() <= 10 {
        return "•".repeat(chars.len().max(4));
    }
    let head: String = chars[..6].iter().collect();
    let tail: String = chars[chars.len() - 4..].iter().collect();
    format!("{head}…{tail}")
}

pub fn parse_tz(name: &str) -> Tz {
    name.parse().unwrap_or(chrono_tz::UTC)
}

/// `yyyy-mm-dd` in a fixed IANA zone — the dashboard's "daily" bucket.
pub fn day_key(ts_ms: i64, tz: &Tz) -> String {
    local(ts_ms, tz).format("%Y-%m-%d").to_string()
}

/// `yyyy-mm-ddTHH` in a fixed IANA zone.
pub fn hour_key(ts_ms: i64, tz: &Tz) -> String {
    local(ts_ms, tz).format("%Y-%m-%dT%H").to_string()
}

fn local(ts_ms: i64, tz: &Tz) -> DateTime<Tz> {
    let utc: DateTime<Utc> = Utc
        .timestamp_millis_opt(ts_ms)
        .single()
        .unwrap_or_else(Utc::now);
    utc.with_timezone(tz)
}

/// Epoch millis of local midnight today, for "today" statistics.
pub fn start_of_today(tz: &Tz) -> i64 {
    let now = Utc::now().with_timezone(tz);
    let naive = now
        .date_naive()
        .and_hms_opt(0, 0, 0)
        .expect("midnight exists");
    tz.from_local_datetime(&naive)
        .earliest()
        .map(|d| d.timestamp_millis())
        // A zone with a midnight DST jump has no local 00:00; the day still
        // starts somewhere, so fall back to the earliest valid instant.
        .unwrap_or_else(|| now.timestamp_millis() - i64::from(now.timestamp_subsec_millis()))
}

pub fn now_ms() -> i64 {
    Utc::now().timestamp_millis()
}

pub fn round(n: f64, digits: u32) -> f64 {
    if !n.is_finite() {
        return 0.0;
    }
    let f = 10f64.powi(digits as i32);
    (n * f).round() / f
}

/// Percentile of an already-sorted slice.
pub fn percentile(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let idx = (((p / 100.0) * sorted.len() as f64).ceil() as isize - 1)
        .clamp(0, sorted.len() as isize - 1) as usize;
    sorted[idx]
}

/// Truncate for storage, always on a character boundary.
pub fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let kept: String = s.chars().take(max).collect();
    let extra = s.chars().count() - max;
    format!("{kept}…[+{extra} chars]")
}

/// `deepseek-*`, `*-chat`, `gpt-4o` style matching, case-insensitive.
pub fn glob_match(value: &str, pattern: &str) -> bool {
    if pattern == "*" {
        return true;
    }
    let v: Vec<char> = value.to_lowercase().chars().collect();
    let p: Vec<char> = pattern.to_lowercase().chars().collect();
    glob_inner(&v, &p)
}

fn glob_inner(v: &[char], p: &[char]) -> bool {
    // Iterative backtracking: linear in the common case, and it cannot blow the
    // stack on a hostile pattern the way a naive recursive matcher can.
    let (mut vi, mut pi) = (0usize, 0usize);
    let (mut star, mut mark) = (usize::MAX, 0usize);
    while vi < v.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == v[vi]) {
            vi += 1;
            pi += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = pi;
            mark = vi;
            pi += 1;
        } else if star != usize::MAX {
            pi = star + 1;
            mark += 1;
            vi = mark;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

/// Recursive merge of JSON objects; arrays and scalars are replaced wholesale.
pub fn deep_merge(base: &Value, patch: &Value) -> Value {
    match (base, patch) {
        (Value::Object(b), Value::Object(p)) => {
            let mut out = b.clone();
            for (k, v) in p {
                if v.is_null() && !b.contains_key(k) {
                    out.insert(k.clone(), v.clone());
                    continue;
                }
                match (out.get(k), v) {
                    (Some(existing @ Value::Object(_)), Value::Object(_)) => {
                        let merged = deep_merge(existing, v);
                        out.insert(k.clone(), merged);
                    }
                    _ => {
                        out.insert(k.clone(), v.clone());
                    }
                }
            }
            Value::Object(out)
        }
        _ => patch.clone(),
    }
}

pub fn obj(value: &Value) -> Option<&Map<String, Value>> {
    value.as_object()
}

/// Write a file so a phone losing power mid-save cannot corrupt it: write a
/// temp file, fsync it, then rename over the target.
pub async fn write_atomic(path: &Path, contents: &str) -> std::io::Result<()> {
    use tokio::io::AsyncWriteExt;
    if let Some(dir) = path.parent() {
        tokio::fs::create_dir_all(dir).await?;
    }
    let tmp = path.with_extension("tmp");
    {
        let mut file = tokio::fs::File::create(&tmp).await?;
        file.write_all(contents.as_bytes()).await?;
        file.sync_all().await?;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        // The file holds backend API keys and client keys.
        let _ = tokio::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600)).await;
    }
    tokio::fs::rename(&tmp, path).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn glob_matches_the_shapes_config_uses() {
        assert!(glob_match("gpt-4o-mini", "gpt-4o*"));
        assert!(glob_match("Deepseek-v4-flash-0731", "deepseek*"));
        assert!(glob_match("anything", "*"));
        assert!(!glob_match("llama-2-7b", "llama-3*"));
        assert!(glob_match("qwen3-coder", "qwen?-*"));
    }

    #[test]
    fn glob_backtracks_without_exploding() {
        // The pathological case for a naive matcher.
        let value = "a".repeat(64);
        assert!(!glob_match(&value, "*a*a*a*a*a*a*a*a*b"));
    }

    #[test]
    fn masking_keeps_enough_to_recognise_but_not_to_use() {
        assert_eq!(mask_secret("sk-relay-abcdefghijklmnop"), "sk-rel…mnop");
        assert_eq!(mask_secret("short"), "•••••");
        assert_eq!(mask_secret(""), "");
    }

    #[test]
    fn truncate_never_splits_a_character() {
        let s = "日本語のテキストです";
        let out = truncate(s, 3);
        assert!(out.starts_with("日本語"));
        assert!(out.contains("+7 chars"));
    }

    #[test]
    fn deep_merge_recurses_into_objects_only() {
        let base = serde_json::json!({"a": {"b": 1, "c": 2}, "list": [1, 2]});
        let patch = serde_json::json!({"a": {"c": 9}, "list": [3]});
        let out = deep_merge(&base, &patch);
        assert_eq!(out, serde_json::json!({"a": {"b": 1, "c": 9}, "list": [3]}));
    }
}
