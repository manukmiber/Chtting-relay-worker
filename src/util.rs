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

/// A version-4 UUID, the plain hyphenated form.
///
/// Hand-rolled rather than pulled in as a dependency: it is twelve lines over
/// the same RNG the rest of this module already uses, and every crate left out
/// of the tree is a crate that cannot fail to cross-compile for Android.
pub fn new_uuid_v4() -> String {
    let mut b = [0u8; 16];
    rand::rng().fill(&mut b);
    // Version 4, variant 1 — the two fields RFC 9562 pins.
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let hex = |range: std::ops::Range<usize>| -> String {
        b[range].iter().map(|x| format!("{x:02x}")).collect()
    };
    format!(
        "{}-{}-{}-{}-{}",
        hex(0..4),
        hex(4..6),
        hex(6..8),
        hex(8..10),
        hex(10..16)
    )
}

/// A relay client key: `Kunci-Zeiko-` and a version-4 UUID.
///
/// The tail used to be 32 characters mixing digits, both cases and symbols.
/// Stronger on paper, unusable in practice: a key carrying `$`, `|`, `{` or
/// `?` cannot be pasted into a shell, an `.env` line, a YAML file or a query
/// string without something eating or reinterpreting a character, so the key
/// that arrives at the relay is not the key that was minted and the caller is
/// told their brand new key is invalid. A UUID is 122 random bits written in
/// nothing but hex and hyphens, which survives every one of those paths
/// untouched — and 122 bits is far past anything worth guessing at.
///
/// Both kinds of key come from here: a company key and a private key differ in
/// how the relay treats the caller, never in how the secret is shaped.
pub fn new_client_key() -> String {
    format!("Kunci-Zeiko-{}", new_uuid_v4())
}

