//! Provider requests decoupled from the scan pipeline.
//!
//! The scan never performs provider I/O (reverse geocoding, LLM
//! descriptions) itself. It installs a `ProviderScanCtx` on the session and
//! enqueues `ProviderJob`s into a `JobQueue`. Suggestions whose unit has
//! outstanding provider requests are published immediately with
//! `metadata_pending = true`: the user sees the full scan result at once,
//! but a pending suggestion is not executable until every request it
//! depends on resolves. A single background worker drains the queue and
//! fetches requests serially per provider, paced by the provider's own
//! minimum interval ("Geocoder rate limiting and identification" in
//! DESIGN.md: nominatim once per second, photon its documented rate). The
//! persistent cache is consulted before any network I/O and successes are
//! stored in it afterward. When the last outstanding request of a unit
//! resolves - on success or on definitive failure - the suggestion is
//! updated in place: successes overlay their values into the rebuilt move,
//! failures keep the generated fallbacks, and `metadata_pending` flips to
//! `false`.
//!
//! A provider failure (non-2xx, timeout, malformed response) is never a
//! scan error: the request is not cached, the suggestion stays published
//! with its fallback values (no longer pending), and the next scan retries.
//! Results carry the scan epoch; results for a superseded scan are dropped
//! without touching the session.
//!
//! Built-in transports implement the configured public geocoders and LLM
//! backends, as well as the neutral `custom` contracts.
//! Jobs are planned per unit only when the destination format for the
//! unit's kind uses the corresponding placeholder (`%l` for the geocoder,
//! `%s` for descriptions); provider inputs that do exist (GPS coordinates
//! from EXIF, extracted document text) are wired in, and document text
//! for description prompts is extracted in the worker per
//! the `[document_extraction]` section (configured commands, default tools
//! when on the PATH, then the in-process readers in `homelinter-analyze`;
//! see `document_text`).

use std::collections::{BTreeMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use base64::{engine::general_purpose, Engine as _};
use http_body_util::{BodyExt, Full};
use hyper::body::Bytes;
use hyper::header::HeaderValue;
use hyper::Method;
use hyper::Request;
use hyper_util::client::legacy::Client as HyperClient;
use hyper_util::rt::TokioExecutor;
use serde_json::{json, Value};
use tokio::sync::Notify;

use homelinter_analyze::Analysis;
use homelinter_model::asset_kind::AssetKind;
use homelinter_model::config::{GeocoderService, RawConfig, DEFAULT_MAX_WORDS};
use homelinter_model::format::{FormatString, Placeholder, Segment};
use homelinter_model::suggestion::Suggestion;
use homelinter_scan::SymlinkIndex;
use homelinter_scan::ToolSet;
use homelinter_suggest::rebuild_move_suggestion;

use crate::cache::{cache_key, KIND_DESCRIPTION, KIND_GEOCODE};
use crate::events::ServerEvent;
use crate::pipeline::{App, AppHandle};

/// What kind of provider a job fetches.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderKind {
    Geocode,
    Description,
}

impl ProviderKind {
    fn kind_tag(self) -> &'static str {
        match self {
            ProviderKind::Geocode => KIND_GEOCODE,
            ProviderKind::Description => KIND_DESCRIPTION,
        }
    }
}

/// Which one unit a provider job refreshes.
#[derive(Debug, Clone)]
pub struct UnitRef {
    /// Scan epoch that produced the job; other epochs are stale.
    pub epoch: u64,
    /// Index into `ProviderScanCtx.analysis.files`.
    pub index: usize,
    /// The unit's path, re-verified when the result is applied.
    pub path: PathBuf,
}

/// The request body for one provider call.
#[derive(Debug, Clone)]
pub enum ProviderInput {
    Geocode {
        latitude: f64,
        longitude: f64,
        language: Option<String>,
    },
    /// A description (LLM) request. `text` carries extracted plain text
    /// when a document extractor produced it; when `vision` is set and the
    /// backend accepts image input (the ollama transport), the worker
    /// loads the raw file bytes into `image` on a provider cache miss.
    /// Otherwise the provider sees the unit path and kind.
    Describe {
        text: Option<String>,
        /// Whether this unit's kind supports vision (photo) and the backend
        /// is image-capable. Gates the `image` payload and the cache key.
        vision: bool,
        /// Raw file bytes for a vision-capable description request, loaded
        /// lazily by the worker. `None` degrades to the text-only prompt.
        image: Option<Vec<u8>>,
    },
}

/// Provider facts: cache identity, pacing, and transport.
#[derive(Debug, Clone)]
pub struct ProviderSettings {
    /// Cache and pacing identity, e.g. `geocode:nominatim`.
    pub provider_id: String,
    /// Provider settings hash joining the cache key; changing the
    /// endpoint/model/language invalidates prior results.
    pub settings_hash: [u8; 32],
    /// Minimum interval between requests to this provider; `0` = none.
    pub min_interval: Duration,
    /// Backend identifier (`nominatim`, `photon`, `google`, `mapbox`,
    /// `custom` for the geocoder; LLM transport names for descriptions).
    pub backend: String,
    pub endpoint: String,
    pub api_key: Option<String>,
    pub language: Option<String>,
    pub user_agent_contact: Option<String>,
    pub model: Option<String>,
    pub max_words: Option<u64>,
}

/// A single queued provider request.
#[derive(Debug, Clone)]
pub struct ProviderJob {
    pub kind: ProviderKind,
    pub settings: ProviderSettings,
    pub unit: UnitRef,
    /// Canonical request tuple for the cache key.
    pub input_hash: [u8; 32],
    pub input: ProviderInput,
}

/// Everything the worker needs to refresh one completed scan's suggestions
/// once provider results arrive. Installed on the session when the scan
/// publishes its batch; replaced (with a new epoch) by the next scan.
#[derive(Debug, Clone)]
pub struct ProviderScanCtx {
    pub epoch: u64,
    pub analysis: Arc<Analysis>,
    pub symlinks: SymlinkIndex,
    pub formats: BTreeMap<AssetKind, FormatString>,
    pub home: PathBuf,
    /// Provider values per analyzed unit, overlaid on `analysis.files` when
    /// a suggestion is rebuilt.
    pub meta: BTreeMap<usize, ProviderUnitMeta>,
    /// Outstanding provider requests per analyzed unit. A unit's suggestion
    /// stays `metadata_pending` until this count reaches zero.
    pub pending: BTreeMap<usize, u32>,
}

/// Latest provider-derived metadata for one analyzed unit.
#[derive(Debug, Clone, Default)]
pub struct ProviderUnitMeta {
    pub location: Option<String>,
    pub subject: Option<String>,
}

/// FIFO queue of provider jobs. The pipeline enqueues synchronously (it may
/// run on the blocking pool); the worker drains asynchronously.
pub struct JobQueue {
    jobs: Mutex<VecDeque<ProviderJob>>,
    wake: Notify,
}

impl JobQueue {
    pub fn new() -> Arc<JobQueue> {
        Arc::new(JobQueue {
            jobs: Mutex::new(VecDeque::new()),
            wake: Notify::new(),
        })
    }

    /// Enqueue one job; never waits on the worker.
    pub fn enqueue(&self, job: ProviderJob) {
        self.jobs.lock().unwrap().push_back(job);
        self.wake.notify_waiters();
    }

    pub fn queued(&self) -> usize {
        self.jobs.lock().unwrap().len()
    }

    pub fn is_empty(&self) -> bool {
        self.queued() == 0
    }

    /// Wait until at least one job exists, then take the whole queue. The
    /// `Notify` permit state makes an enqueue that lands between the
    /// lock-check and the wait wake us on the next loop iteration.
    async fn drain_ready(&self) -> Vec<ProviderJob> {
        loop {
            let notified = self.wake.notified();
            {
                let mut jobs = self.jobs.lock().unwrap();
                if !jobs.is_empty() {
                    return jobs.drain(..).collect();
                }
            }
            notified.await;
        }
    }
}

/// Spawn the provider worker to run for the service's lifetime.
pub fn spawn(app: AppHandle) {
    tokio::spawn(run_provider_worker(app));
}

