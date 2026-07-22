mod config;
mod oc_client;
mod proxy;
mod router;
mod setup;

use anyhow::{anyhow, Context, Result};
use setup::SessionChoice;
use std::io::Write;
use std::net::TcpListener;
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;
use tokio::net::TcpListener as TokioTcpListener;
use tokio::process::{Child, Command};

use oc_client::OcClient;
use proxy::{build_proxy_client, router as proxy_router, AppState};

const SERVE_PREF_PORT: u16 = 4096;
const PROXY_PREF_PORT: u16 = 4097;
const ROUTER_STORAGE_PREFIX: &str = "oc-route-router-";

#[derive(Clone, Copy)]
enum ServerPurpose {
    Primary,
    RouterSidecar,
}

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
        Some(other) => Err(anyhow!("unknown command or option '{other}'; try --help")),
        None => cmd_default().await,
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
    let (mut serve_child, _serve_storage) =
        spawn_opencode_serve(serve_port, ServerPurpose::Primary)?;

    let directory = std::env::current_dir()
        .context("failed to determine the current directory")?
        .to_string_lossy()
        .into_owned();
    let oc = OcClient::new("127.0.0.1", serve_port).with_environment(Some(directory));
    tokio::select! {
        ready = oc.wait_until_ready(Duration::from_secs(60)) => {
            if let Err(error) = ready {
                let _ = serve_child.kill().await;
                let _ = serve_child.wait().await;
                return Err(error.context("OpenCode server failed to start"));
            }
        }
        status = serve_child.wait() => {
            return Err(anyhow!("OpenCode server exited during startup: {}", format_status(status)));
        }
    }
    if let Err(error) = oc.verify_router_agent().await {
        let _ = serve_child.kill().await;
        let _ = serve_child.wait().await;
        return Err(error.context("OpenCode did not accept oc-route's internal protocol agent"));
    }

    let (profile, session_choice) = match setup::run_setup(&oc, &mut profiles).await {
        Ok(v) => v,
        Err(e) => {
            let _ = serve_child.kill().await;
            let _ = serve_child.wait().await;
            return Err(e.context("setup failed"));
        }
    };
    setup::flush_stdout();
    elog(format!(
        "oc-route: profile '{}' (router={}, window={}, pool={:?})",
        profile.name, profile.router_model, profile.sliding_window, profile.model_pool
    ));

    let router_sessions = std::sync::Arc::new(oc_client::RouterSessions::new(oc.clone()));
    let app_state = AppState {
        upstream: format!("http://127.0.0.1:{}", serve_port),
        oc: oc.clone(),
        router_oc: oc.clone(),
        profile: std::sync::Arc::new(profile),
        proxy_client: build_proxy_client(),
        router_sessions: router_sessions.clone(),
    };
    let app = proxy_router(app_state);
    let mut proxy_handle = tokio::spawn(async move {
        axum::serve(proxy_listener, app.into_make_service())
            .await
            .context("proxy server error")
    });

    elog(format!("oc-route: proxy listening on port {}", proxy_port));
    let mut attach_child = match spawn_opencode_attach(proxy_port, &session_choice) {
        Ok(child) => child,
        Err(error) => {
            proxy_handle.abort();
            let _ = proxy_handle.await;
            router_sessions.cleanup_all().await;
            let _ = serve_child.kill().await;
            let _ = serve_child.wait().await;
            return Err(error);
        }
    };

    let result = tokio::select! {
        status = attach_child.wait() => {
            match status {
                Ok(status) if status.success() => Ok(format!("opencode attach exited ({status})")),
                Ok(status) => Err(anyhow!("opencode attach exited unsuccessfully ({status})")),
                Err(error) => Err(anyhow!("opencode attach failed: {error}")),
            }
        }
        status = serve_child.wait() => {
            Err(anyhow!("opencode serve exited unexpectedly: {}", format_status(status)))
        }
        proxy = &mut proxy_handle => {
            match proxy {
                Ok(Ok(())) => Err(anyhow!("routing proxy exited unexpectedly")),
                Ok(Err(error)) => Err(error),
                Err(error) => Err(anyhow!("routing proxy task failed: {error}")),
            }
        }
        _ = tokio::signal::ctrl_c() => {
            Ok("interrupted".to_string())
        }
    };

    if let Ok(reason) = &result {
        elog(format!("oc-route: {reason}"));
    }
    if !proxy_handle.is_finished() {
        proxy_handle.abort();
        let _ = proxy_handle.await;
    }
    let _ = attach_child.kill().await;
    let _ = attach_child.wait().await;
    router_sessions.cleanup_all().await;
    let _ = serve_child.kill().await;
    let _ = serve_child.wait().await;
    result.map(|_| ())
}

