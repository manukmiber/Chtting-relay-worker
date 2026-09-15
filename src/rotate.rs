//! Replacing the running relay with a fresh copy of itself, on a clock.
//!
//! Android's low-memory killer scores processes partly on how long they have
//! been resident. A relay that stays up for days climbs that list until the
//! phone decides it is the thing to kill — usually overnight, and usually
//! without anyone noticing until the morning. Retiring on purpose, every hour,
//! keeps the process young enough that it never reaches the top.
//!
//! The handover is the whole problem. Doing it by exiting and starting again
//! drops every request in flight and refuses connections for as long as the new
//! process takes to bind. So instead both processes are alive at once:
//!
//! ```text
//!   old │████████████ serving ███████████│ draining │ gone
//!   new                  │ binds │███████████ serving ██████████ …
//!                        └ SO_REUSEPORT: the kernel gives new connections
//!                          to whichever socket is open
//! ```
//!
//! 1. The old instance spawns the new one and keeps serving.
//! 2. The new one binds the same ports — `SO_REUSEPORT` makes that legal — and
//!    writes a readiness marker.
//! 3. The old one sees the marker, stops accepting, and finishes the requests
//!    it already has. New connections now go to the new instance only.
//! 4. The old one releases the tunnel, waits for its last request, and exits.
//!
//! Nothing is dropped and no connection is refused, because at every moment at
//! least one process is listening.
//!
//! The safety property that matters: **the old instance never stops accepting
//! until it has seen the new one say it is ready.** A spawn that fails, a
//! binary that will not start, a marker that never appears — all of them leave
//! the old instance running exactly as it was. A missed rotation is a
//! non-event; a rotation that takes the relay down is not.
//!
//! The second safety property, which is newer: **the overlap ends.** A handover
//! is the one moment two processes may serve one port, and it is bounded at
//! both ends. The successor proves it is a successor with a token its
//! predecessor minted (see [`handover`]) rather than with an environment
//! variable anyone could be carrying, and it is not the relay until it holds
//! the port lease in [`crate::lock`] — which it can only take once the
//! predecessor is gone. A predecessor that will not go is moved along. Two
//! instances for a few seconds during a rotation is the design; two instances
//! afterwards is the bug this is written against.

use anyhow::{bail, Result};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use crate::state::AppState;

/// Set in the child so it knows it was spawned by a rotation rather than by a
/// person, and carries the generation number for the log line.
///
/// On its own it proves nothing. An environment variable is inherited by every
/// descendant of whoever set it, exported by a shell profile that once needed
/// it, and left behind in a `tmux` pane from last week — and this one used to
/// be enough to skip the duplicate check entirely. [`HANDOVER_ENV`] is what a
/// real successor also carries.
pub const GENERATION_ENV: &str = "CHTTING_GENERATION";

/// The one-shot token a predecessor mints for the successor it is spawning.
///
/// Its match is written to a file in the run directory that the successor
/// consumes and deletes, so the token is good exactly once, only for the
/// process it was minted for, and only while the predecessor that minted it is
/// still alive. A stale `CHTTING_GENERATION` with no matching token is not a
/// handover, and the process carrying it starts as what it actually is: a
/// hand-started relay that has to take the port lease like any other.
pub const HANDOVER_ENV: &str = "CHTTING_HANDOVER";

/// How long the new instance is given to bind its ports and say so.
const READY_TIMEOUT: Duration = Duration::from_secs(45);

/// Where the two processes leave notes for each other.
pub fn run_dir_in(data: &Path) -> PathBuf {
    data.join("run")
}

fn run_dir(state: &AppState) -> PathBuf {
    run_dir_in(&state.paths.data)
}

/// The marker a starting instance writes once it is actually listening.
fn ready_marker(state: &AppState, generation: u64) -> PathBuf {
    run_dir(state).join(format!("ready-{generation}"))
}

/// The lock the instance holding the tunnel keeps. Only one cloudflared should
/// be running per relay, so the successor waits for this to go before starting
/// its own.
fn tunnel_lock(state: &AppState) -> PathBuf {
    run_dir(state).join("tunnel.lock")
}