/// The background request loop. One request in flight at a time, paced per
/// provider, cache-first, failure-tolerant, session-applying on success.
pub async fn run_provider_worker(app: AppHandle) {
    let queue = app.provider_queue.clone();
    let mut last_start: BTreeMap<String, Instant> = BTreeMap::new();
    // Tool availability for document-text defaults; detected once per
    // worker lifetime, exactly like the scan pipeline's ToolSet.
    let tools = ToolSet::detect();
    loop {
        let jobs = queue.drain_ready().await;
        for job in jobs {
            if job.unit.epoch != app.provider_epoch.load(Ordering::SeqCst) {
                // The scan that produced this job has been superseded.
                continue;
            }
            pace(&job.settings, &mut last_start).await;
            let value = match cache_get(&app, &job).await {
                Some(v) => Some(v),
                None => {
                    // Cache miss: fetch the provider input first (document
                    // text extraction runs off the async runtime, and only
                    // when a provider call is actually needed), then call.
                    let job = prepare_provider_job(&app, &job, &tools).await;
                    let timeout = provider_timeout(&app).await;
                    match tokio::time::timeout(timeout, invoke(&job)).await {
                        Ok(Ok(v)) => {
                            cache_put(&app, &job, &v).await;
                            Some(v)
                        }
                        // Failure or timeout: not cached, the pending
                        // suggestion stays published with its fallbacks,
                        // retried on the next scan.
                        Ok(Err(_)) | Err(_) => None,
                    }
                }
            };
            // Both outcomes resolve the unit's pending request: success
            // overlays the value, failure publishes the fallback.
            resolve_provider_job(&app, &job, value.as_deref()).await;
        }
    }
}

/// Materialize the provider input for a job about to be invoked: extract
/// plain document text for a description job that does not carry any yet.
/// The extraction config (commands plus `[limits]`) is snapshotted from
/// the session at call time; a missing or failed extractor leaves the
/// input unchanged (`text: None`) and the provider sees the unit path and
/// kind only. Extraction is local and bounded, never a scan failure.
async fn prepare_provider_job(app: &App, job: &ProviderJob, tools: &ToolSet) -> ProviderJob {
    let mut job = job.clone();
    match &mut job.input {
        ProviderInput::Describe {
            text,
            vision,
            image,
        } => {
            if text.is_none() {
                let cfg = {
                    let sess = app.session.lock().await;
                    sess.config.clone()
                };
                let path = job.unit.path.clone();
                let tools = tools.clone();
                *text = tokio::task::spawn_blocking(move || {
                    crate::document_text::extract_document_text(&cfg, &path, &tools)
                })
                .await
                .ok()
                .flatten();
            }
            if *vision && image.is_none() {
                let path = job.unit.path.clone();
                *image = tokio::task::spawn_blocking(move || read_image_bytes(&path))
                    .await
                    .ok()
                    .flatten();
            }
        }
        ProviderInput::Geocode { .. } => {}
    }
    job
}

/// Sleep so consecutive requests to the same provider are at least
/// `min_interval` apart, then record the start time.
async fn pace(settings: &ProviderSettings, last: &mut BTreeMap<String, Instant>) {
    if settings.min_interval.is_zero() {
        return;
    }
    if let Some(prev) = last.get(&settings.provider_id) {
        let elapsed = prev.elapsed();
        if elapsed < settings.min_interval {
            tokio::time::sleep(settings.min_interval - elapsed).await;
        }
    }
    last.insert(settings.provider_id.clone(), Instant::now());
}

async fn provider_timeout(app: &App) -> Duration {
    let sess = app.session.lock().await;
    Duration::from_secs(sess.config.provider_timeout())
}

async fn cache_get(app: &App, job: &ProviderJob) -> Option<String> {
    let cache = app.cache.lock().await;
    let key = cache_key(
        job.kind.kind_tag(),
        &job.settings.provider_id,
        &job.settings.settings_hash,
        &job.input_hash,
    );
    cache.get(&key, &job.settings.provider_id).ok().flatten()
}

async fn cache_put(app: &App, job: &ProviderJob, value: &str) {
    let cache = app.cache.lock().await;
    let key = cache_key(
        job.kind.kind_tag(),
        &job.settings.provider_id,
        &job.settings.settings_hash,
        &job.input_hash,
    );
    let _ = cache.put(
        &key,
        job.kind.kind_tag(),
        &job.settings.provider_id,
        &job.input_hash,
        value,
    );
}

/// Exercise one provider request. The implemented transports today are the
/// neutral `custom` contracts from "Provider Plugins": reverse geocoding
/// (`POST endpoint` with `{"latitude", "longitude", "language"}` answering
/// `{"name": ...}`) and completion for descriptions (`POST endpoint` with
/// `{"model", "prompt", "max_words"}` answering `{"content": ...}`).
/// Everything else degrades to a provider failure.
async fn invoke(job: &ProviderJob) -> Result<String, String> {
    match job.kind {
        ProviderKind::Geocode => {
            let (lat, lon, language) = match &job.input {
                ProviderInput::Geocode {
                    latitude,
                    longitude,
                    language,
                } => (*latitude, *longitude, language.as_deref()),
                ProviderInput::Describe { .. } => {
                    return Err("mismatched provider input".to_string())
                }
            };
            match job.settings.backend.as_str() {
                "custom" => neutral_reverse_geocode(&job.settings, lat, lon, language).await,
                "nominatim" => {
                    if job.settings.endpoint.is_empty() {
                        Err("nominatim geocoder needs an endpoint".to_string())
                    } else {
                        nominatim_reverse(&job.settings, lat, lon, language).await
                    }
                }
                "photon" => photon_reverse(&job.settings, lat, lon, language).await,
                "google" => google_reverse(&job.settings, lat, lon, language).await,
                "mapbox" => mapbox_reverse(&job.settings, lat, lon, language).await,
                backend => Err(format!("unknown geocoder backend '{backend}'")),
            }
        }
        ProviderKind::Description => {
            let (text, image) = match &job.input {
                ProviderInput::Describe { text, image, .. } => (text.as_deref(), image.as_deref()),
                ProviderInput::Geocode { .. } => {
                    return Err("mismatched provider input".to_string())
                }
            };
            match job.settings.backend.as_str() {
                "custom" => neutral_complete(&job.settings, &job.unit.path, text).await,
                "ollama" => ollama_complete(&job.settings, &job.unit.path, text, image).await,
                "openrouter" => {
                    if job.settings.api_key.is_none() {
                        Err("openrouter description provider requires an API key".to_string())
                    } else {
                        openrouter_complete(&job.settings, &job.unit.path, text).await
                    }
                }
                backend => Err(format!("unknown description backend '{backend}'")),
            }
        }
    }
}

