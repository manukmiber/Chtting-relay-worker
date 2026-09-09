//! Command-line entry point.

use anyhow::{bail, Result};
use clap::{Parser, Subcommand};
use std::net::SocketAddr;
use std::path::PathBuf;

use chtting_relay::config::{Backend, ConfigStore, Model};
use chtting_relay::logging::{Level, Logger};
use chtting_relay::server;
use chtting_relay::state::{AppState, Paths};
use chtting_relay::system::Host;
use chtting_relay::tokenizer::registry::{BUILTIN_TIKTOKEN, HF_PRESETS};
use chtting_relay::util::{mask_secret, new_client_key, new_id};

#[derive(Parser)]
#[command(
    name = "chtting-relay",
    version,
    about = "Termux-native LLM relay: exact tokenizing, model translation, response reshaping, dashboard, tunnel"
)]
struct Cli {
    /// State directory (config, database, logs, vocabularies).
    #[arg(long, global = true)]
    home: Option<PathBuf>,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Run the relay, the dashboard and (optionally) the tunnel.
    Start {
        /// Override the public relay port.
        #[arg(long)]
        port: Option<u16>,
        /// Do not start the dashboard.
        #[arg(long)]
        no_dashboard: bool,
    },
    /// Check the install and print what is and is not ready.
    Doctor,
    /// Wire the relay into the phone: runit service, home-screen shortcuts,
    /// start-on-boot. The installer runs this; afterwards the dashboard's
    /// Setup screen does the same job with buttons.
    Setup {
        /// Skip the runit service.
        #[arg(long)]
        no_service: bool,
        /// Skip the Termux:Widget shortcuts.
        #[arg(long)]
        no_shortcuts: bool,
        /// Skip the Termux:Boot hook.
        #[arg(long)]
        no_boot: bool,
    },
    /// Show where the config lives, or print it with secrets masked.
    Config {
        #[command(subcommand)]
        action: ConfigAction,
    },
    /// Manage client API keys.
    Key {
        #[command(subcommand)]
        action: KeyAction,
    },
    /// Manage upstream backends.
    Backend {
        #[command(subcommand)]
        action: BackendAction,
    },
    /// Manage public model aliases.
    Model {
        #[command(subcommand)]
        action: ModelAction,
    },
    /// Inspect and install tokenizer vocabularies.
    Tokenizer {
        #[command(subcommand)]
        action: TokenizerAction,
    },
}

#[derive(Subcommand)]
enum ConfigAction {
    /// Print the path to config.json.
    Path,
    /// Print the config with every secret masked.
    Show,
}

#[derive(Subcommand)]
enum KeyAction {
    /// Mint a new client key and print it once.
    New {
        #[arg(long, default_value = "new key")]
        label: String,
    },
    /// List keys, masked.
    List,
}

#[derive(Subcommand)]
enum BackendAction {
    /// Add a backend.
    Add {
        #[arg(long)]
        name: String,
        #[arg(long)]
        base_url: String,
        #[arg(long, default_value = "")]
        api_key: String,
        #[arg(long, default_value = "openai")]
        kind: String,
    },
    List,
}

#[derive(Subcommand)]
enum ModelAction {
    /// Map a public alias onto a backend model.
    Add {
        /// The public name callers send, e.g. manukmiberai/creative-writer
        #[arg(long)]
        id: String,
        /// The backend id from `backend list`.
        #[arg(long)]
        backend: String,
        /// The real name sent upstream, e.g. Deepseek-v4-flash-0731
        #[arg(long)]
        upstream: String,
    },
    List,
}

