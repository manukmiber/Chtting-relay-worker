//! "I forgot the dashboard password."
//!
//! The dashboard password is the only thing standing between whoever finds the
//! dashboard tunnel's URL and a panel that writes the config, reads every
//! stored prompt and reveals every client key. So a reset cannot be something a
//! browser can ask for and then complete on its own: an emailed link, a
//! security question or an "are you the owner?" checkbox would all hand that
//! panel to exactly the person the password is there to keep out.
//!
//! What the owner has that nobody else does is the phone. So that is what the
//! reset asks for. The dashboard mints a six-digit code, the code is shown
//! **on the device** — in the Termux window the relay is running in, and in a
//! file `chtting-relay reset-code` prints — and the browser has to send it
//! back. A remote attacker can start as many resets as they like and never see
//! a single code.
//!
//! What makes six digits enough is that they are never guessable in bulk:
//!
//! * One code is live at a time, for [`CODE_TTL_MS`], and it is random over the
//!   whole million.
//! * [`MAX_ATTEMPTS`] wrong tries destroy it, so a run gets five guesses out of
//!   a million rather than as many as the CPU allows — and the sixth wrong
//!   guess does not just fail, it takes the code with it.
//! * Minting is rate-limited to one per [`MINT_INTERVAL_MS`], and a start while
//!   a code is already live returns that same code instead of printing a new
//!   one. Without that, a stranger could make the operator's terminal scroll
//!   reset banners, which is both noise and a way to hide the real one.
//!
//! The code is written to `data/run/password-reset.json` with mode 0600, which
//! sounds like the weak point and is not: `config/config.json` in the same
//! directory holds the dashboard password itself in the clear, so anything that
//! can read one already has the other. The file exists because the relay is
//! usually started by the keeper with its stderr sent to `/dev/null`, and a
//! banner nobody can see is not a recovery path.

use parking_lot::Mutex;
use rand::Rng;
use serde_json::json;
use std::path::{Path, PathBuf};

use crate::util::{now_ms, random_hex, safe_equal, write_atomic};

/// Digits in the code the operator reads off the phone.
///
/// Six, because it is typed from one screen into another by somebody who is
/// already annoyed. The guessing budget, not the length, is what makes it safe
/// — see the module docs.
pub const CODE_DIGITS: u32 = 6;

/// How long a code lives. Long enough to find the Termux window and come back,
/// short enough that a code left on a screen is not a standing key.
pub const CODE_TTL_MS: i64 = 10 * 60 * 1000;

/// Wrong codes before the challenge is destroyed rather than merely refused.
pub const MAX_ATTEMPTS: u32 = 5;

/// The shortest gap between two *new* codes.
///
/// A start while one is still live returns that one, so this only bites after a
/// code has been burnt or has expired — which is where the "make the operator's
/// terminal scroll" attack would otherwise live.
pub const MINT_INTERVAL_MS: i64 = 30_000;

/// Where a pending code is left for `chtting-relay reset-code` to read.
pub fn code_file_in(data: &Path) -> PathBuf {
    crate::rotate::run_dir_in(data).join("password-reset.json")
}

struct Pending {
    /// Names the challenge. Handed to the browser, so not a secret: it is here
    /// so an answer to a code that has since been replaced is refused as stale
    /// rather than counted as a wrong guess against the new one.
    id: String,
    code: String,
    expires_at: i64,
    attempts: u32,
}

/// The one live challenge, if there is one.
#[derive(Default)]
pub struct Recovery {
    pending: Mutex<Option<Pending>>,
    /// When a code was last minted, for [`MINT_INTERVAL_MS`].
    last_mint: Mutex<i64>,
}

