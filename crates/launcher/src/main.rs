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
//! 2. Take the single-instance lock on the reserved browser profile.
//! 3. Pick a browser (Chromium family first, Firefox as a kiosk fallback).
//! 4. Bind an ephemeral loopback port, build the service, wait for health.
//! 5. Spawn the browser against that origin and wait for it to exit.
//! 6. Signal the service's shutdown watch and wait, bounded, for it to stop.
//!
//! SIGHUP is ignored for the launcher's lifetime, so a launch that is
//! detached from its terminal (`setsid ... &`) does not tear the service down
//! when the terminal or SSH session goes away. (`nohup ... &` alone is not
//! enough on NixOS: the shell resends SIGHUP to the job's process group,
//! where `appimage-run`'s bubblewrap supervisor does not ignore it.)
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
    // A GUI launch from a shell must outlive the terminal or SSH session that
    // started it once it is detached from the shell's session (`setsid ...`).
    // SIGHUP defaults to terminating the process, which would stop the
    // in-process service and could leave the browser window open on a dead
    // origin while the shell's job cleanup signals the launcher.
    ignore_hangup();

    // Resolve everything that can fail on its own before starting the
    // service, so a missing SPA, a missing browser, or a second instance
    // never leaves a running backend behind.
    let web_dir = resolve_web_dir()?;
    let data = BrowserData::create()?;
    // Held until the process exits; see `BrowserData`.
    let _lock = data.lock()?;
    let browser = Browser::detect()?;

    let (port, shutdown_tx, server) = start_server(web_dir).await?;
    let url = format!("http://127.0.0.1:{port}/");

    let mut child = browser.spawn(&url, data.path())?;
    eprintln!(
        "homelinter: {url} opened with {}; close the window to quit",
        browser.describe()
    );

    let outcome = wait_for_browser(&mut child).await;

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
                // A dedicated `--user-data-dir` is load-bearing: with the
                // default profile Chromium hands the URL to an already
                // running browser and exits at once, which would stop the
                // service under the open window.
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
// Browser profile
// ---------------------------------------------------------------------------

/// The reserved browser profile: one directory, reused by every invocation.
///
/// It is neither the user's own browsing profile nor a throwaway directory.
/// Sharing the user's profile would put the app's window into their real
/// browsing session (cookies, history, extensions) and, worse, Chromium would
/// hand the URL to an already running browser and exit at once, stopping the
/// service under the window that stayed open. A fresh directory per run would
/// throw away everything the browser keeps for the app (site data,
/// permissions, window placement) on every launch. One reserved profile has
/// neither problem; it sits under a hidden directory, so a default homelinter
/// scan of the home directory does not walk it.
struct BrowserData {
    path: PathBuf,
}

