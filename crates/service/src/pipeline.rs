//! Phase runners: scan and execute pipelines that drive the sync crates
//! (scan, analyze, suggest, execute) from async handlers, update the shared
//! session, and publish SSE events. Cancellation is cooperative between
//! phases (scan) and between top-level suggestions (execute), exactly as
//! "Session Model > Cancellation" describes.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;

use tokio::sync::Mutex;

use homelinter_analyze::{
    analyze, AnalyzeOptions, BuiltinAudioExtractor, BuiltinExifExtractor,
    DocumentContainerExtractor, ExifToolExtractor, ExtractionContext, MetadataExtractor,
    PdfInfoExtractor,
};
use homelinter_model::config::{config_hash, RawConfig};
use homelinter_model::suggestion::{
    ExecutionReport, ExecutionStatus, OperationKind, OperationStatus, SessionState, Suggestion,
    SuggestionStatus,
};
use homelinter_scan::{DiscoveryOptions, ToolSet};
use homelinter_suggest::{generate_suggestions, GenerateInput};

use crate::audit::AuditEntry;
use crate::events::{EventBus, RejectedId, ServerEvent};
use crate::session::{BatchIdentity, Session};

/// Shared service state: one session, one cache, one audit log, one cancel
/// generation counter. `cancel` increments on `POST /cancel`; a runner
/// compares its captured counter against the live one between units.
pub struct App {
    pub session: Mutex<Session>,
    pub events: EventBus,
    pub cache: Mutex<crate::cache::Cache>,
    pub audit: crate::audit::AuditLog,
    pub cancel: AtomicU64,
    pub cli_config: Option<PathBuf>,
    /// Test/embedding override for the home used by symlink indexing and
    /// relative destinations; None means the platform home.
    pub home_override: Option<PathBuf>,
    /// Directory containing the built SPA (index.html + assets). None
    /// disables static file serving (API-only mode).
    pub web_dir: Option<PathBuf>,
    /// Scan dialog defaults for the SPA (see `ServiceOptions::default_hidden`
    /// and `ServiceOptions::default_target_paths`). Exposed as
    /// `scan_defaults` in `GET /session`; never applied server-side.
    pub default_hidden: bool,
    pub default_target_paths: BTreeMap<String, String>,
    /// Watch channel: a `POST /api/v1/shutdown` sends `true` here and the
    /// HTTP server shuts down gracefully.
    pub shutdown_tx: tokio::sync::watch::Sender<bool>,
    /// Background provider (geocoder/description) request queue drained by
    /// the rate-limited worker spawned in `serve`.
    pub provider_queue: Arc<crate::providers::JobQueue>,
    /// Scan epoch for provider results: incremented per scan start; jobs
    /// carrying an older epoch are dropped by the worker.
    pub provider_epoch: AtomicU64,
}

pub type AppHandle = Arc<App>;

/// The effective home directory for this service instance.
pub fn effective_home(app: &App) -> PathBuf {
    app.home_override
        .clone()
        .or_else(homelinter_model::path::home_dir)
        .unwrap_or_else(|| PathBuf::from("/"))
}

pub fn is_cancelled(app: &App, token: u64) -> bool {
    app.cancel.load(Ordering::SeqCst) != token
}

/// Assemble the analysis extractor set for one scan.
fn build_extractors(
    _cfg: &homelinter_model::config::RawConfig,
) -> Vec<Box<dyn MetadataExtractor + Send + Sync>> {
    let toolset = ToolSet::detect();
    let mut extractors: Vec<Box<dyn MetadataExtractor + Send + Sync>> = Vec::new();
    if toolset.has("exiftool") {
        extractors.push(Box::new(ExifToolExtractor {}));
    }
    if toolset.has("pdfinfo") {
        extractors.push(Box::new(PdfInfoExtractor {}));
    }
    extractors.push(Box::new(DocumentContainerExtractor {}));
    extractors.push(Box::new(BuiltinExifExtractor {}));
    extractors.push(Box::new(BuiltinAudioExtractor {}));
    extractors
}

fn phase_progress(phase: &'static str, scanned: u64, start: Instant) -> ServerEvent {
    ServerEvent::ScanProgress {
        phase,
        scanned,
        elapsed_ms: start.elapsed().as_millis() as u64,
    }
}

// ---------------------------------------------------------------------------
// Scan pipeline
// ---------------------------------------------------------------------------

