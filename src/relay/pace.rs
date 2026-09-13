//! Holding a stream back to a chosen speed.
//!
//! A backend that generates at 170 tokens a second pushes 170 tokens a second
//! down the tunnel, and on a phone's uplink that is where the strain lands —
//! cloudflared, the mobile radio and every other request sharing them. Pacing
//! the reply to, say, 35 tokens a second costs the caller nothing they can feel
//! (it is still faster than anyone reads) and leaves the link with headroom.
//!
//! The pacing is on the relay's own output, and it works by not reading from
//! the backend faster than it writes to the caller — the delay propagates back
//! up the TCP window rather than piling tokens up in memory here.
//!
//! Speed is tracked against a running clock rather than by sleeping a fixed
//! amount per chunk: a fixed sleep drifts, because the time already spent
//! parsing and reshaping is unaccounted for, and drift over a long generation
//! is minutes.

use std::time::Duration;
// Tokio's clock, not the standard library's, so that the same code a test can
// pause is the code that runs in production.
use tokio::time::Instant;

/// Average characters per token across the text these models emit.
///
/// Used only to decide how long to wait, never to bill: an exact count would
/// mean running the tokenizer on every delta, which on a phone would cost more
/// than the pacing saves. English prose runs about 4 characters per token and
/// CJK rather fewer, so this errs towards pacing CJK a little slower than
/// asked — the safe direction for a link that is the reason for the throttle.
const CHARS_PER_TOKEN: f64 = 4.0;

#[derive(Debug)]
pub struct Pacer {
    /// Tokens per second. 0 or less means no pacing at all.
    target: f64,
    started: Option<Instant>,
    /// Tokens released so far, as estimated from the text.
    released: f64,
}

impl Pacer {
    pub fn new(target_tokens_per_second: f64) -> Self {
        Self {
            target: if target_tokens_per_second.is_finite() {
                target_tokens_per_second.max(0.0)
            } else {
                0.0
            },
            started: None,
            released: 0.0,
        }
    }

    pub fn active(&self) -> bool {
        self.target > 0.0
    }

    /// Estimate a delta's size in tokens. Never zero for non-empty text, or a
    /// stream of single characters would pace as if it were free.
    fn tokens(text: &str) -> f64 {
        if text.is_empty() {
            return 0.0;
        }
        (text.chars().count() as f64 / CHARS_PER_TOKEN).max(1.0)
    }

    /// How long to hold this delta back before sending it.
    ///
    /// The clock starts at the first delta, not at construction: waiting out
    /// the backend's own time to first token and then charging it against the
    /// pace would release the whole opening burst at once.
    ///
    /// A delta is charged *after* it is cleared to go, not before, so the first
    /// one leaves immediately and time to first token stays the backend's
    /// number rather than something the throttle invented.
    pub fn delay_for(&mut self, text: &str) -> Option<Duration> {
        if !self.active() {
            return None;
        }
        let now = Instant::now();
        let started = *self.started.get_or_insert(now);
        let due = started + Duration::from_secs_f64(self.released / self.target);
        self.released += Self::tokens(text);
        (due > now).then(|| due - now)
    }

    /// Hold this delta back for as long as [`delay_for`](Self::delay_for) says.
    pub async fn hold(&mut self, text: &str) {
        if let Some(wait) = self.delay_for(text) {
            tokio::time::sleep(wait).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn a_pacer_with_no_target_never_waits() {
        let mut pacer = Pacer::new(0.0);
        assert!(!pacer.active());
        assert_eq!(pacer.delay_for("a hundred tokens of text"), None);
    }

    #[test]
    fn a_negative_or_nonsense_target_is_read_as_off() {
        assert!(!Pacer::new(-5.0).active());
        assert!(!Pacer::new(f64::NAN).active());
        assert!(Pacer::new(35.0).active());
    }

    #[tokio::test(start_paused = true)]
    async fn the_first_delta_goes_out_without_waiting() {
        // The opening token is what time-to-first-token measures, and pacing
        // must not be what makes it look slow.
        let mut pacer = Pacer::new(35.0);
        assert_eq!(pacer.delay_for("hi"), None);
        let long = "x".repeat(4_000);
        let mut fresh = Pacer::new(1.0);
        assert_eq!(fresh.delay_for(&long), None, "even a large first delta");
    }

    #[tokio::test(start_paused = true)]
    async fn the_wait_grows_with_what_has_already_been_released() {
        let mut pacer = Pacer::new(10.0);
        // 40 characters ≈ 10 tokens ≈ one second of budget.
        let text = "x".repeat(40);
        assert_eq!(pacer.delay_for(&text), None, "the first goes out at once");
        let second = pacer.delay_for(&text).expect("the budget is spent");
        assert!(
            second > Duration::from_millis(900) && second <= Duration::from_secs(1),
            "{second:?}"
        );
        let third = pacer.delay_for(&text).expect("still spent");
        assert!(
            third > second,
            "{third:?} should be further out than {second:?}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn waiting_is_measured_against_the_clock_rather_than_accumulated() {
        // A pacer that slept a fixed amount per chunk would drift by whatever
        // the rest of the work cost. This one gives back the time that has
        // actually passed.
        let mut pacer = Pacer::new(100.0);
        let text = "x".repeat(400); // ≈ 100 tokens ≈ 1s
        pacer.delay_for(&text);
        // Pretend a second of real work happened between the two deltas.
        pacer.started = Some(Instant::now() - Duration::from_secs(1));
        assert_eq!(
            pacer.delay_for(""),
            None,
            "the elapsed second already paid for the first delta"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn an_empty_delta_costs_nothing() {
        let mut pacer = Pacer::new(1.0);
        assert_eq!(Pacer::tokens(""), 0.0);
        assert_eq!(pacer.delay_for(""), None);
    }

    #[tokio::test(start_paused = true)]
    async fn a_paced_stream_takes_about_as_long_as_the_target_says() {
        let mut pacer = Pacer::new(20.0);
        let started = Instant::now();
        // 40 deltas of 4 characters: about 40 tokens, so about two seconds.
        for _ in 0..40 {
            pacer.hold("word").await;
        }
        let elapsed = started.elapsed();
        assert!(
            elapsed >= Duration::from_millis(1_900) && elapsed < Duration::from_millis(2_100),
            "{elapsed:?}"
        );

        // The same deltas with no ceiling take no time at all.
        let mut free = Pacer::new(0.0);
        let started = Instant::now();
        for _ in 0..40 {
            free.hold("word").await;
        }
        assert!(started.elapsed() < Duration::from_millis(10));
    }
}