/// What the browser is told when a reset starts. Never the code.
pub struct Started {
    pub id: String,
    pub expires_at: i64,
    /// False when this handed back a code that was already live — the terminal
    /// is not printing a new one, so the frontend says "the code already on
    /// your phone" rather than "a new code".
    pub fresh: bool,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Verdict {
    /// The code matched. The challenge is spent either way.
    Ok,
    Wrong {
        attempts_left: u32,
    },
    /// Expired, already spent, burnt through, or answering a challenge that is
    /// no longer the live one. All of them mean: start again.
    Gone,
}

impl Recovery {
    /// Begin a reset, or hand back the one already running.
    ///
    /// `Err(seconds)` when a new code may not be minted yet.
    pub async fn start(&self, data: &Path, asked_by: &str) -> Result<Started, i64> {
        let now = now_ms();

        // A live code is the answer to "start a reset" — the operator asking
        // twice is far more likely than an operator who needs two codes, and
        // re-minting here would invalidate the code they are already walking
        // across the room to read.
        if let Some(live) = self.pending.lock().as_ref().filter(|p| p.expires_at > now) {
            return Ok(Started {
                id: live.id.clone(),
                expires_at: live.expires_at,
                fresh: false,
            });
        }

        {
            let mut last = self.last_mint.lock();
            let wait = (*last + MINT_INTERVAL_MS - now) / 1000;
            if *last > 0 && wait > 0 {
                return Err(wait.max(1));
            }
            *last = now;
        }

        let id = random_hex(16);
        let code = new_code();
        let expires_at = now + CODE_TTL_MS;
        *self.pending.lock() = Some(Pending {
            id: id.clone(),
            code: code.clone(),
            expires_at,
            attempts: 0,
        });

        // Straight to stderr rather than through the logger: this has to show
        // up whatever `logging.level` says, and it must not be filed away in
        // `relay.log`, which the dashboard will hand to anyone who is signed in.
        eprint!("{}", notice(&code, expires_at, asked_by));
        let _ = write_atomic(
            &code_file_in(data),
            &serde_json::to_string_pretty(&json!({
                "code": code,
                "expiresAt": expires_at,
                "requestedAt": now,
                "askedBy": asked_by,
            }))
            .unwrap_or_default(),
        )
        .await;

        Ok(Started {
            id,
            expires_at,
            fresh: true,
        })
    }

    /// Answer a challenge. The code is spent whatever happens next.
    pub async fn check(&self, data: &Path, id: &str, code: &str) -> Verdict {
        let (verdict, cleared) = self.verify(id, code);
        if cleared {
            let _ = tokio::fs::remove_file(code_file_in(data)).await;
        }
        verdict
    }

    /// The verdict, and whether the live challenge was consumed reaching it.
    fn verify(&self, id: &str, code: &str) -> (Verdict, bool) {
        let now = now_ms();
        let mut slot = self.pending.lock();

        let Some(pending) = slot.as_mut() else {
            return (Verdict::Gone, false);
        };
        if pending.expires_at <= now {
            *slot = None;
            return (Verdict::Gone, true);
        }
        // Answering a challenge that has been replaced is not a wrong guess:
        // counting it would let a stale tab burn the code the operator is
        // currently reading off their phone.
        if !safe_equal(id, &pending.id) {
            return (Verdict::Gone, false);
        }

        pending.attempts += 1;
        if safe_equal(code, &pending.code) {
            *slot = None;
            return (Verdict::Ok, true);
        }
        let left = MAX_ATTEMPTS.saturating_sub(pending.attempts);
        if left == 0 {
            *slot = None;
            return (Verdict::Gone, true);
        }
        (
            Verdict::Wrong {
                attempts_left: left,
            },
            false,
        )
    }