fn query_escape(value: &str) -> String {
    value
        .bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

async fn get_json(
    endpoint: &str,
    api_key: Option<&str>,
    user_agent: Option<&str>,
) -> Result<Value, String> {
    let client = {
        let connector = hyper_rustls::HttpsConnectorBuilder::new()
            .with_webpki_roots()
            .https_or_http()
            .enable_http1()
            .build();
        HyperClient::builder(TokioExecutor::new()).build(connector)
    };
    let uri: hyper::Uri = endpoint
        .parse()
        .map_err(|e| format!("invalid provider endpoint: {e}"))?;
    let mut builder = Request::builder()
        .method(Method::GET)
        .uri(uri)
        .header("accept", "application/json");
    let ua = format!(
        "homelinter/{}{}",
        env!("CARGO_PKG_VERSION"),
        user_agent.map(|v| format!(" ({v})")).unwrap_or_default()
    );
    builder = builder.header(
        "user-agent",
        HeaderValue::from_str(&ua).map_err(|e| e.to_string())?,
    );
    if let Some(key) = api_key {
        builder = builder.header(
            "authorization",
            HeaderValue::from_str(&format!("Bearer {key}")).map_err(|e| e.to_string())?,
        );
    }
    let resp = client
        .request(
            builder
                .body(Full::new(Bytes::new()))
                .map_err(|e| e.to_string())?,
        )
        .await
        .map_err(|e| format!("provider transport error: {e}"))?;
    let status = resp.status();
    let bytes = resp
        .into_body()
        .collect()
        .await
        .map_err(|e| format!("provider body read: {e}"))?
        .to_bytes();
    if !status.is_success() {
        return Err(format!("provider returned {}", status.as_u16()));
    }
    serde_json::from_slice(&bytes).map_err(|_| "provider response is not JSON".to_string())
}

async fn nominatim_reverse(
    s: &ProviderSettings,
    lat: f64,
    lon: f64,
    language: Option<&str>,
) -> Result<String, String> {
    let url = format!(
        "{}/reverse?format=jsonv2&lat={lat}&lon={lon}&addressdetails=1{}",
        s.endpoint.trim_end_matches('/'),
        language
            .map(|v| format!("&accept-language={}", query_escape(v)))
            .unwrap_or_default()
    );
    let v = get_json(&url, None, s.user_agent_contact.as_deref()).await?;
    v.get("display_name")
        .and_then(Value::as_str)
        .or_else(|| v.pointer("/address/city").and_then(Value::as_str))
        .map(str::to_owned)
        .filter(|x| !x.trim().is_empty())
        .ok_or_else(|| "nominatim response has no place name".into())
}

async fn photon_reverse(
    s: &ProviderSettings,
    lat: f64,
    lon: f64,
    language: Option<&str>,
) -> Result<String, String> {
    let mut url = format!(
        "{}/reverse?lat={lat}&lon={lon}",
        s.endpoint.trim_end_matches('/')
    );
    if let Some(lang) = language {
        url.push_str(&format!("&lang={}", query_escape(lang)));
    }
    let v = get_json(&url, None, s.user_agent_contact.as_deref()).await?;
    let p = v
        .pointer("/features/0/properties")
        .ok_or("photon response has no feature")?;
    ["name", "city", "state", "country"]
        .iter()
        .find_map(|k| p.get(*k).and_then(Value::as_str))
        .map(str::to_owned)
        .filter(|x| !x.trim().is_empty())
        .ok_or_else(|| "photon response has no place name".into())
}

async fn google_reverse(
    s: &ProviderSettings,
    lat: f64,
    lon: f64,
    language: Option<&str>,
) -> Result<String, String> {
    let key = s
        .api_key
        .as_deref()
        .ok_or("google geocoder requires an API key")?;
    let mut url = format!(
        "{}/geocode/json?latlng={lat},{lon}&key={}",
        s.endpoint.trim_end_matches('/'),
        query_escape(key)
    );
    if let Some(lang) = language {
        url.push_str(&format!("&language={}", query_escape(lang)));
    }
    let v = get_json(&url, None, s.user_agent_contact.as_deref()).await?;
    if v.get("status").and_then(Value::as_str) != Some("OK") {
        return Err(format!(
            "google geocoder status: {}",
            v["status"].as_str().unwrap_or("unknown")
        ));
    }
    v.pointer("/results/0/formatted_address")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| "google response has no place name".into())
}

async fn mapbox_reverse(
    s: &ProviderSettings,
    lat: f64,
    lon: f64,
    language: Option<&str>,
) -> Result<String, String> {
    let token = s
        .api_key
        .as_deref()
        .ok_or("mapbox geocoder requires an API key")?;
    let mut url = format!(
        "{}/geocoding/v5/mapbox.places/{lon},{lat}.json?access_token={}",
        s.endpoint.trim_end_matches('/'),
        query_escape(token)
    );
    if let Some(lang) = language {
        url.push_str(&format!("&language={}", query_escape(lang)));
    }
    let v = get_json(&url, None, s.user_agent_contact.as_deref()).await?;
    v.pointer("/features/0/place_name")
        .and_then(Value::as_str)
        .or_else(|| v.pointer("/features/0/text").and_then(Value::as_str))
        .map(str::to_owned)
        .ok_or_else(|| "mapbox response has no place name".into())
}

/// Raw file bytes are sent to the model's `images` field when present.
/// Every request carries `keep_alive` so the model stays resident between
/// scans instead of paying a cold-load on each first image request.
const OLLAMA_KEEP_ALIVE: &str = "30m";

async fn ollama_complete(
    s: &ProviderSettings,
    path: &Path,
    text: Option<&str>,
    image: Option<&[u8]>,
) -> Result<String, String> {
    let endpoint = format!("{}/api/chat", s.endpoint.trim_end_matches('/'));
    let prompt = description_prompt(path, text, s.max_words.unwrap_or(DEFAULT_MAX_WORDS));
    let mut message = json!({ "role": "user", "content": prompt });
    if let Some(bytes) = image {
        message["images"] = json!([general_purpose::STANDARD.encode(bytes)]);
    }
    let body = json!({ "model": s.model.clone().unwrap_or_default(), "messages": [message], "stream": false, "options": { "num_predict": s.max_words.unwrap_or(DEFAULT_MAX_WORDS) * 4 }, "keep_alive": OLLAMA_KEEP_ALIVE });
    let v = post_json(&endpoint, s.api_key.as_deref(), body).await?;
    v.pointer("/message/content")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .filter(|x| !x.trim().is_empty())
        .ok_or_else(|| "ollama response has no message content".into())
}

async fn openrouter_complete(
    s: &ProviderSettings,
    path: &Path,
    text: Option<&str>,
) -> Result<String, String> {
    let endpoint = format!("{}/chat/completions", s.endpoint.trim_end_matches('/'));
    let max = s.max_words.unwrap_or(DEFAULT_MAX_WORDS);
    let body = json!({"model": s.model.clone().unwrap_or_default(), "messages": [{"role":"user", "content": description_prompt(path, text, max)}], "max_tokens": max * 4});
    let v = post_json(&endpoint, s.api_key.as_deref(), body).await?;
    v.pointer("/choices/0/message/content")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .filter(|x| !x.trim().is_empty())
        .ok_or_else(|| "openrouter response has no completion content".into())
}

/// POST `body` JSON to `endpoint` and parse the JSON response. Shared by
/// the neutral geocoder and description transports: bearer auth when an
/// api key is configured, JSON content-type, and a bounded failure on any
/// non-2xx status, unreadable body, or non-JSON response.
async fn post_json(endpoint: &str, api_key: Option<&str>, body: Value) -> Result<Value, String> {
    let client = {
        let connector = hyper_rustls::HttpsConnectorBuilder::new()
            .with_webpki_roots()
            .https_or_http()
            .enable_http1()
            .build();
        HyperClient::builder(TokioExecutor::new()).build(connector)
    };
    let uri: hyper::Uri = endpoint
        .parse()
        .map_err(|e| format!("invalid provider endpoint: {e}"))?;
    let mut builder = Request::builder().method(Method::POST).uri(uri);
    if let Some(key) = api_key {
        builder = builder.header(
            "authorization",
            HeaderValue::from_str(&format!("Bearer {key}")).map_err(|e| e.to_string())?,
        );
    }
    let bytes = serde_json::to_vec(&body).map_err(|e| format!("provider serialize: {e}"))?;
    builder = builder.header("content-type", "application/json");
    let req = builder
        .body(Full::new(Bytes::from(bytes)))
        .map_err(|e| format!("provider request build: {e}"))?;
    let resp = client
        .request(req)
        .await
        .map_err(|e| format!("provider transport error: {e}"))?;
    let status = resp.status();
    let bytes = resp
        .into_body()
        .collect()
        .await
        .map_err(|e| format!("provider body read: {e}"))?
        .to_bytes();
    if !status.is_success() {
        return Err(format!("provider returned {}", status.as_u16()));
    }
    serde_json::from_slice(&bytes).map_err(|_| "provider response is not JSON".to_string())
}

/// Reverse geocode against the neutral `custom` contract.
async fn neutral_reverse_geocode(
    s: &ProviderSettings,
    latitude: f64,
    longitude: f64,
    language: Option<&str>,
) -> Result<String, String> {
    if s.endpoint.is_empty() {
        return Err("custom geocoder needs an endpoint".to_string());
    }
    let body: Value = json!({
        "latitude": latitude,
        "longitude": longitude,
        "language": language.unwrap_or(""),
    });
    let value = post_json(&s.endpoint, s.api_key.as_deref(), body).await?;
    value["name"]
        .as_str()
        .map(str::to_string)
        .filter(|n| !n.trim().is_empty())
        .ok_or_else(|| "geocoder response has no 'name' field".to_string())
}

