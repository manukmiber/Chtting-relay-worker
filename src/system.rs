//! The phone side of running the relay: what is installed, what supervises it,
//! and the switches that change either.
//!
//! All of this exists so that the answer to "how do I run this" stops being a
//! list of Termux commands. Everything here is reachable from the dashboard,
//! which is the only caller.
//!
//! One thing cannot move into the dashboard, and it is worth being plain about
//! it: nothing can *start* a relay that is not running, because the dashboard
//! is served by the relay. That is what the service, the boot hook and the
//! home-screen shortcuts are for — between them, starting it needs no typing
//! either.

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
/// A short list on purpose: this hands a web page the package manager, so it
/// is limited to the two packages the relay actually asks for.
pub const PACKAGES: [(&str, &str); 2] = [
    ("cloudflared", "the public tunnel"),
    (
        "termux-services",
        "supervises the relay and brings it back if it dies",
    ),
];

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

    fn service_dir(&self) -> Option<PathBuf> {
        prefix().map(|p| p.join("var/service").join(SERVICE))
    }

    /// What runit makes of the service, if runit is even here.
    pub async fn service_status(&self) -> Value {
        let dir = self.service_dir();
        let installed = dir.as_ref().is_some_and(|d| d.join("run").is_file());
        let wanted_down = dir.as_ref().is_some_and(|d| d.join("down").is_file());
        let sv = on_path("sv").is_some();

        let mut state = if !installed {
            "not installed".to_string()
        } else if wanted_down {
            "installed, not started".to_string()
        } else {
            "installed".to_string()
        };
        let mut supervised = false;

        if installed && sv {
            if let Ok(out) = Command::new("sv").args(["status", SERVICE]).output().await {
                let line = String::from_utf8_lossy(&out.stdout).trim().to_string();
                if !line.is_empty() {
                    supervised = line.starts_with("run:");
                    state = line;
                }
            }
        }

        json!({
            "installed": installed,
            "supervised": supervised,
            "svAvailable": sv,
            "state": state,
            "path": dir.map(|d| d.to_string_lossy().into_owned()),
        })
    }

    /// Write the runit service, left stopped so it cannot race the copy that
    /// is answering this very request for the port.
    pub async fn install_service(&self) -> Result<Value> {
        let Some(dir) = self.service_dir() else {
            bail!("no $PREFIX — a runit service only means something inside Termux");
        };

        let run = format!(
            "#!{shell}\n\
             # Generated by chtting-relay. The dashboard rewrites this file.\n\
             exec 2>&1\n\
             cd {root} || exit 1\n\
             termux-wake-lock 2>/dev/null || true\n\
             exec {binary} start --home {home}\n",
            shell = shell().display(),
            root = sh_quote(&self.paths.root.to_string_lossy()),
            binary = sh_quote(&self.binary().to_string_lossy()),
            home = sh_quote(&self.paths.home.to_string_lossy()),
        );
        write_script(&dir.join("run"), &run).await?;

        // termux-services ships a log runner; use it when it is there so the
        // service's output lands somewhere readable instead of nowhere.
        if let Some(svlogger) = prefix().map(|p| p.join("share/termux-services/svlogger")) {
            if svlogger.is_file() {
                let log = format!(
                    "#!{shell}\nexec {svlogger} \"$@\"\n",
                    shell = shell().display(),
                    svlogger = sh_quote(&svlogger.to_string_lossy()),
                );
                write_script(&dir.join("log/run"), &log).await?;
            }
        }

        // The relay is already running and holding the port. Until the handover
        // this service must stay down, or runsv would spend the next hour
        // restarting a copy that cannot bind.
        tokio::fs::write(dir.join("down"), b"").await?;
        self.logger
            .info(format!("service installed at {}", dir.display()));

        let mut status = self.service_status().await;
        if let Some(map) = status.as_object_mut() {
            map.insert(
                "note".into(),
                Value::String(if on_path("sv").is_some() {
                    "Installed and left stopped. \"Hand over\" restarts the relay under it.".into()
                } else {
                    "Installed. Install the termux-services package to supervise it.".into()
                }),
            );
        }
        Ok(status)
    }

    pub async fn uninstall_service(&self) -> Result<Value> {
        let Some(dir) = self.service_dir() else {
            bail!("no $PREFIX — nothing to remove");
        };
        if dir.exists() {
            // Ask runit to let go first; a supervised directory that vanishes
            // underneath runsv leaves a stray supervisor behind.
            if on_path("sv").is_some() {
                let _ = Command::new("sv").args(["down", SERVICE]).output().await;
            }
            // A symlink from the older install script points at the repo, and
            // removing the directory it points at would be the wrong thing.
            let meta = tokio::fs::symlink_metadata(&dir).await?;
            if meta.file_type().is_symlink() {
                tokio::fs::remove_file(&dir).await?;
            } else {
                tokio::fs::remove_dir_all(&dir).await?;
            }
        }
        Ok(self.service_status().await)
    }

    /// Tell runit we are meant to stay down.
    ///
    /// Without this, stopping a supervised relay does nothing at all: the
    /// process exits, runsv notices within the second and starts another one.
    /// "Stop" has to mean stopped.
    pub async fn supervisor_down(&self) {
        if on_path("sv").is_none() {
            return;
        }
        // Bounded, because this runs on the way out and a hung `sv` must not be
        // what keeps the relay alive.
        let down = Command::new("sv")
            .args(["-w", "1", "down", SERVICE])
            .output();
        match tokio::time::timeout(std::time::Duration::from_secs(5), down).await {
            Ok(Ok(_)) => self.logger.info("runit asked to keep the service down"),
            Ok(Err(err)) => self.logger.warn(format!("`sv down` failed: {err}")),
            Err(_) => self.logger.warn("`sv down` timed out"),
        }
    }

    /// Let the service take over: clear the stop flag, arrange for `sv up` to
    /// run a moment after this process lets go of the port, and go.
    pub async fn hand_over(&self) -> Result<()> {
        let Some(dir) = self.service_dir() else {
            bail!("no $PREFIX — nothing to hand over to");
        };
        if !dir.join("run").is_file() {
            bail!("no service installed yet");
        }
        if on_path("sv").is_none() {
            bail!("`sv` not found — install the termux-services package first");
        }
        let _ = tokio::fs::remove_file(dir.join("down")).await;

        // Detached on purpose: it has to outlive us, because what it waits for
        // is us releasing the port.
        Command::new(shell())
            .arg("-c")
            .arg(format!("sleep 2; sv up {SERVICE}"))
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
        // Under a service, booting means starting runit and letting it do the
        // rest; without one, run the relay directly.
        let body = if self.service_dir().is_some_and(|d| d.join("run").is_file()) {
            format!(
                "#!{shell}\n\
                 # Generated by chtting-relay.\n\
                 termux-wake-lock 2>/dev/null || true\n\
                 . $PREFIX/etc/profile.d/start-services.sh\n",
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
        let supervised = self.service_dir().is_some_and(|d| d.join("run").is_file());

        let start = if supervised {
            format!("#!{}\nsv up {SERVICE}\n", shell.display())
        } else {
            format!(
                "#!{shell}\n\
                 termux-wake-lock 2>/dev/null || true\n\
                 cd {root} || exit 1\n\
                 exec {binary} start --home {home}\n",
                shell = shell.display(),
            )
        };
        // `pkill -x`, never `-f`: matching the whole command line would match
        // this very script, whose name contains the relay's, and the shortcut
        // would kill itself before reaching the relay.
        let stop = if supervised {
            format!("#!{}\nsv down {SERVICE}\n", shell.display())
        } else {
            format!(
                "#!{shell}\n\
                 pkill -x {SERVICE} || true\n\
                 termux-wake-unlock 2>/dev/null || true\n",
                shell = shell.display(),
            )
        };
        let restart = if supervised {
            format!("#!{}\nsv restart {SERVICE}\n", shell.display())
        } else {
            format!(
                "#!{shell}\n\
                 pkill -x {SERVICE} || true\n\
                 sleep 2\n\
                 termux-wake-lock 2>/dev/null || true\n\
                 cd {root} || exit 1\n\
                 exec {binary} start --home {home}\n",
                shell = shell.display(),
            )
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
        assert!(report["packages"].as_array().is_some_and(|p| p.len() == 2));
    }

    #[tokio::test]
    async fn a_service_cannot_be_installed_without_a_termux_prefix() {
        // The desktop test machine has no $PREFIX, which is exactly the case
        // that has to fail with an explanation rather than a panic.
        if prefix().is_some() {
            return;
        }
        let err = host().install_service().await.unwrap_err().to_string();
        assert!(err.contains("$PREFIX"), "{err}");
    }
}
