mod config;
mod oc_client;
mod proxy;
mod router;
mod setup;

use anyhow::{anyhow, Context, Result};
use setup::SessionChoice;
use std::io::Write;
use std::net::TcpListener;
use std::process::Stdio;
use std::time::Duration;
use tokio::net::TcpListener as TokioTcpListener;
use tokio::process::{Child, Command};

use oc_client::OcClient;
use proxy::{build_proxy_client, router as proxy_router, AppState};

const SERVE_PREF_PORT: u16 = 4096;
const PROXY_PREF_PORT: u16 = 4097;

/// Print to stderr. The OpenCode TUI owns stdout (it is a fullscreen app whose
/// alternate-screen rendering lives there); oc-route must keep stdout quiet so its
/// status messages never bleed through the TUI. stderr is safe: it does not scroll
/// into the alternate screen buffer the way stdout does, and these are diagnostics.
fn elog(msg: impl AsRef<str>) {
    let _ = writeln!(std::io::stderr(), "{}", msg.as_ref());
}

// OpenCode's TUI is a fullscreen app that owns stdout. oc-route shares that same
// terminal (opencode attach inherits our stdout/stderr/stdin), so ANYTHING oc-route
// writes while the TUI is running bleeds through its rendering. Default to a quiet
// filter (warn-level); raise with RUST_LOG="oc_route=info" if diagnosing.
const DEFAULT_LOG_FILTER: &str = "warn";

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(DEFAULT_LOG_FILTER)),
        )
        .with_writer(std::io::stderr)
        .init();

    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(|s| s.as_str()) {
        Some("-h") | Some("--help") => {
            print_help();
            Ok(())
        }
        Some("proxy") => cmd_proxy(&args[1..]).await,
        Some("version") | Some("-v") | Some("--version") => {
            println!("oc-route {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        _ => cmd_default().await,
    }
}

async fn cmd_default() -> Result<()> {
    let mut profiles = config::load().context("failed to load profiles")?;
    config::ensure_config_dir_exists().ok();

    let serve_port = acquire_port(SERVE_PREF_PORT);
    let proxy_listener = bind_proxy_listener(PROXY_PREF_PORT)?;
    let proxy_port = proxy_listener
        .local_addr()
        .map(|a| a.port())
        .unwrap_or(PROXY_PREF_PORT);

    println!("oc-route: starting OpenCode server on port {}", serve_port);
    let mut serve_child = spawn_opencode_serve(serve_port)?;

    let oc = OcClient::new("127.0.0.1", serve_port);
    if let Err(e) = oc.wait_until_ready(Duration::from_secs(60)).await {
        let _ = serve_child.kill().await;
        return Err(e.context("OpenCode server failed to start"));
    }

    let (profile, session_choice) = match setup::run_setup(&oc, &mut profiles).await {
        Ok(v) => v,
        Err(e) => {
            let _ = serve_child.kill().await;
            return Err(e.context("setup failed"));
        }
    };
    setup::flush_stdout();
    elog(format!(
        "oc-route: profile '{}' (router={}, window={}, pool={:?})",
        profile.name, profile.router_model, profile.sliding_window, profile.model_pool
    ));

    let app_state = AppState {
        upstream: format!("http://127.0.0.1:{}", serve_port),
        oc: oc.clone(),
        profile: std::sync::Arc::new(profile),
        proxy_client: build_proxy_client(),
    };
    let app = proxy_router(app_state);
    let proxy_handle = tokio::spawn(async move {
        if let Err(e) = axum::serve(proxy_listener, app.into_make_service()).await {
            tracing::error!("proxy server error: {e}");
        }
    });

    elog(format!("oc-route: proxy listening on port {}", proxy_port));
    let mut attach_child = spawn_opencode_attach(proxy_port, &session_choice)?;

    let exit_reason = tokio::select! {
        status = attach_child.wait() => {
            match status {
                Ok(s) => format!("opencode attach exited ({})", s),
                Err(e) => format!("opencode attach failed: {e}"),
            }
        }
        _ = tokio::signal::ctrl_c() => {
            "interrupted".to_string()
        }
    };

    elog(format!("oc-route: {}", exit_reason));
    proxy_handle.abort();
    let _ = attach_child.kill().await;
    let _ = serve_child.kill().await;
    Ok(())
}

async fn cmd_proxy(args: &[String]) -> Result<()> {
    let mut upstream: Option<String> = None;
    let mut profile_name: Option<String> = None;
    let mut bind: String = format!("127.0.0.1:{}", PROXY_PREF_PORT);

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--upstream" => {
                i += 1;
                upstream = args.get(i).map(|s| s.to_string());
            }
            "--profile" => {
                i += 1;
                profile_name = args.get(i).map(|s| s.to_string());
            }
            "--bind" => {
                i += 1;
                if let Some(v) = args.get(i) {
                    bind = v.to_string();
                }
            }
            "-h" | "--help" => {
                println!("usage: oc-route proxy --upstream URL --profile NAME [--bind 127.0.0.1:4097]");
                return Ok(());
            }
            other => return Err(anyhow!("proxy: unknown argument '{}'", other)),
        }
        i += 1;
    }

    let upstream = upstream.ok_or_else(|| anyhow!("proxy: --upstream is required"))?;
    let profile_name = profile_name.ok_or_else(|| anyhow!("proxy: --profile is required"))?;

    let profiles = config::load().context("failed to load profiles")?;
    let profiles_path = config::profiles_path()?;
    let profile = profiles
        .profiles
        .iter()
        .find(|p| p.name == profile_name)
        .cloned()
        .ok_or_else(|| {
            anyhow!(
                "profile '{}' not found in {}",
                profile_name,
                profiles_path.display()
            )
        })?;
    config::validate(&profile)?;

    let (host, port) = parse_bind(&bind)?;
    let listener = bind_listener_on(host, port)?;

    let oc = OcClient::new_from_url(&upstream);
    let app_state = AppState {
        upstream: upstream.trim_end_matches('/').to_string(),
        oc,
        profile: std::sync::Arc::new(profile),
        proxy_client: build_proxy_client(),
    };
    let app = proxy_router(app_state);

    elog(format!(
        "oc-route proxy: listening on {} (upstream via profile '{}')",
        bind, profile_name
    ));
    let shutdown = async move {
        let _ = tokio::signal::ctrl_c().await;
        elog("oc-route proxy: shutting down");
    };
    axum::serve(listener, app.into_make_service())
        .with_graceful_shutdown(shutdown)
        .await
        .context("proxy server error")
}

