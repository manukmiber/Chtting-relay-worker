//! Supervises a `cloudflared` child process so the relay on your phone gets a
//! public HTTPS URL without port forwarding.
//!
//! Three modes:
//!
//! * `quick` — a throwaway `*.trycloudflare.com` URL, no Cloudflare account
//! * `named` — a token-backed named tunnel bound to your own hostname
//! * a `config.yml` you manage yourself
//!
//! One manager supervises one cloudflared, publishing one port. There are two
//! of them: [`Scope::Relay`] publishes the relay port, and [`Scope::Dashboard`]
//! publishes the dashboard port so the control panel can be reached from
//! another device.
//!
//! They are separate processes with separate configuration and separate URLs,
//! and neither can publish the other's port — `build_args` reads the port off
//! its own scope, so there is no configuration in which one tunnel exposes
//! both. That matters: the relay's URL is meant to be handed out, and the
//! dashboard's is a way into the config, the prompts and the client keys.
//!
//! The dashboard tunnel is off by default and [`TunnelManager::start`] refuses
//! to start it without a dashboard password of at least
//! [`MIN_REMOTE_PASSWORD`](crate::config::MIN_REMOTE_PASSWORD) characters. On
//! loopback a weak password guards a surface only this device can reach; behind
//! a tunnel it is the only thing between the internet and a `GET` that returns
//! every client key in the clear.

use anyhow::{anyhow, Result};
use parking_lot::Mutex;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};

use crate::config::ConfigStore;
use crate::logging::Logger;

const MAX_LOG_LINES: usize = 500;

/// Which port a manager publishes, and therefore which config it reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    /// The relay itself. Meant to be public; this is the URL you hand out.
    Relay,
    /// The dashboard. Meant for you, from your other device, and nobody else.
    Dashboard,
}

impl Scope {
    pub fn as_str(self) -> &'static str {
        match self {
            Scope::Relay => "relay",
            Scope::Dashboard => "dashboard",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Stopped,
    Starting,
    Running,
    Failed,
}

impl State {
    fn label(self) -> &'static str {
        match self {
            State::Stopped => "stopped",
            State::Starting => "starting",
            State::Running => "running",
            State::Failed => "failed",
        }
    }
}

#[derive(Default)]
struct Inner {
    url: String,
    state_label: Option<State>,
    last_error: String,
    started_at: i64,
    /// Every restart since the relay started, for the dashboard.
    restarts: u32,
    /// Restarts since the last time it actually came up, which is what the
    /// backoff is calculated from.
    consecutive_failures: u32,
    lines: Vec<String>,
    pid: Option<u32>,
}

/// A handle on the cloudflared that is currently up.
///
/// The [`Child`] itself is *not* here: it belongs to the supervisor task, which
/// is the only thing that may await it. Keeping it in a mutex and holding that
/// mutex across `child.wait()` is what used to make Stop and Restart hang —
/// they waited for a lock the supervisor only released when cloudflared exited
/// on its own, which is precisely what they were trying to make happen.
struct Running {
    /// Dropped or sent to ask the supervisor to kill the process.
    stop: tokio::sync::oneshot::Sender<()>,
    /// Resolves once the supervisor has killed and reaped it, so a stop can
    /// promise the port is actually free before a restart claims it again.
    reaped: tokio::sync::oneshot::Receiver<()>,
}

pub struct TunnelManager {
    scope: Scope,
    config: Arc<ConfigStore>,
    logger: Arc<Logger>,
    inner: Mutex<Inner>,
    running: tokio::sync::Mutex<Option<Running>>,
    /// Set while a deliberate stop is in progress, so the supervisor does not
    /// treat the exit as a crash and restart it — and so a supervisor already
    /// sleeping out its backoff does not bring one back after a stop.
    stopping: Arc<AtomicBool>,
}

impl TunnelManager {
    /// The relay's own tunnel.
    pub fn new(config: Arc<ConfigStore>, logger: Arc<Logger>) -> Self {
        Self::scoped(Scope::Relay, config, logger)
    }

    /// The dashboard's, which publishes the control panel instead.
    pub fn for_dashboard(config: Arc<ConfigStore>, logger: Arc<Logger>) -> Self {
        Self::scoped(Scope::Dashboard, config, logger)
    }

