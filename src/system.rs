//! The phone side of running the relay: what is installed, what supervises it,
//! and the switches that change either.
//!
//! All of this exists so that the answer to "how do I run this" stops being a
//! list of Termux commands. Everything here is reachable from the dashboard,
//! which is the only caller.
//!
//! One thing cannot move into the dashboard, and it is worth being plain about
//! it: nothing can *start* a relay that is not running, because the dashboard
//! is served by the relay. That is what the keeper, the boot hook and the
//! home-screen shortcuts are for — between them, starting it needs no typing
//! either.
//!
//! The keeper is a shell loop this module writes, not `termux-services`: that
//! package is no longer in Termux's repositories, so anything built on `sv`
//! supervises nothing. See [`Host::install_service`].

use anyhow::{anyhow, bail, Result};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::process::Command;

use crate::logging::Logger;
use crate::state::Paths;

/// The one name the service, the boot hook and the shortcuts all go by.
pub const SERVICE: &str = "chtting-relay";

/// What the dashboard may install, and why anyone would want it.
///
/// A short list on purpose: this hands a web page the package manager, so it is
/// limited to what the relay actually asks for. `termux-services` used to be on
/// it; it is gone from Termux's repositories, which is why the relay supervises
/// itself now — see [`Host::install_service`].
pub const PACKAGES: [(&str, &str); 1] = [("cloudflared", "the public tunnel")];

/* ----------------------------------------------------------- the device -- */

fn env_path(key: &str) -> Option<PathBuf> {
    std::env::var_os(key)
        .map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
}

pub fn prefix() -> Option<PathBuf> {
    env_path("PREFIX")
}

pub fn home_dir() -> Option<PathBuf> {
    env_path("HOME")
}

/// Termux, rather than a desktop that happens to be Linux.
pub fn termux() -> bool {
    prefix().is_some_and(|p| p.to_string_lossy().contains("com.termux"))
}

/// The shell to put in a generated script's shebang.
fn shell() -> PathBuf {
    prefix()
        .map(|p| p.join("bin/sh"))
        .filter(|p| p.exists())
        .unwrap_or_else(|| PathBuf::from("/bin/sh"))
}

/// Find an executable the way a shell would, so a missing tool is reported
/// rather than discovered as a spawn error later.
pub fn on_path(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(name))
        .find(|candidate| candidate.is_file())
}

/// Single-quote a path for a generated shell script.
fn sh_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', r"'\''"))
}

async fn write_script(path: &Path, body: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    tokio::fs::write(path, body).await?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        tokio::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).await?;
    }
    Ok(())
}

/* ------------------------------------------------------------------ host -- */

pub struct Host {
    paths: Paths,
    logger: Arc<Logger>,
    /// Whether *this process* is holding Android's wake lock. Termux offers no
    /// way to ask, so the only honest answer is the one we can remember.
    wake_lock: AtomicBool,
}

impl Host {
    pub fn new(paths: Paths, logger: Arc<Logger>) -> Self {
        Self {
            paths,
            logger,
            wake_lock: AtomicBool::new(false),
        }
    }

    /// Where this binary lives, which is what a generated script has to call.
    fn binary(&self) -> PathBuf {
        std::env::current_exe().unwrap_or_else(|_| self.paths.root.join("chtting-relay"))
    }

    /* ------------------------------------------------------------ report -- */

    /// Everything the Setup screen needs about the device, in one round trip.
    pub async fn report(&self) -> Value {
        let service = self.service_status().await;
        json!({
            "termux": termux(),
            "prefix": prefix().map(|p| p.to_string_lossy().into_owned()),
            "home": home_dir().map(|p| p.to_string_lossy().into_owned()),
            "binary": self.binary().to_string_lossy(),
            "root": self.paths.root.to_string_lossy(),
            "stateHome": self.paths.home.to_string_lossy(),
            "service": service,
            "boot": self.boot_status(),
            "shortcuts": self.shortcut_status(),
            "wakeLock": {
                "supported": on_path("termux-wake-lock").is_some(),
                "held": self.wake_lock.load(Ordering::Relaxed),
            },
            "packages": PACKAGES.iter().map(|(name, why)| json!({
                "name": name,
                "why": why,
                "installed": on_path(name).is_some(),
            })).collect::<Vec<_>>(),
        })
    }

