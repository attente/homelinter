//! Browser-hosted homelinter app.
//!
//! This is the Linux launcher behind the AppImage. It owns the same
//! in-process `homelinter-service` that the CLI's `web` command and the
//! macOS Tauri shell use, then opens the SPA in an app-mode window of a
//! browser the user already has, and shuts the service down when that
//! window closes.
//!
//! The alternative, a Tauri build, would pull WebKitGTK and GTK into the
//! bundle. AppImage consumers are frequently on NixOS or other systems
//! where that stack is not in a shared FHS location, and bundling it
//! correctly (GStreamer plugins, GIO modules, fontconfig, glib schemas) is
//! fragile. An app-mode browser window has no location bar, bookmark bar,
//! or tab strip, so the result looks like the macOS shell while the
//! AppImage itself needs no GUI toolkit at all.
//!
//! Lifecycle, mirroring `crates/desktop/src-tauri/src/main.rs`:
//!
//! 1. Resolve the built SPA (bundled as `usr/share/homelinter/webui`).
//! 2. Pick a browser (Chromium family first, Firefox as a kiosk fallback).
//! 3. Bind an ephemeral loopback port, build the service, wait for health.
//! 4. Spawn the browser against that origin and wait for it to exit.
//! 5. Signal the service's shutdown watch and wait, bounded, for it to stop.
//!
//! Nothing is spawned as a child process except the browser, so a crash of
//! either side cannot leave an orphaned backend behind.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use homelinter_clients::Client;
use tokio::process::{Child, Command};
use tokio::sync::watch;
use tokio::task::JoinHandle;

/// How long the backend may take to answer `GET /api/v1/health` before the
/// launcher gives up and reports the failure instead of opening a blank
/// window.
const HEALTH_TIMEOUT: Duration = Duration::from_secs(10);

/// How long the HTTP task may take to finish after the shutdown watch is
/// set. A request stuck in an executor call is otherwise unbounded.
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);

#[tokio::main]
async fn main() {
    if let Err(e) = run().await {
        eprintln!("homelinter: {e}");
        std::process::exit(1);
    }
}

async fn run() -> Result<(), String> {
    // Resolve everything that can fail on its own before starting the
    // service, so a missing SPA or missing browser never leaves a running
    // backend behind.
    let web_dir = resolve_web_dir()?;
    let browser = Browser::detect()?;

    let (port, shutdown_tx, server) = start_server(web_dir).await?;
    let url = format!("http://127.0.0.1:{port}/");

    let profile = ProfileDir::create()?;
    let mut child = browser.spawn(&url, profile.path())?;
    eprintln!(
        "homelinter: {url} opened with {}; close the window to quit",
        browser.describe()
    );

    let outcome = wait_for_browser(&mut child).await;
    // The browser is gone, so its profile is no longer in use.
    drop(profile);

    let _ = shutdown_tx.send(true);
    if tokio::time::timeout(SHUTDOWN_TIMEOUT, server)
        .await
        .is_err()
    {
        eprintln!("homelinter: the service did not stop within {SHUTDOWN_TIMEOUT:?}");
    }
    outcome
}

// ---------------------------------------------------------------------------
// SPA location
// ---------------------------------------------------------------------------

/// Locate the built SPA.
///
/// The AppImage lays it out as `<exe dir>/../share/homelinter/webui`
/// (`usr/bin/homelinter-app` next to `usr/share/homelinter/webui`).
/// `HOMELINTER_WEB_DIR` wins, matching `homelinter web`, so an extracted or
/// unpackaged install can point at a build tree. A `webui/dist` next to the
/// executable or in the working directory keeps `cargo run` usable.
fn resolve_web_dir() -> Result<PathBuf, String> {
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Some(dir) = std::env::var_os("HOMELINTER_WEB_DIR") {
        candidates.push(PathBuf::from(dir));
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            candidates.push(dir.join("../share/homelinter/webui"));
            candidates.push(dir.join("webui"));
            candidates.push(dir.join("webui/dist"));
        }
    }
    candidates.push(PathBuf::from("webui/dist"));
    candidates.push(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../webui/dist"));

    for candidate in &candidates {
        if candidate.join("index.html").is_file() {
            return Ok(candidate.clone());
        }
    }

    let looked: Vec<String> = candidates.iter().map(|c| c.display().to_string()).collect();
    Err(format!(
        "no built SPA found (looked in {}); run `pnpm build` in webui/ or set HOMELINTER_WEB_DIR",
        looked.join(", ")
    ))
}