    pub fn scoped(scope: Scope, config: Arc<ConfigStore>, logger: Arc<Logger>) -> Self {
        Self {
            scope,
            config,
            logger,
            inner: Mutex::new(Inner::default()),
            running: tokio::sync::Mutex::new(None),
            stopping: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn scope(&self) -> Scope {
        self.scope
    }

    /// This manager's own slice of the config.
    fn settings(&self) -> crate::config::TunnelConfig {
        let cfg = self.config.current();
        match self.scope {
            Scope::Relay => cfg.tunnel.clone(),
            Scope::Dashboard => cfg.dashboard.tunnel.clone(),
        }
    }

    /// The one port this manager may publish. Read from the scope rather than
    /// passed in, so there is no call site that could publish the other one.
    fn target_port(&self) -> u16 {
        let cfg = self.config.current();
        match self.scope {
            Scope::Relay => cfg.server.port,
            Scope::Dashboard => cfg.dashboard.port,
        }
    }

    /// The hostname this tunnel is reachable at right now, if it is up.
    ///
    /// A quick tunnel's name is scraped out of cloudflared's output; a named
    /// one's is whatever the operator configured, because cloudflared does not
    /// print it. Used by the dashboard's origin guard to recognise its own
    /// public name without the guard having to be switched off.
    pub fn public_host(&self) -> Option<String> {
        let from_url = {
            let inner = self.inner.lock();
            inner
                .url
                .split_once("//")
                .map(|(_, rest)| rest.trim_end_matches('/').to_string())
                .filter(|h| !h.is_empty())
        };
        from_url.or_else(|| {
            let configured = self.settings().hostname.trim().to_string();
            (!configured.is_empty()).then_some(configured)
        })
    }

    /// Why this tunnel may not be started, if it may not be.
    ///
    /// Only the dashboard has an answer here, and it is the password. The
    /// check lives at `start` rather than in the config validator on purpose:
    /// a config that *describes* a dashboard tunnel is fine to save, and the
    /// moment that matters is the moment a process would actually begin
    /// accepting traffic from the internet.
    fn refuse_to_publish(&self) -> Option<String> {
        if self.scope != Scope::Dashboard {
            return None;
        }
        let password = self.config.current().dashboard.password.clone();
        let len = password.chars().count();
        if len >= crate::config::MIN_REMOTE_PASSWORD {
            return None;
        }
        Some(format!(
            "refusing to publish the dashboard: it needs a password of at least {} characters \
             first (this one is {}). Behind a tunnel that password is the only thing between \
             the internet and a control panel that writes the config, reads every stored prompt \
             and reveals every client key. Set one under Settings → Server.",
            crate::config::MIN_REMOTE_PASSWORD,
            if len == 0 {
                "not set".to_string()
            } else {
                len.to_string()
            },
        ))
    }

    pub fn status(&self) -> serde_json::Value {
        let settings = self.settings();
        let port = self.target_port();
        let blocked = self.refuse_to_publish();
        let inner = self.inner.lock();
        let state = inner.state_label.unwrap_or(State::Stopped);
        serde_json::json!({
            "scope": self.scope.as_str(),
            "publishes": port,
            // Why Start would refuse right now, so the dashboard can say so
            // before somebody presses it rather than after.
            "blocked": blocked,
            "state": state.label(),
            "url": inner.url,
            "mode": settings.mode,
            "pid": inner.pid,
            "startedAt": inner.started_at,
            "uptime_s": if inner.started_at > 0 {
                (crate::util::now_ms() - inner.started_at) / 1000
            } else { 0 },
            "restarts": inner.restarts,
            "autoStart": settings.auto_start,
            "lastError": inner.last_error,
            "logs": inner.lines.iter().rev().take(200).rev().collect::<Vec<_>>(),
        })
    }

    pub async fn version(&self) -> serde_json::Value {
        let bin = self.binary();
        match Command::new(&bin).arg("--version").output().await {
            Ok(out) if out.status.success() => serde_json::json!({
                "installed": true,
                "version": String::from_utf8_lossy(&out.stdout).trim(),
                "binary": bin,
            }),
            Ok(out) => serde_json::json!({
                "installed": false,
                "version": "",
                "binary": bin,
                "error": String::from_utf8_lossy(&out.stderr).trim(),
            }),
            Err(err) => serde_json::json!({
                "installed": false,
                "version": "",
                "binary": bin,
                "error": err.to_string(),
            }),
        }
    }

    fn binary(&self) -> String {
        let binary = self.settings().binary;
        if binary.trim().is_empty() {
            "cloudflared".into()
        } else {
            binary
        }
    }

    /// Only this manager's own port is ever published.
    pub fn build_args(&self) -> Vec<String> {
        let t = self.settings();
        let t = &t;
        let mut args = vec!["--no-autoupdate".to_string()];

        if t.mode == "named" && !t.token.is_empty() {
            args.extend([
                "tunnel".into(),
                "run".into(),
                "--token".into(),
                t.token.clone(),
            ]);
            args.extend(t.extra_args.iter().cloned());
            return args;
        }
        if t.mode == "named" && !t.config_file.is_empty() {
            args.extend([
                "--config".into(),
                t.config_file.clone(),
                "tunnel".into(),
                "run".into(),
            ]);
            args.extend(t.extra_args.iter().cloned());
            return args;
        }

        args.extend([
            "tunnel".into(),
            "--url".into(),
            format!("http://127.0.0.1:{}", self.target_port()),
            // trycloudflare needs no credentials but does need a protocol it
            // can use on mobile networks; http2 survives carrier NAT better
            // than quic does.
            "--protocol".into(),
            "http2".into(),
        ]);
        args.extend(t.extra_args.iter().cloned());
        args
    }

    pub async fn start(self: &Arc<Self>) -> Result<serde_json::Value> {
        // Held for the whole of a start, so two callers cannot both decide
        // nothing is running and spawn a cloudflared each. Nothing under this
        // lock ever waits on the tunnel process itself, so a stop is never
        // blocked for longer than it takes to spawn one.
        let mut running = self.running.lock().await;
        if running.is_some() {
            return Ok(self.status());
        }

        let settings = self.settings();
        if settings.mode == "off" {
            return Err(anyhow!(
                "tunnel mode is \"off\"; set it to quick or named first"
            ));
        }
        // Checked here and nowhere earlier: this is the last moment before a
        // process starts accepting traffic from the internet on this port.
        if let Some(why) = self.refuse_to_publish() {
            return Err(anyhow!(why));
        }

        let check = self.version().await;
        if check["installed"] != serde_json::Value::Bool(true) {
            return Err(anyhow!(
                "cloudflared not found (tried \"{}\"). Install it with: pkg install cloudflared",
                self.binary()
            ));
        }

        let args = self.build_args();
        {
            let mut inner = self.inner.lock();
            inner.state_label = Some(State::Starting);
            inner.url.clear();
            inner.last_error.clear();
            inner.started_at = crate::util::now_ms();
        }
        self.stopping.store(false, Ordering::SeqCst);
        self.log(&format!(
            "starting {} tunnel: {} {}",
            self.scope.as_str(),
            self.binary(),
            args.iter()
                .map(|a| redact_arg(a))
                .collect::<Vec<_>>()
                .join(" ")
        ));

        let mut child = Command::new(self.binary())
            .args(&args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|err| anyhow!("could not start cloudflared: {err}"))?;

        self.inner.lock().pid = child.id();

        if let Some(stdout) = child.stdout.take() {
            self.watch_output(Box::pin(BufReader::new(stdout)));
        }
        if let Some(stderr) = child.stderr.take() {
            // cloudflared writes its status, including the quick URL, to stderr.
            self.watch_output(Box::pin(BufReader::new(stderr)));
        }

        let (stop_tx, stop_rx) = tokio::sync::oneshot::channel();
        let (reaped_tx, reaped_rx) = tokio::sync::oneshot::channel();
        *running = Some(Running {
            stop: stop_tx,
            reaped: reaped_rx,
        });
        drop(running);

        self.supervise(child, stop_rx, reaped_tx);
        Ok(self.status())
    }

    fn watch_output<R>(self: &Arc<Self>, reader: std::pin::Pin<Box<BufReader<R>>>)
    where
        R: tokio::io::AsyncRead + Send + 'static,
    {
        let manager = self.clone();
        tokio::spawn(async move {
            let mut lines = reader.lines();
            while let Ok(Some(line)) = lines.next_line().await {
                manager.log(&line);
            }
        });
    }

    /// Own the child: wait for it to exit, or kill it when asked to, and bring
    /// it back if it went without being asked.
    ///
    /// This task is the only owner of the [`Child`]. Everything else talks to it
    /// through the `stop` channel, which is what lets a stop arrive *while* the
    /// process is running rather than only after it has already gone.
    fn supervise(
        self: &Arc<Self>,
        mut child: Child,
        stop: tokio::sync::oneshot::Receiver<()>,
        reaped: tokio::sync::oneshot::Sender<()>,
    ) {
        let manager = self.clone();
        tokio::spawn(async move {
            let mut exited = None;
            // Either cloudflared goes on its own, or somebody asks us to end it.
            let asked_to_stop = tokio::select! {
                status = child.wait() => {
                    exited = Some(status);
                    false
                }
                _ = stop => true,
            };
            let status = match exited {
                Some(status) => status,
                None => {
                    let _ = child.start_kill();
                    child.wait().await
                }
            };

            // The process is gone either way: drop the handle so a later start
            // is not refused by a tunnel that is no longer there, and say so to
            // whoever asked, before anything below can take time.
            let _ = manager.running.lock().await.take();
            manager.inner.lock().pid = None;
            let _ = reaped.send(());

            if asked_to_stop || manager.stopping.load(Ordering::SeqCst) {
                manager.inner.lock().state_label = Some(State::Stopped);
                return;
            }

            let code = status
                .map(|s| s.to_string())
                .unwrap_or_else(|e| e.to_string());
            {
                let mut inner = manager.inner.lock();
                inner.state_label = Some(State::Failed);
                inner.last_error = format!("cloudflared exited: {code}");
            }
            manager.log(&format!("cloudflared exited ({code})"));

            if !manager.settings().auto_start {
                return;
            }
            // Mobile links drop; back off so a hard failure does not spin.
            let streak = {
                let mut inner = manager.inner.lock();
                inner.restarts += 1;
                inner.consecutive_failures += 1;
                inner.consecutive_failures
            };
            let wait = Duration::from_millis((1_000u64 << streak.min(5)).min(60_000));
            manager.log(&format!("restarting in {}s", wait.as_secs()));
            tokio::time::sleep(wait).await;
            // A stop that arrived while we were backing off had no process to
            // signal, so it is read here instead: coming back now would undo it.
            if manager.stopping.load(Ordering::SeqCst) {
                manager.inner.lock().state_label = Some(State::Stopped);
                return;
            }
            if let Err(err) = manager.start().await {
                manager.logger.warn(format!(
                    "{} tunnel restart failed: {err}",
                    manager.scope.as_str()
                ));
            }
        });
    }

    /// Stop the tunnel, and do not come back until it is actually down.
    ///
    /// The wait matters: [`restart`](Self::restart) binds a new cloudflared
    /// straight afterwards, and a stop that returned while the old one was
    /// still exiting would leave two of them fighting over the same tunnel.
    pub async fn stop(&self) -> Result<serde_json::Value> {
        self.stopping.store(true, Ordering::SeqCst);
        // Taken out from under the lock and awaited outside it: the supervisor
        // needs that same lock to report the process gone.
        let running = self.running.lock().await.take();
        if let Some(running) = running {
            let _ = running.stop.send(());
            // An error means the supervisor is already finished, which is the
            // outcome being waited for.
            let _ = running.reaped.await;
        }
        {
            let mut inner = self.inner.lock();
            inner.state_label = Some(State::Stopped);
            inner.url.clear();
            inner.pid = None;
        }
        self.log("stopped");
        Ok(self.status())
    }

    pub async fn restart(self: &Arc<Self>) -> Result<serde_json::Value> {
        self.stop().await?;
        tokio::time::sleep(Duration::from_millis(300)).await;
        self.start().await
    }

    fn log(&self, line: &str) {
        let text = line.trim_end();
        if text.is_empty() {
            return;
        }
        let stamped = format!("{} {text}", chrono::Utc::now().to_rfc3339());

        let mut inner = self.inner.lock();
        inner.lines.push(stamped);
        if inner.lines.len() > MAX_LOG_LINES {
            let excess = inner.lines.len() - MAX_LOG_LINES;
            inner.lines.drain(..excess);
        }

        if let Some(url) = find_quick_url(text) {
            if inner.url != url {
                inner.url = url.clone();
                inner.state_label = Some(State::Running);
                // Back up and running: the next failure starts its backoff from
                // one second again, rather than from wherever the last streak
                // of failures left it.
                inner.consecutive_failures = 0;
                drop(inner);
                self.logger.info(format!(
                    "cloudflared {} tunnel URL: {url}",
                    self.scope.as_str()
                ));
                return;
            }
        }
        if inner.state_label != Some(State::Running)
            && (text.contains("Registered tunnel connection") || text.contains("registered"))
        {
            inner.state_label = Some(State::Running);
            inner.consecutive_failures = 0;
        }
    }

    /// Start the tunnel and keep it started.
    ///
    /// Requirement 9: after the phone reboots, or Termux is killed and comes
    /// back, the tunnel has to be up again without anyone opening a terminal.
    /// The relay starting is the only event that reliably happens then, so this
    /// hangs off it: one attempt now, and a watcher that keeps trying if
    /// cloudflared is not installed yet or the network is not up.
    ///
    /// [`supervise`](Self::supervise) covers the process dying later. This
    /// covers it never having started.
    pub fn ensure_running(self: &Arc<Self>) {
        let manager = self.clone();
        tokio::spawn(async move {
            let mut wait = Duration::from_secs(5);
            loop {
                let settings = manager.settings();
                if !settings.auto_start || settings.mode == "off" {
                    return;
                }
                // A dashboard tunnel with no password set never comes up on its
                // own and never will until somebody sets one, so say it once
                // and stop rather than retrying every two minutes for ever.
                if let Some(why) = manager.refuse_to_publish() {
                    manager.logger.warn(why);
                    return;
                }
                if manager.running.lock().await.is_some() {
                    return; // already up, and supervise() owns it from here
                }
                match manager.start().await {
                    Ok(_) => return,
                    Err(err) => {
                        manager.logger.warn(format!(
                            "{} tunnel not up yet ({err}); retrying in {wait:?}",
                            manager.scope.as_str()
                        ));
                        tokio::time::sleep(wait).await;
                        // A phone that has just booted may have no network for
                        // a while; back off, but never stop trying.
                        wait = (wait * 2).min(Duration::from_secs(120));
                    }
                }
            }
        });
    }
}

/// Pull a `https://<something>.trycloudflare.com` URL out of a log line.
fn find_quick_url(line: &str) -> Option<String> {
    let start = line.find("https://")?;
    let rest = &line[start..];
    let end = rest
        .find(|c: char| c.is_whitespace() || c == '|' || c == '"')
        .unwrap_or(rest.len());
    let url = &rest[..end];
    url.trim_end_matches('/')
        .ends_with(".trycloudflare.com")
        .then(|| url.trim_end_matches('/').to_string())
}

/// Never print a tunnel token into the log buffer the dashboard shows.
fn redact_arg(arg: &str) -> String {
    if arg.len() > 24 && arg.starts_with("ey") {
        // By characters, never by bytes: `&arg[..6]` panics the moment byte 6
        // lands inside a multi-byte character, and a token is whatever the
        // operator pasted into the config rather than something guaranteed
        // ASCII. Taking the connector down over the way its own log line is
        // written is not a trade worth making.
        let head: String = arg.chars().take(6).collect();
        return format!("{head}…<redacted>");
    }
    arg.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use std::path::PathBuf;

    async fn manager_with(tunnel: crate::config::TunnelConfig, port: u16) -> Arc<TunnelManager> {
        let dir = std::env::temp_dir().join(format!("chtting-tunnel-{}", crate::util::new_id("t")));
        tokio::fs::create_dir_all(&dir).await.unwrap();
        let file = dir.join("config.json");

        let mut cfg = Config {
            tunnel,
            ..Default::default()
        };
        cfg.server.port = port;
        // A model-free config is valid, which is all this test needs.
        tokio::fs::write(&file, serde_json::to_string(&cfg).unwrap())
            .await
            .unwrap();

        let store = Arc::new(ConfigStore::load(&file).await.unwrap());
        Arc::new(TunnelManager::new(
            store,
            Logger::console(crate::logging::Level::Silent),
        ))
    }

    fn quick() -> crate::config::TunnelConfig {
        crate::config::TunnelConfig {
            mode: "quick".into(),
            ..Default::default()
        }
    }

    /// Build a manager for either scope over a whole config, so the dashboard
    /// tunnel's own settings and password can be set.
    async fn manager_for(scope: Scope, build: impl FnOnce(&mut Config)) -> Arc<TunnelManager> {
        let dir = std::env::temp_dir().join(format!("chtting-tunnel-{}", crate::util::new_id("t")));
        tokio::fs::create_dir_all(&dir).await.unwrap();
        let file = dir.join("config.json");
        let mut cfg = Config::default();
        build(&mut cfg);
        tokio::fs::write(&file, serde_json::to_string(&cfg).unwrap())
            .await
            .unwrap();
        let store = Arc::new(ConfigStore::load(&file).await.unwrap());
        Arc::new(TunnelManager::scoped(
            scope,
            store,
            Logger::console(crate::logging::Level::Silent),
        ))
    }

    /// The dashboard tunnel publishes the dashboard port, and only that.
    ///
    /// The two managers read their port from their own scope rather than from
    /// an argument, so there is no configuration in which one publishes the
    /// other's port — which is the whole reason the scope exists.
    #[tokio::test]
    async fn the_dashboard_tunnel_publishes_the_dashboard_port_and_no_other() {
        let manager = manager_for(Scope::Dashboard, |cfg| {
            cfg.server.port = 9001;
            cfg.dashboard.port = 9788;
            cfg.dashboard.password = "a-long-enough-password".into();
            cfg.dashboard.tunnel = crate::config::TunnelConfig {
                mode: "quick".into(),
                ..Default::default()
            };
        })
        .await;

        let args = manager.build_args();
        assert!(
            args.contains(&"http://127.0.0.1:9788".to_string()),
            "{args:?}"
        );
        assert!(
            !args.join(" ").contains("9001"),
            "the dashboard tunnel must never publish the relay port: {args:?}"
        );
        assert_eq!(manager.status()["scope"], "dashboard");
        assert_eq!(manager.status()["publishes"], 9788);
    }

    /// The relay's tunnel reads the relay's settings, not the dashboard's.
    #[tokio::test]
    async fn the_two_tunnels_read_their_own_settings() {
        let build = |cfg: &mut Config| {
            cfg.server.port = 9001;
            cfg.dashboard.port = 9788;
            cfg.dashboard.password = "a-long-enough-password".into();
            cfg.tunnel = crate::config::TunnelConfig {
                mode: "named".into(),
                token: "eyJyZWxheSI".into(),
                ..Default::default()
            };
            cfg.dashboard.tunnel = crate::config::TunnelConfig {
                mode: "named".into(),
                token: "eyJkYXNoIn".into(),
                ..Default::default()
            };
        };

        let relay = manager_for(Scope::Relay, build).await;
        let dash = manager_for(Scope::Dashboard, build).await;
        assert!(relay.build_args().contains(&"eyJyZWxheSI".to_string()));
        assert!(dash.build_args().contains(&"eyJkYXNoIn".to_string()));
        assert!(!relay.build_args().contains(&"eyJkYXNoIn".to_string()));
        assert!(!dash.build_args().contains(&"eyJyZWxheSI".to_string()));
    }

    /// The control this whole feature turns on.
    ///
    /// Behind a tunnel the dashboard password is the only thing between the
    /// internet and a `GET` that returns every client key in the clear, so the
    /// tunnel does not start without a real one.
    #[tokio::test]
    async fn the_dashboard_tunnel_refuses_to_start_without_a_real_password() {
        for password in ["", "hunter2", "short-pass"] {
            let manager = manager_for(Scope::Dashboard, |cfg| {
                cfg.dashboard.password = password.into();
                cfg.dashboard.tunnel = crate::config::TunnelConfig {
                    mode: "quick".into(),
                    ..Default::default()
                };
            })
            .await;

            let err = manager.start().await.unwrap_err().to_string();
            assert!(
                err.contains("refusing to publish the dashboard"),
                "{password:?} was accepted: {err}"
            );
            assert!(err.contains("16"), "the requirement is not stated: {err}");
            // And the dashboard can see the refusal before anybody presses
            // Start, rather than only after.
            assert!(manager.status()["blocked"].is_string());
        }
    }

    /// A long enough password gets past the check — it is the password that is
    /// refused, not the feature.
    #[tokio::test]
    async fn a_long_password_lets_the_dashboard_tunnel_through() {
        let manager = manager_for(Scope::Dashboard, |cfg| {
            cfg.dashboard.password = "correct-horse-battery-staple".into();
            cfg.dashboard.tunnel = crate::config::TunnelConfig {
                mode: "quick".into(),
                binary: "definitely-not-installed-cloudflared".into(),
                ..Default::default()
            };
        })
        .await;

        assert!(manager.status()["blocked"].is_null());
        // It gets as far as looking for cloudflared, which is past the guard.
        let err = manager.start().await.unwrap_err().to_string();
        assert!(err.contains("not found"), "{err}");
    }

    /// The relay's own tunnel has never needed a dashboard password and must
    /// not start needing one.
    #[tokio::test]
    async fn the_relay_tunnel_is_not_held_to_the_dashboard_password() {
        let manager = manager_for(Scope::Relay, |cfg| {
            cfg.dashboard.password = String::new();
            cfg.tunnel = crate::config::TunnelConfig {
                mode: "quick".into(),
                binary: "definitely-not-installed-cloudflared".into(),
                ..Default::default()
            };
        })
        .await;
        assert!(manager.status()["blocked"].is_null());
        assert!(manager
            .start()
            .await
            .unwrap_err()
            .to_string()
            .contains("not found"));
    }

    /// The dashboard tunnel is off unless somebody turns it on. Publishing a
    /// control panel is a decision, and a default that made it for you would
    /// be the wrong one every time.
    #[test]
    fn the_dashboard_tunnel_is_off_by_default() {
        let cfg = Config::default();
        assert_eq!(cfg.dashboard.tunnel.mode, "off");
        assert!(!cfg.dashboard.tunnel.auto_start);
        // While the relay's own stays on, because a relay whose tunnel has to
        // be started by hand is down after every reboot.
        assert_eq!(cfg.tunnel.mode, "quick");
        assert!(cfg.tunnel.auto_start);
    }

    /// The origin guard needs to recognise the name the dashboard is published
    /// at, and a named tunnel's name is only in the config.
    #[tokio::test]
    async fn the_public_hostname_falls_back_to_the_configured_one() {
        let manager = manager_for(Scope::Dashboard, |cfg| {
            cfg.dashboard.password = "correct-horse-battery-staple".into();
            cfg.dashboard.tunnel = crate::config::TunnelConfig {
                mode: "named".into(),
                hostname: "panel.example.com".into(),
                ..Default::default()
            };
        })
        .await;
        assert_eq!(manager.public_host().as_deref(), Some("panel.example.com"));

        // A quick tunnel has no configured name, and nothing to report until
        // cloudflared has printed one.
        let quick = manager_for(Scope::Dashboard, |cfg| {
            cfg.dashboard.password = "correct-horse-battery-staple".into();
            cfg.dashboard.tunnel = crate::config::TunnelConfig {
                mode: "quick".into(),
                ..Default::default()
            };
        })
        .await;
        assert_eq!(quick.public_host(), None);
    }

    #[tokio::test]
    async fn quick_mode_publishes_only_the_relay_port() {
        let manager = manager_with(quick(), 9001).await;
        let args = manager.build_args();
        assert_eq!(
            args,
            vec![
                "--no-autoupdate",
                "tunnel",
                "--url",
                "http://127.0.0.1:9001",
                "--protocol",
                "http2"
            ]
        );
        assert!(
            !args.join(" ").contains("8788"),
            "the dashboard port must never be published"
        );
    }

    #[tokio::test]
    async fn named_mode_runs_the_token_backed_tunnel() {
        let manager = manager_with(
            crate::config::TunnelConfig {
                mode: "named".into(),
                token: "eyJhIjoiabc".into(),
                ..Default::default()
            },
            8787,
        )
        .await;
        assert_eq!(
            manager.build_args(),
            vec!["--no-autoupdate", "tunnel", "run", "--token", "eyJhIjoiabc"]
        );
    }

    #[tokio::test]
    async fn named_mode_falls_back_to_a_config_file_when_no_token_is_set() {
        let manager = manager_with(
            crate::config::TunnelConfig {
                mode: "named".into(),
                config_file: "/home/u/.cloudflared/config.yml".into(),
                ..Default::default()
            },
            8787,
        )
        .await;
        assert_eq!(
            manager.build_args(),
            vec![
                "--no-autoupdate",
                "--config",
                "/home/u/.cloudflared/config.yml",
                "tunnel",
                "run"
            ]
        );
    }

    #[tokio::test]
    async fn extra_arguments_are_passed_through() {
        let manager = manager_with(
            crate::config::TunnelConfig {
                mode: "quick".into(),
                extra_args: vec!["--loglevel".into(), "debug".into()],
                ..Default::default()
            },
            8787,
        )
        .await;
        let args = manager.build_args();
        assert!(args.contains(&"--loglevel".to_string()) && args.contains(&"debug".to_string()));
    }

    #[tokio::test]
    async fn a_missing_binary_is_reported_rather_than_crashing() {
        let manager = manager_with(
            crate::config::TunnelConfig {
                mode: "quick".into(),
                binary: "definitely-not-installed-cloudflared".into(),
                ..Default::default()
            },
            8787,
        )
        .await;
        assert_eq!(manager.version().await["installed"], false);
        let err = manager.start().await.unwrap_err().to_string();
        assert!(err.contains("not found"), "unhelpful error: {err}");
    }

    #[tokio::test]
    async fn mode_off_refuses_to_start() {
        let manager = manager_with(
            crate::config::TunnelConfig {
                mode: "off".into(),
                ..Default::default()
            },
            8787,
        )
        .await;
        let err = manager.start().await.unwrap_err().to_string();
        assert!(err.contains("\"off\""), "unhelpful error: {err}");
    }

    /// A stand-in for cloudflared: answers `--version` and then stays up until
    /// it is killed, which is the shape that matters here.
    async fn fake_cloudflared() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("chtting-fake-{}", crate::util::new_id("f")));
        tokio::fs::create_dir_all(&dir).await.unwrap();
        let script = dir.join("cloudflared");
        tokio::fs::write(
            &script,
            "#!/bin/sh\ncase \"$1\" in --version) echo 'cloudflared test'; exit 0;; esac\nexec sleep 600\n",
        )
        .await
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        tokio::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755))
            .await
            .unwrap();
        script
    }

    /// The regression this file exists to keep fixed: the supervisor used to
    /// hold the mutex guarding the child across `child.wait()`, so Stop and
    /// Restart waited for a lock that was only released once cloudflared had
    /// exited by itself. Both buttons did nothing, for ever.
    #[tokio::test]
    async fn stop_does_not_wait_for_the_tunnel_to_exit_on_its_own() {
        let script = fake_cloudflared().await;
        let manager = manager_with(
            crate::config::TunnelConfig {
                mode: "quick".into(),
                binary: script.to_string_lossy().into_owned(),
                ..Default::default()
            },
            8787,
        )
        .await;

        manager.start().await.unwrap();
        assert!(manager.status()["pid"].is_u64(), "nothing was started");

        tokio::time::timeout(Duration::from_secs(10), manager.stop())
            .await
            .expect("stop blocked until the tunnel exited on its own")
            .unwrap();
        assert_eq!(manager.status()["state"], "stopped");
        assert!(manager.status()["pid"].is_null());
    }

    /// Restart is stop-then-start, so it inherits the same deadlock — and has
    /// to leave a tunnel running afterwards rather than a stopped one.
    #[tokio::test]
    async fn restart_replaces_a_running_tunnel() {
        let script = fake_cloudflared().await;
        let manager = manager_with(
            crate::config::TunnelConfig {
                mode: "quick".into(),
                binary: script.to_string_lossy().into_owned(),
                ..Default::default()
            },
            8787,
        )
        .await;

        manager.start().await.unwrap();
        let first = manager.status()["pid"].as_u64().expect("a first pid");

        tokio::time::timeout(Duration::from_secs(10), manager.restart())
            .await
            .expect("restart blocked on the tunnel it was replacing")
            .unwrap();

        let second = manager.status()["pid"].as_u64().expect("a second pid");
        assert_ne!(first, second, "restart left the old process in place");
        let _ = manager.stop().await;
    }

    #[tokio::test]
    async fn status_is_reportable_before_anything_has_run() {
        let manager = manager_with(quick(), 8787).await;
        let status = manager.status();
        assert_eq!(status["state"], "stopped");
        assert_eq!(status["url"], "");
        assert!(status["pid"].is_null());
        assert_eq!(status["logs"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn the_quick_url_is_scraped_out_of_cloudflareds_chatter() {
        assert_eq!(
            find_quick_url("|  https://cold-water-1234.trycloudflare.com   |"),
            Some("https://cold-water-1234.trycloudflare.com".into())
        );
        assert_eq!(find_quick_url("INF Registered tunnel connection"), None);
        assert_eq!(find_quick_url("https://example.com/not-a-tunnel"), None);
    }

    #[test]
    fn a_tunnel_token_is_never_written_into_the_log_buffer() {
        let token = "eyJhIjoiZGVhZGJlZWYiLCJ0IjoiMTIzNCJ9";
        let redacted = redact_arg(token);
        assert!(!redacted.contains("ZGVhZGJlZWY"));
        assert!(redacted.contains("redacted"));
        assert_eq!(redact_arg("--protocol"), "--protocol");

        // A token that is not ASCII used to take the whole connector down:
        // slicing six *bytes* off it lands inside a character and panics.
        let multibyte = format!("ey{}", "日本語のトークン".repeat(4));
        assert!(multibyte.len() > 24);
        assert!(redact_arg(&multibyte).ends_with("…<redacted>"));
        assert!(!redact_arg(&multibyte).contains("トークン"));
    }
}