/// The pidfile naming the instance that owns the listening sockets.
///
/// A note, not a lock. What decides who may serve is the kernel-held port lease
/// in [`crate::lock`]; this file exists so a refusal can name the pid to stop
/// and so `--replace` has somebody to ask. It is allowed to be missing, stale
/// or wrong without anything going wrong: every path that reads it treats it as
/// a hint and falls back on the lease itself.
pub fn serve_lock_in(data: &Path) -> PathBuf {
    run_dir_in(data).join("serving.pid")
}

fn serve_lock(state: &AppState) -> PathBuf {
    serve_lock_in(&state.paths.data)
}

/// The handover token a predecessor leaves for one named successor.
fn handover_note(data: &Path, generation: u64) -> PathBuf {
    run_dir_in(data).join(format!("handover-{generation}"))
}

/// This process's generation, from the environment. 0 for one started by hand.
pub fn generation() -> u64 {
    std::env::var(GENERATION_ENV)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0)
}

/// Say we are up, so whoever spawned us can retire.
pub async fn announce_ready(state: &Arc<AppState>) {
    let generation = generation();
    if generation == 0 {
        return; // nobody is waiting
    }
    let marker = ready_marker(state, generation);
    if let Some(dir) = marker.parent() {
        let _ = tokio::fs::create_dir_all(dir).await;
    }
    if let Err(err) = tokio::fs::write(&marker, std::process::id().to_string()).await {
        // Not fatal: the predecessor times out and keeps running, which is the
        // safe outcome. Two instances is survivable; none is not.
        state
            .logger
            .warn(format!("could not write the readiness marker: {err}"));
    }
}

/// What a validated handover looks like from the successor's side.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Handover {
    pub generation: u64,
    /// The instance this one is replacing. It is still serving right now, and
    /// it is the pid to move along if it overstays.
    pub predecessor: u32,
}

/// Am I actually a rotation successor?
///
/// Three things have to hold, and any one of them missing means no: the
/// environment carries a generation and a token, a note in the run directory
/// matches that token, and the predecessor named in it is still alive. The note
/// is deleted as it is read, so the token is spent — a process restarted later
/// with the same environment is not a successor a second time.
///
/// Answering `None` is never a licence to serve. It only means this process
/// takes the port lease the way a hand-started one does: by taking it, or by
/// not starting.
pub async fn handover(data: &Path) -> Option<Handover> {
    let generation: u64 = std::env::var(GENERATION_ENV).ok()?.trim().parse().ok()?;
    if generation == 0 {
        return None;
    }
    let presented = std::env::var(HANDOVER_ENV).ok()?;
    let presented = presented.trim();
    if presented.is_empty() {
        return None;
    }

    let note = handover_note(data, generation);
    let written = tokio::fs::read_to_string(&note).await.ok()?;
    // Spent on sight, whether or not it turns out to match: a token that has
    // been looked at is not offered to anybody else.
    let _ = tokio::fs::remove_file(&note).await;

    let mut parts = written.split_whitespace();
    let token = parts.next()?;
    let predecessor: u32 = parts.next()?.parse().ok()?;
    if !crate::util::safe_equal(token, presented) {
        return None;
    }
    // A predecessor that is already gone is not handing anything over, and the
    // lease is free for the taking anyway.
    crate::lock::is_alive(predecessor).then_some(Handover {
        generation,
        predecessor,
    })
}

/// Write down who is serving, for the next process that has to ask.
///
/// Best-effort by design. The port lease is what actually stops a second
/// instance; this only makes the refusal able to say a pid instead of "somebody".
pub async fn record_serving(data: &Path, pid: u32) {
    let lock = serve_lock_in(data);
    if let Some(dir) = lock.parent() {
        let _ = tokio::fs::create_dir_all(dir).await;
    }
    let _ = tokio::fs::write(&lock, pid.to_string()).await;
}

/// Who the pidfile says is serving, if it is somebody that still exists.
///
/// A hint for an error message and a target for `--replace`. A wrong answer
/// here costs a less helpful message, never a second relay.
pub async fn recorded_owner(data: &Path) -> Option<u32> {
    let contents = tokio::fs::read_to_string(serve_lock_in(data)).await.ok()?;
    let pid: u32 = contents.trim().parse().ok()?;
    (pid != std::process::id() && crate::lock::is_alive(pid)).then_some(pid)
}