// ---------------------------------------------------------------------------
// Backend
// ---------------------------------------------------------------------------

async fn start_server(
    web_dir: PathBuf,
) -> Result<(u16, watch::Sender<bool>, JoinHandle<()>), String> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .map_err(|e| format!("cannot bind loopback listener: {e}"))?;
    let port = listener
        .local_addr()
        .map_err(|e| format!("cannot read listener address: {e}"))?
        .port();

    let built = homelinter_service::build_service(homelinter_service::ServiceOptions {
        web_dir: Some(web_dir),
        ..Default::default()
    })
    .map_err(|e| format!("service startup failed: {e}"))?;
    let shutdown_tx = built.shutdown_tx.clone();
    let task = tokio::spawn(async move {
        if let Err(e) = homelinter_service::serve(listener, built.app, built.shutdown_rx).await {
            eprintln!("homelinter: the service stopped with an error: {e}");
        }
    });

    // Never hand a URL to the browser before the service answers: a window
    // that opens on a connection refused would look like a broken app.
    if let Err(e) = wait_health(port).await {
        let _ = shutdown_tx.send(true);
        task.abort();
        return Err(e);
    }
    Ok((port, shutdown_tx, task))
}

async fn wait_health(port: u16) -> Result<(), String> {
    let client = Client::new(port);
    let deadline = tokio::time::Instant::now() + HEALTH_TIMEOUT;
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return Err("the service health check timed out".into());
        }
        match tokio::time::timeout(remaining.min(Duration::from_millis(250)), client.health()).await
        {
            Ok(Ok(_)) => return Ok(()),
            Ok(Err(homelinter_clients::ClientsError::Transport(_))) => {
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
            Ok(Err(e)) => return Err(format!("the service health check failed: {e}")),
            // One attempt overran the remaining budget; the deadline check
            // above covers a service that accepts but never replies.
            Err(_) => {}
        }
    }
}

// ---------------------------------------------------------------------------
// Browser
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Flavor {
    /// Chromium and its derivatives: `--app=<url>` gives a window with no
    /// location bar, bookmarks, or tabs.
    AppMode,
    /// Firefox has no app mode, so kiosk is the closest chrome-less window.
    Kiosk,
}

/// Browser binaries probed in order. Chromium derivatives first because
/// their app mode is a real frameless app window, unlike Firefox's kiosk.
const CANDIDATES: &[(&str, Flavor)] = &[
    ("chromium", Flavor::AppMode),
    ("chromium-browser", Flavor::AppMode),
    ("google-chrome-stable", Flavor::AppMode),
    ("google-chrome", Flavor::AppMode),
    ("brave-browser", Flavor::AppMode),
    ("microsoft-edge-stable", Flavor::AppMode),
    ("vivaldi-stable", Flavor::AppMode),
    ("firefox", Flavor::Kiosk),
    ("firefox-esr", Flavor::Kiosk),
];

struct Browser {
    program: OsString,
    /// Fixed arguments from `HOMELINTER_BROWSER`, for launch wrappers such
    /// as `flatpak run org.chromium.Chromium`.
    prefix: Vec<OsString>,
    flavor: Flavor,
}

impl Browser {
    /// `HOMELINTER_BROWSER` when set (a program plus optional arguments),
    /// otherwise the first candidate found on `PATH`.
    fn detect() -> Result<Self, String> {
        if let Some(spec) = std::env::var_os("HOMELINTER_BROWSER") {
            let spec = spec.to_string_lossy().into_owned();
            let (program, prefix) = parse_spec(&spec)?;
            let path = find_program(&program).ok_or_else(|| {
                if program.contains('/') {
                    format!("HOMELINTER_BROWSER program {program:?} is not an executable file")
                } else {
                    format!("HOMELINTER_BROWSER program {program:?} was not found on PATH")
                }
            })?;
            return Ok(Browser {
                flavor: flavor_of(&program),
                program: path.into_os_string(),
                prefix: prefix.into_iter().map(OsString::from).collect(),
            });
        }
        for (name, flavor) in CANDIDATES {
            if let Some(path) = find_program(name) {
                return Ok(Browser {
                    program: path.into_os_string(),
                    prefix: Vec::new(),
                    flavor: *flavor,
                });
            }
        }
        Err(
            "no Chromium- or Firefox-based browser found on PATH; install one, or set \
             HOMELINTER_BROWSER to the browser to use"
                .into(),
        )
    }

