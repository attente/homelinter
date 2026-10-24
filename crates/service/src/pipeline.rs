//! Phase runners: scan and execute pipelines that drive the sync crates
//! (scan, analyze, suggest, execute) from async handlers, update the shared
//! session, and publish SSE events. Cancellation is cooperative inside the
//! scan phases (the discovery walks and the analysis loop poll the cancel
//! flag per entry/file) and between top-level suggestions (execute), exactly
//! as "Session Model > Cancellation" describes. Scan progress is published
//! at phase boundaries, at most once per second as completed work items
//! during analysis, and at most once per second as visited entries during the
//! discovery and symlink walks; a one-second heartbeat repeats the latest
//! phase event for as long as the scan runs, so the elapsed clock in clients
//! advances even through a phase that reports nothing (suggestion generation,
//! validation, or one slow extraction batch).

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;

use tokio::sync::Mutex;

use homelinter_analyze::{
    analyze, AnalyzeOptions, BuiltinAudioExtractor, BuiltinExifExtractor,
    DocumentContainerExtractor, ExifToolExtractor, ExtractionContext, MetadataExtractor,
    PdfInfoExtractor, PhaseEvent,
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
    /// Provider cache lookups for the current batch (see `CacheStats`).
    pub cache_stats: CacheStats,
}

/// Provider cache lookups since the current scan enqueued its provider work:
/// how many requests were answered from the persistent cache and how many had
/// to reach the provider. The pair is reset when a scan plans its work, so it
/// describes the batch the session is showing rather than the life of the
/// process. A forced refresh (an explicit re-request that discards the cached
/// value) never reads the cache and is therefore neither a hit nor a miss.
#[derive(Debug, Default)]
pub struct CacheStats {
    hits: AtomicU64,
    misses: AtomicU64,
}