/// Give the serve lock up, but only if it is still ours: a successor that
/// already took over must not have its claim deleted by the instance it
/// replaced.
pub async fn release_serving(state: &Arc<AppState>) {
    let lock = serve_lock(state);
    if let Ok(owner) = tokio::fs::read_to_string(&lock).await {
        if owner.trim() == std::process::id().to_string() {
            let _ = tokio::fs::remove_file(&lock).await;
        }
    }
}

/// Take the tunnel lock, waiting a bounded time for a predecessor to drop it.
///
/// Returns once the lock is ours. The wait is why a rotation does not leave two
/// cloudflared processes fighting over one quick-tunnel URL.
pub async fn claim_tunnel(state: &Arc<AppState>) {
    let lock = tunnel_lock(state);
    if let Some(dir) = lock.parent() {
        let _ = tokio::fs::create_dir_all(dir).await;
    }

    let deadline = tokio::time::Instant::now() + Duration::from_secs(90);
    while held_by_a_live_process(&lock).await {
        if tokio::time::Instant::now() >= deadline {
            state
                .logger
                .warn("the previous instance is still holding the tunnel; taking it over anyway");
            break;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    let _ = tokio::fs::write(&lock, std::process::id().to_string()).await;
}

/// Give the tunnel lock up, so a successor can start its own cloudflared.
pub async fn release_tunnel(state: &Arc<AppState>) {
    let lock = tunnel_lock(state);
    // Only remove it if it is still ours; a successor that took over should not
    // have its lock deleted by the instance it replaced.
    if let Ok(owner) = tokio::fs::read_to_string(&lock).await {
        if owner.trim() == std::process::id().to_string() {
            let _ = tokio::fs::remove_file(&lock).await;
        }
    }
}

/// Is a lock file still owned by something that exists?
///
/// A phone that is killed mid-rotation leaves the file behind, and a successor
/// that waited on a dead process's lock would wait the full ninety seconds for
/// nothing.
async fn held_by_a_live_process(lock: &Path) -> bool {
    live_owner(lock).await.is_some()
}

/// The pid in a lock file, if it is not ours and still exists.
async fn live_owner(lock: &Path) -> Option<u32> {
    let contents = tokio::fs::read_to_string(lock).await.ok()?;
    let pid: u32 = contents.trim().parse().ok()?;
    if pid == std::process::id() {
        return None;
    }
    crate::lock::is_alive(pid).then_some(pid)
}

/// Delete markers left by instances that are long gone.
pub async fn sweep(state: &Arc<AppState>) {
    let dir = run_dir(state);
    let Ok(mut entries) = tokio::fs::read_dir(&dir).await else {
        return;
    };
    let generation = generation();
    while let Ok(Some(entry)) = entries.next_entry().await {
        let name = entry.file_name().to_string_lossy().into_owned();
        // A note for a handover that never happened is as finished as a
        // readiness marker for an instance that never came up.
        let Some(rest) = name
            .strip_prefix("ready-")
            .or_else(|| name.strip_prefix("handover-"))
        else {
            continue;
        };
        // Ours and anything newer stays; older markers are finished business.
        if rest.parse::<u64>().is_ok_and(|g| g < generation) || generation == 0 {
            let _ = tokio::fs::remove_file(entry.path()).await;
        }
    }
}

/// Start the successor and wait for it to say it is listening.
///
/// Returns an error without touching this process if anything goes wrong, which
/// is the point: a failed rotation must cost nothing but a log line.
pub async fn spawn_successor(state: &Arc<AppState>) -> Result<u32> {
    let generation = generation() + 1;
    let marker = ready_marker(state, generation);
    let _ = tokio::fs::remove_file(&marker).await;
    if let Some(dir) = marker.parent() {
        tokio::fs::create_dir_all(dir).await?;
    }

    // The token that tells the successor apart from a process that merely
    // inherited our environment. Written where only a successor started from
    // this data directory will look, alongside the pid it is replacing, and
    // spent the first time it is read.
    let token = crate::util::new_id("ho");
    tokio::fs::write(
        handover_note(&state.paths.data, generation),
        format!("{token} {}", std::process::id()),
    )
    .await?;

    let exe = own_binary()?;
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut command = tokio::process::Command::new(&exe);
    command
        .args(&args)
        .env(GENERATION_ENV, generation.to_string())
        .env(HANDOVER_ENV, &token)
        // Detached: it has to outlive us, and it must not die with our session.
        .stdin(std::process::Stdio::null())
        .kill_on_drop(false);

    let child = command.spawn()?;
    let pid = child.id().unwrap_or(0);
    // Let it go; from here it is a peer, not a child of ours to wait on.
    std::mem::forget(child);

    let deadline = tokio::time::Instant::now() + READY_TIMEOUT;
    while tokio::time::Instant::now() < deadline {
        if tokio::fs::try_exists(&marker).await.unwrap_or(false) {
            return Ok(pid);
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    bail!("generation {generation} did not come up within {READY_TIMEOUT:?}");
}

/// The binary to start the successor from.
///
/// `current_exe` is the right answer almost always, but not after the binary
/// has been replaced underneath a running process — an upgrade, or a `cargo
/// build` during development. Linux then reports the old path with a
/// " (deleted)" suffix, and spawning it fails with a bare "no such file or
/// directory" that says nothing about why.
fn own_binary() -> Result<PathBuf> {
    let exe = std::env::current_exe()?;
    if exe.is_file() {
        return Ok(exe);
    }
    // Strip the marker Linux appends and see whether a new binary has taken the
    // old one's place — which is exactly the case after an upgrade, and the case
    // where rotating is most worth doing.
    let text = exe.to_string_lossy();
    if let Some(stripped) = text.strip_suffix(" (deleted)") {
        let replaced = PathBuf::from(stripped);
        if replaced.is_file() {
            return Ok(replaced);
        }
    }
    bail!(
        "this relay's own binary is gone from {}; nothing to start a successor from",
        exe.display()
    );
}

/// Wait for the requests already in flight to finish.
///
/// Bounded, because one client that never hangs up must not keep a retired
/// instance resident forever — which would defeat the whole exercise.
pub async fn drain(state: &Arc<AppState>, timeout: Duration) -> usize {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let in_flight = state.gate.snapshot().in_flight;
        if in_flight == 0 {
            return 0;
        }
        if tokio::time::Instant::now() >= deadline {
            return in_flight;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The process environment is one shared thing, and `cargo test` runs these
    /// on several threads at once. Every test that sets a variable takes this
    /// first, so they take turns instead of reading each other's.
    static ENVIRONMENT: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Hold the environment, and leave it as clean as it was found.
    struct EnvGuard(#[allow(dead_code)] std::sync::MutexGuard<'static, ()>);

    impl EnvGuard {
        fn take() -> EnvGuard {
            let guard = ENVIRONMENT.lock().unwrap_or_else(|e| e.into_inner());
            std::env::remove_var(GENERATION_ENV);
            std::env::remove_var(HANDOVER_ENV);
            EnvGuard(guard)
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            std::env::remove_var(GENERATION_ENV);
            std::env::remove_var(HANDOVER_ENV);
        }
    }

    /// A scratch data directory.
    async fn scratch() -> PathBuf {
        let dir = std::env::temp_dir().join(crate::util::new_id("rot"));
        tokio::fs::create_dir_all(run_dir_in(&dir)).await.unwrap();
        dir
    }

    /// The bypass this closes: `CHTTING_GENERATION` is inherited by every
    /// descendant of whoever set it, and on its own it used to be enough to
    /// skip the duplicate check entirely. A generation without the token its
    /// predecessor minted is somebody carrying an old environment, and it
    /// starts as what it is.
    #[tokio::test]
    async fn a_generation_number_alone_does_not_make_a_successor() {
        let _env = EnvGuard::take();
        let dir = scratch().await;
        std::env::set_var(GENERATION_ENV, "7");
        assert_eq!(handover(&dir).await, None, "no token at all");

        std::env::set_var(HANDOVER_ENV, "ho_invented");
        assert_eq!(handover(&dir).await, None, "a token nobody minted");

        tokio::fs::write(
            handover_note(&dir, 7),
            format!("ho_real {}", std::process::id()),
        )
        .await
        .unwrap();
        assert_eq!(handover(&dir).await, None, "a token that does not match");
    }

    #[tokio::test]
    async fn a_minted_token_is_good_once_and_only_while_its_minter_lives() {
        let _env = EnvGuard::take();
        let dir = scratch().await;
        std::env::set_var(GENERATION_ENV, "3");
        std::env::set_var(HANDOVER_ENV, "ho_abc");

        tokio::fs::write(
            handover_note(&dir, 3),
            format!("ho_abc {}", std::process::id()),
        )
        .await
        .unwrap();
        assert_eq!(
            handover(&dir).await,
            Some(Handover {
                generation: 3,
                predecessor: std::process::id(),
            })
        );
        // Spent: a process restarted later with the same environment is not a
        // successor a second time.
        assert_eq!(
            handover(&dir).await,
            None,
            "the token was still good after being used"
        );

        // And a predecessor that is already gone is handing nothing over.
        tokio::fs::write(handover_note(&dir, 3), "ho_abc 4194305")
            .await
            .unwrap();
        assert_eq!(handover(&dir).await, None);
    }

    #[tokio::test]
    async fn the_pidfile_is_a_hint_and_says_so_by_never_naming_the_dead() {
        let dir = scratch().await;
        assert_eq!(recorded_owner(&dir).await, None, "nothing written yet");

        record_serving(&dir, 4_194_305).await;
        assert_eq!(recorded_owner(&dir).await, None, "a pid that cannot exist");

        record_serving(&dir, std::process::id()).await;
        assert_eq!(
            recorded_owner(&dir).await,
            None,
            "our own pid is not somebody else to ask to stop"
        );

        // pid 1 is alive on every Linux box, and is somebody else.
        record_serving(&dir, 1).await;
        assert_eq!(recorded_owner(&dir).await, Some(1));
    }

    #[tokio::test]
    async fn a_lock_left_by_a_dead_process_is_not_waited_on() {
        let dir = std::env::temp_dir().join(crate::util::new_id("rot"));
        tokio::fs::create_dir_all(&dir).await.unwrap();
        let lock = dir.join("tunnel.lock");

        // A pid that cannot exist: Linux caps at 2^22 and this is past it.
        tokio::fs::write(&lock, "4194305").await.unwrap();
        assert!(!held_by_a_live_process(&lock).await);

        // Our own pid does not count as somebody else holding it.
        tokio::fs::write(&lock, std::process::id().to_string())
            .await
            .unwrap();
        assert!(!held_by_a_live_process(&lock).await);

        // Nothing there at all.
        tokio::fs::remove_file(&lock).await.unwrap();
        assert!(!held_by_a_live_process(&lock).await);

        // Garbage rather than a pid.
        tokio::fs::write(&lock, "not a pid").await.unwrap();
        assert!(!held_by_a_live_process(&lock).await);
    }

    #[tokio::test]
    async fn a_serve_lock_names_its_owner_only_while_that_owner_lives() {
        let dir = std::env::temp_dir().join(crate::util::new_id("rot"));
        tokio::fs::create_dir_all(&dir).await.unwrap();
        let lock = dir.join("serving.pid");

        // A pid that cannot exist: Linux caps at 2^22 and this is past it.
        tokio::fs::write(&lock, "4194305").await.unwrap();
        assert_eq!(live_owner(&lock).await, None, "a dead claim holds nothing");

        tokio::fs::write(&lock, std::process::id().to_string())
            .await
            .unwrap();
        assert_eq!(
            live_owner(&lock).await,
            None,
            "our own claim is not somebody else holding the port"
        );

        tokio::fs::remove_file(&lock).await.unwrap();
        assert_eq!(live_owner(&lock).await, None, "nor is no file at all");
    }

    #[test]
    fn the_successor_is_started_from_a_binary_that_exists() {
        // On a normal run this is just current_exe; the point of the test is
        // that whatever comes back is something that can actually be spawned,
        // because the alternative is a rotation that fails with "no such file".
        let exe = own_binary().expect("a running process has a binary");
        assert!(exe.is_file(), "{exe:?}");
        assert!(!exe.to_string_lossy().contains("(deleted)"));
    }

    #[test]
    fn a_relay_started_by_hand_is_generation_zero() {
        let _env = EnvGuard::take();
        // Nobody is waiting on a marker from an instance a person started, and
        // generation() is what announce_ready checks before writing one.
        std::env::remove_var(GENERATION_ENV);
        assert_eq!(generation(), 0);
        std::env::set_var(GENERATION_ENV, "7");
        assert_eq!(generation(), 7);
        std::env::set_var(GENERATION_ENV, "not a number");
        assert_eq!(generation(), 0);
        std::env::remove_var(GENERATION_ENV);
    }
}
