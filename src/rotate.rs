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

use anyhow::{bail, Result};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use crate::state::AppState;

/// Set in the child so it knows it was spawned by a rotation rather than by a
/// person, and carries the generation number for the log line.
pub const GENERATION_ENV: &str = "CHTTING_GENERATION";

/// How long the new instance is given to bind its ports and say so.
const READY_TIMEOUT: Duration = Duration::from_secs(45);

/// Where the two processes leave notes for each other.
fn run_dir(state: &AppState) -> PathBuf {
    state.paths.data.join("run")
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
fn serve_lock(state: &AppState) -> PathBuf {
    run_dir(state).join("serving.pid")
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

/// Refuse to become a second relay on ports that already have one.
///
/// `SO_REUSEPORT` is what makes the rotation in this module possible, and it is
/// also why nothing else catches this: the bind *succeeds*. Two instances then
/// listen on the same port and the kernel hands each new connection to one of
/// them at random, so the relay answers out of whichever config that process
/// happens to hold. A key minted in one of them is unknown to the other, and
/// the caller sees `invalid API key` on a fraction of requests exactly equal to
/// the stale instance's share of the sockets.
///
/// A rotation is the one case where two instances on one port is correct, and
/// it is told apart by its generation: a successor is spawned with one and
/// takes the lock over, because the predecessor is on its way out. Anything
/// started by hand is generation 0 and has to say so with `--replace`.
///
/// The lock is advisory and deliberately cheap to recover from: a pid that no
/// longer exists, or one that has been recycled by an unrelated process, does
/// not hold anything.
pub async fn claim_serving(state: &Arc<AppState>, replace: bool) -> Result<()> {
    let lock = serve_lock(state);
    if let Some(dir) = lock.parent() {
        let _ = tokio::fs::create_dir_all(dir).await;
    }

    if generation() == 0 && !replace {
        if let Some(pid) = live_owner(&lock).await.filter(|p| is_this_relay(*p)) {
            bail!(
                "this relay is already running as pid {pid}.\n\
                 Starting a second copy would not fail on the port — SO_REUSEPORT lets \
                 both bind it — it would split the traffic between them, and the older \
                 process would answer out of the config it started with. That is what \
                 makes a freshly minted key come back \"invalid API key\" on some \
                 requests and work on others.\n\
                 Stop pid {pid} first, or pass --replace to take the port over."
            );
        }
    }

    tokio::fs::write(&lock, std::process::id().to_string()).await?;
    Ok(())
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

/// Is this pid one of ours, or a number Linux has since handed to something
/// else? Refusing to start because an unrelated process inherited the pid
/// would be a worse failure than the duplicate this is guarding against.
fn is_this_relay(pid: u32) -> bool {
    let Ok(cmdline) = std::fs::read(format!("/proc/{pid}/cmdline")) else {
        return false;
    };
    // /proc separates argv with NULs; argv[0] is the path the binary was run
    // from, which may be absolute, relative, or a bare name.
    cmdline
        .split(|b| *b == 0)
        .next()
        .and_then(|argv0| std::str::from_utf8(argv0).ok())
        .is_some_and(|argv0| {
            std::path::Path::new(argv0)
                .file_name()
                .is_some_and(|n| n == env!("CARGO_PKG_NAME"))
        })
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
    // No kill(0) without libc; /proc answers the same question on Android.
    tokio::fs::try_exists(format!("/proc/{pid}"))
        .await
        .unwrap_or(false)
        .then_some(pid)
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
        let Some(rest) = name.strip_prefix("ready-") else {
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

    let exe = own_binary()?;
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut command = tokio::process::Command::new(&exe);
    command
        .args(&args)
        .env(GENERATION_ENV, generation.to_string())
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
    fn a_pid_that_belongs_to_something_else_does_not_block_a_start() {
        // pid 1 exists on every Linux box and is never this binary. Refusing to
        // start because an unrelated process inherited a recycled pid would be
        // a worse failure than the duplicate instance being guarded against.
        assert!(!is_this_relay(1));
        assert!(!is_this_relay(4_194_305), "and one that cannot exist");
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
