//! HTTP/1.1 REST handlers for the Service API plus the SSE push channel.
//!
//! The server is localhost-only, unauthenticated, and keeps session state
//! in memory. Every path in a request is required to be an absolute
//! normalized host path; relative paths, `~`, and environment-variable
//! references are rejected with 422.

use std::collections::BTreeMap;
use std::path::PathBuf;

use axum::extract::{Path as AxumPath, Query, State};
use axum::http::{HeaderName, HeaderValue, StatusCode};
use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use futures_util::stream::{self, Stream, StreamExt};
use serde::Deserialize;
use serde_json::{json, Value};

use homelinter_model::asset_kind::AssetKind;
use homelinter_model::config::{
    config_hash, effective_config_json, merge_and_validate_config, merge_and_write_config,
    platform_name,
};
use homelinter_model::error::{ApiError, ErrorCode};
use homelinter_model::format::FormatString;
use homelinter_model::path;
use homelinter_model::suggestion::{ExecutionReport, SessionState, Suggestion, SuggestionStatus};

use crate::events::RejectedId;
use crate::events::ServerEvent;
use crate::pipeline::{effective_home, run_execute, start_scan, AppHandle, ExecuteRequest};
use crate::session::BatchIdentity;

// ---------------------------------------------------------------------------
// Error handling
// ---------------------------------------------------------------------------

type ApiResult = Result<Response, (StatusCode, Json<Value>)>;

fn ok(value: Value) -> ApiResult {
    Ok(Json(value).into_response())
}

/// 202-accepted response for operations that run asynchronously.
fn accepted(value: Value) -> ApiResult {
    Ok((StatusCode::ACCEPTED, Json(value)).into_response())
}