/// Complete a description against the neutral `custom` contract: `POST`
/// `endpoint` with `{"model", "prompt", "max_words"}`, where the prompt
/// carries the file identity and the extracted plain text when a document
/// extractor produced it, and a successful response is `{"content": ...}`.
async fn neutral_complete(
    s: &ProviderSettings,
    path: &Path,
    text: Option<&str>,
) -> Result<String, String> {
    if s.endpoint.is_empty() {
        return Err("custom description provider needs an endpoint".to_string());
    }
    let max_words = s.max_words.unwrap_or(DEFAULT_MAX_WORDS);
    let body: Value = json!({
        "model": s.model.clone().unwrap_or_default(),
        "prompt": description_prompt(path, text, max_words),
        "max_words": max_words,
    });
    let value = post_json(&s.endpoint, s.api_key.as_deref(), body).await?;
    value["content"]
        .as_str()
        .map(str::to_string)
        .filter(|c| !c.trim().is_empty())
        .ok_or_else(|| "description response has no 'content' field".to_string())
}

/// Raw image payload bound for vision description requests, in bytes.
/// A photo larger than this degrades to the text-only prompt; once
/// base64-encoded, the body would be ~1.33x this size anyway.

const MAX_IMAGE_BYTES: u64 = 20 * 1024 * 1024;

/// Read a unit's raw bytes for a vision description request, bounded by
/// `MAX_IMAGE_BYTES`. `None` degrades the job to the text-only prompt;
/// reading is local and bounded, never a scan or provider failure.
fn read_image_bytes(path: &Path) -> Option<Vec<u8>> {
    let meta = std::fs::metadata(path).ok()?;
    if meta.len() > MAX_IMAGE_BYTES {
        return None;
    }
    std::fs::read(path).ok()
}

/// Assemble the prompt for a description request: the file identity and
/// the effective word budget, plus the extracted text when a document
/// extractor produced it (documents are preprocessed to plain text before
/// the request per `[document_extraction]`). The completion the model
/// returns is used verbatim as the `%s` value, so the prompt asks for a
/// short noun phrase.
fn description_prompt(path: &Path, text: Option<&str>, max_words: u64) -> String {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string());
    let mut prompt = format!(
        "Describe the subject matter of this file in at most {max_words} words, for use as an archive folder name. Reply with only the phrase.\n\nFile: {name}"
    );
    if let Some(t) = text {
        if !t.is_empty() {
            prompt.push_str("\n\nContent:\n");
            prompt.push_str(t);
        }
    }
    prompt
}

/// Resolve one provider job for a unit: record a successful value (when
/// `Some`), decrement the unit's pending count, and - once the last
/// outstanding request resolves - update the unit's suggestion in place:
/// `metadata_pending` flips to `false`, and on success the move is rebuilt
/// with the collected provider values (selection, status, and rejection
/// reason are preserved). Clients receive an updated `suggestion.batch`
/// only when something changed.
///
/// Returns `false` when nothing was updated (stale epoch, scan not yet
/// published, unit mismatch, pending requests remain, or no suggestion for
/// the unit).
pub async fn resolve_provider_job(app: &AppHandle, job: &ProviderJob, value: Option<&str>) -> bool {
    let mut sess = app.session.lock().await;
    if sess.summary.is_none() {
        // The scan has not published its batch yet.
        return false;
    }
    // Phase 1: ctx bookkeeping - record the value and decrement the unit's
    // pending count.
    {
        let Some(ctx) = &mut sess.provider_ctx else {
            return false;
        };
        if ctx.epoch != job.unit.epoch {
            return false;
        }
        let Some(f) = ctx.analysis.files.get(job.unit.index) else {
            return false;
        };
        if f.path != job.unit.path {
            return false;
        }
        if let Some(v) = value {
            let unit = ctx.meta.entry(job.unit.index).or_default();
            match job.kind {
                ProviderKind::Geocode => unit.location = Some(v.to_string()),
                ProviderKind::Description => unit.subject = Some(v.to_string()),
            }
        }
        let Some(pending) = ctx.pending.get_mut(&job.unit.index) else {
            // No outstanding job was tracked for this unit.
            return false;
        };
        *pending = pending.saturating_sub(1);
        if *pending > 0 {
            // More requests for this unit are outstanding; stay pending.
            return false;
        }
        ctx.pending.remove(&job.unit.index);
    }

    // Phase 2: the unit's last request resolved. Update its suggestion in
    // place by stable id.
    let (id, rebuilt) = {
        let ctx = sess.provider_ctx.as_ref().unwrap();
        let f = &ctx.analysis.files[job.unit.index];
        let mut trial = f.clone();
        if let Some(m) = ctx.meta.get(&job.unit.index) {
            trial.location = m.location.clone().or(trial.location);
            trial.subject = m.subject.clone().or(trial.subject);
        }
        let fmt = ctx.formats.get(&trial.kind);
        let rebuilt = rebuild_move_suggestion(&trial, fmt, &ctx.home, &ctx.symlinks);
        (Suggestion::id_for_target(&f.path), rebuilt)
    };
    let Some(pos) = sess.suggestions.iter().position(|s| s.id == id) else {
        return false;
    };
    match rebuilt {
        // Rebuild error: keep the published fallback, just clear the
        // pending flag.
        Err(_) => {
            sess.suggestions[pos].metadata_pending = false;
        }
        // The file now stays put: the move disappears from the batch.
        Ok(None) => {
            sess.suggestions.remove(pos);
        }
        Ok(Some(mut s)) => {
            let prior = &sess.suggestions[pos];
            s.selected = prior.selected;
            s.status = prior.status;
            s.rejection_reason = prior.rejection_reason.clone();
            s.metadata_pending = false;
            sess.suggestions[pos] = s;
        }
    }
    let mut summary = sess.summary_of_batch();
    if let Some(prev) = &sess.summary {
        summary.total_files_scanned = prev.total_files_scanned;
        summary.total_symlinks_indexed = prev.total_symlinks_indexed;
    }
    sess.summary = Some(summary);
    let items = sess.suggestions.clone();
    let total = items.len();
    drop(sess);
    app.events
        .publish(ServerEvent::SuggestionBatch { items, total });
    true
}

/// Build the geocoder provider settings from the effective configuration.
/// `None` when the geocoder is disabled.
pub fn geocoder_settings(cfg: &RawConfig) -> Option<ProviderSettings> {
    if !cfg.geocoder.enabled {
        return None;
    }
    let service = cfg.geocoder_service();
    let (backend, built_in_endpoint, built_in_interval) = match service {
        GeocoderService::Nominatim => (
            "nominatim",
            "https://nominatim.openstreetmap.org",
            Duration::from_secs(1),
        ),
        GeocoderService::Photon => ("photon", "https://photon.komoot.io", Duration::from_secs(1)),
        GeocoderService::Google => (
            "google",
            "https://maps.googleapis.com/maps/api",
            Duration::ZERO,
        ),
        GeocoderService::Mapbox => ("mapbox", "https://api.mapbox.com", Duration::ZERO),
        GeocoderService::Custom => (
            "custom",
            "",
            // The custom/self-hosted service is the user's responsibility;
            // with no built-in pace it starts from zero and is paced purely
            // by the optional configured rate limit (0 = no delay).
            Duration::ZERO,
        ),
    };
    // The optional configured rate limit is an additional floor on top of
    // the service's built-in public pacing: it can only slow requests down,
    // never exceed the documented usage pace (see "Geocoder rate limiting
    // and identification" in DESIGN.md).
    let min_interval = built_in_interval.max(Duration::from_millis(
        cfg.geocoder.rate_limit_ms.unwrap_or(0),
    ));
    let endpoint = cfg
        .geocoder
        .endpoint
        .clone()
        .unwrap_or_else(|| built_in_endpoint.to_string());
    let mut provider_id = format!("geocode:{backend}");
    if service == GeocoderService::Custom && !endpoint.is_empty() {
        provider_id.push(':');
        provider_id.push_str(&endpoint);
    }
    let language = cfg.geocoder.language.clone();
    let api_key = cfg.geocoder.api_key.clone();
    let settings_hash = sha256(&[
        backend.as_bytes(),
        endpoint.as_bytes(),
        language.as_deref().unwrap_or("").as_bytes(),
        cfg.geocoder.api_key.as_deref().unwrap_or("").as_bytes(),
    ]);
    Some(ProviderSettings {
        provider_id,
        settings_hash,
        min_interval,
        backend: backend.to_string(),
        endpoint,
        api_key,
        language,
        user_agent_contact: cfg.geocoder.user_agent_contact.clone(),
        model: None,
        max_words: None,
    })
}