    /* ----------------------------------------------------------- service -- */

    /// Where the keeper script and its pidfile live.
    ///
    /// Beside the relay's own state rather than in `$PREFIX/var/service`: there
    /// is no runit to read that directory any more, and a script the user can
    /// find and read beats one hidden inside the package tree. Keying it to the
    /// state home rather than `$HOME` also means two installs run by `--home`
    /// get a keeper each instead of fighting over one.
    fn keeper_dir(&self) -> PathBuf {
        self.paths.home.join(format!(".{SERVICE}"))
    }

    fn keeper_script(&self) -> PathBuf {
        self.keeper_dir().join("keeper.sh")
    }

    /// Written by the keeper while it is running; removed when it exits.
    fn keeper_pidfile(&self) -> PathBuf {
        self.keeper_dir().join("keeper.pid")
    }

    /// Created to tell a running keeper not to start the relay again.
    fn keeper_stopfile(&self) -> PathBuf {
        self.keeper_dir().join("stopped")
    }

    /// Is a pid in a file still a process?
    fn pid_alive(path: &Path) -> Option<u32> {
        let pid: u32 = std::fs::read_to_string(path).ok()?.trim().parse().ok()?;
        Path::new(&format!("/proc/{pid}")).exists().then_some(pid)
    }

    /// What is keeping the relay alive, if anything.
    pub async fn service_status(&self) -> Value {
        let script = self.keeper_script();
        let installed = script.is_file();
        let pid = Self::pid_alive(&self.keeper_pidfile());
        let stopped = self.keeper_stopfile().is_file();

        let state = match (installed, pid, stopped) {
            (false, _, _) => "not installed".to_string(),
            (true, Some(pid), _) => format!("run: keeper (pid {pid})"),
            (true, None, true) => "installed, told to stay down".to_string(),
            (true, None, false) => "installed, not running".to_string(),
        };

        json!({
            "installed": installed,
            // What the Setup screen asks: will this come back on its own?
            "supervised": pid.is_some(),
            "kind": "keeper",
            "pid": pid,
            "stopped": stopped,
            "state": state,
            "path": script.to_string_lossy(),
        })
    }

    /// Write the keeper: a small shell loop that restarts the relay if it ever
    /// exits.
    ///
    /// Termux used to ship `termux-services`, a runit setup that did this job,
    /// and the relay used to install a `$PREFIX/var/service` directory for it.
    /// That package is gone from the repositories, so a service directory now
    /// supervises nothing — it just sits there looking installed. This replaces
    /// it with something that depends on nothing but `sh`, which Termux cannot
    /// stop shipping.
    ///
    /// The script is written but not started, so it cannot race the copy of the
    /// relay that is answering this very request. `hand_over` starts it.
    pub async fn install_service(&self) -> Result<Value> {
        let dir = self.keeper_dir();
        let script = self.keeper_script();
        tokio::fs::create_dir_all(&dir).await?;
        let pidfile = self.keeper_pidfile();
        let stopfile = self.keeper_stopfile();

        let body = format!(
            "#!{shell}\n\
             # Generated by chtting-relay. The dashboard rewrites this file.\n\
             # Keeps the relay running: if it exits for any reason, start it again.\n\
             PIDFILE={pidfile}\n\
             STOPFILE={stopfile}\n\
             \n\
             # One keeper at a time. A second one would fight the first over\n\
             # restarts and end up with two relays racing for the same port.\n\
             if [ -f \"$PIDFILE\" ] && kill -0 \"$(cat \"$PIDFILE\")\" 2>/dev/null; then\n\
             \texit 0\n\
             fi\n\
             rm -f \"$STOPFILE\"\n\
             echo $$ > \"$PIDFILE\"\n\
             trap 'rm -f \"$PIDFILE\"' EXIT INT TERM\n\
             \n\
             termux-wake-lock 2>/dev/null || true\n\
             cd {root} || exit 1\n\
             \n\
             while :; do\n\
             \t[ -f \"$STOPFILE\" ] && break\n\
             \t{binary} start --home {home}\n\
             \t[ -f \"$STOPFILE\" ] && break\n\
             \t# A crash loop should not become a busy loop.\n\
             \tsleep 3\n\
             done\n\
             rm -f \"$PIDFILE\"\n",
            shell = shell().display(),
            pidfile = sh_quote(&pidfile.to_string_lossy()),
            stopfile = sh_quote(&stopfile.to_string_lossy()),
            root = sh_quote(&self.paths.root.to_string_lossy()),
            binary = sh_quote(&self.binary().to_string_lossy()),
            home = sh_quote(&self.paths.home.to_string_lossy()),
        );
        write_script(&script, &body).await?;
        self.logger
            .info(format!("keeper installed at {}", script.display()));

        let mut status = self.service_status().await;
        if let Some(map) = status.as_object_mut() {
            map.insert(
                "note".into(),
                Value::String(
                    "Installed and not started yet. \"Hand over\" starts it, and from then \
                     on the relay comes back on its own if it dies."
                        .into(),
                ),
            );
        }
        Ok(status)
    }

