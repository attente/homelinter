//! Homelinter service: an HTTP/1.1 + SSE backend that runs the scan,
//! analyze, suggest, and execute pipelines against one in-memory session.
//!
//! The service owns the single session, the config file chain, the SQLite
//! provider cache, and the append-only audit log. Handlers and SSE event
//! shapes follow the Service API section of DESIGN.md.

pub mod audit;
pub mod cache;
mod document_text;
pub mod events;
mod http;
pub mod pipeline;
pub mod providers;
pub mod session;

pub use http::router;

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::AtomicU64;
use std::sync::Arc;

use tokio::sync::{watch, Mutex};

use homelinter_model::config::load_config;
use homelinter_model::error::ApiError;

use crate::events::EventBus;
use crate::pipeline::App;

/// Startup inputs: optional explicit config file, optional overrides used
/// by tests and embedded runs.
#[derive(Debug, Clone, Default)]
pub struct ServiceOptions {
    /// Explicit config file path (highest precedence in the chain).
    pub cli_config: Option<PathBuf>,
    /// Override the effective home directory.
    pub home_override: Option<PathBuf>,
    /// Override the app data directory (cache, audit).
    pub data_dir_override: Option<PathBuf>,
    /// Directory containing the built SPA (index.html + assets). When set,
    /// the HTTP server also serves static files and the SPA index fallback.
    pub web_dir: Option<PathBuf>,
    /// Scan dialog defaults surfaced to the SPA via `GET /session`
    /// (`scan_defaults`): the `web` command pre-fills the scan form with
    /// them. They are never applied server-side; the SPA sends `hidden` and
    /// `target_paths` per scan request as today.
    pub default_hidden: bool,
    pub default_target_paths: BTreeMap<String, String>,
}

/// A fully wired service instance ready to serve HTTP.
pub struct Built {
    pub app: Arc<App>,
    /// Receiver side of the shutdown watch: `serve` awaits its change.
    pub shutdown_rx: watch::Receiver<bool>,
    pub home: PathBuf,
    pub data_dir: PathBuf,
}

/// Load config, open the cache and audit log, and assemble the session.
pub fn build_service(opts: ServiceOptions) -> Result<Built, ApiError> {
    let home = opts
        .home_override
        .clone()
        .or_else(homelinter_model::path::home_dir)
        .ok_or_else(|| ApiError::internal("cannot determine the home directory"))?;

    let loaded = load_config(opts.cli_config.as_deref())?;

    let data_dir = opts
        .data_dir_override
        .clone()
        .or_else(homelinter_model::path::default_app_data_dir)
        .ok_or_else(|| ApiError::internal("cannot determine the app data directory"))?;
    std::fs::create_dir_all(&data_dir).map_err(|e| {
        ApiError::internal(format!(
            "cannot create data directory {}: {e}",
            data_dir.display()
        ))
    })?;

    let session = session::Session::new(home.clone(), loaded.raw, loaded.config_path);
    let cache = cache::Cache::open_dir(&data_dir);
    let audit = audit::AuditLog::open(data_dir.join("audit.ndjson"));
    let (shutdown_tx, shutdown_rx) = watch::channel(false);

    let provider_queue = providers::JobQueue::new();

    let app = Arc::new(App {
        session: Mutex::new(session),
        events: EventBus::new(),
        cache: Mutex::new(cache),
        audit,
        cancel: AtomicU64::new(0),
        provider_epoch: AtomicU64::new(0),
        provider_queue: provider_queue.clone(),
        cli_config: opts.cli_config,
        home_override: opts.home_override,
        web_dir: opts.web_dir,
        default_hidden: opts.default_hidden,
        default_target_paths: opts.default_target_paths,
        shutdown_tx,
    });

    Ok(Built {
        app,
        shutdown_rx,
        home,
        data_dir,
    })
}

/// Serve the API on `listener` until `POST /api/v1/shutdown` fires the
/// shutdown watch.
pub async fn serve(
    listener: tokio::net::TcpListener,
    app: Arc<App>,
    mut shutdown_rx: watch::Receiver<bool>,
) -> std::io::Result<()> {
    // The provider worker (background geocoding/description fetcher) runs
    // for the service's lifetime alongside the HTTP server.
    providers::spawn(app.clone());
    axum::serve(listener, http::router(app))
        .with_graceful_shutdown(async move {
            let _ = shutdown_rx.changed().await;
        })
        .await
}