/// Build the description provider settings for one asset kind from the
/// effective configuration, following per-kind overrides. `None` when the
/// provider for that kind is disabled.
pub fn description_settings(cfg: &RawConfig, kind: &AssetKind) -> Option<ProviderSettings> {
    let eff = cfg
        .description
        .kind_effective(&kind.name())
        .unwrap_or_else(|| cfg.description.base_effective());
    if !eff.enabled {
        return None;
    }
    let backend = eff.service.name();
    let endpoint = eff
        .endpoint
        .clone()
        .unwrap_or_else(|| eff.service.default_endpoint().to_string());
    let mut provider_id = format!("describe:{backend}");
    if backend == "custom" && !endpoint.is_empty() {
        provider_id.push(':');
        provider_id.push_str(&endpoint);
    }
    let settings_hash = sha256(&[
        backend.as_bytes(),
        endpoint.as_bytes(),
        eff.model.as_deref().unwrap_or("").as_bytes(),
        eff.api_key.as_deref().unwrap_or("").as_bytes(),
        // The word budget governs the request, so a different cap must not
        // reuse cached descriptions from another budget.
        eff.max_words.to_le_bytes().as_slice(),
    ]);
    Some(ProviderSettings {
        provider_id,
        settings_hash,
        min_interval: Duration::from_millis(eff.rate_limit_ms),
        backend: backend.to_string(),
        endpoint,
        api_key: eff.api_key,
        language: None,
        user_agent_contact: None,
        model: eff.model,
        max_words: Some(eff.max_words),
    })
}

/// Plan provider work for a completed scan: one job per unit whose
/// destination format actually uses the corresponding placeholder, per
/// enabled provider. The pipeline calls this before publishing the batch
/// so units with jobs have their suggestions marked `metadata_pending`.
///
/// Gating: a geocode job is planned only when the unit's format contains
/// `%l` (location), the geocoder is enabled, and the unit carries raw GPS
/// coordinates; a description job only when the format contains `%s`
/// (subject/subtitle) and a description provider is enabled for the unit's
/// kind. A unit whose format never uses a placeholder never triggers a
/// provider call.
pub async fn plan_provider_jobs(
    cfg: &RawConfig,
    analysis: &Analysis,
    formats: &BTreeMap<AssetKind, FormatString>,
    epoch: u64,
) -> Vec<ProviderJob> {
    let geo = geocoder_settings(cfg);
    let mut jobs = Vec::new();
    for (i, f) in analysis.files.iter().enumerate() {
        let Some(fmt) = formats.get(&f.kind) else {
            continue;
        };
        let uses_location = fmt
            .segments()
            .iter()
            .any(|s| matches!(s, Segment::Placeholder(Placeholder::Location)));
        let uses_subject = fmt
            .segments()
            .iter()
            .any(|s| matches!(s, Segment::Placeholder(Placeholder::Subject)));
        if uses_location {
            if let (Some(g), Some(lat), Some(lon)) = (&geo, f.latitude, f.longitude) {
                let input_hash = sha256(&[
                    b"geocode",
                    lat.to_le_bytes().as_slice(),
                    lon.to_le_bytes().as_slice(),
                    g.language.as_deref().unwrap_or("").as_bytes(),
                ]);
                jobs.push(ProviderJob {
                    kind: ProviderKind::Geocode,
                    settings: g.clone(),
                    unit: UnitRef {
                        epoch,
                        index: i,
                        path: f.path.clone(),
                    },
                    input_hash,
                    input: ProviderInput::Geocode {
                        latitude: lat,
                        longitude: lon,
                        language: g.language.clone(),
                    },
                });
            }
        }
        if uses_subject {
            if let Some(desc) = description_settings(cfg, &f.kind) {
                // Photos with an image-capable backend (ollama) get the raw
                // bytes analyzed; other kinds stay text/filename-only. The vision
                // bit joins the cache key so a cached text-only guess is never
                // served for a vision job (and vice versa), and a photo's
                // content hash binds the entry to the pixels, not just the path.
                let vision = f.kind == AssetKind::Photo && desc.backend == "ollama";
                let mut parts: Vec<&[u8]> = vec![
                    b"describe",
                    if vision { b"vision" } else { b"text" },
                    f.path.as_os_str().as_encoded_bytes(),
                ];
                let mut hash_owned: Vec<u8> = Vec::new();
                if vision {
                    if let Some(h) = f.full_hash {
                        hash_owned.extend_from_slice(&h);
                    }
                }
                if vision {
                    parts.push(hash_owned.as_slice());
                }
                let input_hash = sha256(&parts);
                jobs.push(ProviderJob {
                    kind: ProviderKind::Description,
                    settings: desc,
                    unit: UnitRef {
                        epoch,
                        index: i,
                        path: f.path.clone(),
                    },
                    input_hash,
                    input: ProviderInput::Describe {
                        text: None,
                        vision,
                        image: None,
                    },
                });
            }
        }
    }
    jobs
}

/// Deterministic settings hash builder for cache keys. Any field change
/// yields a different hash, invalidating prior cached results.
pub fn sha256(parts: &[&[u8]]) -> [u8; 32] {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    for part in parts {
        h.update((part.len() as u64).to_le_bytes());
        h.update(part);
    }
    h.finalize().into()
}

