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
mod svg_raster;
mod video_frames;

pub use http::router;

use std::path::PathBuf;
use std::sync::atomic::AtomicU64;
use std::sync::Arc;

use tokio::sync::{watch, Mutex};

use homelinter_model::config::{fold_config_overrides, load_config_chain, ConfigChain};
use homelinter_model::error::ApiError;
use serde_json::Value as JsonValue;

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
    /// Override the configuration search chain (the system-wide and
    /// per-user files read below the explicit config file). Tests set an
    /// empty chain so config behavior stays hermetic; None uses the host
    /// chain.
    pub config_chain_override: Option<ConfigChain>,
    /// Directory containing the built SPA (index.html + assets). When set,
    /// the HTTP server also serves static files and the SPA index fallback.
    pub web_dir: Option<PathBuf>,
    /// Launch-time CLI overrides as a partial config document. Folded over
    /// the config-file chain to seed the initial effective settings; a
    /// runtime write through `PUT /config` wins over them for the same key.
    pub initial_overrides: Option<JsonValue>,
}

/// A fully wired service instance ready to serve HTTP.
pub struct Built {
    pub app: Arc<App>,
    /// Sender side of the shutdown watch: embedded launchers signal graceful
    /// shutdown by setting it to `true` (the standalone `web` driver instead
    /// waits for `POST /api/v1/shutdown`).
    pub shutdown_tx: watch::Sender<bool>,
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

    let chain = opts
        .config_chain_override
        .clone()
        .unwrap_or_else(ConfigChain::host);
    let loaded = load_config_chain(&chain, opts.cli_config.as_deref())?;
    let launch_overrides = opts
        .initial_overrides
        .clone()
        .unwrap_or_else(|| JsonValue::Object(Default::default()));
    let effective = fold_config_overrides(
        &loaded.raw,
        &launch_overrides,
        &JsonValue::Object(Default::default()),
    )?;

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

    let session = session::Session::new(home.clone(), effective, loaded.config_path);
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
        cache_stats: pipeline::CacheStats::default(),
        cli_config: opts.cli_config,
        config_chain: chain,
        home_override: opts.home_override,
        web_dir: opts.web_dir,
        launch_overrides,
        shutdown_tx: shutdown_tx.clone(),
    });

    Ok(Built {
        app,
        shutdown_tx,
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