fn api_err(e: ApiError) -> (StatusCode, Json<Value>) {
    let status =
        StatusCode::from_u16(e.body.code.status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    (status, Json(e.into()))
}

fn err(code: ErrorCode, message: impl Into<String>) -> (StatusCode, Json<Value>) {
    api_err(ApiError::new(code, message))
}

// ---------------------------------------------------------------------------
// Request shapes
// ---------------------------------------------------------------------------

#[derive(Debug, Default, Deserialize)]
struct ScanBody {
    #[serde(default)]
    root: Option<String>,
    #[serde(default)]
    hidden: bool,
    #[serde(default)]
    target_paths: BTreeMap<String, String>,
}

#[derive(Debug, Deserialize)]
struct IdsBody {
    #[serde(default)]
    ids: Option<Vec<String>>,
    #[serde(default)]
    dry_run: bool,
}

#[derive(Debug, Default, Deserialize)]
struct SelectionBody {
    #[serde(default)]
    ids: Option<Vec<String>>,
    #[serde(default)]
    filter: Option<Filter>,
}

#[derive(Debug, Default, Deserialize)]
struct Filter {
    #[serde(rename = "type", default)]
    suggestion_type: Option<String>,
    #[serde(default)]
    asset_kind: Option<String>,
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    selected: Option<bool>,
}

#[derive(Debug, Deserialize)]
struct ListQuery {
    #[serde(rename = "type", default)]
    suggestion_type: Option<String>,
    #[serde(default)]
    asset_kind: Option<String>,
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    selected: Option<String>,
    #[serde(default)]
    offset: Option<usize>,
    #[serde(default)]
    limit: Option<usize>,
}

#[derive(Debug, Deserialize)]
struct AuditQuery {
    #[serde(default)]
    since: Option<String>,
    #[serde(default)]
    limit: Option<usize>,
}

#[derive(Debug, Deserialize)]
struct ExportBody {
    #[serde(default)]
    ids: Option<Vec<String>>,
    #[serde(default)]
    path: Option<String>,
}

// ---------------------------------------------------------------------------
// Router
// ---------------------------------------------------------------------------

pub fn router(app: AppHandle) -> Router {
    Router::new()
        .route("/api/v1/health", get(health))
        .route("/api/v1/shutdown", post(shutdown))
        .route("/api/v1/cancel", post(cancel))
        .route("/api/v1/session/scan", post(session_scan))
        .route("/api/v1/session", get(session_get))
        .route("/api/v1/suggestions", get(suggestions_list))
        .route("/api/v1/suggestions/{id}", get(suggestion_get))
        .route("/api/v1/selection", get(selection_get).put(selection_put))
        .route("/api/v1/selection/add", post(selection_add))
        .route("/api/v1/selection/remove", post(selection_remove))
        .route("/api/v1/validate", post(validate))
        .route("/api/v1/execute", post(execute))
        .route("/api/v1/execution", get(execution_get))
        .route("/api/v1/export", post(export_script))
        .route("/api/v1/config", get(config_get).put(config_put))
        .route("/api/v1/config/validate", post(config_validate))
        .route("/api/v1/cache", delete(cache_clear))
        .route("/api/v1/report/summary", get(report_summary))
        .route("/api/v1/audit", get(audit_get))
        .route("/api/v1/events", get(events_sse))
        .with_state(app)
}

// ---------------------------------------------------------------------------
// Lifecycle
// ---------------------------------------------------------------------------

async fn health(State(app): State<AppHandle>) -> Json<Value> {
    let sess = app.session.lock().await;
    let mut v = json!({
        "state": sess.state,
        "scan_root": sess.scan_root,
        "platform": sess.platform,
    });
    if sess.state == SessionState::Failed {
        if let Some(f) = &sess.failure {
            v["failure"] = serde_json::to_value(f).unwrap_or(Value::Null);
        }
    }
    Json(v)
}

async fn shutdown(State(app): State<AppHandle>) -> StatusCode {
    let _ = app.shutdown_tx.send(true);
    StatusCode::NO_CONTENT
}

async fn cancel(State(app): State<AppHandle>) -> ApiResult {
    let state = {
        let sess = app.session.lock().await;
        sess.state
    };
    match state {
        SessionState::Idle => {
            // No-op: nothing in flight to cancel.
            ok(json!({ "state": state }))
        }
        SessionState::Scanning | SessionState::Analyzing | SessionState::Executing => {
            // Signal the in-flight runner; the runner applies the terminal
            // state (Failed for scans, Done with a partial report for
            // executions) when it observes the generation change.
            app.cancel.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            ok(json!({ "state": state }))
        }
        SessionState::Ready | SessionState::Done | SessionState::Failed => Err(err(
            ErrorCode::Conflict,
            "nothing is in progress; nothing to cancel",
        )),
    }
}

// ---------------------------------------------------------------------------
// Session & scan
// ---------------------------------------------------------------------------

async fn session_scan(State(app): State<AppHandle>, Json(body): Json<ScanBody>) -> ApiResult {
    // Validate per-scan target_paths first (unknown kind or invalid
    // template -> 422 before any state change).
    for (kind, template) in &body.target_paths {
        AssetKind::from_name(kind)
            .ok_or_else(|| api_err(ApiError::validation(format!("unknown asset kind '{kind}'"))))?;
        FormatString::parse(template).map_err(|e| {
            api_err(ApiError::validation(format!(
                "invalid target_paths.{kind}: {e}"
            )))
        })?;
    }

    let home = effective_home(&app);
    let root: PathBuf = match body.root {
        Some(r) => {
            let p = PathBuf::from(&r);
            path::require_absolute_normalized(&p).map_err(|_| {
                api_err(ApiError::validation(format!(
                    "root must be an absolute normalized path: {r}"
                )))
            })?;
            p
        }
        None => home.clone(),
    };

    {
        let mut sess = app.session.lock().await;
        if sess.state.in_progress() {
            return Err(err(
                ErrorCode::Conflict,
                "a scan or execution is already in progress",
            ));
        }
        sess.state = SessionState::Scanning;
        sess.scan_root = Some(root.clone());
        sess.hidden = body.hidden;
        sess.scan_target_paths = body.target_paths.clone();
        // The new scan replaces the session: previous suggestions,
        // selection, and the execution report are discarded.
        sess.suggestions.clear();
        sess.report = None;
        sess.report_batch = None;
        sess.summary = None;
        sess.failure = None;
        sess.degraded_identity = false;
        sess.warnings.clear();
    }
    app.events
        .publish(ServerEvent::State(SessionState::Scanning));

    tokio::spawn(start_scan(app.clone(), root, body.hidden));
    accepted(json!({ "state": "scanning" }))
}

async fn session_get(State(app): State<AppHandle>) -> ApiResult {
    let sess = app.session.lock().await;
    let suggestions_total = sess.suggestions.len();
    let selection_count = sess.suggestions.iter().filter(|s| s.selected).count();
    let mut v = json!({
        "state": sess.state,
        "scan_root": sess.scan_root,
        "platform": sess.platform,
        "config_hash": sess.config_hash,
        "suggestions_total": suggestions_total,
        "selection_count": selection_count,
    });
    if let Some(f) = &sess.failure {
        v["failure"] = serde_json::to_value(f).unwrap_or(Value::Null);
    }
    drop(sess);
    ok(v)
}

// ---------------------------------------------------------------------------
// Suggestions & selection
// ---------------------------------------------------------------------------

fn status_str(s: &Suggestion) -> &'static str {
    match s.status {
        SuggestionStatus::Pending => "pending",
        SuggestionStatus::Rejected => "rejected",
        SuggestionStatus::Executed => "executed",
        SuggestionStatus::Failed => "failed",
        SuggestionStatus::Skipped => "skipped",
    }
}