pub fn random_hex(len: usize) -> String {
    let mut bytes = vec![0u8; len];
    rand::rng().fill(&mut bytes[..]);
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// SHA-256 of some bytes, as raw digest.
///
/// The one hash the relay uses for identity: the key index looks secrets up by
/// it, and a private key's upstream id is a prefix of it.
pub fn digest(bytes: &[u8]) -> [u8; 32] {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hasher.finalize().into()
}

/// A stable, opaque name for a secret: `u_` and 24 hex characters.
///
/// 96 bits of a SHA-256, which is far past the point where two client keys
/// collide, and reveals nothing about the key it came from. This is what a
/// private key sends upstream as its user id, so a backend can isolate its
/// prompt cache per caller without the relay handing over the credential that
/// would let it *be* that caller.
pub fn fingerprint(secret: &str) -> String {
    let digest = digest(secret.as_bytes());
    let mut out = String::with_capacity(26);
    out.push_str("u_");
    for byte in &digest[..12] {
        use std::fmt::Write;
        let _ = write!(out, "{byte:02x}");
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

/// The pieces of a wall-clock time a price rule and a log line both need.
#[derive(Debug, Clone)]
pub struct LocalParts {
    /// The calendar year in the configured zone. Invoice numbers count within
    /// it, so an invoice issued at 23:30 on new year's eve in Jakarta belongs
    /// to the year Jakarta was in, not the year UTC was in.
    pub year: i32,
    /// 0-23 in the configured zone, never UTC — a peak-hour rule written for
    /// Jakarta evenings must not fire at Jakarta lunchtime.
    pub hour: u32,
    /// Monday is 0, Sunday is 6.
    pub weekday: u32,
    /// The same instant as text, zone offset included.
    pub stamp: String,
}

/// Day of the month, 1-31, in the given zone. What the billing cycle compares
/// its configured day against.
pub fn local_day_of_month(ts_ms: i64, tz: &Tz) -> u32 {
    use chrono::Datelike;
    local(ts_ms, tz).day()
}

pub fn local_parts(ts_ms: i64, tz: &Tz) -> LocalParts {
    use chrono::{Datelike, Timelike};
    let at = local(ts_ms, tz);
    LocalParts {
        year: at.year(),
        hour: at.hour(),
        weekday: at.weekday().num_days_from_monday(),
        stamp: at.format("%Y-%m-%dT%H:%M:%S%.3f%:z").to_string(),
    }
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

/// The UTC weekday (Monday = 0) and `HHMM` clock time of one instant.
///
/// UTC and not the relay's own zone: this answers "which published price
/// window is this request in", and the published windows are quoted in UTC so
/// that a caller anywhere can check a bill without knowing where the phone is.
pub fn utc_parts(ts_ms: i64) -> (u32, u32) {
    use chrono::{Datelike, Timelike};
    let at = DateTime::from_timestamp_millis(ts_ms).unwrap_or_else(Utc::now);
    (
        at.weekday().num_days_from_monday(),
        at.hour() * 100 + at.minute(),
    )
}

/// A rate quoted per million tokens, written out as the price of one token.
///
/// A decimal string rather than a number, and never scientific notation: this
/// is published in a price list and read back against an invoice, where
/// `3.5e-7` is not an answer. Empty for a rate of zero, because "unpriced" and
/// "free" are different claims and only one of them should be publishable by
/// accident.
pub fn per_token(usd_per_m: f64) -> String {
    if !usd_per_m.is_finite() || usd_per_m <= 0.0 {
        return String::new();
    }
    let text = format!("{:.12}", usd_per_m / 1_000_000.0);
    let trimmed = text.trim_end_matches('0');
    if trimmed.ends_with('.') {
        // Rounded away to nothing at twelve places: a rate that small is below
        // anything this can honestly publish.
        return String::new();
    }
    trimmed.to_string()
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
    fn a_client_key_is_kunci_zeiko_and_a_uuid_v4() {
        for _ in 0..200 {
            let key = new_client_key();
            let tail = key
                .strip_prefix("Kunci-Zeiko-")
                .expect("every key carries the prefix");
            assert_eq!(tail.chars().count(), 36, "{key}");
            let parts: Vec<&str> = tail.split('-').collect();
            assert_eq!(
                parts.iter().map(|p| p.len()).collect::<Vec<_>>(),
                vec![8, 4, 4, 4, 12],
                "{key}"
            );
            assert!(parts[2].starts_with('4'), "not version 4: {key}");
            // Nothing outside hex and hyphens, so the key survives a shell, an
            // .env line and a URL without being mangled on the way in.
            assert!(
                tail.chars().all(|c| c.is_ascii_hexdigit() || c == '-'),
                "{key}"
            );
            assert!(
                axum::http::HeaderValue::from_str(&format!("Bearer {key}")).is_ok(),
                "{key} cannot go in a header"
            );
        }
    }

    #[test]
    fn two_keys_are_never_the_same() {
        let keys: std::collections::HashSet<String> = (0..500).map(|_| new_client_key()).collect();
        assert_eq!(keys.len(), 500);
    }

    #[test]
    fn a_uuid_is_version_four_and_shaped_like_one() {
        for _ in 0..200 {
            let id = new_uuid_v4();
            let parts: Vec<&str> = id.split('-').collect();
            assert_eq!(
                parts.iter().map(|p| p.len()).collect::<Vec<_>>(),
                vec![8, 4, 4, 4, 12],
                "{id}"
            );
            assert!(
                id.chars().all(|c| c.is_ascii_hexdigit() || c == '-'),
                "{id}"
            );
            assert!(parts[2].starts_with('4'), "not version 4: {id}");
            assert!(
                matches!(parts[3].as_bytes()[0], b'8' | b'9' | b'a' | b'b'),
                "not variant 1: {id}"
            );
        }
        assert_ne!(new_uuid_v4(), new_uuid_v4());
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
    fn local_parts_are_read_in_the_configured_zone_not_utc() {
        // 2026-09-20T20:30:00Z is 2026-09-21, 03:30, in Jakarta: a different
        // hour, a different day and a different weekday. A peak-hour price rule
        // written for Jakarta evenings must not fire on this request.
        let ts = 1_789_936_200_000;
        let utc = local_parts(ts, &chrono_tz::UTC);
        let jakarta = local_parts(ts, &"Asia/Jakarta".parse().unwrap());
        assert_eq!(utc.hour, 20);
        assert_eq!(jakarta.hour, 3);
        assert_ne!(utc.weekday, jakarta.weekday);
        assert!(jakarta.stamp.ends_with("+07:00"), "{}", jakarta.stamp);
    }

    #[test]
    fn deep_merge_recurses_into_objects_only() {
        let base = serde_json::json!({"a": {"b": 1, "c": 2}, "list": [1, 2]});
        let patch = serde_json::json!({"a": {"c": 9}, "list": [3]});
        let out = deep_merge(&base, &patch);
        assert_eq!(out, serde_json::json!({"a": {"b": 1, "c": 9}, "list": [3]}));
    }
}