#[derive(Subcommand)]
enum TokenizerAction {
    /// Show what is built in, installed and available.
    List,
    /// Download a HuggingFace tokenizer.json.
    Install {
        /// A preset name (deepseek, qwen, llama3, ...) or a repo id.
        name: String,
        /// Save it under a different name.
        #[arg(long)]
        r#as: Option<String>,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let paths = Paths::resolve(cli.home.as_deref());

    // The worker count is read before the runtime exists, so it comes from a
    // plain read of the file rather than the loaded config.
    let workers = preread_worker_threads(&paths.config);
    let mut builder = tokio::runtime::Builder::new_multi_thread();
    builder.enable_all();
    if workers > 0 {
        builder.worker_threads(workers);
    }
    let runtime = builder.build()?;
    runtime.block_on(run(cli, paths))
}

/// Peek at `server.workerThreads` before the async runtime is built.
fn preread_worker_threads(config: &std::path::Path) -> usize {
    std::fs::read_to_string(config)
        .ok()
        .and_then(|raw| serde_json::from_str::<serde_json::Value>(&raw).ok())
        .and_then(|v| {
            v.get("server")
                .and_then(|s| s.get("workerThreads"))
                .and_then(|n| n.as_u64())
        })
        .unwrap_or(0) as usize
}

async fn run(cli: Cli, paths: Paths) -> Result<()> {
    match cli.command.unwrap_or(Command::Start {
        port: None,
        no_dashboard: false,
    }) {
        Command::Start { port, no_dashboard } => start(paths, port, no_dashboard).await,
        Command::Doctor => doctor(paths).await,
        Command::Setup {
            no_service,
            no_shortcuts,
            no_boot,
        } => setup(paths, !no_service, !no_shortcuts, !no_boot).await,
        Command::Config { action } => config_command(paths, action).await,
        Command::Key { action } => key_command(paths, action).await,
        Command::Backend { action } => backend_command(paths, action).await,
        Command::Model { action } => model_command(paths, action).await,
        Command::Tokenizer { action } => tokenizer_command(paths, action).await,
    }
}

/* -------------------------------------------------------------- start -- */

async fn start(paths: Paths, port: Option<u16>, no_dashboard: bool) -> Result<()> {
    // A console logger first, so config problems are reported before the
    // file logger's destination is even known.
    let boot = Logger::console(Level::Info);
    let store = ConfigStore::load(&paths.config).await?;
    let cfg = store.current();

    let logger = if cfg.logging.file_enabled {
        Logger::with_file(
            Level::parse(&cfg.logging.level),
            &paths.logs.join("relay.log"),
        )
    } else {
        Logger::console(Level::parse(&cfg.logging.level))
    };
    drop(boot);

    if let Some(port) = port {
        store
            .update(serde_json::json!({ "server": { "port": port } }))
            .await?;
    }

    let state = AppState::build(paths, logger.clone()).await?;
    let cfg = state.config.current();

    logger.info(format!(
        "chtting-relay {} starting — {} model(s), {} backend(s), {} worker thread(s)",
        env!("CARGO_PKG_VERSION"),
        cfg.models.iter().filter(|m| m.enabled).count(),
        cfg.backends.iter().filter(|b| b.enabled).count(),
        std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1),
    ));

    if cfg.models.is_empty() {
        logger.warn("no models configured yet — open the dashboard and add a backend and a model");
    }

    // The phone suspends Termux the moment the screen goes off, which used to
    // be the start script's job to prevent. Doing it here means it happens
    // however the relay was started — a shortcut, the boot hook, or runit.
    if cfg.server.wake_lock && chtting_relay::system::termux() {
        state.host.acquire_wake_lock().await;
    }

    // The public, tunnel-facing server.
    let relay_addr: SocketAddr = format!("{}:{}", cfg.server.host, cfg.server.port).parse()?;
    let relay = tokio::net::TcpListener::bind(relay_addr)
        .await
        .map_err(|err| {
            anyhow::anyhow!("cannot bind {relay_addr}: {err} — is another copy already running?")
        })?;
    let relay_bound = relay.local_addr()?;
    logger.info(format!("relay listening on http://{relay_bound}"));