fn matches_filter(s: &Suggestion, f: &Filter) -> bool {
    if let Some(t) = &f.suggestion_type {
        let actual = match s.suggestion_type {
            homelinter_model::suggestion::SuggestionType::Move => "move",
            homelinter_model::suggestion::SuggestionType::Delete => "delete",
        };
        if actual != t {
            return false;
        }
    }
    if let Some(k) = &f.asset_kind {
        if s.asset_kind.name() != k {
            return false;
        }
    }
    if let Some(st) = &f.status {
        if status_str(s) != st {
            return false;
        }
    }
    if let Some(sel) = f.selected {
        if s.selected != sel {
            return false;
        }
    }
    true
}

async fn suggestions_list(State(app): State<AppHandle>, Query(q): Query<ListQuery>) -> ApiResult {
    let filter = Filter {
        suggestion_type: q.suggestion_type,
        asset_kind: q.asset_kind,
        status: q.status,
        selected: q.selected.and_then(|s| s.parse().ok()),
    };
    let offset = q.offset.unwrap_or(0);
    let limit = q.limit.unwrap_or(usize::MAX);
    let sess = app.session.lock().await;
    let filtered: Vec<&Suggestion> = sess
        .suggestions
        .iter()
        .filter(|s| matches_filter(s, &filter))
        .collect();
    let total = filtered.len();
    let items: Vec<Value> = filtered
        .iter()
        .skip(offset)
        .take(limit)
        .filter_map(|s| serde_json::to_value(*s).ok())
        .collect();
    drop(sess);
    ok(json!({ "items": items, "total": total }))
}

async fn suggestion_get(State(app): State<AppHandle>, AxumPath(id): AxumPath<String>) -> ApiResult {
    let sess = app.session.lock().await;
    match sess.suggestion_by_id(&id) {
        Some(s) => serde_json::to_value(s)
            .map(|v| ok(v))
            .unwrap_or_else(|_| Err(err(ErrorCode::Internal, "cannot serialize suggestion"))),
        None => Err(err(
            ErrorCode::NotFound,
            format!("no suggestion with id {id}"),
        )),
    }
}

fn resolve_ids(sess: &crate::session::Session, ids: &[String]) -> Result<Vec<String>, ApiError> {
    let mut out = Vec::new();
    for id in ids {
        if sess.suggestion_by_id(id).is_none() {
            return Err(ApiError::validation(format!(
                "unknown suggestion id '{id}'"
            )));
        }
        out.push(id.clone());
    }
    Ok(out)
}

fn publish_validated(app: &AppHandle, rejected: Vec<(String, String)>) {
    let rejected: Vec<RejectedId> = rejected
        .into_iter()
        .map(|(id, reason)| RejectedId { id, reason })
        .collect();
    app.events.publish(ServerEvent::BatchValidated {
        valid: rejected.is_empty(),
        rejected,
    });
}

fn session_selection_json(sess: &crate::session::Session) -> Value {
    json!({ "ids": sess.selection_ids() })
}

async fn selection_get(State(app): State<AppHandle>) -> ApiResult {
    let sess = app.session.lock().await;
    ok(session_selection_json(&sess))
}

async fn selection_put(State(app): State<AppHandle>, Json(body): Json<SelectionBody>) -> ApiResult {
    let ids = body.ids.unwrap_or_default();
    let mut sess = app.session.lock().await;
    let ids = resolve_ids(&sess, &ids).map_err(api_err)?;
    sess.set_selection(&ids);
    let sel = sess.selection_ids();
    let rejected = sess.validate_ids(&sel);
    let body = session_selection_json(&sess);
    drop(sess);
    publish_validated(&app, rejected);
    ok(body)
}

