//! Bringing the relay up to date with its own git checkout, from the dashboard.
//!
//! The relay on a phone is a git clone plus a binary built out of it, and until
//! now moving it forward meant opening Termux and typing three commands. This is
//! those three commands behind a button:
//!
//! ```text
//!   git pull --ff-only  →  cargo build  →  exec the new binary
//! ```
//!
//! Every part of it has to be there. A pull alone changes nothing a caller can
//! see: the dashboard's HTML, CSS and JavaScript are compiled *into* the binary
//! by `include_dir!`, so new source on disk is new source nobody is running. And
//! a build alone can leave the relay a commit behind whatever was tested. So the
//! button pulls, and then builds what it pulled, and then restarts into it —
//! which is what "restart" has to mean for a relay that is also a checkout.
//!
//! Two things are deliberate:
//!
//! * **The relay stays up until the new binary exists.** A pull that fails, a
//!   build that fails, a missing toolchain — all of them leave the running relay
//!   exactly as it was and report why. The only thing that ends the old process
//!   is a new binary on disk.
//! * **The running binary is moved aside before the build, not overwritten.**
//!   Linking over a file that is currently executing fails with `ETXTBSY` on
//!   Linux. Renaming it is free (the running process holds the inode, not the
//!   name), leaves the path the keeper script calls free for the new build, and
//!   gives a failed build something to restore.
//!
//! It takes five to fifteen minutes on a phone, so nothing here blocks a
//! request: [`Updater::run`] is driven from a background task and the dashboard
//! polls [`Updater::status`] for the log as it goes.

use anyhow::{anyhow, bail, Context, Result};
use parking_lot::Mutex;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;

use crate::logging::Logger;
use crate::state::Paths;

/// Enough of a build log to see what went wrong, and no more: this is held in
/// memory on a phone.
const MAX_LOG_LINES: usize = 400;

/// Where the update has got to. Reported as-is to the dashboard.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    /// Nothing has been asked for yet.
    Idle,
    Pulling,
    Building,
    /// The new binary is on disk and this process is on its way out.
    Restarting,
    /// Finished without needing to replace anything.
    UpToDate,
    Failed,
}

impl Step {
    fn label(self) -> &'static str {
        match self {
            Step::Idle => "idle",
            Step::Pulling => "pulling",
            Step::Building => "building",
            Step::Restarting => "restarting",
            Step::UpToDate => "up-to-date",
            Step::Failed => "failed",
        }
    }
}

#[derive(Default)]
struct Progress {
    step: Option<Step>,
    started_at: i64,
    finished_at: i64,
    error: String,
    /// The commit the relay was built from, and the one it is going to.
    before: String,
    after: String,
    lines: Vec<String>,
}

pub struct Updater {
    paths: Paths,
    logger: Arc<Logger>,
    progress: Mutex<Progress>,
    /// One update at a time. Two `cargo build`s in the same target directory
    /// would block on each other's lock file and take twice as long to fail.
    busy: AtomicBool,
}

impl Updater {
    pub fn new(paths: Paths, logger: Arc<Logger>) -> Self {
        Self {
            paths,
            logger,
            progress: Mutex::new(Progress::default()),
            busy: AtomicBool::new(false),
        }
    }

    pub fn status(&self) -> Value {
        let p = self.progress.lock();
        json!({
            "step": p.step.unwrap_or(Step::Idle).label(),
            "busy": self.busy.load(Ordering::SeqCst),
            "startedAt": p.started_at,
            "finishedAt": p.finished_at,
            "error": p.error,
            "before": p.before,
            "after": p.after,
            "repo": self.repo().map(|r| r.to_string_lossy().into_owned()),
            "git": crate::system::on_path("git").is_some(),
            "cargo": crate::system::on_path("cargo").is_some(),
            "logs": p.lines.clone(),
        })
    }

    /// The git checkout this relay was built from.
    ///
    /// Found by walking up from the working directory rather than being
    /// configured, because the keeper already `cd`s into the checkout and a
    /// second copy of that path is a second thing to get wrong.
    pub fn repo(&self) -> Option<PathBuf> {
        let mut dir = self.paths.root.as_path();
        loop {
            if dir.join(".git").exists() {
                return Some(dir.to_path_buf());
            }
            dir = dir.parent()?;
        }
    }

    /// Pull, build if the pull moved anything, and hand back the binary to
    /// restart into.
    ///
    /// `Ok(None)` means there was nothing to do and the relay is already
    /// running the current commit — worth saying rather than restarting for
    /// nothing.
    pub async fn run(self: &Arc<Self>) -> Result<Option<PathBuf>> {
        if self.busy.swap(true, Ordering::SeqCst) {
            bail!("an update is already running");
        }
        let result = self.run_inner().await;
        self.busy.store(false, Ordering::SeqCst);

        let mut p = self.progress.lock();
        p.finished_at = crate::util::now_ms();
        match &result {
            Ok(Some(_)) => p.step = Some(Step::Restarting),
            Ok(None) => p.step = Some(Step::UpToDate),
            Err(err) => {
                p.step = Some(Step::Failed);
                p.error = err.to_string();
            }
        }
        drop(p);
        if let Err(err) = &result {
            self.log(&format!("update failed: {err}"));
            self.logger.warn(format!("update failed: {err}"));
        }
        result
    }