async fn cmd_proxy(args: &[String]) -> Result<()> {
    let mut upstream: Option<String> = None;
    let mut profile_name: Option<String> = None;
    let mut bind: String = format!("127.0.0.1:{}", PROXY_PREF_PORT);
    let mut directory: Option<String> = None;
    let mut username: Option<String> = std::env::var("OPENCODE_SERVER_USERNAME").ok();
    let password: Option<String> = std::env::var("OPENCODE_SERVER_PASSWORD").ok();

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--upstream" => {
                i += 1;
                upstream = Some(required_value(args, i, "--upstream")?.to_string());
            }
            "--profile" => {
                i += 1;
                profile_name = Some(required_value(args, i, "--profile")?.to_string());
            }
            "--bind" => {
                i += 1;
                bind = required_value(args, i, "--bind")?.to_string();
            }
            "--directory" => {
                i += 1;
                directory = Some(required_value(args, i, "--directory")?.to_string());
            }
            "--username" => {
                i += 1;
                username = Some(required_value(args, i, "--username")?.to_string());
            }
            "-h" | "--help" => {
                print_proxy_help();
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

    let router_directory = directory.clone();
    let oc = OcClient::new_from_url(&upstream).with_context(directory, username, password);
    oc.wait_until_ready(Duration::from_secs(10))
        .await
        .context("upstream OpenCode server is not ready")?;

    let (router_oc, mut router_child, _router_storage) = if oc
        .has_router_agent()
        .await
        .context("failed to inspect the upstream OpenCode agents")?
    {
        tracing::info!("using upstream OpenCode's verified oc-route agent");
        (oc.clone(), None, None)
    } else {
        let router_port = acquire_port(SERVE_PREF_PORT);
        let (mut child, storage) = spawn_opencode_serve(router_port, ServerPurpose::RouterSidecar)?;
        let client = OcClient::new("127.0.0.1", router_port).with_environment(router_directory);
        tokio::select! {
            ready = client.wait_until_ready(Duration::from_secs(60)) => {
                if let Err(error) = ready {
                    let _ = child.kill().await;
                    let _ = child.wait().await;
                    return Err(error.context("oc-route's private router server failed to start"));
                }
            }
            status = child.wait() => {
                return Err(anyhow!("oc-route's private router server exited during startup: {}", format_status(status)));
            }
        }
        if let Err(error) = client.verify_router_agent().await {
            let _ = child.kill().await;
            let _ = child.wait().await;
            return Err(error.context("OpenCode did not accept oc-route's internal protocol agent"));
        }
        let models = match client.list_models().await {
            Ok(models) => models,
            Err(error) => {
                let _ = child.kill().await;
                let _ = child.wait().await;
                return Err(
                    error.context("failed to inspect models on oc-route's private router server")
                );
            }
        };
        let router_model_available = models
            .iter()
            .any(|model| format!("{}/{}", model.provider_id, model.id) == profile.router_model);
        if !router_model_available {
            let _ = child.kill().await;
            let _ = child.wait().await;
            anyhow::bail!(
                "router model '{}' is not available to the local private router server",
                profile.router_model
            );
        }
        (client, Some(child), storage)
    };

    let router_sessions = std::sync::Arc::new(oc_client::RouterSessions::new(router_oc.clone()));
    let app_state = AppState {
        upstream: upstream.trim_end_matches('/').to_string(),
        oc: oc.clone(),
        router_oc,
        profile: std::sync::Arc::new(profile),
        proxy_client: build_proxy_client(),
        router_sessions: router_sessions.clone(),
    };
    let app = proxy_router(app_state);

    elog(format!(
        "oc-route proxy: listening on {} (upstream via profile '{}')",
        bind, profile_name
    ));
    let mut proxy_handle = tokio::spawn(async move {
        axum::serve(listener, app.into_make_service())
            .await
            .context("proxy server error")
    });
    let result = tokio::select! {
        proxy = &mut proxy_handle => match proxy {
            Ok(Ok(())) => Err(anyhow!("routing proxy exited unexpectedly")),
            Ok(Err(error)) => Err(error),
            Err(error) => Err(anyhow!("routing proxy task failed: {error}")),
        },
        status = wait_optional_child(&mut router_child) => {
            Err(anyhow!("oc-route's private router server exited unexpectedly: {}", format_status(status)))
        }
        _ = tokio::signal::ctrl_c() => {
            elog("oc-route proxy: shutting down");
            Ok(())
        }
    };
    if !proxy_handle.is_finished() {
        proxy_handle.abort();
        let _ = proxy_handle.await;
    }
    router_sessions.cleanup_all().await;
    if let Some(child) = &mut router_child {
        let _ = child.kill().await;
        let _ = child.wait().await;
    }
    result
}

