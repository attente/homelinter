//! Session drivers: the managed (ephemeral) flow and the standalone `web`
//! flow share one service-spawning routine.

use std::collections::BTreeMap;
use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use homelinter_clients::{Client, ClientsError};
use homelinter_model::config::{load_config, OutputFormat};
use homelinter_model::path::to_absolute;
use homelinter_service::{build_service, serve, Built};
use serde_json::{json, Value};
use tokio::task::JoinHandle;

use crate::args::{ManagedAction, ManagedOptions, ThemeArg, TuiOptions, WebOptions};
use crate::output;
use crate::tui;

/// A running in-process service instance plus the API client for it.
struct Instance {
    client: Client,
    task: JoinHandle<()>,
    data_dir: PathBuf,
    web_dir: Option<PathBuf>,
}

/// Resolve the SPA directory for `web`: `HOMELINTER_WEB_DIR` when set,
/// otherwise `<exe_dir>/webui/dist` if it exists.
fn resolve_web_dir() -> Option<PathBuf> {
    if let Ok(dir) = std::env::var("HOMELINTER_WEB_DIR") {
        let p = PathBuf::from(dir);
        if p.is_dir() {
            return Some(p);
        }
        output::note(&format!(
            "warning: HOMELINTER_WEB_DIR {} is not a directory; falling back",
            p.display()
        ));
    }
    let exe = std::env::current_exe().ok()?;
    let dir = exe.parent()?.join("webui/dist");
    if dir.is_dir() {
        return Some(dir);
    }
    // Development layout: workspace-relative webui/dist next to the repo.
    let dev = PathBuf::from("webui/dist");
    if dev.is_dir() {
        return Some(dev);
    }
    None
}

fn spawn(opts: homelinter_service::ServiceOptions, port: Option<u16>) -> Result<Instance, String> {
    let bind = match port {
        Some(p) => format!("127.0.0.1:{p}"),
        None => "127.0.0.1:0".to_string(),
    };
    let listener = std::net::TcpListener::bind(&bind).map_err(|e| match port {
        Some(p) => format!("cannot bind loopback listener on 127.0.0.1:{p}: {e}"),
        None => format!("cannot bind loopback listener: {e}"),
    })?;
    listener
        .set_nonblocking(true)
        .map_err(|e| format!("cannot set nonblocking: {e}"))?;
    let tokio_listener = tokio::net::TcpListener::from_std(listener)
        .map_err(|e| format!("cannot adopt listener: {e}"))?;
    let addr = tokio_listener
        .local_addr()
        .map_err(|e| format!("cannot read listener address: {e}"))?;

    let Built {
        app,
        shutdown_rx,
        data_dir,
        ..
    } = build_service(opts).map_err(|e| format!("service startup failed: {e}"))?;
    let web_dir = app.web_dir.clone();
    let task = tokio::spawn(async move {
        let _ = serve(tokio_listener, app, shutdown_rx).await;
    });
    let client = Client::new(addr.port());
    Ok(Instance {
        client,
        task,
        data_dir,
        web_dir,
    })
}

async fn wait_health(client: &Client, timeout: Duration) -> Result<(), String> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        match client.health().await {
            Ok(_) => return Ok(()),
            Err(ClientsError::Transport(_)) if tokio::time::Instant::now() < deadline => {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            Err(e) => return Err(format!("service health check failed: {e}")),
        }
    }
}

