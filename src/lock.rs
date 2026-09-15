//! One relay per port, and the kernel is the one enforcing it.
//!
//! The old guard was a pidfile under the data directory: read it, see whether
//! that pid is alive, write ours. Three things were wrong with it, and all
//! three have been observed in the wild.
//!
//! * **It was keyed on the data directory, not on the port.** The contended
//!   resource is the TCP port. Two copies started with different `--home`
//!   values — the keeper passes one, a hand-run `start-termux.sh` resolves it
//!   from `current_dir` — write to two different pidfiles, see an empty lock
//!   each, and both bind the same port. Neither one is doing anything wrong by
//!   its own reckoning.
//! * **Read-then-write is not a claim.** Two processes reaching the check
//!   within the same moment both find no owner and both proceed. The window is
//!   small and a phone rebooting into a keeper, a boot hook and a widget at
//!   once is exactly the thing that hits it.
//! * **Every uncertainty resolved to "allow".** An unreadable `/proc`, a pid
//!   recycled onto something else, a binary invoked under another name — each
//!   made the guard decide there was no incumbent. A lock whose failure mode is
//!   to let a second copy through is not a lock.
//!
//! What replaces it is an abstract unix socket named after the port. Binding
//! one is a single atomic syscall that either succeeds or returns
//! `EADDRINUSE`, so there is no window to race through. The name lives in a
//! kernel-global namespace with no filesystem entry, so `--home`, `$TMPDIR`,
//! the working directory and the config file have no say in it — two processes
//! reaching for port 8787 reach for the same name whatever else differs. And
//! the name is released by the kernel when the holder's last file descriptor
//! closes, which happens on a clean exit, a panic, a `SIGKILL` and Android's
//! low-memory killer alike. There is no stale lease to recover from, ever.
//!
//! `SO_REUSEPORT` on the real listener stays exactly as it was: a rotation
//! still needs both processes listening for the length of the handover. The
//! lease is what says which of the two is the relay, and the successor takes it
//! only once its predecessor is gone — see [`crate::rotate`].

use std::io;
use std::time::Duration;

/// What a start exits with when another instance already owns the port.
///
/// Its own code, and not the `1` every other failure uses, because the keeper
/// has to tell the two apart: a relay that will not start because one is
/// already serving must be waited out, not restarted three seconds later
/// forever.
pub const EXIT_PORT_BUSY: i32 = 3;

/// How often a wait for the lease looks again.
const POLL: Duration = Duration::from_millis(200);

/// The exclusive right to serve on one port, held for as long as this value is
/// alive and released by the kernel the moment the process is not.
#[derive(Debug)]
pub struct PortLease {
    port: u16,
    _held: imp::Held,
}

impl PortLease {
    /// Take the lease, or report that somebody else has it.
    ///
    /// `Ok(None)` is the ordinary "taken" answer. `Err` means the attempt
    /// itself could not be made, which is a different thing and is never
    /// treated as permission to start.
    pub fn try_acquire(port: u16) -> io::Result<Option<PortLease>> {
        Ok(imp::try_hold(port)?.map(|held| PortLease { port, _held: held }))
    }