    /// Drop whatever is pending — after a password change by any other route,
    /// a code still sitting on the phone is a second way in.
    pub async fn forget(&self, data: &Path) {
        *self.pending.lock() = None;
        forget_pending(data).await;
    }
}

/// Throw away a code left behind on disk.
///
/// Called at startup, because the challenge itself only ever lived in this
/// process's memory: after a restart — a crash, the keeper, the hourly
/// rotation — the file would otherwise have `chtting-relay reset-code` printing
/// a code that nothing will accept, which is a worse answer than "nothing
/// pending".
pub async fn forget_pending(data: &Path) {
    let _ = tokio::fs::remove_file(code_file_in(data)).await;
}

/// A uniform six-digit code, zero-padded so every code is the same length.
///
/// `random_range` over the whole space rather than six digits drawn one at a
/// time: the same distribution, and no chance of a modulo bias creeping in
/// later.
fn new_code() -> String {
    let ceiling = 10u32.pow(CODE_DIGITS);
    let n = rand::rng().random_range(0..ceiling);
    format!("{n:0width$}", width = CODE_DIGITS as usize)
}

/// What the phone shows. Rendered the same way whether it is going to stderr
/// or coming back out of `chtting-relay reset-code`.
pub fn notice(code: &str, expires_at: i64, asked_by: &str) -> String {
    let minutes = ((expires_at - now_ms()).max(0) + 59_000) / 60_000;
    let at = chrono::DateTime::from_timestamp_millis(expires_at)
        .map(|t| t.format("%H:%M:%SZ").to_string())
        .unwrap_or_default();
    let spaced: String = code
        .chars()
        .map(|c| c.to_string())
        .collect::<Vec<_>>()
        .join(" ");

    format!(
        "\n\
         ┌──────────────────────────────────────────────┐\n\
         │  chtting-relay — dashboard password reset    │\n\
         ├──────────────────────────────────────────────┤\n\
         │                                              │\n\
         │      code:   {spaced:<32}│\n\
         │                                              │\n\
         └──────────────────────────────────────────────┘\n\
         Type it into the dashboard's \"Forgot password\" screen.\n\
         Good for {minutes} more minute(s) — until {at}.\n\
         Asked for from {asked_by}.\n\
         \n\
         Did not ask for this? Ignore it. A reset needs this code,\n\
         the code is only ever shown here, and it expires by itself.\n\
         \n"
    )
}

/// The pending code, rendered, for the terminal to print. `None` when nothing
/// is waiting or what was waiting has expired.
///
/// Read from the file rather than from memory, because the process that answers
/// this is the operator's `chtting-relay reset-code` and not the relay.
pub async fn pending_notice(data: &Path) -> Option<String> {
    let path = code_file_in(data);
    let raw = tokio::fs::read_to_string(&path).await.ok()?;
    let saved: serde_json::Value = serde_json::from_str(&raw).ok()?;
    let code = saved.get("code")?.as_str()?.to_string();
    let expires_at = saved.get("expiresAt")?.as_i64()?;
    if expires_at <= now_ms() {
        // Nothing here is a secret any more, and leaving it invites somebody to
        // type a code that cannot work and conclude the reset is broken.
        let _ = tokio::fs::remove_file(&path).await;
        return None;
    }
    let asked_by = saved
        .get("askedBy")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown");
    Some(notice(&code, expires_at, asked_by))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp() -> tempfile::TempDir {
        tempfile::tempdir().expect("temp dir")
    }

    #[test]
    fn a_code_is_always_six_digits() {
        for _ in 0..500 {
            let code = new_code();
            assert_eq!(code.chars().count(), CODE_DIGITS as usize, "{code}");
            assert!(code.chars().all(|c| c.is_ascii_digit()), "{code}");
        }
    }

    /// The whole point: the code never leaves the device, so what the browser
    /// is handed has to be enough to answer with and nothing more.
    #[tokio::test]
    async fn starting_a_reset_never_hands_back_the_code() {
        let dir = temp();
        let recovery = Recovery::default();
        let started = recovery.start(dir.path(), "127.0.0.1").await.unwrap();

        let saved: serde_json::Value = serde_json::from_str(
            &tokio::fs::read_to_string(code_file_in(dir.path()))
                .await
                .unwrap(),
        )
        .unwrap();
        let code = saved["code"].as_str().unwrap();
        assert_ne!(started.id, code);
        assert!(!started.id.contains(code));
    }

    #[tokio::test]
    async fn the_code_off_the_phone_is_the_one_that_works() {
        let dir = temp();
        let recovery = Recovery::default();
        let started = recovery.start(dir.path(), "127.0.0.1").await.unwrap();
        assert!(started.fresh);

        let code = pending_code(dir.path()).await;
        assert_eq!(
            recovery
                .check(dir.path(), &started.id, &wrong_code(&code))
                .await,
            Verdict::Wrong { attempts_left: 4 },
            "a wrong code is refused"
        );
        assert_eq!(
            recovery.check(dir.path(), &started.id, &code).await,
            Verdict::Ok
        );

        // Spent: the same code does not work twice, and the file it was read
        // from is gone.
        assert_eq!(
            recovery.check(dir.path(), &started.id, &code).await,
            Verdict::Gone
        );
        assert!(!code_file_in(dir.path()).exists());
    }

    /// Five guesses out of a million, not five per second forever.
    #[tokio::test]
    async fn wrong_codes_burn_the_challenge() {
        let dir = temp();
        let recovery = Recovery::default();
        let started = recovery.start(dir.path(), "10.0.0.9").await.unwrap();
        let code = pending_code(dir.path()).await;

        for expected in (0..MAX_ATTEMPTS).rev() {
            let verdict = recovery
                .check(dir.path(), &started.id, &wrong_code(&code))
                .await;
            if expected == 0 {
                assert_eq!(verdict, Verdict::Gone, "the last wrong guess kills it");
            } else {
                assert_eq!(
                    verdict,
                    Verdict::Wrong {
                        attempts_left: expected
                    }
                );
            }
        }

        // And the real code is worthless now, which is the point: guessing does
        // not merely fail, it costs the attacker the challenge.
        assert_eq!(
            recovery.check(dir.path(), &started.id, &code).await,
            Verdict::Gone
        );
        assert!(!code_file_in(dir.path()).exists());
    }

    /// Asking twice must not reprint — the operator is already reading the
    /// first code, and a stranger must not be able to scroll the terminal.
    #[tokio::test]
    async fn a_second_start_hands_back_the_live_code() {
        let dir = temp();
        let recovery = Recovery::default();
        let first = recovery.start(dir.path(), "127.0.0.1").await.unwrap();
        let code = pending_code(dir.path()).await;

        let again = recovery.start(dir.path(), "127.0.0.1").await.unwrap();
        assert_eq!(again.id, first.id);
        assert!(!again.fresh);
        assert_eq!(
            pending_code(dir.path()).await,
            code,
            "the code did not change"
        );
    }

    /// Once it has been burnt, a new one is not free either.
    #[tokio::test]
    async fn a_new_code_is_rate_limited() {
        let dir = temp();
        let recovery = Recovery::default();
        let started = recovery.start(dir.path(), "127.0.0.1").await.unwrap();
        let code = pending_code(dir.path()).await;
        recovery.check(dir.path(), &started.id, &code).await;

        match recovery.start(dir.path(), "127.0.0.1").await {
            Err(wait) => assert!((1..=MINT_INTERVAL_MS / 1000).contains(&wait)),
            Ok(_) => panic!("a fresh code was minted seconds after the last one"),
        }
    }

    /// A tab left open on an old reset must not spend the attempts of the code
    /// the operator is holding right now.
    #[tokio::test]
    async fn a_stale_challenge_id_does_not_cost_an_attempt() {
        let dir = temp();
        let recovery = Recovery::default();
        let started = recovery.start(dir.path(), "127.0.0.1").await.unwrap();
        let code = pending_code(dir.path()).await;

        for _ in 0..20 {
            assert_eq!(
                recovery
                    .check(dir.path(), "not-the-live-one", "123456")
                    .await,
                Verdict::Gone
            );
        }
        assert_eq!(
            recovery.check(dir.path(), &started.id, &code).await,
            Verdict::Ok
        );
    }

    #[tokio::test]
    async fn an_expired_code_is_not_printed_and_not_accepted() {
        let dir = temp();
        let path = code_file_in(dir.path());
        write_atomic(
            &path,
            &json!({ "code": "123456", "expiresAt": now_ms() - 1, "askedBy": "127.0.0.1" })
                .to_string(),
        )
        .await
        .unwrap();

        assert!(pending_notice(dir.path()).await.is_none());
        assert!(
            !path.exists(),
            "a dead code is cleaned up, not left to confuse"
        );
    }

    #[tokio::test]
    async fn the_notice_shows_the_code_and_says_where_it_came_from() {
        let dir = temp();
        let recovery = Recovery::default();
        recovery.start(dir.path(), "198.51.100.7").await.unwrap();
        let code = pending_code(dir.path()).await;

        let printed = pending_notice(dir.path()).await.expect("a live notice");
        let spaced: String = code.chars().map(String::from).collect::<Vec<_>>().join(" ");
        assert!(printed.contains(&spaced), "{printed}");
        assert!(printed.contains("198.51.100.7"), "{printed}");
    }

    /// The file holds a secret, in a directory shared with the database and the
    /// logs. It is written the way `config.json` is: never readable by anything
    /// but this user, not even for the moment between create and chmod.
    #[tokio::test]
    async fn the_code_file_is_not_world_readable() {
        use std::os::unix::fs::PermissionsExt;
        let dir = temp();
        let recovery = Recovery::default();
        recovery.start(dir.path(), "127.0.0.1").await.unwrap();

        let mode = std::fs::metadata(code_file_in(dir.path()))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600, "mode was {mode:o}");
    }

    /// A code that is certainly not this one.
    fn wrong_code(code: &str) -> String {
        if code == "111111" {
            "222222".into()
        } else {
            "111111".into()
        }
    }

    async fn pending_code(data: &Path) -> String {
        let raw = tokio::fs::read_to_string(code_file_in(data)).await.unwrap();
        serde_json::from_str::<serde_json::Value>(&raw).unwrap()["code"]
            .as_str()
            .unwrap()
            .to_string()
    }
}