    async fn run_inner(self: &Arc<Self>) -> Result<Option<PathBuf>> {
        {
            let mut p = self.progress.lock();
            *p = Progress {
                step: Some(Step::Pulling),
                started_at: crate::util::now_ms(),
                ..Progress::default()
            };
        }

        let repo = self
            .repo()
            .ok_or_else(|| anyhow!("no git checkout above {}", self.paths.root.display()))?;
        if crate::system::on_path("git").is_none() {
            bail!("git is not installed. Install it with: pkg install git");
        }
        self.log(&format!("repository: {}", repo.display()));

        let before = self.head(&repo).await.unwrap_or_default();
        self.progress.lock().before = before.clone();

        // `--ff-only`: a relay is not the place to resolve a merge. A checkout
        // with local commits or edits says so and stops, which is the answer
        // somebody can act on.
        self.run_command("git", &["pull", "--ff-only"], &repo)
            .await
            .context("git pull failed")?;

        let after = self.head(&repo).await.unwrap_or_default();
        self.progress.lock().after = after.clone();

        let binary = self.binary_path(&repo);
        let stale = self.binary_is_older_than_head(&repo, &binary).await;
        if before == after && !stale {
            self.log("already on the newest commit, and the binary is built from it");
            return Ok(None);
        }
        if before == after {
            self.log("no new commits, but the binary is older than the checkout — rebuilding");
        }

        self.build(&repo, &binary).await?;
        Ok(Some(binary))
    }

    /// Compile, having first made room for the result.
    async fn build(self: &Arc<Self>, repo: &Path, binary: &Path) -> Result<()> {
        if crate::system::on_path("cargo").is_none() {
            bail!(
                "cargo is not installed, so the new commits cannot be built. \
                 Install it with: pkg install rust clang binutils pkg-config"
            );
        }
        self.progress.lock().step = Some(Step::Building);
        self.log("building (5 to 15 minutes on a phone) …");

        // Linking over a running executable fails with ETXTBSY. Renaming it
        // costs nothing — this process holds the inode, not the name — frees
        // the path the keeper script calls, and leaves a copy to put back if
        // the build does not finish.
        let parked = binary.with_extension("prev");
        let moved = if binary.exists() {
            let _ = tokio::fs::remove_file(&parked).await;
            tokio::fs::rename(binary, &parked)
                .await
                .with_context(|| format!("could not move {} aside", binary.display()))?;
            true
        } else {
            false
        };

        let mut args: Vec<&str> = vec!["build"];
        if self.profile() == "release-small" {
            // The phone that needed this profile is the phone that cannot
            // afford parallel codegen either.
            args.extend(["--profile", "release-small", "-j1"]);
        } else {
            args.push("--release");
        }

        match self.run_command("cargo", &args, repo).await {
            Ok(()) if binary.exists() => {
                self.log(&format!("built {}", binary.display()));
                Ok(())
            }
            Ok(()) => {
                self.restore(moved, &parked, binary).await;
                bail!(
                    "the build reported success but {} is missing",
                    binary.display()
                )
            }
            Err(err) => {
                self.restore(moved, &parked, binary).await;
                Err(err.context("cargo build failed"))
            }
        }
    }

    /// Put the old binary back after a build that did not produce a new one, so
    /// the keeper still has something to start if this process dies later.
    async fn restore(&self, moved: bool, parked: &Path, binary: &Path) {
        if moved && !binary.exists() {
            let _ = tokio::fs::rename(parked, binary).await;
            self.log("put the previous binary back");
        }
    }

