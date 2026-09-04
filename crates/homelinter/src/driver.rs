//! Session drivers: the managed (ephemeral) flow and the standalone `web`
//! flow share one service-spawning routine.

use std::io::{BufRead, IsTerminal, Write};
use std::path::PathBuf;
use std::time::Duration;

use homelinter_clients::{Client, ClientsError};
use homelinter_model::config::{load_config, OutputFormat};
use homelinter_model::path::to_absolute;
use homelinter_service::{build_service, serve, Built};
use serde_json::{json, Value};
use tokio::task::JoinHandle;

use crate::args::{ManagedAction, ManagedOptions, TuiOptions, WebOptions};
use crate::output;
use crate::tui;

/// A running in-process service instance plus the API client for it.
struct Instance {
    client: Client,
    task: JoinHandle<()>,
    data_dir: PathBuf,
}

fn spawn(opts: homelinter_service::ServiceOptions) -> Result<Instance, String> {
    let listener = std::net::TcpListener::bind("127.0.0.1:0")
        .map_err(|e| format!("cannot bind loopback listener: {e}"))?;
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
    let task = tokio::spawn(async move {
        let _ = serve(tokio_listener, app, shutdown_rx).await;
    });
    let client = Client::new(addr.port());
    Ok(Instance {
        client,
        task,
        data_dir,
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
// Web (standalone service)
// ---------------------------------------------------------------------------

pub async fn web(opts: WebOptions) -> Result<u8, String> {
    let cwd = std::env::current_dir().map_err(|e| format!("cannot read working directory: {e}"))?;
    let root = match &opts.root {
        Some(p) => to_absolute(p, &cwd).map_err(|e| format!("invalid --root: {e}"))?,
        None => homelinter_model::path::home_dir()
            .ok_or_else(|| "cannot determine the home directory".to_string())?,
    };
    // The web command pre-seeds the session's default scan root with the
    // service home; scans may still override it per request.
    let inst = spawn(homelinter_service::ServiceOptions {
        cli_config: opts.config,
        home_override: Some(root),
        data_dir_override: None,
    })?;
    if opts.verbosity >= 0 {
        output::note(&format!(
            "data dir: {} (cache and audit log)",
            inst.data_dir.display()
        ));
    }
    if opts.hidden || !opts.target_paths.is_empty() {
        // Honest seam: the web command accepts --hidden and --target-path
        // but this build does not persist session defaults for them. The
        // SPA must pass hidden/target_paths per scan request.
        output::note("warning: --hidden/--target-path session pre-seed is not wired for `web` in this build; pass them per scan");
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

    let root = match &opts.root {
        Some(p) => to_absolute(p, &cwd).map_err(|e| format!("invalid --root: {e}"))?,
        None => homelinter_model::path::home_dir()
            .ok_or_else(|| "cannot determine the home directory".to_string())?,
    };

    let inst = spawn(homelinter_service::ServiceOptions {
        cli_config: opts.config.clone(),
        home_override: None,
        data_dir_override: None,
    })?;
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
    let code = tui::run(&client, root, opts.hidden, opts.target_paths).await?;
    shutdown_instance(inst).await;
    Ok(code)
}

// ---------------------------------------------------------------------------
// Managed (CLI) sessions
// ---------------------------------------------------------------------------

pub async fn managed(opts: ManagedOptions) -> Result<u8, String> {
    if matches!(opts.action, ManagedAction::Interactive) && !std::io::stdin().is_terminal() {
        return Err(
            "interactive mode requires a terminal; use --execute, --dry-run, or --export"
                .to_string(),
        );
    }
    let verbosity = opts.verbosity;
    let cwd = std::env::current_dir().map_err(|e| format!("cannot read working directory: {e}"))?;

    let root = match &opts.root {
        Some(p) => to_absolute(p, &cwd).map_err(|e| format!("invalid --root: {e}"))?,
        None => homelinter_model::path::home_dir()
            .ok_or_else(|| "cannot determine the home directory".to_string())?,
    };

    let inst = spawn(homelinter_service::ServiceOptions {
        cli_config: opts.config.clone(),
        home_override: None,
        data_dir_override: None,
    })?;
    let client = inst.client.clone();
    wait_health(&client, Duration::from_secs(10)).await?;

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
    let mut scan_body = json!({
        "root": root.to_string_lossy(),
        "hidden": opts.hidden,
    });
    if !opts.target_paths.is_empty() {
        scan_body["target_paths"] = json!(opts.target_paths);
    }
    client
        .start_scan(scan_body)
        .await
        .map_err(|e| format!("cannot start scan: {e}"))?;
    let _sess = wait_for(&client, &["ready", "done"], Duration::from_secs(1800)).await?;
    if verbosity >= 0 {
        output::note("analysis complete; suggestions generated");
    }

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
        ManagedAction::Interactive => interactive(client.clone(), &items, ids, format).await,
        ManagedAction::Execute => run_execution(&client, &ids, false, format, verbosity).await,
        ManagedAction::DryRun => run_execution(&client, &ids, true, format, verbosity).await,
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
    dry_run: bool,
    format: OutputFormat,
    verbosity: i8,
) -> Result<u8, String> {
    if verbosity >= 0 {
        output::note(if dry_run {
            "dry-run requested; the filesystem will not be modified"
        } else {
            "executing suggestions"
        });
    }
    client
        .execute(Some(ids.to_vec()), dry_run)
        .await
        .map_err(|e| format!("cannot start execution: {e}"))?;
    wait_for(client, &["done"], Duration::from_secs(1800)).await?;
    let report = client
        .execution_report()
        .await
        .map_err(|e| format!("cannot read execution report: {e}"))?;
    output::emit_report(format, &report, dry_run)
        .map_err(|e| format!("cannot write output: {e}"))?;
    Ok(0)
}

/// Interactive review: list suggestions, then a menu loop.
async fn interactive(
    client: Client,
    items: &[Value],
    ids: Vec<String>,
    format: OutputFormat,
) -> Result<u8, String> {
    output::print_suggestions(items);
    println!("Suggestions can be reviewed, exported, or executed.");
    let stdin = std::io::stdin();
    loop {
        print!("menu [review/export/execute/quit]: ");
        std::io::stdout().flush().ok();
        let mut line = String::new();
        let n = stdin
            .lock()
            .read_line(&mut line)
            .map_err(|e| format!("cannot read input: {e}"))?;
        if n == 0 {
            return Err("end of input; nothing was executed".to_string());
        }
        let trimmed = line.trim();
        let mut parts = trimmed.splitn(2, char::is_whitespace);
        let action = parts.next().unwrap_or("").to_lowercase();
        match action.as_str() {
            "r" | "review" => output::print_suggestions(items),
            "x" | "execute" => {
                return run_execution(&client, &ids, false, format, 1).await;
            }
            "e" | "export" => {
                if let Some(rest) = parts.next() {
                    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
                    let p = to_absolute(std::path::Path::new(rest.trim()), &cwd)
                        .map_err(|e| format!("invalid export path: {e}"))?;
                    export_and_report(&client, &ids, &p, format).await?;
                } else {
                    println!("usage: export <path>");
                }
            }
            "q" | "quit" | "exit" => {
                println!("exiting without executing; review the exported script or run --execute to apply changes.");
                return Ok(0);
            }
            "" => {}
            other => println!("unknown command '{other}' (review, export, execute, quit)"),
        }
    }
}