    fn describe(&self) -> String {
        let program = Path::new(&self.program)
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| self.program.to_string_lossy().into_owned());
        match self.flavor {
            Flavor::AppMode => format!("{program} (app mode)"),
            Flavor::Kiosk => format!("{program} (kiosk)"),
        }
    }

    fn spawn(&self, url: &str, profile: &Path) -> Result<Child, String> {
        let mut cmd = Command::new(&self.program);
        cmd.args(&self.prefix);
        match self.flavor {
            Flavor::AppMode => {
                // A per-run `--user-data-dir` is load-bearing: with the
                // default profile a second invocation hands the URL to an
                // already running browser and exits at once, which would
                // stop the service under the open window.
                cmd.arg(format!("--app={url}"))
                    .arg(format!("--user-data-dir={}", profile.display()))
                    .arg("--class=homelinter")
                    .arg("--window-size=1280,860")
                    .arg("--no-first-run")
                    .arg("--no-default-browser-check");
            }
            Flavor::Kiosk => {
                // `--new-instance --no-remote` forces a process we can
                // wait on, and the private profile keeps it out of the
                // user's own browsing session.
                cmd.arg("--kiosk")
                    .arg(url)
                    .arg("--new-instance")
                    .arg("--no-remote")
                    .arg("--profile")
                    .arg(profile);
            }
        }
        cmd.stdin(Stdio::null());
        // Put the browser in its own process group so an interrupt can be
        // forwarded to the whole browser tree (a `flatpak run` wrapper and
        // the sandboxed app behind it), not just to the direct child.
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            cmd.as_std_mut().process_group(0);
        }
        cmd.spawn()
            .map_err(|e| format!("cannot start {}: {e}", self.program.to_string_lossy()))
    }
}

/// Split a `HOMELINTER_BROWSER` value into the program and its fixed
/// arguments. Only whitespace separates words; a path with spaces needs a
/// wrapper script.
fn parse_spec(spec: &str) -> Result<(String, Vec<String>), String> {
    let mut words = spec.split_whitespace();
    let program = words
        .next()
        .ok_or_else(|| "HOMELINTER_BROWSER is set but empty".to_string())?;
    Ok((program.to_string(), words.map(str::to_string).collect()))
}