    /// Wait for the lease to come free, giving up after `patience`.
    ///
    /// Polled rather than blocked on, because the holder is another process and
    /// there is nothing to await: what frees the name is that process ending.
    pub async fn acquire_within(port: u16, patience: Duration) -> io::Result<Option<PortLease>> {
        let deadline = tokio::time::Instant::now() + patience;
        loop {
            if let Some(lease) = PortLease::try_acquire(port)? {
                return Ok(Some(lease));
            }
            if tokio::time::Instant::now() >= deadline {
                return Ok(None);
            }
            tokio::time::sleep(POLL).await;
        }
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    /// Start telling anyone who asks which process is holding this port.
    ///
    /// One detached thread doing a blocking accept, which is the whole of it:
    /// the answer is a pid and a newline, and the socket exists anyway. It
    /// costs nothing and it is the only thing that lets `--replace` work
    /// between two copies that disagree about where their data lives.
    fn start_answering(&self) {
        imp::answer_with_our_pid(&self._held);
    }
}

/// Is this port already being served?
///
/// A peek, for status and diagnostics. Do not start on the strength of it: by
/// the time the answer is read it is already history. [`PortLease::try_acquire`]
/// is the only thing that decides.
pub fn port_is_taken(port: u16) -> bool {
    matches!(PortLease::try_acquire(port), Ok(None))
}

/// Ask the lease itself who is holding it.
///
/// The lease is a socket, so the holder can simply answer: it listens on the
/// name it bound and tells anyone who connects what its pid is. That matters
/// because the alternative — a pidfile under the data directory — is exactly
/// what could not be trusted. Two relays started with different `--home`
/// values write to two different pidfiles, so an incumbent found that way is
/// often not found at all, and `--replace` had nobody to ask to stop. The
/// kernel's name is common ground between them however differently they were
/// started.
///
/// `None` means the holder did not answer in time, not that there is none.
pub fn serving_pid(port: u16) -> Option<u32> {
    imp::ask_who_holds(port, Duration::from_secs(2))
}

/* ----------------------------------------------------------- for a life -- */

/// The lease this process is serving under, once it has one.
static HELD: std::sync::OnceLock<PortLease> = std::sync::OnceLock::new();

/// Keep this lease for the rest of the process's life.
///
/// Parked in a `static` rather than carried around because that is exactly its
/// lifetime: a relay holds the port from the moment it is entitled to serve
/// until the process ends, and the kernel takes it back at that moment whether
/// the ending was tidy or not. Nothing can drop it early by mistake.
pub fn hold_for_life(lease: PortLease) {
    if HELD.set(lease).is_ok() {
        if let Some(lease) = HELD.get() {
            lease.start_answering();
        }
    }
}

/// Is this process the one entitled to serve?
///
/// False for a rotation successor between binding its ports and its
/// predecessor letting go — a real state, bounded, and the only time the
/// answer is allowed to be no while the relay is answering requests.
pub fn holding() -> bool {
    HELD.get().is_some()
}

/* ------------------------------------------------------------- eviction -- */

/// Does this pid exist?
///
/// `kill(pid, 0)` rather than a look in `/proc`, because it is the question
/// being asked and it answers on every Unix, including the Android builds where
/// `/proc` may not show another process at all.
pub fn is_alive(pid: u32) -> bool {
    pid > 0 && unsafe { libc::kill(pid as libc::pid_t, 0) } == 0
}

/// Ask a process to stop. `false` when there was nothing to ask.
pub fn ask_to_stop(pid: u32) -> bool {
    pid > 0 && unsafe { libc::kill(pid as libc::pid_t, libc::SIGTERM) } == 0
}

/// Insist.
pub fn insist(pid: u32) -> bool {
    pid > 0 && unsafe { libc::kill(pid as libc::pid_t, libc::SIGKILL) } == 0
}

/// Take the port from whoever has it: ask, wait, insist, wait.
///
/// The lease coming free is the only signal trusted here. A pid that stops
/// answering proves nothing on its own — it may have been the wrong pid, or one
/// of several — so what is waited for is the name being released, whoever was
/// holding it.
///
/// `Ok(None)` means the port is still not ours, which is a refusal to start and
/// never a reason to carry on anyway.
pub async fn take_over(
    port: u16,
    incumbent: Option<u32>,
    polite: Duration,
    firm: Duration,
) -> io::Result<Option<PortLease>> {
    // Whoever the caller had in mind, the lease's own answer is better: it
    // comes from the process actually holding the port rather than from a file
    // that may belong to a different install of the same relay.
    let incumbent = serving_pid(port).or(incumbent);
    if let Some(pid) = incumbent.filter(|p| is_alive(*p)) {
        ask_to_stop(pid);
    }
    if let Some(lease) = PortLease::acquire_within(port, polite).await? {
        return Ok(Some(lease));
    }
    if let Some(pid) = incumbent.filter(|p| is_alive(*p)) {
        insist(pid);
    }
    PortLease::acquire_within(port, firm).await
}

/* ---------------------------------------------------------------- linux -- */

/// The abstract socket, on the systems that have one.
///
/// Linux and Android only, which is every system this relay is built for. The
/// fallback below is there so the test suite and a developer's laptop behave
/// the same way, not because the relay is expected to run there.
#[cfg(any(target_os = "linux", target_os = "android"))]
mod imp {
    use std::io;
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

    /// The bound socket. Dropping it closes the descriptor, which is what
    /// releases the name.
    #[derive(Debug)]
    pub struct Held(std::os::unix::net::UnixListener);

    /// How deep a backlog of "who has the port?" callers to keep. They ask once
    /// and leave, so this only has to cover several arriving at the same moment.
    const BACKLOG: libc::c_int = 16;

    /// What the lease is called in the kernel's abstract namespace.
    ///
    /// The port and nothing else: two processes arguing over a port must arrive
    /// at the same string however differently they were started.
    fn lease_name(port: u16) -> String {
        format!("chtting-relay.serving.{port}")
    }

