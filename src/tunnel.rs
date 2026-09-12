//! Supervises a `cloudflared` child process so the relay on your phone gets a
//! public HTTPS URL without port forwarding.
//!
//! Three modes:
//!
//! * `quick` — a throwaway `*.trycloudflare.com` URL, no Cloudflare account
//! * `named` — a token-backed named tunnel bound to your own hostname
//! * a `config.yml` you manage yourself
//!
//! Only the relay port is ever published. The dashboard binds to loopback and
//! is deliberately not routed through the tunnel.

use anyhow::{anyhow, Result};
use parking_lot::Mutex;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};

use crate::config::ConfigStore;
use crate::logging::Logger;

const MAX_LOG_LINES: usize = 500;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Stopped,
    Starting,
    Running,
    Failed,
}

pub async fn ensure_cloudflared_running(config: &TunnelConfig, relay_port: u16) {
    if !config.auto_start || config.mode == "off" {
        return;
    }

    tokio::spawn(async move {
        loop {
            tracing::info!("[TUNNEL] Memulai Cloudflare Tunnel...");
            let mut cmd = tokio::process::Command::new("cloudflared");
            cmd.arg("tunnel");

            if config.mode == "quick" {
                cmd.args(["--url", &format!("http://127.0.0.1:{}", relay_port)]);
            } else if !config.token.is_empty() {
                cmd.args(["run", "--token", &config.token]);
            }

            cmd.stdout(std::process::Stdio::piped())
               .stderr(std::process::Stdio::piped());

            if let Ok(mut child) = cmd.spawn() {
                let _ = child.wait().await;
                tracing::warn!("[TUNNEL] Cloudflared terhenti, memulai ulang dalam 3 detik...");
            } else {
                tracing::error!("[TUNNEL] Gagal mengeksekusi cloudflared. Pastikan terinstall!");
            }
            tokio::time::sleep(tokio::time::Duration::from_secs(3)).await;
        }
    });
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
    restarts: u32,
    lines: Vec<String>,
    pid: Option<u32>,
}

pub struct TunnelManager {
    config: Arc<ConfigStore>,
    logger: Arc<Logger>,
    inner: Mutex<Inner>,
    child: tokio::sync::Mutex<Option<Child>>,
    /// Set while a deliberate stop is in progress, so the supervisor does not
    /// treat the exit as a crash and restart it.
    stopping: Arc<AtomicBool>,
}

impl TunnelManager {
    pub fn new(config: Arc<ConfigStore>, logger: Arc<Logger>) -> Self {
        Self {
            config,
            logger,
            inner: Mutex::new(Inner::default()),
            child: tokio::sync::Mutex::new(None),
            stopping: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn status(&self) -> serde_json::Value {
        let inner = self.inner.lock();
        let cfg = self.config.current();
        let state = inner.state_label.unwrap_or(State::Stopped);
        serde_json::json!({
            "state": state.label(),
            "url": inner.url,
            "mode": cfg.tunnel.mode,
            "pid": inner.pid,
            "startedAt": inner.started_at,
            "uptime_s": if inner.started_at > 0 {
                (crate::util::now_ms() - inner.started_at) / 1000
            } else { 0 },
            "restarts": inner.restarts,
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
        let cfg = self.config.current();
        if cfg.tunnel.binary.is_empty() {
            "cloudflared".into()
        } else {
            cfg.tunnel.binary.clone()
        }
    }

    /// Only `server.port` is ever published.
    pub fn build_args(&self) -> Vec<String> {
        let cfg = self.config.current();
        let t = &cfg.tunnel;
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
            format!("http://127.0.0.1:{}", cfg.server.port),
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
        {
            let child = self.child.lock().await;
            if child.is_some() {
                return Ok(self.status());
            }
        }

        let cfg = self.config.current();
        if cfg.tunnel.mode == "off" {
            return Err(anyhow!(
                "tunnel mode is \"off\"; set it to quick or named first"
            ));
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
            "starting: {} {}",
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

        *self.child.lock().await = Some(child);
        self.supervise();
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

    /// Watch for the child exiting and bring it back if it was not asked to go.
    fn supervise(self: &Arc<Self>) {
        let manager = self.clone();
        tokio::spawn(async move {
            let status = {
                let mut guard = manager.child.lock().await;
                match guard.as_mut() {
                    Some(child) => child.wait().await,
                    None => return,
                }
            };
            *manager.child.lock().await = None;
            manager.inner.lock().pid = None;

            if manager.stopping.load(Ordering::SeqCst) {
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

            if !manager.config.current().tunnel.auto_start {
                return;
            }
            // Mobile links drop; back off so a hard failure does not spin.
            let restarts = {
                let mut inner = manager.inner.lock();
                inner.restarts += 1;
                inner.restarts
            };
            let wait = std::time::Duration::from_millis((1_000u64 << restarts.min(5)).min(60_000));
            manager.log(&format!("restarting in {}s", wait.as_secs()));
            tokio::time::sleep(wait).await;
            if let Err(err) = manager.start().await {
                manager.logger.warn(format!("tunnel restart failed: {err}"));
            }
        });
    }

    pub async fn stop(&self) -> Result<serde_json::Value> {
        self.stopping.store(true, Ordering::SeqCst);
        if let Some(mut child) = self.child.lock().await.take() {
            let _ = child.kill().await;
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
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
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
                drop(inner);
                self.logger.info(format!("cloudflared tunnel URL: {url}"));
                return;
            }
        }
        if inner.state_label != Some(State::Running)
            && (text.contains("Registered tunnel connection") || text.contains("registered"))
        {
            inner.state_label = Some(State::Running);
        }
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
        return format!("{}…<redacted>", &arg[..6]);
    }
    arg.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

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
    }
}