fn print_help() {
    println!(
        "oc-route {} — OpenCode Model Router",
        env!("CARGO_PKG_VERSION")
    );
    println!();
    println!("USAGE:");
    println!("  oc-route                  interactive setup + OpenCode TUI with model routing");
    println!("  oc-route proxy --upstream URL --profile NAME [OPTIONS]");
    println!("                            run only the routing proxy against an existing server");
    println!("  oc-route version          print version");
    println!();
    println!("The default command starts `opencode serve`, runs an interactive profile/session");
    println!("setup, starts the routing proxy, then launches `opencode attach` pointed at it.");
}

fn print_proxy_help() {
    println!("usage: oc-route proxy --upstream URL --profile NAME [OPTIONS]");
    println!();
    println!("OPTIONS:");
    println!("  --bind HOST:PORT     proxy address (default 127.0.0.1:4097)");
    println!("  --directory PATH     fixed OpenCode project directory");
    println!("  --username NAME      upstream basic-auth username");
    println!("Environment: OPENCODE_SERVER_PASSWORD supplies upstream basic auth securely.");
}

fn required_value<'a>(args: &'a [String], index: usize, option: &str) -> Result<&'a str> {
    args.get(index)
        .map(String::as_str)
        .filter(|value| !value.starts_with('-'))
        .ok_or_else(|| anyhow!("proxy: {option} requires a value"))
}

fn format_status(status: std::io::Result<std::process::ExitStatus>) -> String {
    match status {
        Ok(status) => status.to_string(),
        Err(error) => format!("wait failed: {error}"),
    }
}