/// Poll the session until it reaches one of `want`, or fails.
async fn wait_for(client: &Client, want: &[&str], timeout: Duration) -> Result<Value, String> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let sess = client
            .session()
            .await
            .map_err(|e| format!("session poll failed: {e}"))?;
        let state = sess["state"].as_str().unwrap_or("").to_string();
        if want.iter().any(|w| *w == state) {
            return Ok(sess);
        }
        if state == "failed" {
            return Err(failure_message(&sess));
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(format!(
                "timed out waiting for session state (last: {state})"
            ));
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

fn failure_message(sess: &Value) -> String {
    let f = &sess["failure"];
    if let Some(m) = f["message"].as_str() {
        return format!("session failed: {m}");
    }
    if let Some(m) = f["reason"].as_str() {
        return format!("session failed: {m}");
    }
    "session failed".to_string()
}

/// Wait for the current batch's provider metadata requests to finish,
/// printing progress as `(completed/total)` suggestions. `total` is the
/// number of suggestions that originally had provider requests; `completed`
/// is how many have had all of theirs resolve. No-op when the batch has no
/// provider-dependent suggestions.
/// Print the scan's warnings (for example suppressed duplicate groups left
/// for manual review) as notices, so a CLI run does not hide them.
fn report_scan_warnings(sess: &Value, verbosity: i8) {
    if verbosity < 0 {
        return;
    }
    for w in sess["warnings"].as_array().into_iter().flatten() {
        if let Some(w) = w.as_str() {
            output::note(&format!("warning: {w}"));
        }
    }
}

async fn wait_for_metadata(
    client: &Client,
    verbosity: i8,
    timeout: Duration,
) -> Result<Value, String> {
    let deadline = tokio::time::Instant::now() + timeout;
    let mut last: Option<(usize, usize)> = None;
    loop {
        let sess = client
            .session()
            .await
            .map_err(|e| format!("session poll failed: {e}"))?;
        if sess["state"].as_str() == Some("failed") {
            return Err(failure_message(&sess));
        }
        let progress = &sess["metadata_progress"];
        let completed = progress["completed"].as_u64().unwrap_or(0) as usize;
        let total = progress["total"].as_u64().unwrap_or(0) as usize;
        if total == 0 || completed >= total {
            if verbosity >= 0 && total > 0 {
                output::note(&format!("metadata requests complete ({completed}/{total})"));
            }
            return Ok(sess);
        }
        if last != Some((completed, total)) {
            if verbosity >= 0 {
                output::note(&format!("resolving metadata requests: {completed}/{total}"));
            }
            last = Some((completed, total));
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(format!(
                "timed out waiting for provider metadata requests ({completed}/{total})"
            ));
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

/// Gracefully stop the in-process service and join its task.
async fn shutdown_instance(inst: Instance) {
    let _ = inst.client.shutdown().await;
    let mut task = inst.task;
    if tokio::time::timeout(Duration::from_secs(5), &mut task)
        .await
        .is_err()
    {
        task.abort();
        let _ = task.await;
        output::note("service did not exit promptly; forced termination");
    }
}

// ---------------------------------------------------------------------------
// Launch-time settings overrides
// ---------------------------------------------------------------------------

/// Build a partial config document from CLI flags. These seed the service's
/// initial effective settings; they are folded over the config-file chain
/// and a later runtime write wins over them for the same key.
fn launch_overrides(
    root: Option<&PathBuf>,
    hidden: bool,
    target_paths: &BTreeMap<String, String>,
    theme: Option<ThemeArg>,
    cwd: &Path,
) -> Result<Value, String> {
    let mut doc = serde_json::Map::new();
    let mut scan = serde_json::Map::new();
    if let Some(p) = root {
        let abs = to_absolute(p, cwd).map_err(|e| format!("invalid --root: {e}"))?;
        scan.insert("root".into(), json!(abs.to_string_lossy()));
    }
    if hidden {
        scan.insert("hidden".into(), json!(true));
    }
    if !scan.is_empty() {
        doc.insert("scan".into(), Value::Object(scan));
    }
    if !target_paths.is_empty() {
        doc.insert("target_paths".into(), json!(target_paths));
    }
    if let Some(theme) = theme {
        doc.insert("ui".into(), json!({ "theme": theme.as_str() }));
    }
    Ok(Value::Object(doc))
}

// ---------------------------------------------------------------------------
// Web (standalone service)
// ---------------------------------------------------------------------------

pub async fn web(opts: WebOptions) -> Result<u8, String> {
    let cwd = std::env::current_dir().map_err(|e| format!("cannot read working directory: {e}"))?;
    let overrides = launch_overrides(
        opts.root.as_ref(),
        opts.hidden,
        &opts.target_paths,
        opts.theme,
        &cwd,
    )?;
    // `--root`, `--hidden`, `--target-path`, and `--theme` are runtime
    // settings: they seed the service's initial settings and are editable
    // through the API.
    let inst = spawn(
        homelinter_service::ServiceOptions {
            cli_config: opts.config,
            home_override: None,
            data_dir_override: None,
            config_chain_override: None,
            web_dir: resolve_web_dir(),
            initial_overrides: Some(overrides),
        },
        opts.port,
    )?;
    if opts.verbosity >= 0 {
        output::note(&format!(
            "data dir: {} (cache and audit log)",
            inst.data_dir.display()
        ));
    }
    if let Some(web) = &inst.web_dir {
        output::note(&format!("web UI: serving {} (SPA)", web.display()));
    } else {
        output::note("web UI: no SPA found (API only)");
    }
    if opts.cache_clear {
        inst.client
            .cache_clear()
            .await
            .map_err(|e| format!("cache clear failed: {e}"))?;
    }
    let port = port_of(&inst.client);
    output::readiness(port);
    // The standalone service never shuts itself down; it ends when an
    // operator calls POST /api/v1/shutdown or terminates the process.
    let task = inst.task;
    loop {
        if task.is_finished() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    let _ = task.await;
    Ok(0)
}

fn port_of(client: &Client) -> u16 {
    client
        .base_url()
        .rsplit(':')
        .next()
        .and_then(|p| p.parse().ok())
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// TUI session
// ---------------------------------------------------------------------------

pub async fn tui(opts: TuiOptions) -> Result<u8, String> {
    let verbosity = opts.verbosity;
    let cwd = std::env::current_dir().map_err(|e| format!("cannot read working directory: {e}"))?;
    let overrides = launch_overrides(
        opts.root.as_ref(),
        opts.hidden,
        &opts.target_paths,
        None,
        &cwd,
    )?;

    let inst = spawn(
        homelinter_service::ServiceOptions {
            cli_config: opts.config.clone(),
            home_override: None,
            data_dir_override: None,
            web_dir: None,
            initial_overrides: Some(overrides),
            ..Default::default()
        },
        None,
    )?;
    let client = inst.client.clone();
    wait_health(&client, Duration::from_secs(10)).await?;

    if opts.cache_clear {
        client
            .cache_clear()
            .await
            .map_err(|e| format!("cache clear failed: {e}"))?;
    }
    if verbosity >= 0 {
        output::note(&format!(
            "data dir: {} (cache and audit log)",
            inst.data_dir.display()
        ));
    }

    // The TUI owns the terminal from here on; it returns once the user
    // quits, and any in-flight operation has been cancelled or finished.
    let code = tui::run(&client).await?;
    shutdown_instance(inst).await;
    Ok(code)
}

// ---------------------------------------------------------------------------
// Managed (CLI) sessions
// ---------------------------------------------------------------------------

pub async fn managed(opts: ManagedOptions) -> Result<u8, String> {
    if matches!(opts.action, ManagedAction::Interactive) && !std::io::stdin().is_terminal() {
        return Err(
            "interactive review requires a terminal; run `homelinter tui`, or pass --execute or --export to act without review"
                .to_string(),
        );
    }
    let verbosity = opts.verbosity;
    let cwd = std::env::current_dir().map_err(|e| format!("cannot read working directory: {e}"))?;
    let overrides = launch_overrides(
        opts.root.as_ref(),
        opts.hidden,
        &opts.target_paths,
        None,
        &cwd,
    )?;

    let inst = spawn(
        homelinter_service::ServiceOptions {
            cli_config: opts.config.clone(),
            home_override: None,
            data_dir_override: None,
            web_dir: None,
            initial_overrides: Some(overrides),
            ..Default::default()
        },
        None,
    )?;
    let client = inst.client.clone();
    wait_health(&client, Duration::from_secs(10)).await?;

    // The effective scan root for the progress note comes from the
    // service's current settings, not from a request parameter.
    let cfg = client
        .config()
        .await
        .map_err(|e| format!("cannot read settings: {e}"))?;
    let root = cfg["scan"]["root"]
        .as_str()
        .map(PathBuf::from)
        .or_else(homelinter_model::path::home_dir)
        .ok_or_else(|| "cannot determine the scan root".to_string())?;

    // The effective output format: --format wins over the config file.
    let format = match opts.format {
        Some(f) => f.to_model(),
        None => {
            let loaded = load_config(opts.config.as_deref())
                .map_err(|e| format!("cannot read config: {e}"))?;
            loaded.raw.output_format()
        }
    };

    if opts.cache_clear {
        client
            .cache_clear()
            .await
            .map_err(|e| format!("cache clear failed: {e}"))?;
        if verbosity >= 0 {
            output::note("cleared the metadata cache");
        }
    }

    if verbosity >= 0 {
        output::note(&format!("scanning {}", root.display()));
    }
    // Scans run against the service's current settings; the request carries
    // no per-scan parameters.
    client
        .start_scan(json!({}))
        .await
        .map_err(|e| format!("cannot start scan: {e}"))?;
    let _sess = wait_for(&client, &["ready", "done"], Duration::from_secs(1800)).await?;
    if verbosity >= 0 {
        output::note("analysis complete; suggestions generated");
    }
    // Provider metadata (geocoding, descriptions) resolves in the
    // background; pending suggestions are not executable until it does.
    // Show progress and wait so the CLI acts on complete metadata.
    let sess = wait_for_metadata(&client, verbosity, Duration::from_secs(1800)).await?;
    report_scan_warnings(&sess, verbosity);

    let suggestions = client
        .suggestions(None)
        .await
        .map_err(|e| format!("cannot list suggestions: {e}"))?;
    let items = suggestions["items"].as_array().cloned().unwrap_or_default();
    let ids: Vec<String> = items
        .iter()
        .map(|s| s["id"].as_str().unwrap_or("").to_string())
        .filter(|id| !id.is_empty())
        .collect();

    if ids.is_empty() {
        if verbosity >= 0 {
            output::note("no suggestions to act on");
        }
        shutdown_instance(inst).await;
        return Ok(0);
    }

    let result = match &opts.action {
        ManagedAction::Interactive => interactive_summary(&items),
        ManagedAction::Execute => run_execution(&client, &ids, format, verbosity).await,
        ManagedAction::Export(path) => {
            let path =
                to_absolute(path, &cwd).map_err(|e| format!("invalid --export path: {e}"))?;
            export_and_report(&client, &ids, &path, format).await
        }
    };

    let code = result?;
    shutdown_instance(inst).await;
    Ok(code)
}

async fn export_and_report(
    client: &Client,
    ids: &[String],
    path: &std::path::Path,
    format: OutputFormat,
) -> Result<u8, String> {
    match client
        .export(
            Some(ids.to_vec()),
            Some(path.to_string_lossy().into_owned()),
        )
        .await
    {
        Ok(homelinter_clients::ExportOutcome::Written {
            path,
            platform,
            count,
        }) => {
            output::emit_export(format, &path, &platform, count)
                .map_err(|e| format!("cannot write output: {e}"))?;
            Ok(0)
        }
        Ok(homelinter_clients::ExportOutcome::ScriptText { .. }) => {
            Err("service returned script text although a path was requested".to_string())
        }
        Err(e) => Err(format!("export failed: {e}")),
    }
}

async fn run_execution(
    client: &Client,
    ids: &[String],
    format: OutputFormat,
    verbosity: i8,
) -> Result<u8, String> {
    if verbosity >= 0 {
        output::note("executing suggestions");
    }
    client
        .execute(Some(ids.to_vec()))
        .await
        .map_err(|e| format!("cannot start execution: {e}"))?;
    wait_for(client, &["done"], Duration::from_secs(1800)).await?;
    let report = client
        .execution_report()
        .await
        .map_err(|e| format!("cannot read execution report: {e}"))?;
    output::emit_report(format, &report).map_err(|e| format!("cannot write output: {e}"))?;
    Ok(0)
}

/// Demoted interactive review: the full-screen review experience lives in
/// `homelinter tui`. A plain scan prints how many suggestions were generated
/// and points the user at the review and non-interactive action entry points.
fn interactive_summary(items: &[Value]) -> Result<u8, String> {
    let out = std::io::stdout();
    let mut out = out.lock();
    let _ = writeln!(
        out,
        "{} suggestion(s) generated; nothing was executed.",
        items.len()
    );
    let _ = writeln!(out);
    let _ = writeln!(out, "Review and apply them in the full-screen TUI:");
    let _ = writeln!(out, "  homelinter tui");
    let _ = writeln!(out, "Or act on all suggestions without review:");
    let _ = writeln!(out, "  homelinter scan --execute");
    let _ = writeln!(out, "  homelinter scan --export <path>");
    let _ = out.flush();
    Ok(0)
}