    let relay_app = server::public::router(state.clone());
    let relay_task = tokio::spawn(async move {
        axum::serve(
            relay,
            relay_app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await
    });

    // The local control panel.
    let mut dashboard_task = None;
    if cfg.dashboard.enabled && !no_dashboard {
        let addr: SocketAddr = format!("{}:{}", cfg.dashboard.host, cfg.dashboard.port).parse()?;
        match tokio::net::TcpListener::bind(addr).await {
            Ok(listener) => {
                logger.info(format!(
                    "dashboard listening on http://{}",
                    listener.local_addr()?
                ));
                let app = server::dashboard::router(state.clone());
                dashboard_task = Some(tokio::spawn(
                    async move { axum::serve(listener, app).await },
                ));
            }
            Err(err) => logger.error(format!("cannot bind the dashboard on {addr}: {err}")),
        }
    }

    if cfg.tunnel.auto_start && cfg.tunnel.mode != "off" {
        match state.tunnel.start().await {
            Ok(_) => logger.info("cloudflared starting"),
            Err(err) => logger.warn(format!("tunnel did not start: {err}")),
        }
    }

    // Housekeeping: expire rate-limit windows so an unbounded set of keys
    // cannot grow the map forever.
    let sweeper = state.clone();
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(std::time::Duration::from_secs(120));
        loop {
            ticker.tick().await;
            sweeper.limiter.sweep();
        }
    });

    tokio::select! {
        result = relay_task => { result??; }
        result = async {
            match dashboard_task {
                Some(task) => task.await,
                // Nothing to wait for; park this branch forever.
                None => std::future::pending().await,
            }
        } => { result??; }
        // Ctrl-C from a Termux session, SIGTERM from runit's `sv down`. Both
        // mean the same thing and deserve the same tidy exit.
        _ = shutdown_signal() => {
            logger.info("shutting down");
            let _ = state.tunnel.stop().await;
            state.host.release_wake_lock().await;
            // Commit whatever the metrics writer still had queued.
            state.store.flush().await;
        }
    }
    Ok(())
}

async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let mut term = match signal(SignalKind::terminate()) {
            Ok(stream) => stream,
            // Without SIGTERM there is still Ctrl-C.
            Err(_) => return tokio::signal::ctrl_c().await.unwrap_or(()),
        };
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = term.recv() => {}
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

/* ------------------------------------------------------------ doctor -- */

async fn doctor(paths: Paths) -> Result<()> {
    println!("chtting-relay {}", env!("CARGO_PKG_VERSION"));
    println!(
        "  platform      {} {}",
        std::env::consts::OS,
        std::env::consts::ARCH
    );
    println!(
        "  termux        {}",
        if std::env::var("PREFIX").is_ok_and(|p| p.contains("com.termux")) {
            "yes"
        } else {
            "no"
        }
    );
    println!(
        "  cores         {}",
        std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1)
    );
    println!("  config        {}", paths.config.display());
    println!("  data          {}", paths.data.display());

    let store = ConfigStore::load(&paths.config).await?;
    let cfg = store.current();
    println!("  models        {}", cfg.models.len());
    println!("  backends      {}", cfg.backends.len());
    println!("  client keys   {}", cfg.keys.len());
    println!("  relay port    {}", cfg.server.port);
    println!(
        "  dashboard     {}:{}",
        cfg.dashboard.host, cfg.dashboard.port
    );
    println!("  max in flight {}", cfg.server.max_concurrent_requests);
    println!(
        "  queue         {} waiting, {}s to give up",
        cfg.server.queue_capacity,
        cfg.server.queue_timeout_ms / 1000
    );
    if cfg.openrouter.enabled {
        let listed = cfg
            .models
            .iter()
            .filter(|m| m.enabled && m.openrouter.listed)
            .count();
        println!(
            "  openrouter    {listed} model(s) published at {}",
            cfg.openrouter.path
        );
    }

    println!("\ntokenizers");
    for name in BUILTIN_TIKTOKEN {
        println!("  {name:<16} built in (exact, no download)");
    }
    let dir = &paths.tokenizers;
    let mut installed = 0;
    if let Ok(mut entries) = tokio::fs::read_dir(dir).await {
        while let Ok(Some(entry)) = entries.next_entry().await {
            let name = entry.file_name().to_string_lossy().to_string();
            if let Some(stem) = name.strip_suffix(".tokenizer.json") {
                let size = entry.metadata().await.map(|m| m.len()).unwrap_or(0);
                println!("  {stem:<16} installed ({size} bytes)");
                installed += 1;
            }
        }
    }
    if installed == 0 {
        println!("  (no HuggingFace vocabularies installed — see `tokenizer install`)");
    }

    let logger = Logger::console(Level::Silent);
    let state = AppState::build(paths, logger).await?;
    let version = state.tunnel.version().await;
    println!(
        "\ncloudflared     {}",
        if version["installed"] == serde_json::Value::Bool(true) {
            version["version"]
                .as_str()
                .unwrap_or("installed")
                .to_string()
        } else {
            "not installed (pkg install cloudflared)".to_string()
        }
    );

    let problems = chtting_relay::config::validate(&state.config.current());
    if problems.is_empty() {
        println!("\nconfig is valid.");
    } else {
        println!("\nconfig problems:");
        for problem in problems {
            println!("  - {problem}");
        }
    }
    Ok(())
}