fn flavor_of(program: &str) -> Flavor {
    let name = Path::new(program)
        .file_name()
        .map(|n| n.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    if name.contains("firefox") {
        Flavor::Kiosk
    } else {
        Flavor::AppMode
    }
}

/// Resolve a program name against `PATH`, or accept an explicit path.
fn find_program(name: &str) -> Option<PathBuf> {
    let explicit = name.contains('/');
    let candidates: Vec<PathBuf> = if explicit {
        vec![PathBuf::from(name)]
    } else {
        let path = std::env::var_os("PATH")?;
        std::env::split_paths(&path)
            .map(|dir| dir.join(name))
            .collect()
    };
    candidates.into_iter().find(|c| is_executable(c))
}

fn is_executable(path: &Path) -> bool {
    let Ok(meta) = std::fs::metadata(path) else {
        return false;
    };
    if !meta.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        meta.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

// ---------------------------------------------------------------------------
// Per-run browser profile
// ---------------------------------------------------------------------------

/// A private, disposable browser profile directory.
///
/// It lives under `XDG_RUNTIME_DIR` (tmpfs) when available so nothing is
/// written to the home directory this tool exists to inspect, and it is
/// removed when the launcher exits.
struct ProfileDir {
    path: PathBuf,
}

impl ProfileDir {
    fn create() -> Result<Self, String> {
        let base = std::env::var_os("XDG_RUNTIME_DIR")
            .map(PathBuf::from)
            .filter(|dir| dir.is_dir())
            .unwrap_or_else(std::env::temp_dir);
        let path = base.join(format!("homelinter-app-{}", std::process::id()));
        std::fs::create_dir_all(&path).map_err(|e| {
            format!(
                "cannot create the browser profile directory {}: {e}",
                path.display()
            )
        })?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700));
        }
        Ok(Self { path })
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for ProfileDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

// ---------------------------------------------------------------------------
// Waiting for the window to close
// ---------------------------------------------------------------------------

/// Block until the browser exits, or until the launcher is interrupted.
///
/// A clean browser exit is the normal quit path. Ctrl-C and SIGTERM close
/// the browser first, so an interrupted AppImage never leaves a stray
/// app-mode window pointed at a service that is about to stop.
async fn wait_for_browser(child: &mut Child) -> Result<(), String> {
    let mut terminate = terminate_signal();
    let status = tokio::select! {
        status = child.wait() => Some(status.map_err(|e| format!("cannot wait for the browser: {e}"))?),
        _ = tokio::signal::ctrl_c() => None,
        _ = terminate.recv() => None,
    };
    match status {
        Some(status) if status.success() => Ok(()),
        Some(status) => Err(format!("the browser exited with {status}")),
        None => {
            // Interrupted: close the browser, then let the caller stop the
            // service. SIGTERM first so the browser shuts its own children
            // down; a browser that ignores it is killed after a grace
            // period rather than leaving a window on a dead origin.
            if let Some(pid) = child.id() {
                signal_browser(pid);
            }
            let graceful = tokio::time::timeout(Duration::from_secs(3), child.wait()).await;
            if graceful.is_err() {
                let _ = child.start_kill();
                let _ = child.wait().await;
            }
            Ok(())
        }
    }
}

/// Ask the browser's process group to terminate. Returns whether the signal
/// was delivered.
#[cfg(unix)]
fn signal_browser(pid: u32) -> bool {
    use rustix::process::{kill_process_group, Pid, Signal};

    // The pid is one we spawned as a group leader (`process_group(0)`), so
    // signalling its process group reaches the browser and its helpers.
    let Some(pgid) = Pid::from_raw(pid as i32) else {
        return false;
    };
    kill_process_group(pgid, Signal::Term).is_ok()
}

#[cfg(not(unix))]
fn signal_browser(_pid: u32) -> bool {
    false
}

#[cfg(unix)]
fn terminate_signal() -> tokio::signal::unix::Signal {
    use tokio::signal::unix::{signal, SignalKind};
    signal(SignalKind::terminate()).expect("install a SIGTERM handler")
}

#[cfg(not(unix))]
fn terminate_signal() -> NeverSignal {
    NeverSignal
}

/// Stand-in stream whose `recv` never completes, for platforms without
/// SIGTERM.
#[cfg(not(unix))]
struct NeverSignal;

#[cfg(not(unix))]
impl NeverSignal {
    async fn recv(&mut self) -> Option<()> {
        std::future::pending().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spec_splits_program_and_arguments() {
        assert_eq!(
            parse_spec("chromium").unwrap(),
            ("chromium".to_string(), Vec::new())
        );
        assert_eq!(
            parse_spec("  flatpak run org.chromium.Chromium ").unwrap(),
            (
                "flatpak".to_string(),
                vec!["run".to_string(), "org.chromium.Chromium".to_string()]
            )
        );
        assert!(parse_spec("   ").is_err());
    }

    #[test]
    fn firefox_names_use_kiosk() {
        assert_eq!(flavor_of("firefox"), Flavor::Kiosk);
        assert_eq!(flavor_of("/usr/bin/firefox-esr"), Flavor::Kiosk);
        assert_eq!(flavor_of("chromium"), Flavor::AppMode);
        assert_eq!(
            flavor_of("/opt/google/chrome/google-chrome"),
            Flavor::AppMode
        );
    }

    #[test]
    fn explicit_missing_path_is_not_found() {
        assert!(find_program("/nonexistent/homelinter/browser").is_none());
    }
}
