//! Command-line entry point.

use anyhow::{bail, Result};
use clap::{Parser, Subcommand};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use chtting_relay::config::{Backend, ConfigStore, Model};
use chtting_relay::logging::{Level, Logger};
use chtting_relay::rotate;
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
        /// Start even though another copy is already serving, and take the
        /// ports over from it. Without this a duplicate is refused.
        #[arg(long)]
        replace: bool,
    },
    /// Check the install and print what is and is not ready.
    Doctor,
    /// Print the six-digit code the dashboard's "Forgot password" is waiting
    /// for.
    ///
    /// The code is shown on this device and nowhere else — that is what makes
    /// it proof the person resetting the password is holding the phone. Ask
    /// for one from the dashboard first; this prints whatever is pending.
    ResetCode,
    /// Wire the relay into the phone: the keeper that restarts it, home-screen
    /// shortcuts, start-on-boot. The installer runs this; afterwards the
    /// dashboard's Setup screen does the same job with buttons.
    Setup {
        /// Skip the keeper.
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
        replace: false,
    }) {
        Command::Start {
            port,
            no_dashboard,
            replace,
        } => start(paths, port, no_dashboard, replace).await,
        Command::Doctor => doctor(paths).await,
        Command::ResetCode => reset_code(paths).await,
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

async fn start(paths: Paths, port: Option<u16>, no_dashboard: bool, replace: bool) -> Result<()> {
    // A console logger first, so config problems are reported before the
    // file logger's destination is even known.
    let boot = Logger::console(Level::Info);
    let store = ConfigStore::load(&paths.config).await?;

    // The override moves before the port is read, because the port it names is
    // the one that has to be free.
    if let Some(port) = port {
        store
            .update(serde_json::json!({ "server": { "port": port } }))
            .await?;
    }
    let cfg = store.current();

    // Whether this process may serve at all is settled here, before a database
    // is opened, before a vocabulary is loaded, and before a line is logged
    // that would read like the relay coming up. The old order did all of that
    // first and checked afterwards, so a refused duplicate still announced
    // itself as "chtting-relay starting" — which is how a phone with one relay
    // on it came to look like a phone with two.
    let logger = if cfg.logging.file_enabled {
        Logger::with_file(
            Level::parse(&cfg.logging.level),
            &paths.logs.join("relay.log"),
        )
    } else {
        Logger::console(Level::parse(&cfg.logging.level))
    };
    drop(boot);

    // Whether this process may serve at all is settled here, before a database
    // is opened, before a vocabulary is loaded, and before a line is logged
    // that would read like the relay coming up. The old order did all of that
    // first and checked afterwards, so a refused duplicate still announced
    // itself as "chtting-relay starting" — which is how a phone with one relay
    // on it came to look like a phone with two.
    let handover = rotate::handover(&paths.data).await;
    if handover.is_none() {
        match hold_the_port(&paths, cfg.server.port, replace, &logger).await {
            Some(lease) => chtting_relay::lock::hold_for_life(lease),
            None => std::process::exit(chtting_relay::lock::EXIT_PORT_BUSY),
        }
    }

    let state = AppState::build(paths, logger.clone()).await?;
    let cfg = state.config.current();

    // A password-reset code only ever lived in the memory of the process that
    // minted it, so one left on disk by a process that is gone is a code that
    // cannot work. Clear it before anything can read it.
    chtting_relay::reset::forget_pending(&state.paths.data).await;

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
    // however the relay was started — a shortcut, the boot hook, or the keeper.
    if cfg.server.wake_lock && chtting_relay::system::termux() {
        state.host.acquire_wake_lock().await;
    }

    // Requirement 19: an instance may be taking over from one that is still
    // serving, so the ports are bound with SO_REUSEPORT and both are listening
    // for as long as the handover takes.
    match handover {
        Some(h) => logger.info(format!(
            "generation {}: taking over from pid {}",
            h.generation, h.predecessor
        )),
        // The lease was taken before any of this ran, so reaching here at all
        // means this process is the one entitled to the port.
        None => rotate::record_serving(&state.paths.data, std::process::id()).await,
    }

    // The public, tunnel-facing server. `retire` is what stops it accepting
    // without dropping the requests it already has.
    let relay_addr: SocketAddr = format!("{}:{}", cfg.server.host, cfg.server.port).parse()?;
    let relay =
        listen(relay_addr).map_err(|err| anyhow::anyhow!("cannot bind {relay_addr}: {err}"))?;
    let relay_bound = relay.local_addr()?;
    logger.info(format!("relay listening on http://{relay_bound}"));

    let (retire, mut retire_rx) = tokio::sync::watch::channel(false);
    let relay_app = server::public::router(state.clone());
    let relay_task = tokio::spawn(async move {
        axum::serve(
            relay,
            relay_app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .with_graceful_shutdown(async move {
            // Resolves when the rotation says to stop accepting; axum then
            // finishes the responses already in flight and returns.
            let _ = retire_rx.changed().await;
        })
        .await
    });

    // The local control panel.
    //
    // The same panel is also mounted under `/dashboard` on the relay's port, by
    // `server::public::router`, for the operator who would rather run one
    // cloudflared than two. That mount reads the config on every request, but
    // `--no-dashboard` is a decision about this run rather than a setting, so it
    // has to be told.
    let mut dashboard_task = None;
    if no_dashboard {
        state.panel.suppress();
    }
    if cfg.dashboard.enabled && !no_dashboard {
        let addr: SocketAddr = format!("{}:{}", cfg.dashboard.host, cfg.dashboard.port).parse()?;
        match listen(addr) {
            Ok(listener) => {
                logger.info(format!(
                    "dashboard listening on http://{}",
                    listener.local_addr()?
                ));
                let app = server::dashboard::router(state.clone());
                let mut retire_rx = retire.subscribe();
                dashboard_task = Some(tokio::spawn(async move {
                    // With connect info, like the relay: once the dashboard can
                    // be reached through a tunnel, "who is guessing this
                    // password" stops being a question with one answer.
                    axum::serve(
                        listener,
                        app.into_make_service_with_connect_info::<SocketAddr>(),
                    )
                    .with_graceful_shutdown(async move {
                        let _ = retire_rx.changed().await;
                    })
                    .await
                }));
            }
            Err(err) => logger.error(format!("cannot bind the dashboard on {addr}: {err}")),
        }
    }

    // Both ports are open, so anyone waiting on this instance can stand down.
    rotate::announce_ready(&state).await;
    rotate::sweep(&state).await;

    // A successor is listening but is not yet the relay: its predecessor still
    // holds the port lease while it drains. Taking that lease is what ends the
    // overlap, and this is the only place in the program where a process serves
    // without holding it.
    if let Some(h) = handover {
        finish_the_handover(state.clone(), h);
    }
    watch_the_lease(state.clone());

    // Requirement 9: the tunnel comes up with the relay and stays up. Not a
    // one-shot attempt that gives up if the phone has no network yet a second
    // after boot — ensure_running keeps trying, and the supervisor inside the
    // manager brings cloudflared back if it dies later.
    let wants = |t: &chtting_relay::config::TunnelConfig| t.auto_start && t.mode != "off";
    let relay_tunnel = wants(&cfg.tunnel);
    // The dashboard's is a separate process publishing a separate port, and it
    // comes up the same way — but only if somebody switched it on. It refuses
    // to start without a real password, and says so once rather than retrying.
    let dash_tunnel = cfg.dashboard.enabled && !no_dashboard && wants(&cfg.dashboard.tunnel);
    if relay_tunnel || dash_tunnel {
        let tunnel_state = state.clone();
        let tunnel_logger = logger.clone();
        tokio::spawn(async move {
            // One cloudflared generation per relay: during a rotation the
            // retiring instance still owns them, and starting a second would
            // hand out a second quick-tunnel URL. The claim covers both
            // tunnels, because both belong to the generation that holds it.
            rotate::claim_tunnel(&tunnel_state).await;
            if relay_tunnel {
                tunnel_logger.info("cloudflared: bringing the relay tunnel up");
                tunnel_state.tunnel.ensure_running();
            }
            if dash_tunnel {
                tunnel_logger.info("cloudflared: bringing the dashboard tunnel up");
                tunnel_state.dashboard_tunnel.ensure_running();
            }
        });
    }

    // Requirement 19: retire on a clock, before Android decides to do it for us.
    if rotate_every(&cfg).is_some() {
        rotate_on_a_clock(state.clone(), retire.clone());
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

    // The billing cycle, for keys that asked to be invoiced without anyone
    // pressing a button.
    billing_cycle(state.clone());

    tokio::select! {
        result = relay_task => { result??; }
        result = async {
            match dashboard_task {
                Some(task) => task.await,
                // Nothing to wait for; park this branch forever.
                None => std::future::pending().await,
            }
        } => { result??; }
        // Ctrl-C from a Termux session, SIGTERM from anything else. Both mean
        // the same thing and deserve the same tidy exit.
        _ = shutdown_signal() => {
            logger.info("shutting down");
            let _ = retire.send(true);
            rotate::release_serving(&state).await;
            rotate::release_tunnel(&state).await;
            let _ = state.tunnel.stop().await;
            // Both, always: a dashboard tunnel left running past the process
            // that owned it would keep a URL alive with nothing behind it —
            // and, after a rotation, with the *successor* behind it, which is
            // a control panel published by a process that never agreed to.
            let _ = state.dashboard_tunnel.stop().await;
            state.host.release_wake_lock().await;
            // Commit whatever the metrics writer still had queued.
            state.store.flush().await;
        }
    }
    Ok(())
}

/// How long between rotations, or `None` when they are switched off.
///
/// Minutes win over hours when both are set: the finer setting is the more
/// deliberate one, and it is the one a test reaches for.
fn rotate_every(cfg: &chtting_relay::config::Config) -> Option<std::time::Duration> {
    if cfg.server.rotate_minutes > 0 {
        return Some(std::time::Duration::from_secs(
            u64::from(cfg.server.rotate_minutes) * 60,
        ));
    }
    if cfg.server.rotate_hours > 0 {
        return Some(std::time::Duration::from_secs(
            u64::from(cfg.server.rotate_hours) * 3_600,
        ));
    }
    None
}

fn humanise(d: std::time::Duration) -> String {
    let secs = d.as_secs();
    if secs % 3_600 == 0 {
        format!("{}h", secs / 3_600)
    } else {
        format!("{}m", secs / 60)
    }
}

/// Take the port for a relay that nobody handed it to, or explain why not.
///
/// `None` means do not start. There is no third answer: every uncertainty here
/// — a lease that cannot be taken, a predecessor that will not go, a syscall
/// that fails — resolves to not starting, because the failure it is guarding
/// against is invisible. Two relays on one port do not crash and do not log;
/// they split the traffic, and the older one answers out of the config it
/// started with. That is what makes a key minted a minute ago come back
/// `invalid API key` on some requests and work on others.
async fn hold_the_port(
    paths: &Paths,
    port: u16,
    replace: bool,
    logger: &Logger,
) -> Option<chtting_relay::lock::PortLease> {
    use chtting_relay::lock::PortLease;

    match PortLease::try_acquire(port) {
        Ok(Some(lease)) => return Some(lease),
        Ok(None) => {}
        Err(err) => {
            logger.error(format!(
                "cannot tell whether port {port} is already being served ({err}), so this \
                 start is refused. A relay that cannot check is not allowed to guess."
            ));
            return None;
        }
    }

    // Who has it. The lease answers for itself, which is the only source that
    // works between two copies started with different `--home` values — and
    // that disagreement is precisely how two relays used to end up on one port.
    // The pidfile is the fallback, and being wrong or missing changes nothing
    // about the refusal itself.
    let incumbent =
        chtting_relay::lock::serving_pid(port).or(rotate::recorded_owner(&paths.data).await);

    if !replace {
        let who = match incumbent {
            Some(pid) => format!("pid {pid}"),
            None => "another process".into(),
        };
        logger.error(format!(
            "port {port} is already being served by {who}.\n\
             Starting a second copy would not fail on the port itself — SO_REUSEPORT lets \
             both bind it — it would split the traffic between them, and the older process \
             would answer out of the config it started with. That is what makes a freshly \
             minted key come back \"invalid API key\" on some requests and work on others.\n\
             Stop the one that is running first, or pass --replace to take the port over."
        ));
        return None;
    }

    logger.info(format!(
        "--replace: taking port {port} from {}",
        incumbent
            .map(|p| format!("pid {p}"))
            .unwrap_or_else(|| "whoever holds it".into())
    ));
    match chtting_relay::lock::take_over(port, incumbent, REPLACE_POLITE, REPLACE_FIRM).await {
        Ok(Some(lease)) => Some(lease),
        Ok(None) => {
            logger.error(format!(
                "port {port} is still held after asking and then insisting. Refusing to \
                 start beside whatever is holding it."
            ));
            None
        }
        Err(err) => {
            logger.error(format!("could not take port {port} over: {err}"));
            None
        }
    }
}

/// How long `--replace` waits after asking politely, and after insisting.
const REPLACE_POLITE: std::time::Duration = std::time::Duration::from_secs(20);
const REPLACE_FIRM: std::time::Duration = std::time::Duration::from_secs(10);

/// How long a successor lets its predecessor take to finish draining and go.
/// Generous: the predecessor is draining real requests, and cutting it short
/// costs a caller their answer.
const HANDOVER_GRACE: std::time::Duration = std::time::Duration::from_secs(90);

/// Turn a successor into the relay by taking the lease its predecessor holds.
///
/// The predecessor stopped accepting the moment it saw this instance come up,
/// so traffic is already arriving here and the overlap costs nothing while it
/// lasts. What it must not do is last: a predecessor stuck draining forever
/// used to leave two processes listening indefinitely, which is the state this
/// whole mechanism exists to prevent. So it is given the grace period, and then
/// it is moved along.
fn finish_the_handover(state: Arc<AppState>, h: rotate::Handover) {
    tokio::spawn(async move {
        let port = state.config.current().server.port;
        let logger = state.logger.clone();

        if let Ok(Some(lease)) =
            chtting_relay::lock::PortLease::acquire_within(port, HANDOVER_GRACE).await
        {
            chtting_relay::lock::hold_for_life(lease);
            rotate::record_serving(&state.paths.data, std::process::id()).await;
            logger.info(format!("generation {} now owns the port", h.generation));
            return;
        }

        logger.warn(format!(
            "pid {} has not let the port go {HANDOVER_GRACE:?} after handing over; moving it along",
            h.predecessor
        ));
        match chtting_relay::lock::take_over(
            port,
            Some(h.predecessor),
            REPLACE_POLITE,
            REPLACE_FIRM,
        )
        .await
        {
            Ok(Some(lease)) => {
                chtting_relay::lock::hold_for_life(lease);
                rotate::record_serving(&state.paths.data, std::process::id()).await;
                logger.info(format!("generation {} now owns the port", h.generation));
            }
            // Somebody else is on this port and it is not the predecessor. Two
            // relays is the thing being prevented, so this one stands down
            // rather than be the second.
            _ => {
                logger.error(format!(
                    "port {port} belongs to something this instance cannot displace; \
                     shutting down rather than serving beside it"
                ));
                state.store.flush().await;
                std::process::exit(chtting_relay::lock::EXIT_PORT_BUSY);
            }
        }
    });
}

/// Keep checking that this process is still the one entitled to the port.
///
/// Redundant while everything works, which is the point of it. The lease cannot
/// be taken away — the kernel holds it against all comers — so the only way to
/// be serving without it is to have never finished a handover, and this is what
/// notices. It also puts the pidfile back if something overwrote it, so the
/// next `--replace` asks the right process.
fn watch_the_lease(state: Arc<AppState>) {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(std::time::Duration::from_secs(30));
        ticker.tick().await;
        loop {
            ticker.tick().await;
            if !chtting_relay::lock::holding() {
                state.logger.error(
                    "still serving without the port lease long after the handover window; \
                     shutting down rather than stay a second relay",
                );
                state.store.flush().await;
                std::process::exit(chtting_relay::lock::EXIT_PORT_BUSY);
            }
            if rotate::recorded_owner(&state.paths.data).await.is_some() {
                // Somebody wrote their pid over ours. We hold the lease, so
                // they are not serving; put the note back before the next
                // --replace asks the wrong process to stop.
                rotate::record_serving(&state.paths.data, std::process::id()).await;
            }
        }
    });
}

/// Bind a listener that can share its port with the instance being replaced.
///
/// `SO_REUSEPORT` is what makes a zero-gap handover possible: the successor
/// binds while the predecessor is still accepting, and the kernel hands new
/// connections to whichever sockets are open. Without it the port would be
/// refused until the old process let go, and every client connecting in that
/// window would see a failure.
///
/// `SO_REUSEADDR` alone does not do this — it only allows binding over a socket
/// in TIME_WAIT, not over one that is actively listening.
fn listen(addr: SocketAddr) -> std::io::Result<tokio::net::TcpListener> {
    use socket2::{Domain, Protocol, Socket, Type};

    let domain = if addr.is_ipv6() {
        Domain::IPV6
    } else {
        Domain::IPV4
    };
    let socket = Socket::new(domain, Type::STREAM, Some(Protocol::TCP))?;
    socket.set_reuse_address(true)?;
    #[cfg(all(unix, not(any(target_os = "solaris", target_os = "illumos"))))]
    socket.set_reuse_port(true)?;
    socket.set_nonblocking(true)?;
    socket.bind(&addr.into())?;
    // The same backlog tokio's own bind uses.
    socket.listen(1024)?;
    tokio::net::TcpListener::from_std(std::net::TcpListener::from(socket))
}

/// Retire this instance every `rotateHours`, handing over to a fresh one.
///
/// Ordering is everything here, and it is deliberately pessimistic: nothing
/// this process owns is given up until the successor has confirmed it is
/// listening. If the successor never comes up, this instance carries on and
/// tries again at the next tick — a missed rotation is invisible, a failed
/// handover would not be.
fn rotate_on_a_clock(state: Arc<AppState>, retire: tokio::sync::watch::Sender<bool>) {
    tokio::spawn(async move {
        loop {
            let Some(every) = rotate_every(&state.config.current()) else {
                return; // switched off while running
            };
            tokio::time::sleep(every).await;

            let generation = rotate::generation();
            state.logger.info(format!(
                "rotating: generation {generation} has served {}, starting generation {}",
                humanise(every),
                generation + 1
            ));

            let pid = match rotate::spawn_successor(&state).await {
                Ok(pid) => pid,
                Err(err) => {
                    state.logger.warn(format!(
                        "rotation skipped, carrying on as generation {generation}: {err}"
                    ));
                    continue;
                }
            };

            // The successor is listening on the same ports. From here new
            // connections can only reach it, because this one stops accepting.
            state.logger.info(format!(
                "generation {} is up as pid {pid}; this one is draining",
                generation + 1
            ));
            let _ = retire.send(true);

            let timeout = std::time::Duration::from_millis(
                state.config.current().server.rotate_drain_timeout_ms,
            );
            let stranded = rotate::drain(&state, timeout).await;
            if stranded > 0 {
                state.logger.warn(format!(
                    "retiring with {stranded} request(s) still in flight after {timeout:?}"
                ));
            }

            // The successor is waiting on these before starting its own.
            let _ = state.tunnel.stop().await;
            let _ = state.dashboard_tunnel.stop().await;
            rotate::release_tunnel(&state).await;
            // A successor that is already up owns the lock by now, so this
            // only fires when the handover never happened.
            rotate::release_serving(&state).await;
            state.store.flush().await;
            state.logger.info("retired");
            std::process::exit(0);
        }
    });
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

/* -------------------------------------------------------- reset code -- */

/// Show the pending password-reset code.
///
/// Read out of `data/run/` rather than asked of the running relay, and that is
/// deliberate: the relay is normally started by the keeper with its stderr
/// pointed at `/dev/null`, so the banner it printed when the code was minted
/// went nowhere. This is the copy that survives — and it needs no port, no
/// session and no password, only the phone the file is on.
async fn reset_code(paths: Paths) -> Result<()> {
    match chtting_relay::reset::pending_notice(&paths.data).await {
        Some(notice) => print!("{notice}"),
        None => println!(
            "Nothing pending — no code has been asked for, or the last one has expired.\n\n\
             Open the dashboard, choose \"Forgot password?\", then run this again.\n\
             A code is good for {} minutes once it is issued.",
            chtting_relay::reset::CODE_TTL_MS / 60_000,
        ),
    }
    Ok(())
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
    // The one question that turns "my key stopped working" into a two-second
    // diagnosis: is something already serving on this port?
    let port = cfg.server.port;
    let owner = match chtting_relay::lock::port_is_taken(port) {
        false => "free".to_string(),
        true => match chtting_relay::lock::serving_pid(port)
            .or(rotate::recorded_owner(&paths.data).await)
        {
            Some(pid) => format!("already served by pid {pid}"),
            None => "already served by another process".into(),
        },
    };
    println!("  relay port    {port} — {owner}");
    println!(
        "  dashboard     {}:{}",
        cfg.dashboard.host, cfg.dashboard.port
    );
    println!(
        "  password      {}",
        match (
            cfg.dashboard.password.chars().count(),
            chtting_relay::reset::pending_notice(&paths.data)
                .await
                .is_some(),
        ) {
            (0, _) => "not set — the dashboard is open to anything on this device".to_string(),
            (n, true) =>
                format!("{n} characters — a reset code is waiting: run `chtting-relay reset-code`"),
            (n, false) => format!("{n} characters"),
        }
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
        println!("not Termux — the keeper, shortcuts and boot hook are Termux-only.");
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
                "  keeper      {}",
                status["path"].as_str().unwrap_or("installed")
            ),
            Err(err) => println!("  keeper      skipped: {err}"),
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

/// Issue the invoices the calendar says are due.
///
/// Checked hourly rather than scheduled for a moment: a phone sleeps, gets
/// killed and comes back, and a task waiting for one exact instant is a task
/// that misses it. An hourly look at "is it the cycle day, and has this key
/// already been billed today?" survives all of that and cannot double-bill,
/// because the answer to the second half is read from the invoices themselves.
///
/// Only keys with `billing.autoInvoice` are touched. An invoice closes a
/// billing period, so for everyone else it stays somebody's decision.
fn billing_cycle(state: Arc<chtting_relay::state::AppState>) {
    use chtting_relay::store::invoice::{self, IssueRequest, Issued};

    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(std::time::Duration::from_secs(3600));
        loop {
            ticker.tick().await;
            let cfg = state.config.current();
            if !cfg.billing.enabled || !cfg.billing.auto_issue {
                continue;
            }

            let tz = cfg.tz();
            let now = chtting_relay::util::now_ms();
            let today = chtting_relay::util::day_key(now, &tz);
            if chtting_relay::util::local_day_of_month(now, &tz) != cfg.billing.cycle_day {
                continue;
            }

            for key in cfg.keys.iter().filter(|k| k.billing.auto_invoice) {
                // Already billed today: the hourly tick has come round again on
                // the same cycle day, not a new one.
                let key_id = key.id.clone();
                let tz_for_read = tz;
                let today_for_read = today.clone();
                let billed_today = state
                    .store
                    .read(move |conn| {
                        let last = invoice::list(conn, Some(&key_id), 1)?.into_iter().next();
                        Ok(last.is_some_and(|i| {
                            chtting_relay::util::day_key(i.issued_at, &tz_for_read)
                                == today_for_read
                        }))
                    })
                    .await
                    .unwrap_or(true);
                if billed_today {
                    continue;
                }

                let key = key.clone();
                let billing = cfg.billing.clone();
                let label = key.display_name().to_string();
                let issued = state
                    .store
                    .write(move |conn| {
                        invoice::issue(
                            conn,
                            IssueRequest {
                                key: &key,
                                billing: &billing,
                                note: "issued automatically on the billing cycle".into(),
                                tax_percent: None,
                                // Never forced: a period under the minimum, or
                                // with nothing in it, rolls into the next one.
                                force: false,
                                tz,
                            },
                        )
                    })
                    .await;

                match issued {
                    Ok(Issued::Invoice(inv)) => state.logger.info(format!(
                        "invoice {} issued automatically for {label}: {} {:.6}",
                        inv.number, inv.currency, inv.total_usd,
                    )),
                    // Nothing to bill is the ordinary state of a quiet key.
                    Ok(Issued::Skipped(_)) => {}
                    Err(err) => state
                        .logger
                        .error(format!("could not invoice {label} automatically: {err}")),
                }
            }
        }
    });
}