async fn selection_add(State(app): State<AppHandle>, Json(body): Json<SelectionBody>) -> ApiResult {
    let mut sess = app.session.lock().await;
    match body.ids {
        Some(ids) => {
            let ids = resolve_ids(&sess, &ids).map_err(api_err)?;
            sess.add_to_selection(&ids);
        }
        None => {
            let f = body.filter.unwrap_or_default();
            for s in sess.suggestions.iter_mut() {
                if matches_filter(s, &f) {
                    s.selected = true;
                }
            }
        }
    }
    let sel = sess.selection_ids();
    let rejected = sess.validate_ids(&sel);
    let body = session_selection_json(&sess);
    drop(sess);
    publish_validated(&app, rejected);
    ok(body)
}

async fn selection_remove(
    State(app): State<AppHandle>,
    Json(body): Json<SelectionBody>,
) -> ApiResult {
    let mut sess = app.session.lock().await;
    match body.ids {
        Some(ids) => {
            let ids = resolve_ids(&sess, &ids).map_err(api_err)?;
            sess.remove_from_selection(&ids);
        }
        None => {
            let f = body.filter.unwrap_or_default();
            for s in sess.suggestions.iter_mut() {
                if matches_filter(s, &f) {
                    s.selected = false;
                }
            }
        }
    }
    let sel = sess.selection_ids();
    let rejected = sess.validate_ids(&sel);
    let body = session_selection_json(&sess);
    drop(sess);
    publish_validated(&app, rejected);
    ok(body)
}

// ---------------------------------------------------------------------------
// Validation, execution & export
// ---------------------------------------------------------------------------

async fn validate(State(app): State<AppHandle>, Json(body): Json<IdsBody>) -> ApiResult {
    let mut sess = app.session.lock().await;
    let ids = match body.ids {
        Some(ids) => resolve_ids(&sess, &ids).map_err(api_err)?,
        None => sess.selection_ids(),
    };
    let mut rejected = sess.validate_ids(&ids);
    // Surface pre-existing rejections in the reported set as well.
    for id in &ids {
        let existing = sess.suggestion_by_id(id);
        let Some(s) = existing else { continue };
        if s.status == SuggestionStatus::Rejected {
            if let Some(reason) = &s.rejection_reason {
                if !rejected.iter().any(|(i, _)| i == id) {
                    rejected.push((id.clone(), reason.clone()));
                }
            }
        }
    }
    drop(sess);
    let rejected: Vec<Value> = rejected
        .into_iter()
        .map(|(id, reason)| json!({ "id": id, "reason": reason }))
        .collect();
    ok(json!({ "valid": rejected.is_empty(), "rejected": rejected }))
}

async fn execute(State(app): State<AppHandle>, Json(body): Json<IdsBody>) -> ApiResult {
    let request_ids = {
        let sess = app.session.lock().await;
        if !matches!(sess.state, SessionState::Ready | SessionState::Done) {
            return Err(err(
                ErrorCode::Conflict,
                "a new execution is only accepted from ready or done",
            ));
        }
        match &body.ids {
            Some(ids) => resolve_ids(&sess, ids).map_err(api_err)?,
            None => {
                let sel = sess.selection_ids();
                if sel.is_empty() {
                    return Err(err(
                        ErrorCode::BadRequest,
                        "no ids given and the selection is empty",
                    ));
                }
                sel
            }
        }
    };

    if body.dry_run {
        // Dry runs always run fresh (they consume no idempotency) but are
        // still validated first.
        let mut sess = app.session.lock().await;
        let executable = sess.executable_ids(&request_ids);
        let rejected = sess.validate_ids(&executable);
        drop(sess);
        if !rejected.is_empty() {
            publish_validated(&app, rejected);
        }
    } else if {
        // Idempotent re-run: exact previous batch -> stored report.
        let sess = app.session.lock().await;
        sess.executable_ids(&request_ids).is_empty()
            && sess.report_batch.as_ref() == Some(&BatchIdentity::of(&request_ids))
            && sess.report.is_some()
    } {
        let (report,) = {
            let sess = app.session.lock().await;
            (sess.report.clone().expect("checked above"),)
        };
        app.events
            .publish(ServerEvent::ExecutionComplete { report });
        return accepted(json!({ "state": "done" }));
    } else {
        let mut sess = app.session.lock().await;
        let executable = sess.executable_ids(&request_ids);
        let rejected = sess.validate_ids(&executable);
        let executable = sess.executable_ids(&request_ids);
        drop(sess);
        if !rejected.is_empty() {
            publish_validated(&app, rejected);
        }
        if executable.is_empty() {
            // Everything resolved to already-executed or rejected: normal
            // acceptance with an empty report.
            let root = {
                let sess = app.session.lock().await;
                sess.root()
                    .map(|r| r.to_path_buf())
                    .unwrap_or_else(|| sess.home.clone())
            };
            let report = ExecutionReport::empty(&root);
            {
                let mut sess = app.session.lock().await;
                sess.state = SessionState::Done;
                sess.report = Some(report.clone());
                sess.report_batch = Some(BatchIdentity::of(&request_ids));
            }
            app.events
                .publish(ServerEvent::ExecutionComplete { report });
            app.events.publish(ServerEvent::State(SessionState::Done));
            return accepted(json!({ "state": "done" }));
        }
    }

    {
        let mut sess = app.session.lock().await;
        sess.state = SessionState::Executing;
    }
    app.events
        .publish(ServerEvent::State(SessionState::Executing));
    let req = ExecuteRequest {
        ids: request_ids,
        dry_run: body.dry_run,
    };
    tokio::spawn(run_execute(app.clone(), req));
    accepted(json!({ "state": "executing" }))
}