/* ------------------------------------------------------------- setup -- */

/// Do once, from the installer, what the dashboard's Setup screen does with
/// buttons: make the relay a thing the phone runs rather than a thing you have
/// to remember to start.
async fn setup(paths: Paths, service: bool, shortcuts: bool, boot: bool) -> Result<()> {
    if !chtting_relay::system::termux() {
        println!("not Termux — the service, shortcuts and boot hook are Termux-only.");
        return Ok(());
    }

    let store = ConfigStore::load(&paths.config).await?;
    let dashboard_url = format!("http://127.0.0.1:{}", store.current().dashboard.port);
    let host = Host::new(paths, Logger::console(Level::Silent));

    // Order matters: the shortcuts and the boot hook both ask whether a service
    // exists, and write a different script depending on the answer.
    if service {
        match host.install_service().await {
            Ok(status) => println!(
                "  service     {}",
                status["path"].as_str().unwrap_or("installed")
            ),
            Err(err) => println!("  service     skipped: {err}"),
        }
    }
    if shortcuts {
        match host.install_shortcuts(&dashboard_url).await {
            Ok(status) => println!(
                "  shortcuts   {} (needs the Termux:Widget app)",
                status["path"].as_str().unwrap_or("written")
            ),
            Err(err) => println!("  shortcuts   skipped: {err}"),
        }
    }
    if boot {
        match host.install_boot().await {
            Ok(status) => println!(
                "  boot        {} (needs the Termux:Boot app)",
                status["path"].as_str().unwrap_or("written")
            ),
            Err(err) => println!("  boot        skipped: {err}"),
        }
    }
    Ok(())
}

/* ---------------------------------------------------------- commands -- */

async fn config_command(paths: Paths, action: ConfigAction) -> Result<()> {
    match action {
        ConfigAction::Path => println!("{}", paths.config.display()),
        ConfigAction::Show => {
            let store = ConfigStore::load(&paths.config).await?;
            println!("{}", serde_json::to_string_pretty(&store.redacted())?);
        }
    }
    Ok(())
}

async fn key_command(paths: Paths, action: KeyAction) -> Result<()> {
    let store = ConfigStore::load(&paths.config).await?;
    match action {
        KeyAction::New { label } => {
            let key = new_client_key();
            store
                .upsert(
                    "keys",
                    serde_json::json!({
                        "id": new_id("key"),
                        "label": label,
                        "key": key,
                        "enabled": true,
                        "models": ["*"],
                    }),
                )
                .await?;
            println!("{key}");
            eprintln!("(shown once — the dashboard only ever displays it masked)");
        }
        KeyAction::List => {
            let cfg = store.current();
            if cfg.keys.is_empty() {
                println!("no client keys yet — run `chtting-relay key new`");
            }
            for key in &cfg.keys {
                println!(
                    "{:<24} {:<20} {} {}",
                    key.id,
                    key.label,
                    mask_secret(&key.key),
                    if key.enabled { "" } else { "(disabled)" }
                );
            }
        }
    }
    Ok(())
}