impl BrowserData {
    fn create() -> Result<Self, String> {
        let path = profile_dir()?;
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

    /// The lock file sits beside the profile rather than inside it, so the
    /// browser's own files stay untouched.
    fn lock_path(&self) -> PathBuf {
        self.path.with_extension("lock")
    }

    /// Take the single-instance lock. Exactly one launcher may use the
    /// profile: a second one could only take over the window that is already
    /// open, and its own service would then stop under it.
    fn lock(&self) -> Result<InstanceLock, String> {
        InstanceLock::acquire(&self.lock_path())
    }
}

/// Resolve the profile directory.
///
/// `HOMELINTER_BROWSER_DATA_DIR` wins, so an unpackaged or relocated install
/// can put it elsewhere; otherwise it is `$XDG_DATA_HOME/homelinter/browser`
/// (`~/.local/share/homelinter/browser` by default).
fn profile_dir() -> Result<PathBuf, String> {
    if let Some(dir) = std::env::var_os("HOMELINTER_BROWSER_DATA_DIR") {
        return Ok(PathBuf::from(dir));
    }
    let base = dirs::data_dir().ok_or_else(|| {
        "cannot locate a data directory for the browser profile; set \
         HOMELINTER_BROWSER_DATA_DIR to choose one"
            .to_string()
    })?;
    Ok(base.join("homelinter").join("browser"))
}

/// An exclusive `flock` on the profile, held until the launcher exits. The
/// kernel drops it with the process, so a killed launcher never blocks the
/// next run.
struct InstanceLock {
    _file: std::fs::File,
}

impl InstanceLock {
    fn acquire(path: &Path) -> Result<Self, String> {
        let file = std::fs::File::options()
            .create(true)
            .write(true)
            .truncate(false)
            .open(path)
            .map_err(|e| format!("cannot open the lock file {}: {e}", path.display()))?;
        match lock_exclusive(&file) {
            Ok(()) => Ok(Self { _file: file }),
            Err(LockError::Busy) => Err(format!(
                "homelinter is already running; close its window before starting another \
                 (browser profile: {})",
                path.with_extension("").display()
            )),
            Err(LockError::Failed(e)) => Err(format!("cannot lock {}: {e}", path.display())),
        }
    }
}

/// Whether the profile lock could not be taken, and why.
enum LockError {
    /// Another launcher holds it.
    Busy,
    /// The lock could not be attempted at all.
    Failed(std::io::Error),
}

#[cfg(unix)]
fn lock_exclusive(file: &std::fs::File) -> Result<(), LockError> {
    use rustix::fs::{flock, FlockOperation};

    match flock(file, FlockOperation::NonBlockingLockExclusive) {
        Ok(()) => Ok(()),
        Err(rustix::io::Errno::WOULDBLOCK) => Err(LockError::Busy),
        Err(e) => Err(LockError::Failed(e.into())),
    }
}

#[cfg(not(unix))]
fn lock_exclusive(_file: &std::fs::File) -> Result<(), LockError> {
    // The AppImage is a Unix artifact; elsewhere the profile is shared
    // without the cross-process guard rather than refusing to start.
    Ok(())
}

// ---------------------------------------------------------------------------
// Waiting for the window to close
// ---------------------------------------------------------------------------

/// Block until the browser exits, or until the launcher is interrupted.
///
/// A clean browser exit is the normal quit path. Ctrl-C and SIGTERM close
/// the browser first, so an interrupted AppImage never leaves a stray
/// app-mode window pointed at a service that is about to stop. SIGHUP is
/// not an interrupt here: `ignore_hangup` catches it so the launcher and
/// service survive the terminal going away.
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

/// Catch and drain SIGHUP for the launcher's lifetime.
///
/// Installing the handler replaces SIGHUP's default terminate action. The
/// stream is drained on a background task so later hangups are discarded
/// instead of accumulating, and the launcher keeps waiting for the browser.
/// The normal quit paths (window closed, Ctrl-C, SIGTERM) are unchanged.
#[cfg(unix)]
fn ignore_hangup() {
    use tokio::signal::unix::{signal, SignalKind};
    let mut hangup = signal(SignalKind::hangup()).expect("install a SIGHUP handler");
    tokio::spawn(async move { while hangup.recv().await.is_some() {} });
}

#[cfg(not(unix))]
fn ignore_hangup() {}

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

    #[test]
    fn the_lock_sits_beside_the_profile() {
        let data = BrowserData {
            path: PathBuf::from("/home/u/.local/share/homelinter/browser"),
        };
        assert_eq!(
            data.lock_path(),
            PathBuf::from("/home/u/.local/share/homelinter/browser.lock")
        );
    }

    #[test]
    fn only_one_instance_holds_the_profile() {
        let dir = std::env::temp_dir().join(format!(
            "homelinter-lock-test-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("browser.lock");

        let first = InstanceLock::acquire(&path).unwrap();
        assert!(InstanceLock::acquire(&path).is_err());
        // The kernel releases the lock with the last handle to it.
        drop(first);
        assert!(InstanceLock::acquire(&path).is_ok());

        let _ = std::fs::remove_dir_all(&dir);
    }
}