    pub async fn uninstall_service(&self) -> Result<Value> {
        // Tell a running keeper to stop before taking its script away, or it
        // would keep restarting a relay from a file that no longer exists.
        self.supervisor_down().await;
        let _ = tokio::fs::remove_file(self.keeper_script()).await;
        let _ = tokio::fs::remove_file(self.keeper_pidfile()).await;
        Ok(self.service_status().await)
    }

    /// Tell the keeper we are meant to stay down.
    ///
    /// Without this, stopping a supervised relay does nothing at all: the
    /// process exits, the keeper notices within three seconds and starts
    /// another one. "Stop" has to mean stopped.
    pub async fn supervisor_down(&self) {
        let stopfile = self.keeper_stopfile();
        if let Some(dir) = stopfile.parent() {
            let _ = tokio::fs::create_dir_all(dir).await;
        }
        if tokio::fs::write(&stopfile, b"stopped from the dashboard\n")
            .await
            .is_ok()
        {
            self.logger.info("keeper asked to stay down");
        }
        // The keeper only reads the flag between relay runs, so a keeper that
        // is idle in its `sleep 3` is left to notice on its own.
    }

    /// Start the keeper, so it takes over supervising this relay.
    ///
    /// It does not have to wait for this process to let go of the port: both
    /// relays bind with `SO_REUSEPORT`, so the new one is listening before the
    /// old one stops, and nothing is refused in between.
    pub async fn hand_over(&self) -> Result<()> {
        let script = self.keeper_script();
        if !script.is_file() {
            bail!("no keeper installed yet");
        }
        let _ = tokio::fs::remove_file(self.keeper_stopfile()).await;

        // Detached on purpose: it has to outlive us, because supervising us is
        // the whole job.
        Command::new(shell())
            .arg(&script)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()?;
        Ok(())
    }

    /* -------------------------------------------------------------- boot -- */

    fn boot_script(&self) -> Option<PathBuf> {
        home_dir().map(|h| h.join(".termux/boot").join(SERVICE))
    }

    fn boot_status(&self) -> Value {
        let path = self.boot_script();
        json!({
            "installed": path.as_ref().is_some_and(|p| p.is_file()),
            "path": path.map(|p| p.to_string_lossy().into_owned()),
            // Termux:Boot is a separate app; a script alone does nothing.
            "appHint": "needs the Termux:Boot app from F-Droid, opened once",
        })
    }

    pub async fn install_boot(&self) -> Result<Value> {
        let Some(path) = self.boot_script() else {
            bail!("no $HOME — cannot place a boot script");
        };
        // With a keeper installed, booting means starting the keeper and
        // letting it start the relay — and keep starting it. Without one, run
        // the relay directly, which at least gets it up.
        let keeper = Some(self.keeper_script())
            .filter(|p| p.is_file())
            .map(|p| sh_quote(&p.to_string_lossy()));
        let body = if let Some(keeper) = keeper {
            format!(
                "#!{shell}\n\
                 # Generated by chtting-relay.\n\
                 termux-wake-lock 2>/dev/null || true\n\
                 exec {keeper}\n",
                shell = shell().display(),
            )
        } else {
            format!(
                "#!{shell}\n\
                 # Generated by chtting-relay.\n\
                 termux-wake-lock 2>/dev/null || true\n\
                 cd {root} || exit 1\n\
                 exec {binary} start --home {home}\n",
                shell = shell().display(),
                root = sh_quote(&self.paths.root.to_string_lossy()),
                binary = sh_quote(&self.binary().to_string_lossy()),
                home = sh_quote(&self.paths.home.to_string_lossy()),
            )
        };
        write_script(&path, &body).await?;
        Ok(self.boot_status())
    }