impl CacheStats {
    pub fn record_hit(&self) {
        self.hits.fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_miss(&self) {
        self.misses.fetch_add(1, Ordering::Relaxed);
    }

    pub fn reset(&self) {
        self.hits.store(0, Ordering::Relaxed);
        self.misses.store(0, Ordering::Relaxed);
    }

    /// `(hits, misses)` for the current batch.
    pub fn counts(&self) -> (u64, u64) {
        (
            self.hits.load(Ordering::Relaxed),
            self.misses.load(Ordering::Relaxed),
        )
    }
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

/// Publishes `scan.progress` for one scan: worker reports are throttled to
/// one event per second so a fast pass cannot flood the SSE stream, and a
/// heartbeat re-publishes the latest state every second so clients always see
/// the elapsed clock advance, even while a phase is silent.
struct ProgressReporter {
    app: AppHandle,
    start: Instant,
    /// Last publish instant; guarded by a mutex because the analysis and walk
    /// passes report from worker threads.
    last_publish: std::sync::Mutex<Instant>,
    /// Most recent phase state, re-published by the heartbeat.
    latest: std::sync::Mutex<PhaseEvent>,
}

impl ProgressReporter {
    fn new(app: AppHandle, start: Instant) -> Arc<ProgressReporter> {
        Arc::new(ProgressReporter {
            app,
            start,
            last_publish: std::sync::Mutex::new(start),
            latest: std::sync::Mutex::new(PhaseEvent {
                phase: "discovery",
                scanned: 0,
                total: None,
                detail: None,
            }),
        })
    }

    /// Record a worker-thread report and publish it if a second has passed
    /// since the last publish.
    fn report(&self, ev: PhaseEvent) {
        *self.latest.lock().unwrap() = ev;
        let now = Instant::now();
        let mut last = self.last_publish.lock().unwrap();
        if now.duration_since(*last) >= std::time::Duration::from_secs(1) {
            *last = now;
            drop(last);
            self.publish(ev);
        }
    }

    /// Publish a phase-boundary state unconditionally; it also becomes the
    /// state the heartbeat repeats.
    fn report_now(&self, ev: PhaseEvent) {
        *self.latest.lock().unwrap() = ev;
        *self.last_publish.lock().unwrap() = Instant::now();
        self.publish(ev);
    }

    /// Publish the latest state with a fresh elapsed reading.
    fn publish_latest(&self) {
        let ev = *self.latest.lock().unwrap();
        self.publish(ev);
    }

    fn publish(&self, ev: PhaseEvent) {
        self.app.events.publish(ServerEvent::ScanProgress {
            phase: ev.phase,
            scanned: ev.scanned,
            total: ev.total,
            detail: ev.detail,
            elapsed_ms: self.start.elapsed().as_millis() as u64,
        });
    }

    /// Start the once-per-second heartbeat. Dropping the returned sender stops
    /// it, which happens on every exit path of `start_scan`.
    fn heartbeat(self: &Arc<ProgressReporter>) -> tokio::sync::oneshot::Sender<()> {
        let (stop, mut stopped) = tokio::sync::oneshot::channel::<()>();
        let reporter = Arc::clone(self);
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(std::time::Duration::from_secs(1));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            // The first tick completes immediately; skip it so the heartbeat
            // never duplicates the event published at scan start.
            tick.tick().await;
            loop {
                tokio::select! {
                    _ = &mut stopped => break,
                    _ = tick.tick() => reporter.publish_latest(),
                }
            }
        });
        stop
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
    // Phase progress for this scan; the heartbeat keeps the elapsed clock
    // advancing through phases that report nothing. Dropping `_heartbeat` when
    // this function returns stops it on every path, including failures.
    let reporter = ProgressReporter::new(app.clone(), start);
    let _heartbeat = reporter.heartbeat();

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
    reporter.report_now(PhaseEvent {
        phase: "discovery",
        scanned: 0,
        total: None,
        detail: None,
    });

    // Phase 1 + symlink pass: discovery. Runs on the blocking pool. The
    // probe is polled inside the walks (per entry) so a cancel lands
    // promptly instead of waiting for the whole tree.
    let home2 = home.clone();
    let cancelled = {
        let app = app.clone();
        move || is_cancelled(&app, cancel_token)
    };
    let scan_reporter = Arc::clone(&reporter);
    let scan_progress = move |stage: homelinter_scan::ScanStage, visited: u64| {
        // Visited entries, not managed files: the walks have no total until
        // they finish, so the count is reported without one.
        scan_reporter.report(PhaseEvent {
            phase: stage.phase(),
            scanned: visited,
            total: None,
            detail: None,
        });
    };
    let scan_result = tokio::task::spawn_blocking(move || {
        homelinter_scan::scan_root(&root, &home2, &scan_opts, &cancelled, &scan_progress)
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
    reporter.report_now(PhaseEvent {
        phase: "discovery",
        scanned: scan.primary_units,
        total: Some(scan.primary_units),
        detail: None,
    });
    reporter.report_now(PhaseEvent {
        phase: "symlinks",
        scanned: scan.symlinks.entries.len() as u64,
        total: Some(scan.symlinks.entries.len() as u64),
        detail: None,
    });

    // Phase 2: analysis.
    // Total files scanned comes from discovery, not analysis, which keeps
    // only managed files (unmanaged files are never extracted or hashed).
    // Capture the totals before moving the discovery vectors into the
    // blocking analysis task: nothing else below needs them, so cloning a
    // potentially large file list would be pure overhead.
    let total_files = scan.files.len() as u64;
    let total_symlinks = scan.symlinks.entries.len() as u64;
    let files = scan.files;
    let repos = scan.repos;
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
        // work items (extracted files per extractor, hashed files,
        // repositories), each with the phase's total when known; publish a
        // `scan.progress` event at most every second so a long
        // extraction/hashing pass shows movement in clients instead of the
        // frozen phase line left by the last phase-boundary event.
        let progress = {
            let reporter = Arc::clone(&reporter);
            move |ev: PhaseEvent| reporter.report(ev)
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
    reporter.report_now(PhaseEvent {
        phase: "metadata",
        scanned: analysis.files.len() as u64,
        total: Some(analysis.files.len() as u64),
        detail: None,
    });

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
    // Validation marks the rejected suggestions in place (and leaves the
    // pending ones untouched), so the batch itself is the source of the
    // published reasons: validating a deep copy of every suggestion just to
    // copy the same reasons back would double the batch's memory churn.
    let rejected_ids = homelinter_suggest::validate_batch(&mut suggestions, true);
    let rejected_set: std::collections::HashSet<&str> =
        rejected_ids.iter().map(String::as_str).collect();
    let rejected: Vec<RejectedId> = suggestions
        .iter()
        .filter(|s| rejected_set.contains(s.id.as_str()))
        .filter_map(|s| {
            s.rejection_reason.as_ref().map(|reason| RejectedId {
                id: s.id.clone(),
                reason: reason.clone(),
            })
        })
        .collect();
    if is_cancelled(&app, cancel_token) {
        return fail(app, "scan cancelled".into()).await;
    }
    reporter.report_now(PhaseEvent {
        phase: "suggestions",
        scanned: suggestions.len() as u64,
        total: Some(suggestions.len() as u64),
        detail: None,
    });

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
    // scan itself was never blocked on them. The cache counters restart with
    // this batch, so the header's hit/miss pair describes the scan on screen.
    app.cache_stats.reset();
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
    let (root, scan_started_at) = {
        let sess = app.session.lock().await;
        (
            sess.root().unwrap_or(&sess.home).to_path_buf(),
            sess.scan_started_at.clone(),
        )
    };
    let mut report = ExecutionReport::empty(&root);
    report.scan_started_at = scan_started_at;

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

    // Keeper relocations from moves that already executed, so this run can
    // republish a stale delete record before it hands the suggestion to the
    // executor (which seeds the same map itself). A keeper moved in an
    // earlier run is common here: the service executes one suggestion per
    // request, so a cancelled run or a selection that excluded the delete
    // leaves the keeper's move already done.
    let mut relocated: std::collections::BTreeMap<std::path::PathBuf, std::path::PathBuf> = {
        let sess = app.session.lock().await;
        homelinter_execute::relocations_of(&sess.suggestions)
    };

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
        // Point a duplicate delete at its keeper's executed destination. The
        // session is republished rather than the clone alone so repairs and
        // the reason text stay consistent with the keeper's new path.
        if s.suggestion_type == homelinter_model::suggestion::SuggestionType::Delete {
            let recorded = s
                .metadata
                .get("duplicate_of")
                .and_then(|v| v.as_str())
                .map(std::path::PathBuf::from);
            if let Some(recorded) = recorded.as_ref() {
                if let Some(new_path) = relocated.get(recorded) {
                    let mut sess = app.session.lock().await;
                    homelinter_execute::publish_keeper_relocation(
                        &mut sess.suggestions,
                        recorded,
                        new_path,
                    );
                    s = sess.suggestions[i].clone();
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
            .filter(|o| o.operation == OperationKind::RepairSymlink)
            .any(|o| o.status == OperationStatus::Failed);
        if entry.result == ExecutionStatus::Partial || (parent_ok && dependent_failed) {
            report.partial = true;
        }

        {
            let mut sess = app.session.lock().await;
            sess.suggestions[i].status = s.status;
            if s.status == SuggestionStatus::Executed {
                // A done suggestion leaves the selection: its checkbox clears
                // and disables, the batch total stops counting it as selected,
                // and a later run cannot try to execute it again.
                sess.suggestions[i].selected = false;
            }
            if s.suggestion_type == homelinter_model::suggestion::SuggestionType::Move
                && s.status == SuggestionStatus::Executed
            {
                // A keeper that moved changes what every delete of it must
                // point at, including deletes executed in a later batch.
                if let Some(dst) = &s.new_path {
                    homelinter_execute::publish_keeper_relocation(
                        &mut sess.suggestions,
                        std::path::Path::new(&s.target),
                        std::path::Path::new(dst),
                    );
                }
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

    report.executed_at = Some(chrono::Utc::now().to_rfc3339());
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