async fn execution_get(State(app): State<AppHandle>) -> ApiResult {
    let sess = app.session.lock().await;
    match &sess.report {
        Some(r) => serde_json::to_value(r)
            .map(|v| ok(v))
            .unwrap_or_else(|_| Err(err(ErrorCode::Internal, "cannot serialize report"))),
        None => Err(err(ErrorCode::NotFound, "no execution report yet")),
    }
}

async fn export_script(State(app): State<AppHandle>, Json(body): Json<ExportBody>) -> Response {
    let platform = platform_name();
    let executable: Vec<Suggestion> = {
        let sess = app.session.lock().await;
        if !matches!(sess.state, SessionState::Ready | SessionState::Done) {
            return err(
                ErrorCode::Conflict,
                "export requires a ready or done session",
            )
            .into_response();
        }
        let request_ids = match &body.ids {
            Some(ids) => match resolve_ids(&sess, ids) {
                Ok(v) => v,
                Err(e) => return api_err(e).into_response(),
            },
            None => {
                let sel = sess.selection_ids();
                if sel.is_empty() {
                    return err(
                        ErrorCode::BadRequest,
                        "no ids given and the selection is empty",
                    )
                    .into_response();
                }
                sel
            }
        };
        request_ids
            .iter()
            .filter_map(|id| sess.suggestion_by_id(id).cloned())
            .filter(|s| s.status == SuggestionStatus::Pending)
            .collect()
    };

    let script = match homelinter_execute::export_script(&executable, platform) {
        Ok(s) => s,
        Err(e) => return api_err(ApiError::validation(e)).into_response(),
    };
    let count = executable.len();

    if let Some(dest) = body.path {
        let dest = PathBuf::from(&dest);
        if let Err(e) = path::require_absolute_normalized(&dest) {
            return api_err(e).into_response();
        }
        if let Err(e) = write_script_atomically(&dest, &script) {
            return api_err(e).into_response();
        }
        return Json(json!({
            "path": dest.to_string_lossy(),
            "platform": platform,
            "count": count,
        }))
        .into_response();
    }

    // Inline script: platform and count in response headers.
    let mut response = Response::new(axum::body::Body::from(script));
    response.headers_mut().insert(
        HeaderName::from_static("x-homelinter-platform"),
        HeaderValue::from_str(platform).unwrap_or(HeaderValue::from_static("")),
    );
    response.headers_mut().insert(
        HeaderName::from_static("x-homelinter-count"),
        HeaderValue::from_str(&count.to_string()).unwrap_or(HeaderValue::from_static("0")),
    );
    response
}