/// Fire-and-forget model warmup for vision-capable description jobs.
/// A tiny text-only chat to the same endpoint/model forces Ollama to load
/// the model out-of-band (no timeout), so the worker's first image request
/// finds it warm instead of spending its provider timeout on model load.
/// Failures are ignored: the worker's real request simply pays the load
/// cost, and every request's `keep_alive` keeps the model resident between
/// scans. One warmup per scan is enough; it is spawned at scan time so it
/// runs concurrently with publishing.
pub fn warmup_vision_models(jobs: &[ProviderJob]) {
    for job in jobs {
        if matches!(&job.input, ProviderInput::Describe { vision: true, .. })
            && job.settings.backend == "ollama"
        {
            let endpoint = job.settings.endpoint.clone();
            let model = job.settings.model.clone().unwrap_or_default();
            if model.is_empty() {
                return;
            }
            tokio::spawn(async move {
                let body = json!({
                    "model": model,
                    "messages": [{"role": "user", "content": "ping"}],
                    "stream": false,
                    "options": { "num_predict": 1 },
                    "keep_alive": OLLAMA_KEEP_ALIVE,
                });
                let _ = post_json(&format!("{endpoint}/api/chat"), None, body).await;
            });
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::time::SystemTime;

    use axum::routing::post;
    use axum::{Json, Router};

    use homelinter_analyze::AnalyzedFile;
    use homelinter_model::suggestion::{Provenance, ScanSummary, SessionState};
    use homelinter_suggest::{generate_suggestions, GenerateInput};

    use crate::build_service;
    use crate::pipeline::AppHandle;
    use crate::ServiceOptions;

    fn tmp_dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "hl-prov-{tag}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// A session primed like the end of a real scan: one photo whose move
    /// suggestion resolves with an empty `%l`, ready for a late geocode.
    async fn primed_app() -> (AppHandle, u64) {
        primed_app_with_format("Pictures/%Y/%l/%f").await
    }

    /// A session primed like the end of a real scan: one file whose move
    /// suggestion resolves against `format`, ready for a late provider
    /// request. `%l` resolves to `unknown` and `%s` to `unknown` until a
    /// provider result overlays it.
    async fn primed_app_with_format(format: &str) -> (AppHandle, u64) {
        let dir = tmp_dir("app");
        let home = dir.join("home");
        std::fs::create_dir_all(&home).unwrap();
        let built = build_service(ServiceOptions {
            home_override: Some(home.clone()),
            data_dir_override: Some(dir.join("data")),
            ..Default::default()
        })
        .unwrap();
        let app = built.app;
        let epoch = 1u64;
        app.provider_epoch.store(epoch, Ordering::SeqCst);

        let photo = home.join("IMG_0001.jpg");
        std::fs::write(&photo, b"photo bytes").unwrap();

        let file = AnalyzedFile {
            path: photo.clone(),
            kind: AssetKind::Photo,
            size: 11,
            modified: SystemTime::now(),
            created: None,
            full_hash: None,
            provenance: Provenance::default(),
            extracted_date: None,
            date_source: "",
            embedded_title: None,
            embedded_author: None,
            camera_model: None,
            latitude: None,
            longitude: None,
            location: None,
            subject: None,
            bitrate_kbps: None,
            duration_secs: None,
            page_count: None,
        };
        let analysis = Arc::new(Analysis {
            files: vec![file],
            repos: Vec::new(),
            groups: Vec::new(),
            degraded_identity: false,
        });
        let mut formats = BTreeMap::new();
        formats.insert(AssetKind::Photo, FormatString::parse(format).unwrap());
        let symlinks = SymlinkIndex::default();

        let input = GenerateInput {
            analysis: &analysis,
            symlinks: &symlinks,
            formats: &formats,
            home: &home,
        };
        let suggestions = generate_suggestions(&input).unwrap().suggestions;
        assert_eq!(suggestions.len(), 1);
        assert!(!suggestions[0]
            .new_path
            .as_deref()
            .unwrap()
            .contains("paris"));

        let mut sess = app.session.lock().await;
        let mut suggestion = suggestions.into_iter().next().unwrap();
        suggestion.metadata_pending = true;
        sess.provider_ctx = Some(ProviderScanCtx {
            epoch,
            analysis: analysis.clone(),
            symlinks: symlinks.clone(),
            formats: formats.clone(),
            home: home.clone(),
            meta: Default::default(),
            pending: BTreeMap::from([(0, 1)]),
        });
        sess.suggestions = vec![suggestion];
        sess.summary = Some(ScanSummary::default());
        sess.state = SessionState::Ready;
        drop(sess);
        (app, epoch)
    }

    /// A stub `custom` geocoder: POST / answers the neutral contract.
    async fn stub_geocoder() -> u16 {
        let router = Router::new().route(
            "/",
            post(|Json(_body): Json<Value>| async move { Json(json!({ "name": "paris" })) }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let _ = axum::serve(listener, router).await;
        });
        port
    }

    /// A stub `custom` description provider: POST / answers the neutral
    /// contract with `content`, but only when the request carries the
    /// model and an expected word budget; otherwise it answers an empty
    /// content so the transport treats it as a malformed response.
    async fn stub_describe(content: &'static str) -> u16 {
        let router = Router::new().route(
            "/",
            post(move |Json(body): Json<Value>| async move {
                let ok = body["model"].as_str() == Some("test-model")
                    && body["max_words"].as_u64() == Some(8)
                    && body["prompt"].as_str().is_some_and(|p| {
                        p.contains("IMG_0001.jpg") && p.contains("subject matter")
                    });
                if ok {
                    Json(json!({ "content": content }))
                } else {
                    Json(json!({ "content": "" }))
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let _ = axum::serve(listener, router).await;
        });
        port
    }

    async fn geocode_job(app: &AppHandle, port: u16, epoch: u64) -> ProviderJob {
        let mut job = ProviderJob {
            kind: ProviderKind::Geocode,
            settings: ProviderSettings {
                provider_id: format!("geocode:custom:{port}"),
                settings_hash: sha256(&[b"custom", port.to_string().as_bytes()]),
                min_interval: Duration::ZERO,
                backend: "custom".to_string(),
                endpoint: format!("http://127.0.0.1:{port}"),
                api_key: None,
                language: None,
                user_agent_contact: None,
                model: None,
                max_words: None,
            },
            unit: UnitRef {
                epoch,
                index: 0,
                path: PathBuf::new(),
            },
            input_hash: sha256(&[&[1u8], b"latlon"]),
            input: ProviderInput::Geocode {
                latitude: 48.8566,
                longitude: 2.3522,
                language: None,
            },
        };
        let sess = app.session.lock().await;
        job.unit.path = sess.provider_ctx.as_ref().unwrap().analysis.files[0]
            .path
            .clone();
        drop(sess);
        job
    }

    async fn describe_job(app: &AppHandle, port: u16, epoch: u64) -> ProviderJob {
        let mut job = ProviderJob {
            kind: ProviderKind::Description,
            settings: ProviderSettings {
                provider_id: format!("describe:custom:{port}"),
                settings_hash: sha256(&[b"custom", port.to_string().as_bytes()]),
                min_interval: Duration::ZERO,
                backend: "custom".to_string(),
                endpoint: format!("http://127.0.0.1:{port}"),
                api_key: None,
                language: None,
                user_agent_contact: None,
                model: Some("test-model".to_string()),
                max_words: Some(8),
            },
            unit: UnitRef {
                epoch,
                index: 0,
                path: PathBuf::new(),
            },
            input_hash: sha256(&[&[3u8], b"desc"]),
            input: ProviderInput::Describe {
                text: None,
                vision: false,
                image: None,
            },
        };
        let sess = app.session.lock().await;
        job.unit.path = sess.provider_ctx.as_ref().unwrap().analysis.files[0]
            .path
            .clone();
        drop(sess);
        job
    }

    #[tokio::test]
    async fn worker_fetches_applies_and_caches() {
        let (app, epoch) = primed_app().await;
        let port = stub_geocoder().await;
        let queue = app.provider_queue.clone();
        let job = geocode_job(&app, port, epoch).await;
        queue.enqueue(job.clone());
        spawn(app.clone());

        // Wait for the pending flag to clear with the geocoded location.
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut refreshed = false;
        while Instant::now() < deadline {
            let sess = app.session.lock().await;
            let s = sess.suggestions.first().cloned();
            refreshed = s.is_some_and(|s| {
                !s.metadata_pending && s.new_path.as_deref().is_some_and(|p| p.contains("paris"))
            });
            drop(sess);
            if refreshed {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(refreshed, "geocoded path never appeared");

        // The result was cached under the provider identity.
        let cache = app.cache.lock().await;
        let key = super::cache_key(
            KIND_GEOCODE,
            &job.settings.provider_id,
            &job.settings.settings_hash,
            &job.input_hash,
        );
        assert_eq!(
            cache
                .get(&key, &job.settings.provider_id)
                .unwrap()
                .as_deref(),
            Some("paris")
        );
    }

    #[tokio::test]
    async fn worker_fetches_applies_and_caches_description() {
        let (app, epoch) = primed_app_with_format("Documents/%Y/%s/%f").await;
        let port = stub_describe("march invoices").await;
        let queue = app.provider_queue.clone();
        let job = describe_job(&app, port, epoch).await;
        queue.enqueue(job.clone());
        spawn(app.clone());

        // Wait for the pending flag to clear with the described subject.
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut refreshed = false;
        while Instant::now() < deadline {
            let sess = app.session.lock().await;
            let s = sess.suggestions.first().cloned();
            refreshed = s.is_some_and(|s| {
                !s.metadata_pending
                    && s.new_path
                        .as_deref()
                        .is_some_and(|p| p.contains("march-invoices"))
            });
            drop(sess);
            if refreshed {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(refreshed, "described path never appeared");

        // The result was cached under the provider identity.
        let cache = app.cache.lock().await;
        let key = super::cache_key(
            KIND_DESCRIPTION,
            &job.settings.provider_id,
            &job.settings.settings_hash,
            &job.input_hash,
        );
        assert_eq!(
            cache
                .get(&key, &job.settings.provider_id)
                .unwrap()
                .as_deref(),
            Some("march invoices")
        );
    }

    #[tokio::test]
    async fn neutral_custom_description_transport() {
        // The stub enforces the whole neutral contract on the wire: bearer
        // auth, model, word budget, and a prompt carrying the file identity
        // and the extracted document text. Any deviation answers an empty
        // content, which the transport must treat as a failure.
        let router = Router::new().route(
            "/",
            post(
                |headers: axum::http::HeaderMap, Json(body): Json<Value>| async move {
                    let auth = headers.get("authorization").and_then(|v| v.to_str().ok());
                    let ok = auth == Some("Bearer secret")
                        && body["model"].as_str() == Some("contract-model")
                        && body["max_words"].as_u64() == Some(12)
                        && body["prompt"].as_str().is_some_and(|p| {
                            p.contains("receipt.pdf") && p.contains("Vendor: ACME, March 2024")
                        });
                    if ok {
                        Json(json!({ "content": "travel expenses" }))
                    } else {
                        Json(json!({ "content": "" }))
                    }
                },
            ),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let _ = axum::serve(listener, router).await;
        });

        let path = tmp_dir("contract").join("receipt.pdf");
        std::fs::write(&path, b"%PDF-1.4 fake").unwrap();
        let settings = ProviderSettings {
            provider_id: format!("describe:custom:{port}"),
            settings_hash: [0u8; 32],
            min_interval: Duration::ZERO,
            backend: "custom".to_string(),
            endpoint: format!("http://127.0.0.1:{port}"),
            api_key: Some("secret".to_string()),
            language: None,
            user_agent_contact: None,
            model: Some("contract-model".to_string()),
            max_words: Some(12),
        };
        let unit = UnitRef {
            epoch: 1,
            index: 0,
            path: path.clone(),
        };
        let job = ProviderJob {
            kind: ProviderKind::Description,
            settings: settings.clone(),
            unit: unit.clone(),
            input_hash: [0u8; 32],
            input: ProviderInput::Describe {
                text: Some("Vendor: ACME, March 2024".to_string()),
                vision: false,
                image: None,
            },
        };
        assert_eq!(invoke(&job).await.unwrap(), "travel expenses".to_string());

        // A request that does not carry the expected inputs (here: text
        // from a different document) is answered with empty content, which
        // degrades to a provider failure rather than an empty subject.
        let other = ProviderJob {
            input: ProviderInput::Describe {
                text: Some("Unrelated notes".to_string()),
                vision: false,
                image: None,
            },
            ..job
        };
        let err = invoke(&other).await.unwrap_err();
        assert!(err.contains("'content'"), "{err}");
    }

    #[tokio::test]
    async fn ollama_vision_transport_sends_image_bytes() {
        // The stub enforces the ollama chat shape for vision: the user
        // message carries the expected model and an `images` array holding
        // base64 of the raw bytes. Any deviation answers empty content,
        // which the transport must treat as a failure.
        let router = Router::new().route(
            "/api/chat",
            post(|Json(body): Json<Value>| async move {
                let img = body["messages"][0]["images"]
                    .as_array()
                    .and_then(|a| a.first())
                    .and_then(|v| v.as_str());
                let decoded =
                    img.and_then(|v| base64::engine::general_purpose::STANDARD.decode(v).ok());
                let ok = body["model"].as_str() == Some("vl-model")
                    && body["messages"][0].get("content").is_some()
                    && decoded.as_deref() == Some(&b"fake-image-bytes"[..]);
                if ok {
                    Json(json!({ "message": { "content": "sunset beach" } }))
                } else {
                    Json(json!({ "message": { "content": "" } }))
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let _ = axum::serve(listener, router).await;
        });

        let path = tmp_dir("vision").join("img.jpg");
        std::fs::write(&path, b"fake-image-bytes").unwrap();
        let settings = ProviderSettings {
            provider_id: format!("describe:ollama:{port}"),
            settings_hash: [0u8; 32],
            min_interval: Duration::ZERO,
            backend: "ollama".to_string(),
            endpoint: format!("http://127.0.0.1:{port}"),
            api_key: None,
            language: None,
            user_agent_contact: None,
            model: Some("vl-model".to_string()),
            max_words: Some(8),
        };
        let out = ollama_complete(&settings, &path, None, Some(&b"fake-image-bytes"[..]))
            .await
            .unwrap();
        assert_eq!(out, "sunset beach".to_string());

        // Without image bytes the request degrades to the text-only prompt:
        // no `images` key, and a model that requires vision answers empty,
        // which is a provider failure.
        let stub = Router::new().route(
            "/api/chat",
            post(|Json(body): Json<Value>| async move {
                let ok = body["messages"][0].get("images").is_none();
                if ok {
                    Json(json!({ "message": { "content": "from filename" } }))
                } else {
                    Json(json!({ "message": { "content": "" } }))
                }
            }),
        );
        let listener2 = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port2 = listener2.local_addr().unwrap().port();
        tokio::spawn(async move {
            let _ = axum::serve(listener2, stub).await;
        });
        let settings2 = ProviderSettings {
            provider_id: format!("describe:ollama:{port2}"),
            settings_hash: [0u8; 32],
            min_interval: Duration::ZERO,
            backend: "ollama".to_string(),
            endpoint: format!("http://127.0.0.1:{port2}"),
            api_key: None,
            language: None,
            user_agent_contact: None,
            model: Some("vl-model".to_string()),
            max_words: Some(8),
        };
        let out2 = ollama_complete(&settings2, &path, None, None)
            .await
            .unwrap();
        assert_eq!(out2, "from filename".to_string());
    }

    #[tokio::test]
    async fn configured_backends_fail_cleanly_without_credentials_or_endpoint() {
        let (app, epoch) = primed_app_with_format("Documents/%Y/%s/%f").await;
        let mut desc = describe_job(&app, 1, epoch).await;
        desc.settings.backend = "openrouter".to_string();
        desc.settings.endpoint = "https://openrouter.ai/api/v1".to_string();
        let err = invoke(&desc).await.unwrap_err();
        assert!(err.contains("API key"), "{err}");

        let mut geo = geocode_job(&app, 1, epoch).await;
        geo.settings.backend = "nominatim".to_string();
        geo.settings.endpoint.clear();
        let err = invoke(&geo).await.unwrap_err();
        assert!(err.contains("endpoint"), "{err}");
    }

    #[tokio::test]
    async fn pacing_respects_min_interval() {
        let settings = ProviderSettings {
            provider_id: "geocode:nominatim".to_string(),
            settings_hash: [0u8; 32],
            min_interval: Duration::from_millis(150),
            backend: "nominatim".into(),
            endpoint: "x".into(),
            api_key: None,
            language: None,
            user_agent_contact: None,
            model: None,
            max_words: None,
        };
        let mut last = BTreeMap::new();
        pace(&settings, &mut last).await;
        let t0 = Instant::now();
        pace(&settings, &mut last).await;
        assert!(
            t0.elapsed() >= Duration::from_millis(140),
            "pacing fell short"
        );

        // A different provider is never held back.
        let mut fast = settings.clone();
        fast.provider_id = "geocode:custom".into();
        fast.min_interval = Duration::ZERO;
        let t1 = Instant::now();
        pace(&fast, &mut last).await;
        assert!(t1.elapsed() < Duration::from_millis(20));
    }

    #[tokio::test]
    async fn stale_epoch_result_is_dropped() {
        let (app, epoch) = primed_app().await;
        let job = geocode_job(&app, 9, epoch + 1).await; // wrong epoch
        let applied = resolve_provider_job(&app, &job, Some("paris")).await;
        assert!(!applied);
        let sess = app.session.lock().await;
        // Nothing changed: the suggestion stays pending and non-executable.
        assert!(sess.suggestions[0].metadata_pending);
        assert!(sess
            .executable_ids(&[sess.suggestions[0].id.clone()])
            .is_empty());
    }

    #[tokio::test]
    async fn pending_until_all_jobs_resolve() {
        let (app, epoch) = primed_app().await;
        let port = stub_geocoder().await;
        let geo = geocode_job(&app, port, epoch).await;
        let mut desc = geo.clone();
        desc.kind = ProviderKind::Description;
        desc.input = ProviderInput::Describe {
            text: None,
            vision: false,
            image: None,
        };
        desc.input_hash = sha256(&[&[2u8], b"desc"]);
        desc.settings.provider_id = "describe:none".into();

        // Two outstanding requests for unit 0.
        {
            let mut sess = app.session.lock().await;
            sess.provider_ctx.as_mut().unwrap().pending.insert(0, 2);
        }

        // The geocode success alone must not clear the pending flag.
        let resolved = resolve_provider_job(&app, &geo, Some("paris")).await;
        assert!(!resolved);
        let sess = app.session.lock().await;
        assert!(sess.suggestions[0].metadata_pending);
        assert!(sess
            .executable_ids(&[sess.suggestions[0].id.clone()])
            .is_empty());
        drop(sess);

        // The failing description resolves the unit: the suggestion
        // publishes the geocoded location and becomes executable.
        let resolved = resolve_provider_job(&app, &desc, None).await;
        assert!(resolved);
        let sess = app.session.lock().await;
        assert!(!sess.suggestions[0].metadata_pending);
        let p = sess.suggestions[0].new_path.clone().unwrap_or_default();
        assert!(p.contains("paris"), "geocode overlay missing: {p}");
        assert_eq!(
            sess.executable_ids(&[sess.suggestions[0].id.clone()]),
            vec![sess.suggestions[0].id.clone()]
        );
    }

    #[tokio::test]
    async fn failed_provider_clears_pending_keeps_fallback() {
        let (app, epoch) = primed_app().await;
        let job = geocode_job(&app, 1, epoch).await;
        // The provider failed: the suggestion stays published with its
        // fallback path and is no longer pending.
        let resolved = resolve_provider_job(&app, &job, None).await;
        assert!(resolved);
        let sess = app.session.lock().await;
        assert!(!sess.suggestions[0].metadata_pending);
        assert!(sess.suggestions[0]
            .new_path
            .as_deref()
            .unwrap()
            .contains("unknown"));
    }

    #[test]
    fn geocoder_settings_from_config() {
        let cfg: RawConfig = toml::from_str(
            r#"
            [geocoder]
            enabled = true
            service = "custom"
            endpoint = "http://geo.local"
            language = "en"
            "#,
        )
        .unwrap();
        let s = geocoder_settings(&cfg).unwrap();
        assert_eq!(s.backend, "custom");
        assert_eq!(s.endpoint, "http://geo.local");
        assert_eq!(s.min_interval, Duration::ZERO);
        assert_eq!(s.language.as_deref(), Some("en"));

        // A configured rate limit paces the custom service.
        let paced: RawConfig = toml::from_str(
            "[geocoder]\nenabled = true\nservice = \"custom\"\nendpoint = \"http://geo.local\"\nrate_limit_ms = 3000\n"
        )
        .unwrap();
        assert_eq!(
            geocoder_settings(&paced).unwrap().min_interval,
            Duration::from_millis(3000)
        );

        // Disabled -> None.
        let off: RawConfig = toml::from_str("").unwrap();
        assert!(geocoder_settings(&off).is_none());

        // Nominatim carries the built-in public pace; a configured rate
        // limit below it is absorbed, above it raises the floor.
        let nom: RawConfig = toml::from_str("[geocoder]\nenabled = true\n").unwrap();
        let s = geocoder_settings(&nom).unwrap();
        assert_eq!(s.backend, "nominatim");
        assert_eq!(s.min_interval, Duration::from_secs(1));
        let below: RawConfig =
            toml::from_str("[geocoder]\nenabled = true\nrate_limit_ms = 500\n").unwrap();
        assert_eq!(
            geocoder_settings(&below).unwrap().min_interval,
            Duration::from_secs(1)
        );
        let above: RawConfig =
            toml::from_str("[geocoder]\nenabled = true\nrate_limit_ms = 2500\n").unwrap();
        assert_eq!(
            geocoder_settings(&above).unwrap().min_interval,
            Duration::from_millis(2500)
        );
    }

    #[test]
    fn description_settings_carry_rate_limit() {
        let cfg: RawConfig = toml::from_str(
            r#"
            [description]
            enabled = true
            model = "m"
            rate_limit_ms = 750
            "#,
        )
        .unwrap();
        let s = description_settings(&cfg, &AssetKind::Document).unwrap();
        assert_eq!(s.min_interval, Duration::from_millis(750));

        // Unset -> no delay; a per-kind override inherits the base and
        // can raise the floor for its own kind.
        let cfg2: RawConfig = toml::from_str(
            r#"
            [description]
            enabled = true
            model = "m"

            [description.photo]
            rate_limit_ms = 2000
            "#,
        )
        .unwrap();
        let base = description_settings(&cfg2, &AssetKind::Document).unwrap();
        assert_eq!(base.min_interval, Duration::ZERO);
        let photo = description_settings(&cfg2, &AssetKind::Photo).unwrap();
        assert_eq!(photo.min_interval, Duration::from_millis(2000));
    }

    fn file_at(
        dir: &PathBuf,
        name: &str,
        kind: AssetKind,
        lat: Option<f64>,
        lon: Option<f64>,
    ) -> AnalyzedFile {
        AnalyzedFile {
            path: dir.join(name),
            kind,
            size: 1,
            modified: SystemTime::now(),
            created: None,
            full_hash: None,
            provenance: Default::default(),
            extracted_date: None,
            date_source: "",
            embedded_title: None,
            embedded_author: None,
            camera_model: None,
            latitude: lat,
            longitude: lon,
            location: None,
            subject: None,
            bitrate_kbps: None,
            duration_secs: None,
            page_count: None,
        }
    }

    #[tokio::test]
    async fn plans_jobs_only_when_format_uses_placeholder() {
        let cfg: RawConfig = toml::from_str(
            r#"
            [geocoder]
            enabled = true
            service = "custom"
            endpoint = "http://geo.local"
            language = "en"

            [description]
            enabled = true
            "#,
        )
        .unwrap();
        let dir = tmp_dir("files");

        // Photo with a %l format and GPS: geocode planned.
        let photo = file_at(
            &dir,
            "IMG_0001.jpg",
            AssetKind::Photo,
            Some(48.8566),
            Some(2.3522),
        );
        // Photo with a %l format but no GPS: nothing planned.
        let photo_no_gps = file_at(&dir, "IMG_0002.jpg", AssetKind::Photo, None, None);
        // Document with a %s format: description planned, no geocode.
        let doc = file_at(&dir, "a.pdf", AssetKind::Document, Some(10.0), Some(20.0));
        // Audio with a format using neither placeholder: nothing planned.
        let audio = file_at(&dir, "b.mp3", AssetKind::Audio, None, None);
        let analysis = Arc::new(Analysis {
            files: vec![photo, photo_no_gps, doc, audio],
            repos: Vec::new(),
            groups: Vec::new(),
            degraded_identity: false,
        });
        let mut formats = BTreeMap::new();
        formats.insert(
            AssetKind::Photo,
            FormatString::parse("Pictures/%l/%f").unwrap(),
        );
        formats.insert(
            AssetKind::Document,
            FormatString::parse("Documents/%s/%f").unwrap(),
        );
        formats.insert(AssetKind::Audio, FormatString::parse("Music/%f").unwrap());

        let jobs = plan_provider_jobs(&cfg, &analysis, &formats, 7).await;

        let photo_jobs: Vec<&ProviderJob> = jobs.iter().filter(|j| j.unit.index == 0).collect();
        assert_eq!(photo_jobs.len(), 1);
        assert_eq!(photo_jobs[0].kind, ProviderKind::Geocode);
        assert_eq!(photo_jobs[0].unit.epoch, 7);
        match &photo_jobs[0].input {
            ProviderInput::Geocode {
                latitude,
                longitude,
                language,
            } => {
                assert_eq!(*latitude, 48.8566);
                assert_eq!(*longitude, 2.3522);
                assert_eq!(language.as_deref(), Some("en"));
            }
            _ => panic!("expected geocode input"),
        }
        // Photo with GPS but no format %l: no geocode job.
        assert!(jobs.iter().all(|j| j.unit.index != 1));
        // Document: one description job (format %s), never a geocode.
        let doc_jobs: Vec<&ProviderJob> = jobs.iter().filter(|j| j.unit.index == 2).collect();
        assert_eq!(doc_jobs.len(), 1);
        assert_eq!(doc_jobs[0].kind, ProviderKind::Description);
        // Audio format uses neither placeholder: no jobs.
        assert!(jobs.iter().all(|j| j.unit.index != 3));
    }

    #[tokio::test]
    async fn disabled_providers_plan_nothing() {
        let cfg: RawConfig = toml::from_str("").unwrap(); // all disabled
        let dir = tmp_dir("off");
        let photo = file_at(
            &dir,
            "IMG_0001.jpg",
            AssetKind::Photo,
            Some(48.8),
            Some(2.3),
        );
        let analysis = Arc::new(Analysis {
            files: vec![photo],
            repos: Vec::new(),
            groups: Vec::new(),
            degraded_identity: false,
        });
        let mut formats = BTreeMap::new();
        formats.insert(
            AssetKind::Photo,
            FormatString::parse("Pictures/%l/%s/%f").unwrap(),
        );
        let jobs = plan_provider_jobs(&cfg, &analysis, &formats, 1).await;
        assert!(jobs.is_empty());
    }
}
