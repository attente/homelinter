//! Phase runners: scan and execute pipelines that drive the sync crates
//! (scan, analyze, suggest, execute) from async handlers, update the shared
//! session, and publish SSE events. Cancellation is cooperative inside the
//! scan phases (the discovery walks and the analysis loop poll the cancel
//! flag per entry/file) and between top-level suggestions (execute), exactly
//! as "Session Model > Cancellation" describes. Scan progress is published
//! at phase boundaries and, during analysis, at most once per second as
//! completed work items, so a long extraction or hashing pass shows live
//! movement in clients instead of a frozen phase line.

use std::collections::BTreeSet;
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
use serde_json::Value as JsonValue;

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
    /// Configuration search chain (system + per-user files read below the
    /// explicit config file); the host chain by default, empty in tests.
    pub config_chain: homelinter_model::config::ConfigChain,
    /// Test/embedding override for the home used by symlink indexing and
    /// relative destinations; None means the platform home.
    pub home_override: Option<PathBuf>,
    /// Directory containing the built SPA (index.html + assets). None
    /// disables static file serving (API-only mode).
    pub web_dir: Option<PathBuf>,
    /// Launch-time CLI overrides as a partial config document, retained so
    /// runtime writes can be folded over them with the right precedence
    /// (runtime wins). Never applied directly to scans; scans snapshot the
    /// session's effective settings.
    pub launch_overrides: JsonValue,
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
/// `Scanning`, and snapshotted the current effective settings (`cfg`) and
/// the resolved scan root at that moment: every phase runs against that one
/// snapshot, so a settings write during a scan never changes the running
/// work and the batch reflects a single committed configuration.
pub async fn start_scan(app: AppHandle, root: PathBuf, cfg: RawConfig) {
    let home = effective_home(&app);
    let hidden = cfg.scan_hidden();
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
            exclude_patterns: cfg.scan.effective_exclude_paths(),
            exclude_extensions: cfg.scan.exclude_extensions.clone(),
        },
        cfg.duplication.large_file_threshold,
    );
    app.events.publish(phase_progress("discovery", 0, start));

    // Phase 1 + symlink pass: discovery. Runs on the blocking pool. The
    // probe is polled inside the walks (per entry) so a cancel lands
    // promptly instead of waiting for the whole tree.
    let home2 = home.clone();
    let cancelled = {
        let app = app.clone();
        move || is_cancelled(&app, cancel_token)
    };
    let scan_result = tokio::task::spawn_blocking(move || {
        homelinter_scan::scan_root(&root, &home2, &scan_opts, &cancelled)
    })
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
    let analysis = {
        let cancelled = {
            let app = app.clone();
            move || is_cancelled(&app, cancel_token)
        };
        // Analysis progress: the blocking analysis loop reports completed
        // work items (extracted files, hashed files, repositories); publish
        // a `scan.progress` event at most every second so a long
        // extraction/hashing pass shows movement in clients instead of the
        // frozen phase line left by the last phase-boundary event.
        let progress = {
            let app = app.clone();
            // The analysis passes report from worker threads, so the
            // throttle state is a mutex; a lock is held only long enough to
            // read and update the last-publish instant.
            let last = std::sync::Mutex::new(start);
            move |phase: &'static str, scanned: u64| {
                let now = Instant::now();
                let mut last = last.lock().unwrap();
                if now.duration_since(*last) >= std::time::Duration::from_secs(1) {
                    *last = now;
                    drop(last);
                    app.events.publish(phase_progress(phase, scanned, start));
                }
            }
        };
        tokio::task::spawn_blocking(move || {
            analyze(&files, &repos, &analyze_opts, &cancelled, &progress)
        })
        .await
    };
    let mut analysis = match analysis {
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

    // Manual metadata edits persisted by the user are keyed by content
    // identity and applied before suggestion generation, so an override wins
    // over embedded values and provider jobs skip overridden fields.
    crate::providers::apply_metadata_overrides(&app, &mut Arc::make_mut(&mut analysis).files).await;

    // Phase 3: suggestion generation.
    let formats = match cfg.resolved_target_paths() {
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

    // Total files scanned comes from discovery, not analysis, which keeps
    // only managed files (unmanaged files are never extracted or hashed).
    let total_files = scan.files.len() as u64;
    let total_symlinks = scan.symlinks.entries.len() as u64;

    // Plan provider work before batch validation: suggestions whose unit
    // has outstanding provider requests are marked `metadata_pending` first
    // so validation defers them. Their destinations are built on fallback
    // metadata and a collision may disappear once the provider result
    // arrives; they are re-validated against real metadata when their last
    // request resolves (providers.rs). Everyone sees the full scan result,
    // yet pending moves are never executable.
    let provider_jobs =
        crate::providers::plan_provider_jobs(&cfg, &analysis, &formats, provider_epoch).await;
    // Warm a vision-capable model out-of-band (no timeout) so the worker's
    // first image request does not eat its provider timeout on model load.
    crate::providers::warmup_vision_models(&provider_jobs);
    let mut pending: std::collections::BTreeMap<usize, u32> = std::collections::BTreeMap::new();
    for job in &provider_jobs {
        *pending.entry(job.unit.index).or_default() += 1;
    }
    // Units whose suggestion was actually marked `metadata_pending`; the
    // denominator of the client metadata-progress display. A unit that
    // produced no suggestion is not part of it.
    let mut pending_units: BTreeSet<usize> = BTreeSet::new();
    let unit_of: std::collections::BTreeMap<String, usize> = analysis
        .files
        .iter()
        .enumerate()
        .map(|(i, f)| (Suggestion::id_for_target(&f.path), i))
        .collect();
    for s in suggestions.iter_mut() {
        if let Some(i) = unit_of.get(&s.id) {
            if pending.contains_key(i) {
                s.metadata_pending = true;
                pending_units.insert(*i);
            }
        }
    }
    let pending_total = pending_units.len();

    // Full-batch validation before the batch is presented. Pending
    // suggestions are excluded (their destinations are provisional); every
    // rejection of the rest carries the machine-readable reason, which is
    // copied onto the suggestion so clients can show why it was rejected.
    let rejected: Vec<RejectedId> = {
        let mut pending_snapshot = suggestions.clone();
        let ids = homelinter_suggest::validate_batch(&mut pending_snapshot, true);
        ids.into_iter()
            .filter_map(|id| pending_snapshot.iter().find(|s| s.id == id))
            .filter_map(|s| {
                s.rejection_reason.as_ref().map(|reason| RejectedId {
                    id: s.id.clone(),
                    reason: reason.clone(),
                })
            })
            .collect()
    };
    for s in suggestions.iter_mut() {
        if let Some(r) = rejected.iter().find(|r| r.id == s.id) {
            s.status = SuggestionStatus::Rejected;
            s.rejection_reason = Some(r.reason.clone());
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

    {
        let mut sess = app.session.lock().await;
        sess.state = SessionState::Ready;
        sess.batch_config_hash = Some(config_hash(&cfg));
        sess.degraded_identity = analysis.degraded_identity;
        sess.warnings = analysis.warnings.clone();
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
            generations: Default::default(),
            refreshing: Default::default(),
            total_units: pending_units.clone(),
            pending_units: pending_units.clone(),
        });
        sess.suggestions = suggestions;
        let mut summary = sess.summary_of_batch();
        summary.total_files_scanned = total_files;
        summary.total_symlinks_indexed = total_symlinks;
        sess.summary = Some(summary.clone());
        sess.report = None;
        sess.report_batch = None;
        sess.failure = None;
        let total = sess.suggestions.len();
        drop(sess);

        app.events.publish(ServerEvent::SuggestionBatch { total });
        app.events.publish(ServerEvent::MetadataProgress {
            completed: 0,
            total: pending_total,
        });
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
    let mut batch: Vec<(usize, String, bool)> = Vec::new();
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
                    batch.push((
                        i,
                        s.sort_key(),
                        s.suggestion_type != homelinter_model::suggestion::SuggestionType::Move,
                    ));
                }
            }
        }
    }
    // Moves first, then deletes: a keeper may itself be scheduled for a move,
    // so its relocation must exist before a duplicate delete confirms the
    // keeper or retargets repairs at it.
    batch.sort_by(|a, b| a.2.cmp(&b.2).then_with(|| a.1.cmp(&b.1)));

    // Keeper relocations from moves that completed earlier in this batch, so
    // a duplicate delete still finds its keeper after the keeper moved.
    let mut relocated: std::collections::BTreeMap<std::path::PathBuf, std::path::PathBuf> =
        std::collections::BTreeMap::new();

    let mut pos = 0usize;
    while pos < batch.len() {
        let (i, _, _) = batch[pos];
        if is_cancelled(&app, cancel_token) {
            // Finish nothing more: mark every remaining suggestion skipped
            // with the cancellation reason and include them in the report.
            let mut sess = app.session.lock().await;
            for (j, _, _) in batch.iter().skip(pos) {
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
        let mut s = {
            let sess = app.session.lock().await;
            sess.suggestions[i].clone()
        };
        // Point a duplicate delete at its keeper's executed destination.
        let mut keeper_moved_to: Option<String> = None;
        if s.suggestion_type == homelinter_model::suggestion::SuggestionType::Delete {
            let recorded = s
                .metadata
                .get("duplicate_of")
                .and_then(|v| v.as_str())
                .map(std::path::PathBuf::from);
            if let Some(recorded) = recorded.as_ref() {
                if let Some(new_path) = relocated.get(recorded) {
                    // The keeper now lives at its destination: confirm the
                    // delete against that path and point its repairs there.
                    homelinter_execute::retarget_keeper_repairs(&mut s, recorded, new_path);
                    let new_path_str = new_path.to_string_lossy().into_owned();
                    s.metadata.insert(
                        "duplicate_of".into(),
                        serde_json::Value::String(new_path_str.clone()),
                    );
                    keeper_moved_to = Some(new_path_str);
                }
            }
        }
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
        if entry.result == ExecutionStatus::Partial || (parent_ok && dependent_failed) {
            report.partial = true;
        }

        {
            let mut sess = app.session.lock().await;
            sess.suggestions[i].status = s.status;
            if let Some(new_path) = &keeper_moved_to {
                sess.suggestions[i].metadata.insert(
                    "duplicate_of".into(),
                    serde_json::Value::String(new_path.clone()),
                );
            }
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
        if s.suggestion_type == homelinter_model::suggestion::SuggestionType::Move
            && s.status == SuggestionStatus::Executed
        {
            if let Some(dst) = &s.new_path {
                relocated.insert(
                    std::path::PathBuf::from(&s.target),
                    std::path::PathBuf::from(dst),
                );
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