    pub async fn remove_boot(&self) -> Result<Value> {
        if let Some(path) = self.boot_script() {
            let _ = tokio::fs::remove_file(path).await;
        }
        Ok(self.boot_status())
    }

    /* --------------------------------------------------------- shortcuts -- */

    fn shortcut_dir(&self) -> Option<PathBuf> {
        home_dir().map(|h| h.join(".shortcuts"))
    }

    /// The four things a home-screen widget should be able to do.
    fn shortcut_names(&self) -> [String; 4] {
        [
            format!("{SERVICE}-start"),
            format!("{SERVICE}-stop"),
            format!("{SERVICE}-restart"),
            format!("{SERVICE}-dashboard"),
        ]
    }

    fn shortcut_status(&self) -> Value {
        let dir = self.shortcut_dir();
        let files: Vec<Value> = self
            .shortcut_names()
            .iter()
            .map(|name| {
                json!({
                    "name": name,
                    "installed": dir.as_ref().is_some_and(|d| d.join(name).is_file()),
                })
            })
            .collect();
        json!({
            "installed": files.iter().all(|f| f["installed"] == Value::Bool(true)),
            "path": dir.map(|d| d.to_string_lossy().into_owned()),
            "files": files,
            "appHint": "needs the Termux:Widget app from F-Droid",
        })
    }

    /// Write the widget scripts, so starting and stopping the relay is a tap on
    /// the home screen rather than a session in Termux.
    pub async fn install_shortcuts(&self, dashboard_url: &str) -> Result<Value> {
        let Some(dir) = self.shortcut_dir() else {
            bail!("no $HOME — cannot place shortcuts");
        };
        let shell = shell();
        let root = sh_quote(&self.paths.root.to_string_lossy());
        let binary = sh_quote(&self.binary().to_string_lossy());
        let home = sh_quote(&self.paths.home.to_string_lossy());
        let keeper = Some(self.keeper_script())
            .filter(|p| p.is_file())
            .map(|p| sh_quote(&p.to_string_lossy()));
        let stopfile = sh_quote(&self.keeper_stopfile().to_string_lossy());

        let start = match &keeper {
            // The keeper starts the relay and keeps starting it; running it
            // twice is harmless, because it checks its own pidfile first.
            Some(keeper) => format!(
                "#!{shell}\nrm -f {stopfile}\nexec {keeper}\n",
                shell = shell.display()
            ),
            None => format!(
                "#!{shell}\n\
                 termux-wake-lock 2>/dev/null || true\n\
                 cd {root} || exit 1\n\
                 exec {binary} start --home {home}\n",
                shell = shell.display(),
            ),
        };
        // `pkill -x`, never `-f`: matching the whole command line would match
        // this very script, whose name contains the relay's, and the shortcut
        // would kill itself before reaching the relay.
        // `pkill -x`, never `-f`: matching the whole command line would match
        // this very script, whose name contains the relay's, and the shortcut
        // would kill itself before reaching the relay.
        //
        // The stop flag goes down before the relay does, or the keeper would
        // have another one up within three seconds. "Stop" has to mean stopped.
        let stop = match &keeper {
            Some(_) => format!(
                "#!{shell}\n\
                 : > {stopfile}\n\
                 pkill -x {SERVICE} || true\n\
                 termux-wake-unlock 2>/dev/null || true\n",
                shell = shell.display(),
            ),
            None => format!(
                "#!{shell}\n\
                 pkill -x {SERVICE} || true\n\
                 termux-wake-unlock 2>/dev/null || true\n",
                shell = shell.display(),
            ),
        };
        let restart = match &keeper {
            // Killing it is the restart: the keeper starts the next one.
            Some(_) => format!(
                "#!{shell}\nrm -f {stopfile}\npkill -x {SERVICE} || true\n",
                shell = shell.display()
            ),
            None => format!(
                "#!{shell}\n\
                 pkill -x {SERVICE} || true\n\
                 sleep 2\n\
                 termux-wake-lock 2>/dev/null || true\n\
                 cd {root} || exit 1\n\
                 exec {binary} start --home {home}\n",
                shell = shell.display(),
            ),
        };
        let open = format!(
            "#!{}\ntermux-open-url {}\n",
            shell.display(),
            sh_quote(dashboard_url)
        );

        let names = self.shortcut_names();
        for (name, body) in names.iter().zip([start, stop, restart, open]) {
            write_script(&dir.join(name), &body).await?;
        }
        self.logger.info(format!(
            "home-screen shortcuts written to {}",
            dir.display()
        ));
        Ok(self.shortcut_status())
    }