    /// Which profile this relay was built with, read off the path it is running
    /// from: an update should not quietly move a phone that could only manage
    /// `release-small` onto the profile that ran it out of memory.
    fn profile(&self) -> &'static str {
        let exe = std::env::current_exe().unwrap_or_default();
        if exe.to_string_lossy().contains("release-small") {
            "release-small"
        } else {
            "release"
        }
    }

    /// Where the build will leave the binary.
    fn binary_path(&self, repo: &Path) -> PathBuf {
        let target = std::env::var_os("CARGO_TARGET_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| repo.join("target"));
        target.join(self.profile()).join(crate::system::SERVICE)
    }

    async fn head(&self, repo: &Path) -> Option<String> {
        let out = Command::new("git")
            .args(["rev-parse", "--short", "HEAD"])
            .current_dir(repo)
            .output()
            .await
            .ok()?;
        out.status
            .success()
            .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
    }

    /// True when the binary predates the commit that is checked out — which is
    /// what "somebody already pulled by hand" looks like from here.
    async fn binary_is_older_than_head(&self, repo: &Path, binary: &Path) -> bool {
        let Ok(meta) = tokio::fs::metadata(binary).await else {
            return true; // nothing built at all
        };
        let Ok(built) = meta.modified() else {
            return false;
        };
        let Ok(out) = Command::new("git")
            .args(["log", "-1", "--format=%ct"])
            .current_dir(repo)
            .output()
            .await
        else {
            return false;
        };
        let Ok(committed) = String::from_utf8_lossy(&out.stdout).trim().parse::<u64>() else {
            return false;
        };
        built
            .duration_since(std::time::UNIX_EPOCH)
            .is_ok_and(|since| since.as_secs() < committed)
    }

    /// Run a command in the checkout and copy everything it says into the log
    /// the dashboard is watching.
    async fn run_command(self: &Arc<Self>, program: &str, args: &[&str], dir: &Path) -> Result<()> {
        self.log(&format!("$ {program} {}", args.join(" ")));
        let mut child = Command::new(program)
            .args(args)
            .current_dir(dir)
            // A build started from the dashboard has no terminal, and cargo's
            // progress bars are unreadable without one.
            .env("CARGO_TERM_COLOR", "never")
            .env("CARGO_TERM_PROGRESS_WHEN", "never")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .with_context(|| format!("could not run {program}"))?;

        if let Some(stdout) = child.stdout.take() {
            self.watch(BufReader::new(stdout));
        }
        if let Some(stderr) = child.stderr.take() {
            self.watch(BufReader::new(stderr));
        }

        let status = child.wait().await?;
        if !status.success() {
            bail!("{program} exited with {status}");
        }
        Ok(())
    }

    fn watch<R>(self: &Arc<Self>, reader: BufReader<R>)
    where
        R: tokio::io::AsyncRead + Unpin + Send + 'static,
    {
        let updater = self.clone();
        tokio::spawn(async move {
            let mut lines = reader.lines();
            while let Ok(Some(line)) = lines.next_line().await {
                updater.log(&line);
            }
        });
    }

    fn log(&self, line: &str) {
        let text = line.trim_end();
        if text.is_empty() {
            return;
        }
        let mut p = self.progress.lock();
        p.lines.push(text.to_string());
        if p.lines.len() > MAX_LOG_LINES {
            let excess = p.lines.len() - MAX_LOG_LINES;
            p.lines.drain(..excess);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::logging::Level;

    fn updater_at(root: &Path) -> Arc<Updater> {
        Arc::new(Updater::new(
            Paths::resolve(Some(root)),
            Logger::console(Level::Silent),
        ))
    }

    fn scratch() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("chtting-update-{}", crate::util::new_id("u")));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[tokio::test]
    async fn a_directory_that_is_not_a_checkout_is_reported_rather_than_guessed_at() {
        // `Paths::resolve` reads the working directory for `root`, so point it
        // at somewhere that has no `.git` above it.
        let dir = scratch();
        let mut paths = Paths::resolve(Some(&dir));
        paths.root = dir.clone();
        let updater = Arc::new(Updater::new(paths, Logger::console(Level::Silent)));

        assert!(updater.repo().is_none());
        let err = updater.run().await.unwrap_err().to_string();
        assert!(err.contains("no git checkout"), "unhelpful error: {err}");
        assert_eq!(updater.status()["step"], "failed");
    }

    #[tokio::test]
    async fn the_checkout_is_found_by_walking_up_from_the_working_directory() {
        let dir = scratch();
        std::fs::create_dir_all(dir.join(".git")).unwrap();
        std::fs::create_dir_all(dir.join("src/relay")).unwrap();

        let mut paths = Paths::resolve(Some(&dir));
        paths.root = dir.join("src/relay");
        let updater = Arc::new(Updater::new(paths, Logger::console(Level::Silent)));
        assert_eq!(updater.repo().as_deref(), Some(dir.as_path()));
    }

    #[tokio::test]
    async fn two_updates_at_once_are_refused_rather_than_racing_the_same_target_dir() {
        let dir = scratch();
        let updater = updater_at(&dir);
        updater.busy.store(true, Ordering::SeqCst);
        let err = updater.run().await.unwrap_err().to_string();
        assert!(err.contains("already running"), "unhelpful error: {err}");
    }

    #[test]
    fn the_build_lands_where_the_running_binary_came_from() {
        let dir = scratch();
        let updater = updater_at(&dir);
        let binary = updater.binary_path(Path::new("/repo"));
        // Whichever profile this test binary implies, the path is inside the
        // checkout's target directory and named after the service.
        assert!(binary.ends_with(format!("{}/{}", updater.profile(), crate::system::SERVICE)));
    }

    /// Run git in a directory, with an identity, so a machine with no global
    /// git config can still make a commit.
    fn git(args: &[&str], dir: &Path) {
        std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .env("GIT_AUTHOR_NAME", "test")
            .env("GIT_AUTHOR_EMAIL", "test@example.invalid")
            .env("GIT_COMMITTER_NAME", "test")
            .env("GIT_COMMITTER_EMAIL", "test@example.invalid")
            .output()
            .unwrap();
    }

    /// A throwaway origin and a clone of it, which is the shape the relay is
    /// actually installed in.
    async fn checkout_with_origin() -> Option<(PathBuf, PathBuf)> {
        // No git on this machine: the caller skips, and the rest of the suite
        // still covers everything that does not shell out.
        crate::system::on_path("git")?;
        let base = scratch();
        let origin = base.join("origin.git");
        let seed = base.join("seed");
        let clone = base.join("clone");

        git(&["init", "--bare", "-b", "main", "origin.git"], &base);
        std::fs::create_dir_all(&seed).unwrap();
        git(&["init", "-b", "main"], &seed);
        std::fs::write(seed.join("README"), "one\n").unwrap();
        git(&["add", "-A"], &seed);
        git(&["commit", "-m", "first"], &seed);
        git(
            &["remote", "add", "origin", origin.to_str().unwrap()],
            &seed,
        );
        git(&["push", "-u", "origin", "main"], &seed);
        git(
            &["clone", origin.to_str().unwrap(), clone.to_str().unwrap()],
            &base,
        );
        clone.join(".git").exists().then_some((seed, clone))
    }

    fn updater_for(checkout: &Path) -> Arc<Updater> {
        let mut paths = Paths::resolve(Some(checkout));
        paths.root = checkout.to_path_buf();
        Arc::new(Updater::new(paths, Logger::console(Level::Silent)))
    }

    /// A binary that is newer than the commit it was built from, which is how
    /// the updater tells "already built" from "somebody pulled by hand".
    fn fake_binary(updater: &Updater, checkout: &Path) -> PathBuf {
        let binary = updater.binary_path(checkout);
        std::fs::create_dir_all(binary.parent().unwrap()).unwrap();
        std::fs::write(&binary, b"#!/bin/sh\nexit 0\n").unwrap();
        binary
    }

    #[tokio::test]
    async fn a_checkout_that_is_already_current_is_not_rebuilt_for_nothing() {
        let Some((_seed, clone)) = checkout_with_origin().await else {
            return; // no git here; the rest of the suite still covers the logic
        };
        let updater = updater_for(&clone);
        fake_binary(&updater, &clone);

        let built = updater.run().await.expect("the pull should succeed");
        assert!(
            built.is_none(),
            "nothing changed, so nothing to restart into"
        );
        assert_eq!(updater.status()["step"], "up-to-date");
        assert!(
            updater.status()["logs"]
                .as_array()
                .unwrap()
                .iter()
                .any(|line| line.as_str().unwrap_or("").contains("git pull")),
            "the pull should be in the log: {:?}",
            updater.status()["logs"]
        );
    }

    /// The property the whole design turns on: a build that does not finish
    /// leaves the relay exactly as it was, with a binary still on disk for the
    /// keeper to start.
    #[tokio::test]
    async fn a_build_that_fails_puts_the_previous_binary_back_and_says_why() {
        let Some((seed, clone)) = checkout_with_origin().await else {
            return;
        };
        if crate::system::on_path("cargo").is_none() {
            return;
        }
        let updater = updater_for(&clone);
        let binary = fake_binary(&updater, &clone);

        // A second commit upstream, so the pull has something to fetch and the
        // update goes on to build it. The checkout is not a cargo project, so
        // the build fails immediately — which is the case being tested.
        std::fs::write(seed.join("README"), "two\n").unwrap();
        git(&["add", "-A"], &seed);
        git(&["commit", "-m", "second"], &seed);
        git(&["push"], &seed);

        let err = updater.run().await.unwrap_err().to_string();
        assert!(err.contains("build"), "unhelpful error: {err}");
        assert_eq!(updater.status()["step"], "failed");
        assert!(
            binary.exists(),
            "the previous binary was left where the linker wanted it"
        );
        // The pull itself did happen: this is a build failure, not a pull one.
        let status = updater.status();
        assert_ne!(status["before"], status["after"]);
    }

    #[test]
    fn the_log_does_not_grow_without_end() {
        let dir = scratch();
        let updater = updater_at(&dir);
        for i in 0..(MAX_LOG_LINES + 50) {
            updater.log(&format!("line {i}"));
        }
        let logs = updater.status()["logs"].as_array().unwrap().len();
        assert_eq!(logs, MAX_LOG_LINES);
    }
}