fn write_script_atomically(dest: &std::path::Path, script: &str) -> Result<(), ApiError> {
    let parent = dest
        .parent()
        .ok_or_else(|| ApiError::validation("export path has no parent directory"))?;
    std::fs::create_dir_all(parent)
        .map_err(|e| ApiError::validation(format!("cannot create {}: {e}", parent.display())))?;
    let tmp = dest.with_extension("export.tmp");
    std::fs::write(&tmp, script)
        .map_err(|e| ApiError::validation(format!("cannot write {}: {e}", tmp.display())))?;
    std::fs::rename(&tmp, dest)
        .map_err(|e| ApiError::validation(format!("cannot write {}: {e}", dest.display())))?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

async fn config_get(State(app): State<AppHandle>) -> ApiResult {
    let sess = app.session.lock().await;
    let cfg = effective_config_json(&sess.config, sess.config_path.as_deref());
    drop(sess);
    ok(cfg)
}

async fn config_put(State(app): State<AppHandle>, Json(body): Json<Value>) -> ApiResult {
    if !body.is_object() {
        return Err(err(
            ErrorCode::ValidationFailed,
            "configuration must be a table",
        ));
    }
    let cli = app.cli_config.clone();
    let loaded = tokio::task::spawn_blocking(move || merge_and_write_config(&body, cli.as_deref()))
        .await
        .map_err(|e| err(ErrorCode::Internal, format!("config task panicked: {e}")))?
        .map_err(api_err)?;
    let cfg = {
        let mut sess = app.session.lock().await;
        sess.config = loaded.raw;
        sess.config_path = loaded.config_path;
        sess.config_hash = config_hash(&sess.config);
        effective_config_json(&sess.config, sess.config_path.as_deref())
    };
    app.events.publish(ServerEvent::ConfigUpdated {
        config: cfg.clone(),
    });
    ok(cfg)
}

async fn config_validate(State(app): State<AppHandle>, Json(body): Json<Value>) -> ApiResult {
    if !body.is_object() {
        return Err(err(
            ErrorCode::ValidationFailed,
            "configuration must be a table",
        ));
    }
    let cli = app.cli_config.clone();
    let raw = tokio::task::spawn_blocking(move || merge_and_validate_config(&body, cli.as_deref()))
        .await
        .map_err(|e| err(ErrorCode::Internal, format!("config task panicked: {e}")))?
        .map_err(api_err)?;
    ok(json!({ "valid": true, "config": effective_config_json(&raw, None) }))
}

// ---------------------------------------------------------------------------
// Cache, audit & reporting
// ---------------------------------------------------------------------------

async fn cache_clear(State(app): State<AppHandle>) -> StatusCode {
    let cache = app.cache.lock().await;
    let _ = cache.clear();
    drop(cache);
    app.events.publish(ServerEvent::CacheCleared);
    StatusCode::NO_CONTENT
}

async fn report_summary(State(app): State<AppHandle>) -> ApiResult {
    let sess = app.session.lock().await;
    let s = sess
        .summary
        .clone()
        .unwrap_or_else(homelinter_model::suggestion::ScanSummary::default);
    serde_json::to_value(s)
        .map(|v| ok(v))
        .unwrap_or_else(|_| Err(err(ErrorCode::Internal, "cannot serialize summary")))
}

async fn audit_get(State(app): State<AppHandle>, Query(q): Query<AuditQuery>) -> ApiResult {
    let limit = q.limit.unwrap_or(100).min(1000);
    let entries: Vec<Value> = app
        .audit
        .read_newest_first(q.since.as_deref(), limit)
        .into_iter()
        .filter_map(|e| serde_json::to_value(e).ok())
        .collect();
    ok(Value::Array(entries))
}

// ---------------------------------------------------------------------------
// Events (SSE)
// ---------------------------------------------------------------------------

/// On connect: current session state and, when a batch exists, the current
/// suggestion.batch snapshot; afterwards live events stream until the
/// client disconnects.
async fn events_sse(
    State(app): State<AppHandle>,
) -> Sse<impl Stream<Item = Result<SseEvent, std::convert::Infallible>> + Send> {
    let initial: Vec<Result<SseEvent, std::convert::Infallible>> = {
        let sess = app.session.lock().await;
        let mut v = vec![Ok(sse_event(ServerEvent::State(sess.state)))];
        if !sess.suggestions.is_empty() {
            let items = sess.suggestions.clone();
            let total = items.len();
            v.push(Ok(sse_event(ServerEvent::SuggestionBatch { items, total })));
        }
        v
    };

    let rx = app.events.subscribe();
    let live = stream::unfold(rx, |mut rx| async move {
        loop {
            match rx.recv().await {
                Ok(ev) => return Some((Ok(sse_event(ev)), rx)),
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Closed) => return None,
            }
        }
    });

    let merged = stream::iter(initial).chain(live);
    Sse::new(merged).keep_alive(KeepAlive::default())
}

fn sse_event(ev: ServerEvent) -> SseEvent {
    SseEvent::default()
        .event(ev.name())
        .data(serde_json::to_string(&ev.data()).unwrap_or_else(|_| "{}".into()))
}