    /// The address of one port's lease, built by hand.
    ///
    /// By hand rather than through `std::os::linux::net::SocketAddrExt`,
    /// because that module is gated on the `linux` target and the build that
    /// matters most here is `android`. A `sockaddr_un` is the same shape on
    /// both.
    fn lease_addr(port: u16) -> io::Result<(libc::sockaddr_un, libc::socklen_t)> {
        let name = lease_name(port);
        let bytes = name.as_bytes();
        // SAFETY: `sockaddr_un` is plain old data; an all-zero one is the
        // documented starting point for filling it in.
        let mut addr: libc::sockaddr_un = unsafe { std::mem::zeroed() };
        addr.sun_family = libc::AF_UNIX as libc::sa_family_t;
        // sun_path[0] stays NUL. That leading NUL is the whole trick: it puts
        // the name in the abstract namespace, where there is no file to be left
        // behind by a process that died badly.
        if bytes.len() + 1 > addr.sun_path.len() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "the lease name does not fit in a unix socket path",
            ));
        }
        for (slot, byte) in addr.sun_path[1..].iter_mut().zip(bytes) {
            *slot = *byte as libc::c_char;
        }
        // Exactly the bytes that are meaningful: the family, the leading NUL,
        // and the name. A longer length would make the trailing zeroes part of
        // the name and stop two processes agreeing on it.
        let len = std::mem::size_of::<libc::sa_family_t>() + 1 + bytes.len();
        Ok((addr, len as libc::socklen_t))
    }

    /// A close-on-exec unix socket.
    ///
    /// Close-on-exec is not a detail: the dashboard's Restart re-executes this
    /// binary in place, and an inherited lease would make the new image find
    /// the port taken by itself and refuse to start.
    fn new_socket() -> io::Result<OwnedFd> {
        // SAFETY: the descriptor is taken into an `OwnedFd` immediately, so it
        // cannot leak on any path out of here.
        unsafe {
            let raw = libc::socket(libc::AF_UNIX, libc::SOCK_STREAM | libc::SOCK_CLOEXEC, 0);
            if raw < 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(OwnedFd::from_raw_fd(raw))
        }
    }

    pub fn try_hold(port: u16) -> io::Result<Option<Held>> {
        let (addr, len) = lease_addr(port)?;
        let owned = new_socket()?;

        // SAFETY: `addr` outlives both calls, `len` describes exactly the bytes
        // filled in above, and the descriptor is owned.
        unsafe {
            if libc::bind(
                owned.as_raw_fd(),
                std::ptr::addr_of!(addr).cast::<libc::sockaddr>(),
                len,
            ) != 0
            {
                let err = io::Error::last_os_error();
                return match err.kind() {
                    io::ErrorKind::AddrInUse => Ok(None),
                    _ => Err(err),
                };
            }
            // Listening is what lets the holder be asked who it is. The name is
            // already ours from the bind above; this only makes it answer.
            if libc::listen(owned.as_raw_fd(), BACKLOG) != 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(Some(Held(std::os::unix::net::UnixListener::from(owned))))
        }
    }

    /// Answer every caller with this process's pid, forever, on one thread.
    pub fn answer_with_our_pid(held: &Held) {
        let Ok(listener) = held.0.try_clone() else {
            return; // the lease still holds; only the courtesy is lost
        };
        let reply = format!("{}\n", std::process::id());
        std::thread::Builder::new()
            .name("port-lease".into())
            .spawn(move || {
                for stream in listener.incoming() {
                    let Ok(mut stream) = stream else { continue };
                    use std::io::Write;
                    let _ = stream.write_all(reply.as_bytes());
                    let _ = stream.flush();
                }
            })
            .ok();
    }

    /// Connect to the lease and read the pid on the other end.
    pub fn ask_who_holds(port: u16, patience: std::time::Duration) -> Option<u32> {
        use std::io::Read;
        use std::os::unix::net::UnixStream;

        let (addr, len) = lease_addr(port).ok()?;
        let owned = new_socket().ok()?;
        // SAFETY: as above — `addr` outlives the call and the fd is owned.
        let connected = unsafe {
            libc::connect(
                owned.as_raw_fd(),
                std::ptr::addr_of!(addr).cast::<libc::sockaddr>(),
                len,
            )
        };
        if connected != 0 {
            return None;
        }

        let stream = UnixStream::from(owned);
        // A holder that bound but has not started answering yet leaves the
        // connection open and silent; the timeout is what stops that becoming
        // a hang in whoever asked.
        stream.set_read_timeout(Some(patience)).ok()?;
        let mut said = String::new();
        stream.take(32).read_to_string(&mut said).ok()?;
        said.trim().parse().ok()
    }
}