/// Run the scan pipeline in the background. The caller has already checked
/// the state machine under the session lock, set the session to
/// `Scanning`, and snapshotted the configuration (`cfg`) and per-scan
/// target-path overrides (`target_paths`) at that moment: every phase runs
/// against that one snapshot, so a config save during a scan never changes
/// the running work and the batch reflects a single committed configuration.
pub async fn start_scan(
    app: AppHandle,
    root: PathBuf,
    hidden: bool,
    cfg: RawConfig,
    target_paths: BTreeMap<String, String>,
) {
    let home = effective_home(&app);
    let cancel_token = app.cancel.load(Ordering::SeqCst);
    // Provider results are gated by the epoch of the scan that enqueued
    // them; superseded scans' outstanding jobs are dropped without effect.
    // The epoch handed to the jobs is the post-increment value, so the worker's
    // comparison against the live counter treats them as current until the next
    // scan bumps the counter.
    let provider_epoch = app.provider_epoch.fetch_add(1, Ordering::SeqCst) + 1;
    let start = Instant::now();

    let (scan_opts, threshold) = (
        DiscoveryOptions {
            root: root.clone(),
            hidden,
            hidden_definition: cfg.hidden_definition(),
            exclude_patterns: cfg.scan.exclude_paths.clone(),
        },
        cfg.duplication.large_file_threshold,
    );
    app.events.publish(phase_progress("discovery", 0, start));

    // Phase 1 + symlink pass: discovery. Runs on the blocking pool.
    let home2 = home.clone();
    let scan_result =
        tokio::task::spawn_blocking(move || homelinter_scan::scan_root(&root, &home2, &scan_opts))
            .await;
    let scan = match scan_result {
        Ok(Ok(s)) => s,
        Ok(Err(e)) => return fail(app.clone(), format!("discovery failed: {e}")).await,
        Err(e) => return fail(app.clone(), format!("scan task panicked: {e}")).await,
    };
    if is_cancelled(&app, cancel_token) {
        return fail(app, "scan cancelled".into()).await;
    }
    app.events
        .publish(phase_progress("discovery", scan.primary_units, start));
    app.events.publish(phase_progress(
        "symlinks",
        scan.symlinks.entries.len() as u64,
        start,
    ));

    // Phase 2: analysis.
    let files = scan.files.clone();
    let repos = scan.repos.clone();
    let analyze_opts = AnalyzeOptions {
        large_file_threshold: threshold,
        toolset: ToolSet::detect(),
        // The in-process kamadak-exif reader handles supported containers.
        extractors: build_extractors(&cfg),
        ctx: ExtractionContext {
            tool_timeout: std::time::Duration::from_secs(cfg.tool_timeout()),
            tool_stdout_max_bytes: cfg.tool_stdout_max_bytes(),
            config: Some(cfg.clone()),
        },
    };
    let analysis =
        tokio::task::spawn_blocking(move || analyze(&files, &repos, &analyze_opts)).await;
    let analysis = match analysis {
        Ok(a) => Arc::new(a),
        Err(e) => return fail(app.clone(), format!("analysis task panicked: {e}")).await,
    };
    if is_cancelled(&app, cancel_token) {
        return fail(app, "scan cancelled".into()).await;
    }
    app.events.publish(phase_progress(
        "metadata",
        analysis.files.len() as u64,
        start,
    ));

    // Phase 3: suggestion generation.
    let formats = match cfg.resolved_target_paths(&target_paths) {
        Ok(f) => f,
        Err(e) => {
            return fail(app.clone(), format!("invalid target_paths: {e}")).await;
        }
    };
    let analysis2 = analysis.clone();
    let symlinks = scan.symlinks.clone();
    let home3 = home.clone();
    let formats3 = formats.clone();
    let generated = tokio::task::spawn_blocking(move || {
        let input = GenerateInput {
            analysis: &analysis2,
            symlinks: &symlinks,
            formats: &formats3,
            home: &home3,
        };
        generate_suggestions(&input)
            .map(|o| o.suggestions)
            .map_err(|e| e.0)
    })
    .await;
    let mut suggestions = match generated {
        Ok(Ok(s)) => s,
        Ok(Err(e)) => return fail(app.clone(), e).await,
        Err(e) => return fail(app.clone(), format!("suggestion task panicked: {e}")).await,
    };

    // Full-batch validation before the batch is presented.
    let rejected: Vec<RejectedId> = {
        let mut pending = suggestions.clone();
        let ids = homelinter_suggest::validate_batch(&mut pending);
        ids.into_iter()
            .filter_map(|id| pending.iter().find(|s| s.id == id))
            .filter_map(|s| {
                s.rejection_reason.as_ref().map(|reason| RejectedId {
                    id: s.id.clone(),
                    reason: reason.clone(),
                })
            })
            .collect()
    };
    for s in suggestions.iter_mut() {
        if rejected.iter().any(|r| r.id == s.id) {
            s.status = SuggestionStatus::Rejected;
        }
    }
    if is_cancelled(&app, cancel_token) {
        return fail(app, "scan cancelled".into()).await;
    }
    app.events.publish(phase_progress(
        "suggestions",
        suggestions.len() as u64,
        start,
    ));

    let total_files = analysis.files.len() as u64;
    let total_symlinks = scan.symlinks.entries.len() as u64;

    // Plan provider work before publishing: suggestions whose unit has
    // outstanding provider requests are published immediately but marked
    // `metadata_pending`, so the user sees the full scan result yet can
    // never execute a move built on incomplete metadata.
    let provider_jobs =
        crate::providers::plan_provider_jobs(&cfg, &analysis, &formats, provider_epoch).await;
    let mut pending: std::collections::BTreeMap<usize, u32> = std::collections::BTreeMap::new();
    for job in &provider_jobs {
        *pending.entry(job.unit.index).or_default() += 1;
    }

    {
        let mut sess = app.session.lock().await;
        sess.state = SessionState::Ready;
        sess.batch_config_hash = Some(config_hash(&cfg));
        sess.degraded_identity = analysis.degraded_identity;
        sess.warnings = analysis
            .groups
            .iter()
            .filter_map(|g| g.metadata_warning.clone())
            .collect();
        // Provider context: lets the background worker refresh this scan's
        // provider-dependent move suggestions when geocoder or description
        // results arrive, without re-running the scan.
        sess.provider_ctx = Some(crate::providers::ProviderScanCtx {
            epoch: provider_epoch,
            analysis: analysis.clone(),
            symlinks: scan.symlinks.clone(),
            formats: formats.clone(),
            home: home.clone(),
            meta: Default::default(),
            pending: pending.clone(),
        });
        if !pending.is_empty() {
            // Mark suggestions whose unit still has outstanding provider
            // jobs as metadata-pending (non-executable until resolved).
            let unit_of: std::collections::BTreeMap<String, usize> = analysis
                .files
                .iter()
                .enumerate()
                .map(|(i, f)| (Suggestion::id_for_target(&f.path), i))
                .collect();
            for s in suggestions.iter_mut() {
                if unit_of.get(&s.id).is_some_and(|i| pending.contains_key(i)) {
                    s.metadata_pending = true;
                }
            }
        }
        sess.suggestions = suggestions;
        let mut summary = sess.summary_of_batch();
        summary.total_files_scanned = total_files;
        summary.total_symlinks_indexed = total_symlinks;
        sess.summary = Some(summary.clone());
        sess.report = None;
        sess.report_batch = None;
        sess.failure = None;
        let items = sess.suggestions.clone();
        let total = items.len();
        drop(sess);

        app.events
            .publish(ServerEvent::SuggestionBatch { items, total });
        app.events.publish(ServerEvent::BatchValidated {
            valid: rejected.is_empty(),
            rejected,
        });
        app.events.publish(ServerEvent::ScanComplete { summary });
        app.events.publish(ServerEvent::State(SessionState::Ready));
    }

    // Enqueue provider work (reverse geocoding, descriptions). The worker
    // fetches these in the background with per-provider rate limiting and
    // clears each suggestion's pending flag as its requests resolve; the
    // scan itself was never blocked on them.
    for job in provider_jobs {
        app.provider_queue.enqueue(job);
    }
}

