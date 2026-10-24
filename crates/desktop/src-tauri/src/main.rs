//! Native desktop shell for homelinter.
//!
//! The shell owns one in-process service (the same `homelinter-service`
//! that the CLI's `web` command uses), points a WebView at its loopback
//! origin, and signals the service's shutdown watch when the app exits.
//! Nothing is spawned as a child process, so closing the app cannot leave
//! an orphaned backend behind.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Duration;

use tauri::{Manager, RunEvent, WebviewUrl, WebviewWindowBuilder};
use tokio::sync::watch;
use tokio::task::JoinHandle;

/// The running service handle kept by the Tauri app.
struct ServerState {
    shutdown_tx: watch::Sender<bool>,
    task: Mutex<Option<JoinHandle<()>>>,
}

fn main() {
    tauri::Builder::default()
        // The SPA runs on the loopback origin, so its native scan-root picker
        // is an IPC command that the `main` capability allows remotely.
        .plugin(tauri_plugin_dialog::init())
        .setup(|app| {
            let web_dir = resolve_web_dir(app.handle());
            let (port, state) = tauri::async_runtime::block_on(start_server(web_dir))
                .map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;
            app.manage(state);

            let url = format!("http://127.0.0.1:{port}/");
            // A WebView window has no browser chrome: no location bar,
            // bookmarks, navigation buttons, or extension UI. The window is
            // a normal resizable macOS window, and the user can minimize or
            // quit it as usual.
            WebviewWindowBuilder::new(app, "main", WebviewUrl::External(url.parse()?))
                .title("homelinter")
                .inner_size(1280.0, 860.0)
                .min_inner_size(900.0, 600.0)
                .resizable(true)
                .build()?;
            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("error while building homelinter")
        .run(|app_handle, event| {
            if let RunEvent::Exit = event {
                if let Some(state) = app_handle.try_state::<ServerState>() {
                    let _ = state.shutdown_tx.send(true);
                    if let Some(task) = state.task.lock().ok().and_then(|mut task| task.take()) {
                        let _ = tauri::async_runtime::block_on(async {
                            let _ = tokio::time::timeout(Duration::from_secs(5), task).await;
                        });
                    }
                }
            }
        });
}

/// Locate the built SPA. Packaged builds read `Contents/Resources/webui`;
/// development runs fall back to the workspace's `webui/dist`.
fn resolve_web_dir(app: &tauri::AppHandle) -> PathBuf {
    if let Ok(dir) = std::env::var("HOMELINTER_WEB_DIR") {
        let path = PathBuf::from(dir);
        if path.join("index.html").is_file() {
            return path;
        }
        eprintln!(
            "homelinter: warning: HOMELINTER_WEB_DIR {} has no index.html",
            path.display()
        );
    }

    if let Ok(resource_dir) = app.path().resource_dir() {
        // Tauri's resource mapping may preserve the source directory name.
        // Accept both `Resources/webui` and `Resources/webui/dist`.
        for relative in ["webui", "webui/dist", "dist"] {
            let candidate = resource_dir.join(relative);
            if candidate.join("index.html").is_file() {
                return candidate;
            }
        }
    }

    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let candidate = manifest_dir.join("../../../webui/dist");
    if candidate.join("index.html").is_file() {
        return candidate;
    }

    let candidate = PathBuf::from("webui/dist");
    if candidate.join("index.html").is_file() {
        return candidate;
    }

    eprintln!(
        "homelinter: warning: no built SPA found; run `pnpm build` in webui/ \
         (expected {})",
        manifest_dir.join("../../../webui/dist").display()
    );
    manifest_dir.join("../../../webui/dist")
}

async fn start_server(web_dir: PathBuf) -> Result<(u16, ServerState), String> {
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
            eprintln!("homelinter: service stopped with an error: {e}");
        }
    });

    // The window must not race the first request: wait until the service
    // answers before handing the URL to the WebView.
    let client = homelinter_clients::Client::new(port);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            let _ = shutdown_tx.send(true);
            task.abort();
            return Err("service health check timed out".into());
        }
        let attempt = remaining.min(Duration::from_millis(250));
        match tokio::time::timeout(attempt, client.health()).await {
            Ok(Ok(_)) => break,
            Ok(Err(homelinter_clients::ClientsError::Transport(_))) => {
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
            Ok(Err(e)) => {
                let _ = shutdown_tx.send(true);
                task.abort();
                return Err(format!("service health check failed: {e}"));
            }
            // A single attempt timed out; the loop's deadline covers the
            // case where the service accepts the connection but never replies.
            Err(_) => {}
        }
    }

    Ok((
        port,
        ServerState {
            shutdown_tx,
            task: Mutex::new(Some(task)),
        },
    ))
}