    pub async fn remove_shortcuts(&self) -> Result<Value> {
        if let Some(dir) = self.shortcut_dir() {
            for name in self.shortcut_names() {
                let _ = tokio::fs::remove_file(dir.join(name)).await;
            }
        }
        Ok(self.shortcut_status())
    }

    /* --------------------------------------------------------- wake lock -- */

    /// Ask Android not to suspend Termux while the screen is off.
    pub async fn set_wake_lock(&self, on: bool) -> Result<Value> {
        let binary = if on {
            "termux-wake-lock"
        } else {
            "termux-wake-unlock"
        };
        if on_path(binary).is_none() {
            bail!("{binary} not found — this only works inside Termux");
        }
        let out = Command::new(binary).output().await?;
        if !out.status.success() {
            bail!(
                "{binary} failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        self.wake_lock.store(on, Ordering::Relaxed);
        self.logger.info(if on {
            "wake lock held"
        } else {
            "wake lock released"
        });
        Ok(json!({ "held": on }))
    }

    /// Take the lock at startup without making a failure fatal: a relay that
    /// runs is better than one that refuses to because the phone said no.
    pub async fn acquire_wake_lock(&self) {
        if let Err(err) = self.set_wake_lock(true).await {
            self.logger.warn(format!("no wake lock: {err}"));
        }
    }

    pub async fn release_wake_lock(&self) {
        if self.wake_lock.load(Ordering::Relaxed) {
            let _ = self.set_wake_lock(false).await;
        }
    }

    /* ---------------------------------------------------------- packages -- */

    /// `pkg install` for the short list in [`PACKAGES`], and nothing else.
    pub async fn install_package(&self, name: &str) -> Result<Value> {
        if !PACKAGES.iter().any(|(n, _)| *n == name) {
            bail!("\"{name}\" is not one of the packages this dashboard installs");
        }
        if on_path("pkg").is_none() {
            bail!("`pkg` not found — this only works inside Termux");
        }
        self.logger.info(format!("pkg install {name}"));

        let out = tokio::time::timeout(
            std::time::Duration::from_secs(600),
            Command::new("pkg").args(["install", "-y", name]).output(),
        )
        .await
        .map_err(|_| anyhow!("`pkg install {name}` gave up after 10 minutes"))??;

        let mut output = String::from_utf8_lossy(&out.stdout).into_owned();
        output.push_str(&String::from_utf8_lossy(&out.stderr));
        Ok(json!({
            "ok": out.status.success() && on_path(name).is_some(),
            "installed": on_path(name).is_some(),
            // The tail is the part that says what happened; the rest is a
            // package list nobody reads on a phone screen.
            "output": output.lines().rev().take(40).collect::<Vec<_>>()
                .into_iter().rev().collect::<Vec<_>>().join("\n"),
        }))
    }
}

/* ------------------------------------------------------------- lifecycle -- */

/// Replace this process with a fresh copy of itself.
///
/// Everything the relay owns lives in files, so a restart is an exec and
/// nothing more: the listening sockets close on their own (Rust opens them
/// close-on-exec), the config is read again, and the dashboard is back on the
/// same port a second later. The PID does not change, so a supervisor watching
/// this process never notices — which is why this is the one restart path for
/// both the supervised and the unsupervised case.
#[cfg(unix)]
pub fn exec_self() -> anyhow::Error {
    use std::os::unix::process::CommandExt;
    let exe = match std::env::current_exe() {
        Ok(path) => path,
        Err(err) => return anyhow!("cannot find my own binary to restart: {err}"),
    };
    let args: Vec<String> = std::env::args().skip(1).collect();
    // `exec` only returns when it failed.
    anyhow!(
        "restart failed: {}",
        std::process::Command::new(exe).args(args).exec()
    )
}

#[cfg(not(unix))]
pub fn exec_self() -> anyhow::Error {
    anyhow!("restarting in place is only implemented for Unix")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn host() -> Host {
        let dir = std::env::temp_dir().join(format!("chtting-host-{}", crate::util::new_id("h")));
        Host::new(
            Paths::resolve(Some(&dir)),
            Logger::console(crate::logging::Level::Silent),
        )
    }

    #[test]
    fn a_path_with_a_quote_in_it_cannot_break_out_of_a_generated_script() {
        assert_eq!(sh_quote("/data/home"), "'/data/home'");

        // Reading the escaping and deciding it looks safe is not the test. A
        // real shell is, so ask one: whatever went in has to come back out as
        // one argument, unchanged, having run nothing on the way.
        for original in [
            "/data/data/com.termux/files/home",
            "/tmp/a'; rm -rf /; echo '",
            "/tmp/with a space/$HOME/`id`/$(id)",
            "/tmp/quote'/and\"double",
        ] {
            let out = std::process::Command::new("/bin/sh")
                .arg("-c")
                .arg(format!("printf %s {}", sh_quote(original)))
                .output()
                .expect("a shell to test the quoting against");
            assert_eq!(
                String::from_utf8_lossy(&out.stdout),
                original,
                "the shell did not see the path it was given"
            );
        }
    }

    #[tokio::test]
    async fn only_the_two_named_packages_may_be_installed() {
        let err = host()
            .install_package("busybox; rm -rf /")
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("not one of the packages"), "{err}");

        // A real package name still has to get past the Termux check, which is
        // what a test machine fails — but it fails for the right reason.
        let err = host().install_package("cloudflared").await.unwrap_err();
        assert!(
            !err.to_string().contains("not one of the packages"),
            "an allowed package was rejected as unknown"
        );
    }

    #[tokio::test]
    async fn the_report_describes_a_desktop_honestly_rather_than_guessing() {
        let report = host().report().await;
        // Nothing here should claim Termux facilities on a machine without them.
        assert_eq!(report["termux"], termux());
        assert_eq!(report["service"]["installed"], false);
        assert_eq!(report["wakeLock"]["held"], false);
        assert!(report["packages"].as_array().is_some_and(|p| !p.is_empty()));
        assert!(
            !report.to_string().contains("termux-services"),
            "termux-services is gone from Termux and must not be offered"
        );
    }

    #[tokio::test]
    async fn the_keeper_restarts_the_relay_and_stops_when_told_to() {
        let host = host();
        assert_eq!(host.service_status().await["installed"], false);

        let status = host.install_service().await.unwrap();
        assert_eq!(status["installed"], true);
        // Written but not started: it must not race the relay that is already
        // holding the port.
        assert_eq!(status["supervised"], false);

        let script = std::fs::read_to_string(host.keeper_script()).unwrap();
        assert!(script.contains("while :;"), "no restart loop:\n{script}");
        assert!(script.contains("start --home"), "{script}");
        assert!(script.contains("STOPFILE"), "no way to stop it:\n{script}");
        assert!(
            !script.contains("sv up") && !script.contains("termux-services"),
            "the keeper must not depend on a package Termux no longer ships"
        );

        // "Stop" has to mean stopped, or the keeper starts another one.
        host.supervisor_down().await;
        assert!(host.keeper_stopfile().is_file());
        assert_eq!(host.service_status().await["stopped"], true);

        host.uninstall_service().await.unwrap();
        assert_eq!(host.service_status().await["installed"], false);
    }

    #[tokio::test]
    async fn the_keeper_is_a_shell_script_a_shell_will_accept() {
        let host = host();
        host.install_service().await.unwrap();
        // Reading it and deciding it looks right is not the test; asking a
        // shell to parse it is.
        let out = std::process::Command::new("/bin/sh")
            .arg("-n")
            .arg(host.keeper_script())
            .output()
            .expect("a shell to check the script with");
        assert!(
            out.status.success(),
            "the generated keeper is not valid shell: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    #[tokio::test]
    async fn two_installs_under_different_homes_get_a_keeper_each() {
        let a = host();
        let b = host();
        a.install_service().await.unwrap();
        assert_ne!(a.keeper_script(), b.keeper_script());
        assert_eq!(b.service_status().await["installed"], false);
    }
}