/// Transition the session to `failed` with the given reason and publish the
/// state event.
pub async fn fail(app: AppHandle, reason: String) {
    let mut sess = app.session.lock().await;
    sess.state = SessionState::Failed;
    sess.failure = Some(homelinter_model::suggestion::SessionFailure {
        reason,
        cancelled: None,
    });
    drop(sess);
    app.events.publish(ServerEvent::State(SessionState::Failed));
}

// ---------------------------------------------------------------------------
// Execute pipeline
// ---------------------------------------------------------------------------

pub struct ExecuteRequest {
    /// Requested suggestion ids (batch identity, in request order).
    pub ids: Vec<String>,
}

/// Execute one top-level suggestion on the blocking pool. Returns the
/// mutated suggestion (its status reflects the outcome) and its report
/// entry. Per-operation events are published as they are known.
async fn run_one(
    app: &App,
    s: Suggestion,
    root: &std::path::Path,
) -> Result<(Suggestion, homelinter_model::suggestion::ReportEntry), String> {
    let bus = app.events.clone();
    let id = s.id.clone();
    let root_owned = root.to_path_buf();
    tokio::task::spawn_blocking(move || {
        let mut items = vec![s];
        let report = homelinter_execute::execute_batch(&mut items, &root_owned, false);
        let s = items.pop().expect("one suggestion in");
        let entry = report.entries.into_iter().next().unwrap_or_else(|| {
            homelinter_model::suggestion::ReportEntry {
                suggestion_id: id.clone(),
                result: ExecutionStatus::Skipped,
                operations: Vec::new(),
            }
        });
        for op in &entry.operations {
            let operation = match op.operation {
                OperationKind::Move => "move",
                OperationKind::Delete => "delete",
                OperationKind::RepairSymlink => "repair_symlink",
            };
            let status = match op.status {
                OperationStatus::Completed => "completed",
                OperationStatus::Failed => "failed",
                OperationStatus::Skipped => "skipped",
            };
            bus.publish(ServerEvent::ExecutionOp {
                suggestion_id: id.clone(),
                operation: operation.into(),
                status: status.into(),
                detail: serde_json::json!({
                    "source": op.source,
                    "destination": op.destination,
                    "detail": op.detail,
                }),
            });
        }
        (s, entry)
    })
    .await
    .map_err(|e| format!("execution task panicked: {e}"))
}