async fn backend_command(paths: Paths, action: BackendAction) -> Result<()> {
    let store = ConfigStore::load(&paths.config).await?;
    match action {
        BackendAction::Add {
            name,
            base_url,
            api_key,
            kind,
        } => {
            let backend = Backend {
                id: new_id("be"),
                name: name.clone(),
                base_url,
                api_key,
                kind,
                ..Default::default()
            };
            let saved = store
                .upsert("backends", serde_json::to_value(backend)?)
                .await?;
            println!(
                "added backend \"{name}\" with id {}",
                saved["id"].as_str().unwrap_or("?")
            );
        }
        BackendAction::List => {
            let cfg = store.current();
            if cfg.backends.is_empty() {
                println!("no backends yet — run `chtting-relay backend add --help`");
            }
            for b in &cfg.backends {
                println!(
                    "{:<24} {:<20} {} {}",
                    b.id,
                    b.name,
                    b.base_url,
                    if b.enabled { "" } else { "(disabled)" }
                );
            }
        }
    }
    Ok(())
}

async fn model_command(paths: Paths, action: ModelAction) -> Result<()> {
    let store = ConfigStore::load(&paths.config).await?;
    match action {
        ModelAction::Add {
            id,
            backend,
            upstream,
        } => {
            if store.current().find_backend(&backend).is_none() {
                bail!("no backend with id \"{backend}\" — run `chtting-relay backend list`");
            }
            let model = Model {
                id: id.clone(),
                backend,
                upstream_model: upstream.clone(),
                ..Default::default()
            };
            store.upsert("models", serde_json::to_value(model)?).await?;
            println!("{id}  ->  {upstream}");
        }
        ModelAction::List => {
            let cfg = store.current();
            if cfg.models.is_empty() {
                println!("no models yet — run `chtting-relay model add --help`");
            }
            for m in &cfg.models {
                println!(
                    "{:<32} -> {:<28} {} {}",
                    m.id,
                    m.upstream_model,
                    m.backend,
                    if m.enabled { "" } else { "(disabled)" }
                );
            }
        }
    }
    Ok(())
}

async fn tokenizer_command(paths: Paths, action: TokenizerAction) -> Result<()> {
    match action {
        TokenizerAction::List => {
            println!("built in (exact, nothing to download):");
            for name in BUILTIN_TIKTOKEN {
                println!("  {name}");
            }
            println!("\ndownloadable presets:");
            for (name, repo) in HF_PRESETS {
                println!("  {name:<14} {repo}");
            }
            println!("\ninstalled in {}:", paths.tokenizers.display());
            let mut any = false;
            if let Ok(mut entries) = tokio::fs::read_dir(&paths.tokenizers).await {
                while let Ok(Some(entry)) = entries.next_entry().await {
                    let name = entry.file_name().to_string_lossy().to_string();
                    if let Some(stem) = name.strip_suffix(".tokenizer.json") {
                        println!("  {stem}");
                        any = true;
                    }
                }
            }
            if !any {
                println!("  (none)");
            }
        }
        TokenizerAction::Install { name, r#as } => {
            let (url, save_as) = match HF_PRESETS.iter().find(|(n, _)| *n == name) {
                Some((n, repo)) => (
                    chtting_relay::tokenizer::registry::hf_url(repo),
                    r#as.unwrap_or_else(|| (*n).to_string()),
                ),
                None if BUILTIN_TIKTOKEN.contains(&name.as_str()) => {
                    println!("\"{name}\" is compiled into the relay; nothing to download.");
                    return Ok(());
                }
                None if name.contains('/') => {
                    let default = name.rsplit('/').next().unwrap_or(&name).to_lowercase();
                    (
                        chtting_relay::tokenizer::registry::hf_url(&name),
                        r#as.unwrap_or(default),
                    )
                }
                None => bail!("unknown tokenizer \"{name}\" — run `chtting-relay tokenizer list`"),
            };

            tokio::fs::create_dir_all(&paths.tokenizers).await?;
            println!("downloading {url}");
            let bytes = reqwest::get(&url)
                .await?
                .error_for_status()?
                .bytes()
                .await?;
            // Validate before saving, so a failed download cannot masquerade
            // as an installed vocabulary.
            if tokenizers::Tokenizer::from_bytes(&bytes).is_err() {
                bail!(
                    "downloaded {} bytes, but it is not a valid tokenizer.json",
                    bytes.len()
                );
            }
            let path = paths.tokenizers.join(format!("{save_as}.tokenizer.json"));
            tokio::fs::write(&path, &bytes).await?;
            println!("saved {} ({} bytes)", path.display(), bytes.len());
        }
    }
    Ok(())
}
