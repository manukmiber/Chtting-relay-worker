//! Everything a request handler needs, assembled once at startup and shared
//! by reference from then on.

use anyhow::Result;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tokio::sync::Semaphore;

use crate::config::ConfigStore;
use crate::logging::Logger;
use crate::store::{QuotaTracker, RateLimiter, Store};
use crate::tokenizer::registry::Registry;
use crate::tokenizer::TokenCounter;
use crate::tunnel::TunnelManager;

#[derive(Debug, Clone)]
pub struct Paths {
    /// Where the binary's own assets live.
    pub root: PathBuf,
    /// Writable state: config, database, logs, vocabularies.
    pub home: PathBuf,
    pub config: PathBuf,
    pub data: PathBuf,
    pub logs: PathBuf,
    pub tokenizers: PathBuf,
}

impl Paths {
    pub fn resolve(home: Option<&Path>) -> Self {
        let root = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let home = home
            .map(Path::to_path_buf)
            .or_else(|| std::env::var_os("CHTTING_HOME").map(PathBuf::from))
            .unwrap_or_else(|| root.clone());
        let data = std::env::var_os("CHTTING_DATA")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join("data"));
        Self {
            config: std::env::var_os("CHTTING_CONFIG")
                .map(PathBuf::from)
                .unwrap_or_else(|| home.join("config/config.json")),
            logs: data.join("logs"),
            tokenizers: std::env::var_os("CHTTING_TOKENIZER_DIR")
                .map(PathBuf::from)
                .unwrap_or_else(|| data.join("tokenizers")),
            data,
            home,
            root,
        }
    }
}

/// Counters the dashboard shows and the load test asserts on.
#[derive(Debug, Default)]
pub struct Stats {
    pub started_at_ms: AtomicU64,
    /// Requests refused because the in-flight ceiling was already reached.
    pub rejected_overload: AtomicU64,
    pub in_flight: AtomicU64,
}

impl Stats {
    pub fn uptime_s(&self) -> u64 {
        let started = self.started_at_ms.load(Ordering::Relaxed);
        if started == 0 {
            return 0;
        }
        (crate::util::now_ms() as u64).saturating_sub(started) / 1000
    }
}

pub struct AppState {
    pub config: Arc<ConfigStore>,
    pub store: Arc<Store>,
    pub counter: Arc<TokenCounter>,
    pub upstream: Arc<crate::relay::upstream::Upstream>,
    pub logger: Arc<Logger>,
    pub limiter: Arc<RateLimiter>,
    pub quotas: Arc<QuotaTracker>,
    pub tunnel: Arc<TunnelManager>,
    pub paths: Paths,
    pub stats: Arc<Stats>,
    /// Bounds how many calls may be in flight upstream at once. Past the
    /// ceiling the relay answers 503 immediately rather than queueing work the
    /// phone cannot finish.
    pub in_flight: Arc<Semaphore>,
}

impl AppState {
    pub async fn build(paths: Paths, logger: Arc<Logger>) -> Result<Arc<Self>> {
        let config = Arc::new(ConfigStore::load(&paths.config).await?);
        let cfg = config.current();

        let store = Store::open(&paths.data.join("relay.db"), logger.clone()).await?;
        let registry = Arc::new(Registry::new(paths.tokenizers.clone(), logger.clone()));
        let counter = Arc::new(TokenCounter::new(registry));
        let upstream = Arc::new(crate::relay::upstream::Upstream::new(logger.clone())?);
        let tunnel = Arc::new(TunnelManager::new(config.clone(), logger.clone()));

        let today = crate::util::day_key(crate::util::now_ms(), &cfg.tz());
        let quotas = Arc::new(QuotaTracker::new(today.clone()));
        if let Err(err) = quotas.seed(&store, &today).await {
            logger.warn(format!("could not seed today's quota counters: {err}"));
        }

        let stats = Arc::new(Stats::default());
        stats
            .started_at_ms
            .store(crate::util::now_ms() as u64, Ordering::Relaxed);

        let permits = cfg.server.max_concurrent_requests.max(1);
        Ok(Arc::new(Self {
            config,
            store,
            counter,
            upstream,
            logger,
            limiter: Arc::new(RateLimiter::new()),
            quotas,
            tunnel,
            paths,
            stats,
            in_flight: Arc::new(Semaphore::new(permits)),
        }))
    }

    pub fn today(&self) -> String {
        crate::util::day_key(crate::util::now_ms(), &self.config.current().tz())
    }
}