fn print_help() {
    println!("oc-route {} — OpenCode Model Router", env!("CARGO_PKG_VERSION"));
    println!();
    println!("USAGE:");
    println!("  oc-route                  interactive setup + OpenCode TUI with model routing");
    println!("  oc-route proxy --upstream URL --profile NAME [--bind 127.0.0.1:4097]");
    println!("                            run only the routing proxy against an existing server");
    println!("  oc-route version          print version");
    println!();
    println!("The default command starts `opencode serve`, runs an interactive profile/session");
    println!("setup, starts the routing proxy, then launches `opencode attach` pointed at it.");
}

fn acquire_port(preferred: u16) -> u16 {
    if TcpListener::bind(("127.0.0.1", preferred)).is_ok() {
        return preferred;
    }
    find_free_port().unwrap_or(preferred)
}

fn find_free_port() -> Option<u16> {
    TcpListener::bind(("127.0.0.1", 0))
        .ok()
        .and_then(|l| l.local_addr().ok())
        .map(|a| a.port())
}

fn parse_bind(bind: &str) -> Result<(String, u16)> {
    let (host, port_s) = bind
        .rsplit_once(':')
        .ok_or_else(|| anyhow!("invalid --bind '{}', expected host:port", bind))?;
    let port: u16 = port_s
        .parse()
        .with_context(|| format!("invalid port in --bind '{}'", bind))?;
    Ok((host.to_string(), port))
}

fn bind_listener_on(host: String, port: u16) -> Result<TokioTcpListener> {
    let std_listener = TcpListener::bind((host.as_str(), port))
        .with_context(|| format!("failed to bind proxy listener on {}:{}", host, port))?;
    std_listener
        .set_nonblocking(true)
        .context("failed to set proxy listener non-blocking")?;
    TokioTcpListener::from_std(std_listener).context("failed to convert proxy listener to tokio")
}

fn bind_proxy_listener(preferred: u16) -> Result<TokioTcpListener> {
    let std_listener = if let Ok(l) = TcpListener::bind(("127.0.0.1", preferred)) {
        l
    } else {
        let port = find_free_port().context("no free port available for proxy")?;
        TcpListener::bind(("127.0.0.1", port))
            .with_context(|| format!("failed to bind proxy listener on port {}", port))?
    };
    std_listener
        .set_nonblocking(true)
        .context("failed to set proxy listener non-blocking")?;
    TokioTcpListener::from_std(std_listener).context("failed to convert proxy listener to tokio")
}

fn spawn_opencode_serve(port: u16) -> Result<Child> {
    let log_path = config::profile_dir()?.join("opencode-serve.log");
    let log_file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .with_context(|| format!("failed to open {}", log_path.display()))?;
    let child = Command::new("opencode")
        .arg("serve")
        .arg("--port")
        .arg(port.to_string())
        .arg("--hostname")
        .arg("127.0.0.1")
        .stdin(Stdio::null())
        .stdout(Stdio::from(log_file.try_clone()?))
        .stderr(Stdio::from(log_file))
        .spawn()
        .context("failed to spawn `opencode serve`")?;
    Ok(child)
}

fn spawn_opencode_attach(proxy_port: u16, choice: &SessionChoice) -> Result<Child> {
    let url = format!("http://127.0.0.1:{}", proxy_port);
    let mut cmd = Command::new("opencode");
    cmd.arg("attach").arg(&url);
    match choice {
        SessionChoice::Continue => {
            cmd.arg("--continue");
        }
        SessionChoice::Select(id) => {
            cmd.arg("--session").arg(id);
        }
        SessionChoice::New => {}
    }
    let child = cmd
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()
        .context("failed to spawn `opencode attach`")?;
    Ok(child)
}

