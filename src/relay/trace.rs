//! The per-request narration.
//!
//! One request leaves a trail of lines, all carrying the same uuid, so that a
//! slow call can be taken apart without guessing which stage cost the time:
//!
//! ```text
//! req 6f1c… in    model=… key=… user=… effort=high stream=true bytes=812
//! req 6f1c… tok   12.4ms  (1 284 caller / 1 806 upstream, deepseek, exact)
//! req 6f1c… inj   0.8ms   prompt=spr_… mode=prepend
//! req 6f1c… ttft  612.0ms
//! req 6f1c… done  8 412.0ms status=200 stop
//! req 6f1c… sum   …one line with everything…
//! ```
//!
//! Both destinations get the same text: the [`Logger`](crate::logging::Logger)
//! writes to stderr, which is what Termux shows, and to `relay.log`, which is
//! what the dashboard's Logs tab reads. There is no second, quieter channel.

use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::Arc;

use crate::logging::Logger;

/// Resident memory of this process, in MiB, refreshed at most once a second.
///
/// Every finished request puts this on its row, and the underlying read is a
/// blocking `open`/`read`/`close` of a ~1 KB procfs file plus a scan for the
/// line that matters — done straight on an async worker thread, so under load
/// it is hundreds of synchronous file reads a second stealing time from the
/// streams sharing that thread.
///
/// What it answers is "roughly how much memory is this phone using", which does
/// not move meaningfully inside a second. So it is read on a timer and every
/// request in between reads the number that was already there: the log line and
/// the dashboard say the same thing they said before, for none of the cost.
pub fn rss_mb() -> f64 {
    // Bits of an f64 and a millisecond stamp, so reading is two relaxed loads
    // and no lock on the path that every request takes.
    static CACHED: AtomicU64 = AtomicU64::new(0);
    static READ_AT: AtomicI64 = AtomicI64::new(i64::MIN);
    const TTL_MS: i64 = 1_000;

    let now = crate::util::now_ms();
    let read_at = READ_AT.load(Ordering::Relaxed);
    if now.saturating_sub(read_at) < TTL_MS {
        return f64::from_bits(CACHED.load(Ordering::Relaxed));
    }
    // A race here costs one extra read of the same file, never a wrong answer.
    READ_AT.store(now, Ordering::Relaxed);
    let fresh = read_rss_mb();
    CACHED.store(fresh.to_bits(), Ordering::Relaxed);
    fresh
}

/// `/proc/self/status` rather than `statm`: the former is already in kB, while
/// the latter is in pages and would need the page size, which is not always
/// 4 KiB on arm64. Returns 0 where there is no procfs.
fn read_rss_mb() -> f64 {
    let Ok(status) = std::fs::read_to_string("/proc/self/status") else {
        return 0.0;
    };
    for line in status.lines() {
        if let Some(rest) = line.strip_prefix("VmRSS:") {
            let kb: f64 = rest
                .split_whitespace()
                .next()
                .and_then(|n| n.parse().ok())
                .unwrap_or(0.0);
            return crate::util::round(kb / 1024.0, 2);
        }
    }
    0.0
}

/// Groups digits so a six-figure token count is readable at a glance on a
/// phone screen: `1284` becomes `1 284`.
pub fn grouped(n: i64) -> String {
    let negative = n < 0;
    let digits = n.unsigned_abs().to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3 + 1);
    for (i, ch) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 {
            out.push(' ');
        }
        out.push(ch);
    }
    if negative {
        out.insert(0, '-');
    }
    out
}

/// A handle that writes one request's lines.
///
/// Cheap to clone and safe to hold across the stream: it owns nothing but the
/// logger, a short id and a switch.
#[derive(Clone)]
pub struct Trace {
    logger: Arc<Logger>,
    /// The first segment of the uuid. Enough to pick a request out of a log,
    /// short enough that the line still fits a phone.
    short: String,
    on: bool,
}

impl Trace {
    pub fn new(logger: Arc<Logger>, uid: &str, on: bool) -> Self {
        Self {
            logger,
            short: uid.split('-').next().unwrap_or(uid).to_string(),
            on,
        }
    }

    /// A trace that never writes, for the paths that have no request.
    pub fn silent(logger: Arc<Logger>) -> Self {
        Self {
            logger,
            short: String::new(),
            on: false,
        }
    }

    pub fn enabled(&self) -> bool {
        self.on
    }

    /// One phase line. `phase` is padded so the columns line up in a terminal.
    ///
    /// The detail arrives as a closure rather than a string, and that is the
    /// whole point: these lines are off by default, and taking a `String` meant
    /// every caller built one — a `format!` with a dozen fields, token counts
    /// grouped into their own allocations — and handed it over to be dropped
    /// unread. Three of those on every single request, for output nobody asked
    /// for. A closure is not called at all when the trace is off.
    pub fn phase(&self, phase: &str, detail: impl FnOnce() -> String) {
        if !self.on {
            return;
        }
        self.logger
            .info(format!("req {} {phase:<5} {}", self.short, detail()));
    }

    /// A millisecond figure, always with one decimal so the column is stable.
    pub fn timed(&self, phase: &str, ms: f64, detail: impl FnOnce() -> String) {
        if !self.on {
            return;
        }
        self.phase(phase, || {
            let detail = detail();
            let gap = if detail.is_empty() { "" } else { "  " };
            format!("{ms:>9.1}ms{gap}{detail}")
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn digits_are_grouped_in_threes() {
        assert_eq!(grouped(0), "0");
        assert_eq!(grouped(42), "42");
        assert_eq!(grouped(1_284), "1 284");
        assert_eq!(grouped(999_999), "999 999");
        assert_eq!(grouped(1_234_567), "1 234 567");
        assert_eq!(grouped(-8_192), "-8 192");
    }

    #[test]
    fn the_short_id_is_the_first_uuid_segment() {
        let logger = Logger::console(crate::logging::Level::Silent);
        let trace = Trace::new(logger, "6f1c9a2b-7d43-4e0f-9c1a-2b3c4d5e6f70", true);
        assert_eq!(trace.short, "6f1c9a2b");
    }

    #[test]
    fn resident_memory_is_read_or_honestly_zero() {
        let mb = rss_mb();
        assert!(mb >= 0.0, "memory cannot be negative");
        #[cfg(target_os = "linux")]
        assert!(mb > 0.0, "a running process has resident memory");
    }
}