async fn wait_optional_child(
    child: &mut Option<Child>,
) -> std::io::Result<std::process::ExitStatus> {
    match child {
        Some(child) => child.wait().await,
        None => std::future::pending().await,
    }
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

fn spawn_opencode_serve(
    port: u16,
    purpose: ServerPurpose,
) -> Result<(Child, Option<tempfile::TempDir>)> {
    let log_name = match purpose {
        ServerPurpose::Primary => "opencode-serve.log",
        ServerPurpose::RouterSidecar => "opencode-router.log",
    };
    let log_path = config::profile_dir()?.join(log_name);
    let log_file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .with_context(|| format!("failed to open {}", log_path.display()))?;
    let mut command = Command::new("opencode");
    command
        .arg("serve")
        .arg("--port")
        .arg(port.to_string())
        .arg("--hostname")
        .arg("127.0.0.1")
        .stdin(Stdio::null())
        .stdout(Stdio::from(log_file.try_clone()?))
        .stderr(Stdio::from(log_file));
    command.env(
        "OPENCODE_CONFIG_CONTENT",
        opencode_config_with_router_agent()?,
    );
    terminate_child_if_parent_dies(&mut command);
    let storage = if matches!(purpose, ServerPurpose::RouterSidecar) {
        // Keep internal routing sessions completely separate from the upstream
        // OpenCode database while avoiding the large RSS cost of SQLite :memory:.
        if let Err(error) = cleanup_stale_router_storage(&std::env::temp_dir()) {
            tracing::warn!("could not clean stale private router storage: {error}");
        }
        let storage = tempfile::Builder::new()
            .prefix(ROUTER_STORAGE_PREFIX)
            .tempdir()
            .context("failed to create private router database directory")?;
        std::fs::write(
            storage.path().join("owner.pid"),
            std::process::id().to_string(),
        )
        .context("failed to record private router database ownership")?;
        command.env("OPENCODE_DB", storage.path().join("opencode.db"));
        Some(storage)
    } else {
        None
    };
    let child = command
        .spawn()
        .context("failed to spawn `opencode serve`")?;
    Ok((child, storage))
}

/// Add the private router agent to OpenCode's last-applied in-memory config layer.
/// Existing JSON/JSONC content is parsed, merged, and passed only to the child.
fn opencode_config_with_router_agent() -> Result<String> {
    let raw = match std::env::var("OPENCODE_CONFIG_CONTENT") {
        Ok(raw) => Some(raw),
        Err(std::env::VarError::NotPresent) => None,
        Err(error) => return Err(error).context("could not read OPENCODE_CONFIG_CONTENT"),
    };
    merge_router_agent_config(raw.as_deref())
}

fn merge_router_agent_config(raw: Option<&str>) -> Result<String> {
    let mut config = match raw {
        Some(raw) => json5::from_str::<serde_json::Value>(raw)
            .context("OPENCODE_CONFIG_CONTENT is not valid JSONC")?,
        None => serde_json::json!({}),
    };

    let root = config
        .as_object_mut()
        .ok_or_else(|| anyhow!("top-level value is not an object"))?;
    let agents = root.entry("agent").or_insert_with(|| serde_json::json!({}));
    let Some(agents) = agents.as_object_mut() else {
        return Err(anyhow!("agent value is not an object"));
    };
    agents.insert(
        "oc-route".to_string(),
        serde_json::json!({
            "description": "Private model-selection protocol used by oc-route",
            "mode": "primary",
            "hidden": true,
            "prompt": router::ROUTER_SYSTEM_PROMPT,
            "permission": { "*": "deny" }
        }),
    );
    serde_json::to_string(&config).context("failed to serialize merged OpenCode config")
}

fn spawn_opencode_attach(proxy_port: u16, choice: &SessionChoice) -> Result<Child> {
    let url = format!("http://127.0.0.1:{}", proxy_port);
    let mut cmd = Command::new("opencode");
    cmd.arg("attach").arg(&url);
    terminate_child_if_parent_dies(&mut cmd);
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

/// Linux does not automatically terminate a child when its parent is killed.
/// Register SIGTERM before exec so an abruptly terminated oc-route cannot strand
/// an OpenCode server or attach process. The parent check closes the fork/exec race
/// where oc-route dies immediately before the child installs the signal.
fn terminate_child_if_parent_dies(command: &mut Command) {
    // SAFETY: pre_exec restricts the closure to async-signal-safe operations. prctl,
    // getppid, and raise are direct libc syscalls and the closure captures nothing.
    unsafe {
        command.pre_exec(|| {
            if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            if libc::getppid() == 1 {
                libc::raise(libc::SIGTERM);
            }
            Ok(())
        });
    }
}

fn cleanup_stale_router_storage(temp_root: &Path) -> Result<()> {
    let entries = match std::fs::read_dir(temp_root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error).context("failed to inspect temporary directory"),
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        if !name.to_string_lossy().starts_with(ROUTER_STORAGE_PREFIX) {
            continue;
        }
        let path = entry.path();
        let owner = match std::fs::read_to_string(path.join("owner.pid"))
            .ok()
            .and_then(|value| value.trim().parse::<u32>().ok())
        {
            Some(owner) => owner,
            None => continue,
        };
        if Path::new("/proc").join(owner.to_string()).exists() {
            continue;
        }
        if let Err(error) = std::fs::remove_dir_all(&path) {
            tracing::warn!("failed to remove stale {}: {error}", path.display());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn router_agent_overlay_is_private_and_preserves_existing_config() {
        let merged = merge_router_agent_config(Some(
            r#"{"theme":"system","agent":{"review":{"prompt":"review"}}}"#,
        ))
        .unwrap();
        let value: serde_json::Value = serde_json::from_str(&merged).unwrap();
        assert_eq!(value["theme"], "system");
        assert_eq!(value["agent"]["review"]["prompt"], "review");
        assert_eq!(
            value["agent"]["oc-route"]["prompt"],
            router::ROUTER_SYSTEM_PROMPT
        );
        assert_eq!(value["agent"]["oc-route"]["hidden"], true);
        assert_eq!(value["agent"]["oc-route"]["permission"]["*"], "deny");
    }

    #[test]
    fn router_agent_overlay_accepts_jsonc_but_refuses_wrong_shapes() {
        let merged = merge_router_agent_config(Some("{/* comment */ theme: 'system',}")).unwrap();
        let value: serde_json::Value = serde_json::from_str(&merged).unwrap();
        assert_eq!(value["theme"], "system");
        assert_eq!(
            value["agent"]["oc-route"]["prompt"],
            router::ROUTER_SYSTEM_PROMPT
        );
        assert!(merge_router_agent_config(Some("[]")).is_err());
        assert!(merge_router_agent_config(Some(r#"{"agent":[]}"#)).is_err());
    }

    #[test]
    fn stale_router_storage_is_removed_without_touching_live_or_unowned_paths() {
        let root = tempfile::tempdir().unwrap();
        let stale = root.path().join(format!("{ROUTER_STORAGE_PREFIX}stale"));
        let live = root.path().join(format!("{ROUTER_STORAGE_PREFIX}live"));
        let unowned = root.path().join(format!("{ROUTER_STORAGE_PREFIX}unowned"));
        let unrelated = root.path().join("something-else");
        for path in [&stale, &live, &unowned, &unrelated] {
            std::fs::create_dir(path).unwrap();
        }
        std::fs::write(stale.join("owner.pid"), u32::MAX.to_string()).unwrap();
        std::fs::write(live.join("owner.pid"), std::process::id().to_string()).unwrap();

        cleanup_stale_router_storage(root.path()).unwrap();

        assert!(!stale.exists());
        assert!(live.exists());
        assert!(unowned.exists());
        assert!(unrelated.exists());
    }
}