/// Run the execute pipeline for a batch of pending suggestion ids. The
/// caller has set the session to `Executing` under the lock beforehand.
pub async fn run_execute(app: AppHandle, req: ExecuteRequest) {
    let cancel_token = app.cancel.load(Ordering::SeqCst);
    let root = {
        let sess = app.session.lock().await;
        sess.root().unwrap_or(&sess.home).to_path_buf()
    };
    let mut report = ExecutionReport::empty(&root);

    // Deterministic execution order: batch ids sorted by suggestion sort key.
    // Only Pending suggestions run; already-executed or rejected ids are
    // covered by the stored report and never re-queued.
    let mut batch: Vec<(usize, String)> = Vec::new();
    {
        let sess = app.session.lock().await;
        for id in &req.ids {
            if let Some((i, s)) = sess
                .suggestions
                .iter()
                .enumerate()
                .find(|(_, s)| &s.id == id)
            {
                if s.status == SuggestionStatus::Pending && !s.metadata_pending {
                    batch.push((i, s.sort_key()));
                }
            }
        }
    }
    batch.sort_by(|a, b| a.1.cmp(&b.1));

    let mut pos = 0usize;
    while pos < batch.len() {
        let (i, _) = batch[pos];
        if is_cancelled(&app, cancel_token) {
            // Finish nothing more: mark every remaining suggestion skipped
            // with the cancellation reason and include them in the report.
            let mut sess = app.session.lock().await;
            for (j, _) in batch.iter().skip(pos) {
                sess.suggestions[*j].status = SuggestionStatus::Skipped;
                report
                    .entries
                    .push(homelinter_model::suggestion::ReportEntry {
                        suggestion_id: sess.suggestions[*j].id.clone(),
                        result: ExecutionStatus::Skipped,
                        operations: Vec::new(),
                    });
            }
            break;
        }
        let s = {
            let sess = app.session.lock().await;
            sess.suggestions[i].clone()
        };
        let (s, entry) = match run_one(&app, s, &root).await {
            Ok(v) => v,
            Err(reason) => {
                let mut sess = app.session.lock().await;
                sess.suggestions[i].status = SuggestionStatus::Failed;
                sess.state = SessionState::Failed;
                sess.failure = Some(homelinter_model::suggestion::SessionFailure {
                    reason,
                    cancelled: None,
                });
                drop(sess);
                app.events.publish(ServerEvent::State(SessionState::Failed));
                return;
            }
        };

        // Report-level partial flag: parent completed, a dependent failed.
        let parent_ok = entry
            .operations
            .first()
            .is_some_and(|o| o.status == OperationStatus::Completed);
        let dependent_failed = entry
            .operations
            .iter()
            .skip(1)
            .any(|o| o.status == OperationStatus::Failed);
        if parent_ok && dependent_failed {
            report.partial = true;
        }

        {
            let mut sess = app.session.lock().await;
            sess.suggestions[i].status = s.status;
            if s.status == SuggestionStatus::Executed {
                let audit = AuditEntry {
                    timestamp: chrono::Utc::now().to_rfc3339(),
                    operation: match s.suggestion_type {
                        homelinter_model::suggestion::SuggestionType::Move => "move",
                        homelinter_model::suggestion::SuggestionType::Delete => "delete",
                    }
                    .to_string(),
                    source: s.target.clone(),
                    destination: s.new_path.clone(),
                    reason: s.reason.clone(),
                    scan_root: root.to_string_lossy().into_owned(),
                    result: "executed".into(),
                };
                let _ = app.audit.append(&audit);
            }
        }
        report.entries.push(entry);
        pos += 1;
    }

    {
        let mut sess = app.session.lock().await;
        sess.report = Some(report.clone());
        sess.report_batch = Some(BatchIdentity::of(&req.ids));
        sess.failure = None;
        sess.state = SessionState::Done;
    }
    app.events
        .publish(ServerEvent::ExecutionComplete { report });
    app.events.publish(ServerEvent::State(SessionState::Done));
}