/* ------------------------------------------------------------ elsewhere -- */

/// `flock` on a file named after the port, for systems without an abstract
/// namespace.
///
/// Weaker in one way that matters: the file has to live somewhere, and two
/// processes with different ideas of the temporary directory would pick
/// different files. Good enough for a laptop running the tests; the systems the
/// relay actually ships to take the branch above.
#[cfg(not(any(target_os = "linux", target_os = "android")))]
mod imp {
    use std::fs::OpenOptions;
    use std::io;
    use std::os::fd::AsRawFd;

    #[derive(Debug)]
    pub struct Held(#[allow(dead_code)] std::fs::File);

    /// Nothing to answer on: a `flock` is not a socket. Callers fall back to
    /// the pidfile, which is all this branch ever had.
    pub fn answer_with_our_pid(_held: &Held) {}

    pub fn ask_who_holds(_port: u16, _patience: std::time::Duration) -> Option<u32> {
        None
    }

    pub fn try_hold(port: u16) -> io::Result<Option<Held>> {
        let path = std::env::temp_dir().join(format!("chtting-relay.serving.{port}.lock"));
        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(path)?;
        // SAFETY: the descriptor is owned by `file`, which outlives the call.
        let locked = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if locked == 0 {
            return Ok(Some(Held(file)));
        }
        let err = io::Error::last_os_error();
        match err.kind() {
            io::ErrorKind::WouldBlock => Ok(None),
            _ => Err(err),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A port number nothing in the test suite binds, derived per test so two
    /// tests running at once do not fight over one lease.
    fn spare_port(salt: u16) -> u16 {
        40_000 + salt
    }

    #[test]
    fn a_second_claim_on_one_port_is_refused_while_the_first_is_held() {
        let port = spare_port(1);
        let first = PortLease::try_acquire(port)
            .expect("the attempt itself must work")
            .expect("nobody is holding a port this test invented");

        assert!(
            PortLease::try_acquire(port).unwrap().is_none(),
            "two processes could both have believed they owned the port"
        );
        assert!(port_is_taken(port));

        // Released by dropping it, which is what the kernel does for a process
        // that exits however badly.
        drop(first);
        assert!(
            PortLease::try_acquire(port).unwrap().is_some(),
            "the lease outlived its holder"
        );
    }

    #[test]
    fn leases_on_different_ports_do_not_see_each_other() {
        let a = PortLease::try_acquire(spare_port(2)).unwrap().unwrap();
        let b = PortLease::try_acquire(spare_port(3)).unwrap().unwrap();
        assert_eq!(a.port(), spare_port(2));
        assert_eq!(b.port(), spare_port(3));
    }

    #[tokio::test]
    async fn waiting_for_a_held_lease_gives_up_rather_than_hanging() {
        let port = spare_port(4);
        let _held = PortLease::try_acquire(port).unwrap().unwrap();
        let waited = PortLease::acquire_within(port, Duration::from_millis(400))
            .await
            .unwrap();
        assert!(waited.is_none(), "it took a lease that was not free");
    }

    #[tokio::test]
    async fn waiting_returns_the_moment_the_holder_lets_go() {
        let port = spare_port(5);
        let held = PortLease::try_acquire(port).unwrap().unwrap();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(150)).await;
            drop(held);
        });
        let waited = PortLease::acquire_within(port, Duration::from_secs(5))
            .await
            .unwrap();
        assert!(waited.is_some(), "a freed lease was not picked up");
    }

    #[test]
    fn a_lease_parked_for_life_is_never_handed_back() {
        let port = spare_port(6);
        let lease = PortLease::try_acquire(port).unwrap().unwrap();
        assert!(!holding());
        hold_for_life(lease);
        assert!(holding());
        assert!(
            PortLease::try_acquire(port).unwrap().is_none(),
            "parking the lease must not have released it"
        );
    }

    #[test]
    fn the_holder_of_a_lease_says_who_it_is() {
        let port = spare_port(7);
        // Nothing holding it, so there is nobody to answer.
        assert_eq!(serving_pid(port), None);

        let lease = PortLease::try_acquire(port).unwrap().unwrap();
        lease.start_answering();
        assert_eq!(
            serving_pid(port),
            Some(std::process::id()),
            "the lease could not say who was holding it, so --replace would \
             have had nobody to ask to stop"
        );
        drop(lease);
    }

    #[test]
    fn liveness_is_asked_of_the_kernel_not_of_proc() {
        assert!(is_alive(std::process::id()));
        // Past Linux's pid ceiling, so it cannot be anything.
        assert!(!is_alive(4_194_305));
        assert!(!is_alive(0));
    }
}
