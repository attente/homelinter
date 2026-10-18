//! Full-screen interactive terminal client (ratatui + crossterm).
//!
//! The TUI drives one embedded service instance through the HTTP API exactly
//! like the managed CLI modes do (see "Client Lifecycle" in DESIGN.md). It
//! keeps one `GET /events` connection as the fast path for live progress and
//! polls `GET /session` every frame as a backstop, so the UI stays responsive
//! and the session state stays honest even if an SSE connection is dropped.
//!
//! Screens: a scanning/progress screen while the batch is generated, a
//! two-pane suggestion browser with client-side filtering and server-side
//! selection, a live operation log during execution, a
//! report view when a run finishes, and a read-only configuration view with a
//! metadata-cache viewer. Confirmation and text-input dialogs are
//! modal overlays. A terminal of at least 80x24 is required.

use crossterm::event::{Event, EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use futures_util::StreamExt;
use homelinter_clients::{Client, SseEvent, SseReader};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Alignment, Constraint, Flex, Layout, Position, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Clear, List, ListItem, Paragraph, Wrap};
use ratatui::{Frame, Terminal};
use serde_json::Value;
use std::collections::BTreeMap;
use std::io::{self, IsTerminal};
use std::path::PathBuf;
use std::time::Duration;

/// Minimum terminal dimensions required by the design.
const MIN_COLS: u16 = 80;
const MIN_ROWS: u16 = 24;

/// Restore the terminal when the TUI exits, on any path.
struct Restore;
impl Drop for Restore {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen);
    }
}

// ---------------------------------------------------------------------------
// Data
// ---------------------------------------------------------------------------

/// One top-level suggestion rendered in the browser.
#[derive(Debug, Clone)]
struct Row {
    id: String,
    typ: String,
    kind: String,
    status: String,
    selected: bool,
    target: String,
    new_path: String,
    reason: String,
    rejection: String,
    dependents: Vec<String>,
    pending_meta: bool,
}

impl Row {
    fn from_json(v: &Value) -> Row {
        let dependents: Vec<String> = v["dependent_operations"]
            .as_array()
            .map(|ops| {
                ops.iter()
                    .filter_map(|op| {
                        let obj = op.as_object()?;
                        let mut s = obj
                            .get("type")
                            .and_then(|t| t.as_str())
                            .unwrap_or("op")
                            .to_string();
                        let mut rest: Vec<String> = obj
                            .iter()
                            .filter(|(k, _)| k.as_str() != "type")
                            .map(|(k, val)| {
                                let text = match val {
                                    Value::String(s) => s.clone(),
                                    other => other.to_string(),
                                };
                                format!("{k}={text}")
                            })
                            .collect();
                        rest.sort();
                        if !rest.is_empty() {
                            s.push_str(&format!(" ({})", rest.join(", ")));
                        }
                        Some(s)
                    })
                    .collect()
            })
            .unwrap_or_default();
        Row {
            id: str_field(v, "id"),
            typ: str_field(v, "type"),
            kind: str_field(v, "asset_kind"),
            status: if str_field(v, "status").is_empty() {
                "pending".to_string()
            } else {
                str_field(v, "status")
            },
            selected: v["selected"].as_bool().unwrap_or(false),
            target: str_field(v, "target"),
            new_path: str_field(v, "new_path"),
            reason: str_field(v, "reason"),
            rejection: v["rejection_reason"].as_str().unwrap_or("").to_string(),
            pending_meta: v["metadata_pending"].as_bool().unwrap_or(false),
            dependents,
        }
    }

    fn is_executable(&self) -> bool {
        (self.status == "pending" || self.status == "executed") && !self.pending_meta
    }
}

fn str_field(v: &Value, key: &str) -> String {
    v[key].as_str().unwrap_or("").to_string()
}

/// Cache kinds the viewer can filter on, in display order.
const CACHE_KINDS: [&str; 4] = ["geocode", "description", "author", "filename_analysis"];

/// Rows fetched per cache-viewer page.
const CACHE_PAGE: usize = 200;

/// One cached provider result rendered in the cache viewer.
#[derive(Debug, Clone, Default)]
struct CacheRow {
    kind: String,
    provider: String,
    value: String,
    created_at: String,
    cache_key: String,
    input_hash: String,
}

impl CacheRow {
    fn from_json(v: &Value) -> CacheRow {
        CacheRow {
            kind: str_field(v, "kind"),
            provider: str_field(v, "provider"),
            value: str_field(v, "value"),
            created_at: str_field(v, "created_at"),
            cache_key: str_field(v, "cache_key"),
            input_hash: str_field(v, "input_hash"),
        }
    }
}

/// Client-side filter. Affects navigation and bulk selection only; the
/// service keeps the authoritative selection.
#[derive(Debug, Clone, Default)]
struct Filter {
    typ: Option<String>,
    kind: Option<String>,
    status: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    Scan,
    Browse,
    Run,
    Report,
    Config,
    Cache,
}

#[derive(Debug, Clone, Copy)]
enum StaleAction {
    Run,
    Export,
}

#[derive(Debug, Clone)]
enum ConfirmAction {
    Quit,
    Run,
    ProceedStale,
}

struct Confirm {
    title: String,
    message: String,
    yes: ConfirmAction,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PromptKind {
    ExportPath,
    ScanRoot,
}

struct Prompt {
    title: String,
    kind: PromptKind,
    value: String,
    cursor: usize,
}

#[derive(Default)]
struct RunView {
    log: Vec<String>,
    report: Option<Value>,
    failed_message: Option<String>,
}

// ---------------------------------------------------------------------------
// Application state
// ---------------------------------------------------------------------------

struct App {
    client: Client,
    phase: Phase,
    state: String,
    root: String,
    hidden: bool,
    target_paths: BTreeMap<String, String>,
    rows: Vec<Row>,
    kinds: Vec<String>,
    progress_phase: String,
    progress_scanned: u64,
    progress_elapsed_ms: u64,
    metadata_completed: usize,
    metadata_total: usize,
    filter: Filter,
    cursor: usize,
    list_offset: usize,
    confirm: Option<Confirm>,
    prompt: Option<Prompt>,
    help: bool,
    toast: Option<String>,
    run: RunView,
    frame: u64,
    quit: bool,
    config_doc: Option<Value>,
    config_scroll: u16,
    cache_entries: Vec<CacheRow>,
    cache_total: u64,
    cache_kind: Option<String>,
    cache_offset: usize,
    cache_scroll: u16,
    config_hash: Option<String>,
    batch_config_hash: Option<String>,
    failure_reason: Option<String>,
    stale_action: Option<StaleAction>,
}

impl App {
    fn new(
        client: Client,
        root: PathBuf,
        hidden: bool,
        target_paths: BTreeMap<String, String>,
    ) -> App {
        App {
            client,
            phase: Phase::Scan,
            state: "starting".to_string(),
            root: root.to_string_lossy().into_owned(),
            hidden,
            target_paths,
            rows: Vec::new(),
            kinds: Vec::new(),
            progress_phase: String::new(),
            progress_scanned: 0,
            progress_elapsed_ms: 0,
            metadata_completed: 0,
            metadata_total: 0,
            filter: Filter::default(),
            cursor: 0,
            list_offset: 0,
            confirm: None,
            prompt: None,
            help: false,
            toast: None,
            run: RunView::default(),
            frame: 0,
            quit: false,
            config_doc: None,
            config_scroll: 0,
            cache_entries: Vec::new(),
            cache_total: 0,
            cache_kind: None,
            cache_offset: 0,
            cache_scroll: 0,
            config_hash: None,
            batch_config_hash: None,
            failure_reason: None,
            stale_action: None,
        }
    }

    // ------------------------------------------------------------------
    // API actions
    // ------------------------------------------------------------------

    async fn start_scan(&mut self) -> Result<(), String> {
        // Root, hidden, and target paths are runtime settings, not per-scan
        // parameters: persist the form's values, then scan against the
        // service's current settings.
        let mut settings = serde_json::json!({
            "scan": { "root": self.root, "hidden": self.hidden },
        });
        if !self.target_paths.is_empty() {
            settings["target_paths"] = serde_json::json!(self.target_paths);
        }
        self.client
            .config_put(settings)
            .await
            .map_err(|e| format!("cannot save scan settings: {e}"))?;
        self.phase = Phase::Scan;
        self.state = "scanning".to_string();
        self.rows.clear();
        self.kinds.clear();
        self.cursor = 0;
        self.list_offset = 0;
        self.filter = Filter::default();
        self.metadata_completed = 0;
        self.metadata_total = 0;
        self.client
            .start_scan(serde_json::json!({}))
            .await
            .map_err(|e| format!("cannot start scan: {e}"))?;
        Ok(())
    }

    async fn refresh_rows(&mut self) -> Result<(), String> {
        let v = self
            .client
            .suggestions(None)
            .await
            .map_err(|e| format!("cannot list suggestions: {e}"))?;
        let items = v["items"].as_array().cloned().unwrap_or_default();
        self.rows = items.iter().map(Row::from_json).collect();
        self.kinds = self.unique_kinds();
        if self.cursor >= self.rows.len() && !self.rows.is_empty() {
            self.cursor = self.rows.len() - 1;
        }
        Ok(())
    }

    fn unique_kinds(&self) -> Vec<String> {
        let mut seen: Vec<String> = Vec::new();
        for r in &self.rows {
            if !r.kind.is_empty() && !seen.iter().any(|k| *k == r.kind) {
                seen.push(r.kind.clone());
            }
        }
        seen.sort();
        seen
    }

    /// Apply one `suggestion.updated` event in place, keyed by stable id. A
    /// provider resolution changes a single row, so the whole batch is never
    /// re-fetched or re-rendered.
    fn upsert_row(&mut self, item: &Value) {
        let row = Row::from_json(item);
        if let Some(existing) = self.rows.iter_mut().find(|r| r.id == row.id) {
            *existing = row;
        } else {
            self.rows.push(row);
        }
        self.kinds = self.unique_kinds();
        if self.cursor >= self.rows.len() {
            self.cursor = self.rows.len().saturating_sub(1);
        }
    }

    /// The currently selected ids. When nothing is selected, selects the
    /// highlighted row first so single-item actions never silently no-op.
    async fn ensure_selection(&mut self) -> Result<Vec<String>, String> {
        let selected: Vec<String> = self
            .rows
            .iter()
            .filter(|r| r.selected)
            .map(|r| r.id.clone())
            .collect();
        if !selected.is_empty() {
            return Ok(selected);
        }
        let Some(idx) = self.filtered_index(self.cursor) else {
            return Ok(Vec::new());
        };
        let id = self.rows[idx].id.clone();
        self.client
            .selection_add(Some(vec![id.clone()]), None)
            .await
            .map_err(|e| format!("cannot select suggestion: {e}"))?;
        self.refresh_rows().await?;
        Ok(vec![id])
    }

    async fn toggle_current(&mut self) {
        let Some(idx) = self.filtered_index(self.cursor) else {
            return;
        };
        let id = self.rows[idx].id.clone();
        let select = !self.rows[idx].selected;
        let res = if select {
            self.client.selection_add(Some(vec![id]), None).await
        } else {
            self.client.selection_remove(Some(vec![id]), None).await
        };
        match res {
            Ok(_) => self.refresh_after_mutation().await,
            Err(e) => self.toast = Some(format!("selection update failed: {e}")),
        }
    }

    /// Discard the cached provider metadata for the highlighted suggestion
    /// and ask the service to fetch it again. Only move suggestions carry
    /// provider metadata; a delete has nothing to re-request.
    async fn refresh_metadata_current(&mut self) {
        let Some(idx) = self.filtered_index(self.cursor) else {
            return;
        };
        let (id, typ) = (self.rows[idx].id.clone(), self.rows[idx].typ.clone());
        if typ != "move" {
            self.toast = Some("provider metadata does not shape a delete suggestion".to_string());
            return;
        }
        match self.client.refresh_metadata(&id).await {
            Ok(_) => self.refresh_after_mutation().await,
            Err(e) => self.toast = Some(format!("re-request failed: {e}")),
        }
    }

    /// Re-apply the effective metadata for the highlighted move suggestion
    /// and rebuild its destination, without a scan or a provider call. Only
    /// move suggestions have a destination.
    async fn rebuild_current(&mut self) {
        let Some(idx) = self.filtered_index(self.cursor) else {
            return;
        };
        let (id, typ) = (self.rows[idx].id.clone(), self.rows[idx].typ.clone());
        if typ != "move" {
            self.toast = Some("metadata does not shape a delete suggestion".to_string());
            return;
        }
        match self.client.rebuild_suggestion(&id).await {
            Ok(_) => self.refresh_after_mutation().await,
            Err(e) => self.toast = Some(format!("rebuild failed: {e}")),
        }
    }

    async fn select_all_filtered(&mut self) {
        let ids: Vec<String> = self
            .filtered_ids()
            .into_iter()
            .filter(|id| {
                self.rows
                    .iter()
                    .find(|r| &r.id == id)
                    .map(|r| !r.selected)
                    .unwrap_or(false)
            })
            .collect();
        if ids.is_empty() {
            return;
        }
        match self.client.selection_add(Some(ids), None).await {
            Ok(_) => self.refresh_after_mutation().await,
            Err(e) => self.toast = Some(format!("selection failed: {e}")),
        }
    }

    async fn clear_selection(&mut self) {
        let ids: Vec<String> = self
            .rows
            .iter()
            .filter(|r| r.selected)
            .map(|r| r.id.clone())
            .collect();
        if ids.is_empty() {
            return;
        }
        match self.client.selection_remove(Some(ids), None).await {
            Ok(_) => self.refresh_after_mutation().await,
            Err(e) => self.toast = Some(format!("selection update failed: {e}")),
        }
    }

    async fn refresh_after_mutation(&mut self) {
        if let Err(e) = self.refresh_rows().await {
            self.toast = Some(format!("refresh failed: {e}"));
        }
    }

    fn batch_is_stale(&self) -> bool {
        matches!((&self.config_hash, &self.batch_config_hash), (Some(current), Some(batch)) if current != batch)
    }

    /// Start an execution for the current selection.
    async fn run_selected(&mut self) -> Result<(), String> {
        if self.batch_is_stale() && self.stale_action.is_none() {
            self.stale_action = Some(StaleAction::Run);
            self.confirm = Some(Confirm {
                title: "Stale suggestions".to_string(),
                message: "Configuration changed since scan. Proceed anyway? (y) or cancel (n). Press r to rescan.".to_string(),
                yes: ConfirmAction::ProceedStale,
            });
            return Ok(());
        }
        let ids = self.ensure_selection().await?;
        if ids.is_empty() {
            self.toast = Some("nothing to run: the batch is empty".to_string());
            return Ok(());
        }
        let label = "execution";
        self.client
            .execute(Some(ids))
            .await
            .map_err(|e| format!("cannot start {label}: {e}"))?;
        self.run = RunView::default();
        self.phase = Phase::Run;
        self.state = "executing".to_string();
        Ok(())
    }

    async fn export_selected(&mut self, raw_path: String) {
        let path = match self.resolve_path(&raw_path) {
            Ok(p) => p,
            Err(e) => {
                self.toast = Some(e);
                return;
            }
        };
        if self.batch_is_stale() && self.stale_action.is_none() {
            self.stale_action = Some(StaleAction::Export);
            self.confirm = Some(Confirm {
                title: "Stale suggestions".to_string(),
                message: "Configuration changed since scan. Proceed anyway? (y) or cancel (n). Press r to rescan.".to_string(),
                yes: ConfirmAction::ProceedStale,
            });
            return;
        }
        let ids = match self.ensure_selection().await {
            Ok(ids) => ids,
            Err(e) => {
                self.toast = Some(e);
                return;
            }
        };
        if ids.is_empty() {
            self.toast = Some("nothing to export: the batch is empty".to_string());
            return;
        }
        let outcome = self
            .client
            .export(Some(ids), Some(path.to_string_lossy().into_owned()))
            .await;
        match outcome {
            Ok(homelinter_clients::ExportOutcome::Written { path, count, .. }) => {
                self.toast = Some(format!("exported {count} suggestions to {path}"));
            }
            Ok(homelinter_clients::ExportOutcome::ScriptText { .. }) => {
                self.toast =
                    Some("service returned script text although a path was requested".to_string());
            }
            Err(e) => self.toast = Some(format!("export failed: {e}")),
        }
    }

    fn resolve_path(&self, input: &str) -> Result<PathBuf, String> {
        let input = input.trim();
        if input.is_empty() {
            return Err("path must not be empty".to_string());
        }
        if input.contains('~') || input.contains('$') {
            return Err("path must not contain '~' or environment references".to_string());
        }
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        homelinter_model::path::to_absolute(std::path::Path::new(input), &cwd)
            .map_err(|e| e.to_string())
    }

    /// Cancel an in-progress scan or run and wait for the session to settle.
    async fn cancel_in_flight(&mut self) {
        if !matches!(self.state.as_str(), "scanning" | "analyzing" | "executing") {
            return;
        }
        let _ = self.client.cancel().await;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            match self.client.session().await {
                Ok(sess) => {
                    let st = str_field(&sess, "state");
                    self.state = st.clone();
                    if matches!(st.as_str(), "ready" | "done" | "failed" | "idle") {
                        break;
                    }
                }
                Err(_) => break,
            }
            if tokio::time::Instant::now() >= deadline {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    // ------------------------------------------------------------------
    // Filtering
    // ------------------------------------------------------------------

    fn row_matches(&self, r: &Row) -> bool {
        if let Some(t) = &self.filter.typ {
            if r.typ != *t {
                return false;
            }
        }
        if let Some(k) = &self.filter.kind {
            if r.kind != *k {
                return false;
            }
        }
        if let Some(s) = &self.filter.status {
            if r.status != *s {
                return false;
            }
        }
        true
    }

    fn filtered(&self) -> Vec<usize> {
        self.rows
            .iter()
            .enumerate()
            .filter(|(_, r)| self.row_matches(r))
            .map(|(i, _)| i)
            .collect()
    }

    fn filtered_index(&self, pos: usize) -> Option<usize> {
        self.filtered().get(pos).copied()
    }

    fn filtered_ids(&self) -> Vec<String> {
        self.filtered()
            .into_iter()
            .map(|i| self.rows[i].id.clone())
            .collect()
    }

    fn filtered_len(&self) -> usize {
        self.filtered().len()
    }

    // ------------------------------------------------------------------
    // SSE and polling
    // ------------------------------------------------------------------

    async fn on_sse(&mut self, ev: SseEvent) {
        match ev.event.as_str() {
            "session.state" => {
                if let Ok(v) = serde_json::from_str::<Value>(&ev.data) {
                    self.state = str_field(&v, "state");
                }
            }
            "scan.progress" => {
                if let Ok(v) = serde_json::from_str::<Value>(&ev.data) {
                    self.progress_phase = str_field(&v, "phase");
                    self.progress_scanned = v["scanned"].as_u64().unwrap_or(0);
                    self.progress_elapsed_ms = v["elapsed_ms"].as_u64().unwrap_or(0);
                }
            }
            "metadata.progress" => {
                if let Ok(v) = serde_json::from_str::<Value>(&ev.data) {
                    self.metadata_completed = v["completed"].as_u64().unwrap_or(0) as usize;
                    self.metadata_total = v["total"].as_u64().unwrap_or(0) as usize;
                }
            }
            "suggestion.batch" => {
                // The event carries only the batch total; fetch the rows
                // over REST (see "Service API > Events").
                if let Err(e) = self.refresh_rows().await {
                    self.toast = Some(e);
                }
            }
            "suggestion.updated" => {
                if let Ok(v) = serde_json::from_str::<Value>(&ev.data) {
                    self.upsert_row(&v["item"]);
                }
            }
            "suggestion.removed" => {
                if let Ok(v) = serde_json::from_str::<Value>(&ev.data) {
                    let id = str_field(&v, "id");
                    self.rows.retain(|r| r.id != id);
                    self.kinds = self.unique_kinds();
                    if self.cursor >= self.rows.len() {
                        self.cursor = self.rows.len().saturating_sub(1);
                    }
                }
            }
            "batch.validated" => {
                if let Ok(v) = serde_json::from_str::<Value>(&ev.data) {
                    let rejected = v["rejected"].as_array().cloned().unwrap_or_default();
                    if !rejected.is_empty() {
                        self.toast = Some(format!(
                            "re-validation rejected {} suggestion(s)",
                            rejected.len()
                        ));
                    }
                }
                self.refresh_after_mutation().await;
            }
            "execution.op" => {
                if let Ok(v) = serde_json::from_str::<Value>(&ev.data) {
                    let line = format!(
                        "[{}] {} {}",
                        str_field(&v, "status"),
                        str_field(&v, "operation"),
                        str_field(&v, "suggestion_id")
                    );
                    if self.run.log.len() > 400 {
                        self.run.log.remove(0);
                    }
                    self.run.log.push(line);
                }
            }
            "execution.complete" => {
                if let Ok(v) = serde_json::from_str::<Value>(&ev.data) {
                    self.run.report = v.get("report").cloned();
                }
            }
            _ => {}
        }
    }

    /// Periodic work: session poll plus phase transitions.
    async fn poll(&mut self) {
        self.frame += 1;
        if let Ok(sess) = self.client.session().await {
            self.state = str_field(&sess, "state");
            self.config_hash = sess["config_hash"].as_str().map(str::to_string);
            self.batch_config_hash = sess["batch_config_hash"].as_str().map(str::to_string);
            self.failure_reason = sess["failure"]["reason"].as_str().map(str::to_string);
            self.metadata_completed =
                sess["metadata_progress"]["completed"].as_u64().unwrap_or(0) as usize;
            self.metadata_total = sess["metadata_progress"]["total"].as_u64().unwrap_or(0) as usize;
        }
        match self.phase {
            Phase::Scan => {
                if self.state == "ready" {
                    if self.rows.is_empty() {
                        let _ = self.refresh_rows().await;
                    }
                    self.phase = Phase::Browse;
                } else if self.state == "failed" {
                    self.phase = Phase::Browse;
                    self.toast = Some(match &self.failure_reason {
                        Some(reason) => format!("scan failed: {reason}"),
                        None => "scan failed; no suggestions were generated".to_string(),
                    });
                }
            }
            Phase::Run => {
                if self.state == "done" {
                    self.finish_run().await;
                } else if self.state == "failed" {
                    self.run.failed_message = Some(match &self.failure_reason {
                        Some(reason) => format!("run failed: {reason}"),
                        None => "the run failed".to_string(),
                    });
                    self.phase = Phase::Report;
                }
            }
            _ => {}
        }
    }

    async fn finish_run(&mut self) {
        match self.client.execution_report().await {
            Ok(report) => self.run.report = Some(report),
            Err(e) => {
                self.run.failed_message = Some(format!("cannot read execution report: {e}"));
            }
        }
        self.phase = Phase::Report;
    }

    async fn open_config(&mut self) {
        match self.client.config().await {
            Ok(config) => {
                self.config_doc = Some(config);
                self.config_scroll = 0;
                self.phase = Phase::Config;
            }
            Err(e) => self.toast = Some(format!("cannot read config: {e}")),
        }
    }

    /// Fetch the current cache page and switch to the viewer, keeping the
    /// kind filter and page the viewer already had.
    async fn open_cache(&mut self) {
        match self.load_cache().await {
            Ok(()) => self.phase = Phase::Cache,
            Err(e) => self.toast = Some(e),
        }
    }

    async fn load_cache(&mut self) -> Result<(), String> {
        let v = self
            .client
            .cache_list(self.cache_kind.as_deref(), self.cache_offset, CACHE_PAGE)
            .await
            .map_err(|e| format!("cannot read cache: {e}"))?;
        self.cache_entries = v["entries"]
            .as_array()
            .map(|items| items.iter().map(CacheRow::from_json).collect())
            .unwrap_or_default();
        self.cache_total = v["total"].as_u64().unwrap_or(0);
        self.cache_scroll = 0;
        Ok(())
    }

    fn cycle_cache_kind(&mut self) {
        self.cache_kind = match self.cache_kind.as_deref() {
            None => Some(CACHE_KINDS[0].to_string()),
            Some(_) => {
                let idx = CACHE_KINDS
                    .iter()
                    .position(|k| Some(*k) == self.cache_kind.as_deref())
                    .map(|i| i + 1)
                    .unwrap_or(0);
                if idx >= CACHE_KINDS.len() {
                    None
                } else {
                    Some(CACHE_KINDS[idx].to_string())
                }
            }
        };
        self.cache_offset = 0;
    }

    async fn report_done(&mut self) {
        let _ = self.refresh_rows().await;
        self.run = RunView::default();
        self.phase = Phase::Browse;
        self.state = "ready".to_string();
    }

    // ------------------------------------------------------------------
    // Key handling
    // ------------------------------------------------------------------

    async fn on_key(&mut self, key: KeyEvent) {
        if key.kind != KeyEventKind::Press {
            return;
        }
        if let Some(p) = self.prompt.take() {
            self.on_prompt_key(p, key).await;
            return;
        }
        if let Some(c) = self.confirm.take() {
            self.on_confirm_key(c, key).await;
            return;
        }
        if self.help {
            match key.code {
                KeyCode::Esc | KeyCode::Char('?') | KeyCode::Char('q') => self.help = false,
                _ => {}
            }
            return;
        }
        if let KeyCode::Char('c') = key.code {
            if key.modifiers.contains(KeyModifiers::CONTROL) {
                self.cancel_in_flight().await;
                self.quit = true;
                return;
            }
        }
        match self.phase {
            Phase::Scan => match key.code {
                KeyCode::Char('?') => self.help = true,
                KeyCode::Esc | KeyCode::Char('q') => self.ask_quit(),
                _ => {}
            },
            Phase::Browse => self.on_browse_key(key).await,
            Phase::Run => match key.code {
                // Ignore all keys except a quit request during a run.
                KeyCode::Esc | KeyCode::Char('q') => self.ask_quit(),
                _ => {}
            },
            Phase::Report => {
                self.report_done().await;
            }
            Phase::Config => match key.code {
                KeyCode::Up => self.config_scroll = self.config_scroll.saturating_sub(1),
                KeyCode::Down => self.config_scroll = self.config_scroll.saturating_add(1),
                KeyCode::Char('v') | KeyCode::Char('l') => self.open_cache().await,
                KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('c') => {
                    self.phase = Phase::Browse
                }
                KeyCode::Char('C') => match self.client.cache_clear().await {
                    Ok(()) => self.toast = Some("cache cleared".to_string()),
                    Err(e) => self.toast = Some(format!("cache clear failed: {e}")),
                },
                _ => {}
            },
            Phase::Cache => match key.code {
                KeyCode::Up => self.cache_scroll = self.cache_scroll.saturating_sub(1),
                KeyCode::Down => self.cache_scroll = self.cache_scroll.saturating_add(1),
                KeyCode::PageDown | KeyCode::Char('n') => {
                    let next = self.cache_offset + CACHE_PAGE;
                    if (next as u64) < self.cache_total {
                        self.cache_offset = next;
                        if let Err(e) = self.load_cache().await {
                            self.toast = Some(e);
                        }
                    }
                }
                KeyCode::PageUp | KeyCode::Char('p') => {
                    self.cache_offset = self.cache_offset.saturating_sub(CACHE_PAGE);
                    if let Err(e) = self.load_cache().await {
                        self.toast = Some(e);
                    }
                }
                KeyCode::Char('k') => {
                    self.cycle_cache_kind();
                    if let Err(e) = self.load_cache().await {
                        self.toast = Some(e);
                    }
                }
                KeyCode::Char('r') => {
                    if let Err(e) = self.load_cache().await {
                        self.toast = Some(e);
                    }
                }
                KeyCode::Char('C') => match self.client.cache_clear().await {
                    Ok(()) => {
                        self.cache_offset = 0;
                        self.toast = Some("cache cleared".to_string());
                        if let Err(e) = self.load_cache().await {
                            self.toast = Some(e);
                        }
                    }
                    Err(e) => self.toast = Some(format!("cache clear failed: {e}")),
                },
                KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('c') => {
                    self.phase = Phase::Config
                }
                _ => {}
            },
        }
    }

    async fn on_browse_key(&mut self, key: KeyEvent) {
        let total = self.filtered_len();
        match key.code {
            KeyCode::Up => {
                if self.cursor > 0 {
                    self.cursor -= 1;
                }
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if self.cursor + 1 < total {
                    self.cursor += 1;
                }
            }
            KeyCode::PageUp => {
                self.cursor = self.cursor.saturating_sub(10);
            }
            KeyCode::PageDown => {
                self.cursor = (self.cursor + 10).min(total.saturating_sub(1));
            }
            KeyCode::Home => {
                self.cursor = 0;
                self.list_offset = 0;
            }
            KeyCode::End => {
                self.cursor = total.saturating_sub(1);
            }
            KeyCode::Char(' ') => self.toggle_current().await,
            KeyCode::Char('a') => self.select_all_filtered().await,
            KeyCode::Char('A') => self.clear_selection().await,
            KeyCode::Char('t') => {
                self.filter.typ = match self.filter.typ.as_deref() {
                    None => Some("move".to_string()),
                    Some("move") => Some("delete".to_string()),
                    Some(_) => None,
                };
            }
            KeyCode::Char('k') => {
                let choices = self.kind_cycle();
                let current = self.filter.kind.as_deref().unwrap_or("");
                let pos = choices.iter().position(|c| c == current).unwrap_or(0);
                self.filter.kind = if choices.is_empty() {
                    None
                } else {
                    let next = choices[(pos + 1) % choices.len()].clone();
                    if next.is_empty() {
                        None
                    } else {
                        Some(next)
                    }
                };
            }
            KeyCode::Char('u') => {
                let choices = [
                    "", "pending", "rejected", "executed", "partial", "failed", "skipped",
                ];
                let current = self.filter.status.as_deref().unwrap_or("");
                let pos = choices.iter().position(|c| *c == current).unwrap_or(0);
                self.filter.status = {
                    let next = choices[(pos + 1) % choices.len()];
                    if next.is_empty() {
                        None
                    } else {
                        Some(next.to_string())
                    }
                };
            }
            KeyCode::Char('c') => self.filter = Filter::default(),
            KeyCode::Char('x') => {
                let n = self.executable_count();
                if n == 0 {
                    self.toast = Some(
                        "nothing to execute: select suggestions with space or 'a'".to_string(),
                    );
                } else {
                    self.confirm = Some(Confirm {
                        title: "Execute".to_string(),
                        message: format!("Run {n} suggestion(s)? The filesystem will be modified."),
                        yes: ConfirmAction::Run,
                    });
                }
            }
            KeyCode::Char('e') => {
                if self.rows.is_empty() {
                    self.toast = Some("nothing to export: the batch is empty".to_string());
                } else {
                    self.prompt = Some(Prompt {
                        title: "Export path (absolute; no ~ or $)".to_string(),
                        kind: PromptKind::ExportPath,
                        value: String::new(),
                        cursor: 0,
                    });
                }
            }
            KeyCode::Char('g') => self.open_config().await,
            KeyCode::Char('m') => self.refresh_metadata_current().await,
            KeyCode::Char('b') => self.rebuild_current().await,
            KeyCode::Char('n') => {
                self.prompt = Some(Prompt {
                    title: self.scan_prompt_title(),
                    kind: PromptKind::ScanRoot,
                    value: self.root.clone(),
                    cursor: self.root.len(),
                });
            }
            KeyCode::Char('?') => self.help = true,
            KeyCode::Esc | KeyCode::Char('q') => self.ask_quit(),
            _ => {}
        }
    }

    fn kind_cycle(&self) -> Vec<String> {
        let mut out = vec!["".to_string()];
        out.extend(self.kinds.iter().cloned());
        out
    }

    fn executable_count(&self) -> usize {
        self.rows
            .iter()
            .filter(|r| r.selected && r.is_executable())
            .count()
    }

    fn scan_prompt_title(&self) -> String {
        format!(
            "Scan root (empty = home) [hidden: {}; Tab toggles]",
            if self.hidden { "yes" } else { "no" }
        )
    }

    async fn on_confirm_key(&mut self, c: Confirm, key: KeyEvent) {
        match key.code {
            KeyCode::Char('y') | KeyCode::Enter => match c.yes {
                ConfirmAction::Quit => {
                    self.cancel_in_flight().await;
                    self.quit = true;
                }
                ConfirmAction::Run => {
                    if let Err(e) = self.run_selected().await {
                        self.toast = Some(e);
                    }
                }
                ConfirmAction::ProceedStale => {
                    let action = self.stale_action.take();
                    match action {
                        Some(StaleAction::Run) => {
                            if let Err(e) = self.run_selected().await {
                                self.toast = Some(e);
                            }
                        }
                        Some(StaleAction::Export) => {
                            self.prompt = Some(Prompt {
                                title: "Export path (absolute; no ~ or $)".to_string(),
                                kind: PromptKind::ExportPath,
                                value: String::new(),
                                cursor: 0,
                            });
                        }
                        None => {}
                    }
                }
            },
            KeyCode::Char('r') => match c.yes {
                ConfirmAction::ProceedStale => {
                    // Discard the pending run/export and regenerate the batch
                    // under the current configuration with the same session
                    // settings, so it is no longer stale.
                    self.stale_action = None;
                    if let Err(e) = self.start_scan().await {
                        self.toast = Some(e);
                        self.phase = Phase::Browse;
                    }
                }
                _ => self.confirm = Some(c),
            },
            KeyCode::Char('n') | KeyCode::Esc => {}
            _ => self.confirm = Some(c),
        }
    }

    async fn on_prompt_key(&mut self, mut p: Prompt, key: KeyEvent) {
        match key.code {
            KeyCode::Char(ch) => {
                if !ch.is_control() {
                    p.value.insert(p.cursor, ch);
                    p.cursor += 1;
                }
                self.prompt = Some(p);
            }
            KeyCode::Backspace => {
                if p.cursor > 0 {
                    p.cursor -= 1;
                    p.value.remove(p.cursor);
                }
                self.prompt = Some(p);
            }
            KeyCode::Delete => {
                if p.cursor < p.value.len() {
                    p.value.remove(p.cursor);
                }
                self.prompt = Some(p);
            }
            KeyCode::Left => {
                p.cursor = p.cursor.saturating_sub(1);
                self.prompt = Some(p);
            }
            KeyCode::Right => {
                if p.cursor < p.value.len() {
                    p.cursor += 1;
                }
                self.prompt = Some(p);
            }
            KeyCode::Home => {
                p.cursor = 0;
                self.prompt = Some(p);
            }
            KeyCode::End => {
                p.cursor = p.value.len();
                self.prompt = Some(p);
            }
            KeyCode::Tab => match p.kind {
                PromptKind::ScanRoot => {
                    self.hidden = !self.hidden;
                    p.title = self.scan_prompt_title();
                }
                PromptKind::ExportPath => {}
            },
            KeyCode::Enter => {
                let kind = p.kind;
                let value = p.value;
                match kind {
                    PromptKind::ExportPath => self.export_selected(value).await,
                    PromptKind::ScanRoot => {
                        let resolved = if value.trim().is_empty() {
                            homelinter_model::path::home_dir()
                                .ok_or_else(|| "cannot determine home directory".to_string())
                        } else {
                            self.resolve_path(&value)
                        };
                        match resolved {
                            Ok(root) => {
                                self.root = root.to_string_lossy().into_owned();
                                if let Err(e) = self.start_scan().await {
                                    self.toast = Some(e);
                                    self.phase = Phase::Browse;
                                }
                            }
                            Err(e) => self.toast = Some(e),
                        }
                    }
                }
            }
            KeyCode::Esc => {}
            _ => self.prompt = Some(p),
        }
    }

    fn ask_quit(&mut self) {
        self.confirm = Some(Confirm {
            title: "Quit".to_string(),
            message: "Quit homelinter? A running scan or execution is cancelled.".to_string(),
            yes: ConfirmAction::Quit,
        });
    }
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

impl App {
    fn draw(&mut self, f: &mut Frame) {
        match self.phase {
            Phase::Scan => self.draw_scan(f),
            Phase::Browse => self.draw_browse(f),
            Phase::Run => self.draw_run(f),
            Phase::Report => self.draw_report(f),
            Phase::Config => self.draw_config(f),
            Phase::Cache => self.draw_cache(f),
        }
        if let Some(p) = &self.prompt {
            self.draw_prompt(f, p);
        } else if let Some(c) = &self.confirm {
            self.draw_confirm(f, c);
        } else if self.help {
            self.draw_help(f);
        }
    }

    fn header_line(&self) -> Line<'static> {
        let state_color = match self.state.as_str() {
            "ready" => Color::Green,
            "done" => Color::Cyan,
            "failed" => Color::Red,
            _ => Color::Yellow,
        };
        let selected = self.rows.iter().filter(|r| r.selected).count();
        let mut spans = vec![
            Span::styled(
                " homelinter ",
                Style::new().fg(Color::Black).bg(Color::Cyan),
            ),
            Span::raw("  "),
            Span::styled(self.state.clone(), Style::new().fg(state_color).bold()),
            Span::raw(format!("  root: {}  ", shorten(&self.root, 30))),
            Span::raw(format!(
                "batch: {}  shown: {}  selected: {}",
                self.rows.len(),
                self.filtered_len(),
                selected
            )),
        ];
        if self.metadata_total > 0 {
            spans.push(Span::styled(
                format!(
                    "  metadata: {}/{}",
                    self.metadata_completed, self.metadata_total
                ),
                Style::new().fg(Color::Yellow),
            ));
        }
        Line::from(spans)
    }

    fn filter_line(&self) -> Line<'static> {
        let t = self.filter.typ.as_deref().unwrap_or("all");
        let k = self.filter.kind.as_deref().unwrap_or("all");
        let s = self.filter.status.as_deref().unwrap_or("all");
        Line::from(vec![
            Span::styled(" filter ", Style::new().fg(Color::Black).bg(Color::Blue)),
            Span::raw(format!(" t:type={t}  k:kind={k}  u:status={s}  c:clear  ")),
            Span::styled("space", Style::new().fg(Color::Green)),
            Span::raw(" toggle  "),
            Span::styled("a", Style::new().fg(Color::Green)),
            Span::raw(" all  "),
            Span::styled("A", Style::new().fg(Color::Green)),
            Span::raw(" none"),
        ])
    }

    fn draw_scan(&mut self, f: &mut Frame) {
        let area = f.area();
        let spinner = "-\\|/"
            .chars()
            .nth((self.frame % 4) as usize)
            .unwrap_or('-');
        let lines = vec![
            Line::from(vec![Span::styled(
                " Scanning files",
                Style::new().fg(Color::Cyan).bold(),
            )]),
            Line::from(""),
            Line::from(format!(" state: {}  {spinner}", self.state)),
            Line::from(format!(" root:  {}", self.root)),
            Line::from(format!(
                " hidden: {}   target overrides: {}",
                if self.hidden { "yes" } else { "no" },
                self.target_paths.len()
            )),
        ];
        let mut text = Text::from(lines);
        if !self.progress_phase.is_empty() {
            text.lines.push(Line::from(""));
            text.lines.push(Line::from(format!(
                " phase: {}   scanned: {}   elapsed: {} ms",
                self.progress_phase, self.progress_scanned, self.progress_elapsed_ms
            )));
        }
        if self.metadata_total > 0 {
            text.lines.push(Line::from(format!(
                " metadata: {}/{} resolved",
                self.metadata_completed, self.metadata_total
            )));
        }
        text.lines.push(Line::from(""));
        text.lines
            .push(Line::from(" q/Esc: cancel and quit   ?: help"));
        f.render_widget(Paragraph::new(text).block(block("Scanning")), area);
    }

    fn draw_browse(&mut self, f: &mut Frame) {
        let area = f.area();
        let [header, filter_row, list_area, detail_area, foot] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(4),
            Constraint::Length(9),
            Constraint::Length(3),
        ])
        .areas(area);
        f.render_widget(Paragraph::new(self.header_line()), header);
        f.render_widget(Paragraph::new(self.filter_line()), filter_row);

        let filtered = self.filtered();
        let n = filtered.len();
        if self.cursor >= n {
            self.cursor = if n > 0 { n - 1 } else { 0 };
        }
        let vis = list_area.height as usize;
        if vis > 0 {
            if self.cursor < self.list_offset {
                self.list_offset = self.cursor;
            }
            let window_end = self.list_offset.saturating_add(vis);
            if self.cursor >= window_end {
                self.list_offset = self.cursor.saturating_add(1).saturating_sub(vis);
            }
        }
        let shown: Vec<&Row> = filtered
            .iter()
            .skip(self.list_offset)
            .take(vis)
            .filter_map(|&i| self.rows.get(i))
            .collect();

        let items: Vec<ListItem> = shown
            .iter()
            .map(|r| ListItem::new(row_label(r, &self.root, self.frame)))
            .collect();
        let title = format!(" Suggestions ({n} matching) ");
        let list = List::new(items)
            .block(Block::bordered().title(title))
            .highlight_style(Style::new().fg(Color::Yellow).add_modifier(Modifier::BOLD));
        let mut state = ratatui::widgets::ListState::default();
        if !shown.is_empty() {
            let pos = self
                .cursor
                .saturating_sub(self.list_offset)
                .min(shown.len() - 1);
            state.select(Some(pos));
        }
        f.render_stateful_widget(list, list_area, &mut state);

        let detail = match self
            .cursor
            .checked_sub(self.list_offset)
            .and_then(|i| shown.get(i))
        {
            Some(row) => detail_text(row),
            None => Text::from(vec![Line::from(
                "No suggestions in this view. Press n to scan another root or q to quit.",
            )]),
        };
        f.render_widget(
            Paragraph::new(detail)
                .block(block("Detail"))
                .wrap(Wrap { trim: false }),
            detail_area,
        );

        self.draw_foot(f, foot);
    }

    fn draw_run(&mut self, f: &mut Frame) {
        let area = f.area();
        let spinner = "-\\|/"
            .chars()
            .nth((self.frame % 4) as usize)
            .unwrap_or('-');
        let mut text = Text::from(vec![
            Line::from(vec![Span::styled(
                " Running",
                Style::new().fg(Color::Magenta).bold(),
            )]),
            Line::from(format!(" state: {}  {spinner}", self.state)),
            Line::from(""),
        ]);
        let max = area.height.saturating_sub(6) as usize;
        let start = self.run.log.len().saturating_sub(max);
        for entry in self.run.log.iter().skip(start) {
            text.lines.push(Line::from(entry.as_str()));
        }
        text.lines.push(Line::from(""));
        text.lines
            .push(Line::from(" q/Esc: quit (cancels the run)"));
        f.render_widget(Paragraph::new(text).block(block("Execution")), area);
    }

    fn draw_report(&mut self, f: &mut Frame) {
        let area = f.area();
        let mut text = Text::from(vec![Line::from(vec![Span::styled(
            " Run finished",
            Style::new().fg(Color::Green).bold(),
        )])]);
        if let Some(msg) = &self.run.failed_message {
            text.lines.push(Line::from(msg.as_str()));
        }
        if let Some(report) = &self.run.report {
            let partial = report["partial"].as_bool().unwrap_or(false);
            let entries = report["entries"].as_array().cloned().unwrap_or_default();
            let mut counts: BTreeMap<String, usize> = BTreeMap::new();
            let mut entry_lines: Vec<String> = Vec::new();
            for e in &entries {
                let result = str_field(e, "result");
                *counts.entry(result.clone()).or_insert(0) += 1;
                let line = format!("[{}] {}", result, str_field(e, "suggestion_id"));
                entry_lines.push(line);
            }
            let summary = if counts.is_empty() {
                "no entries executed".to_string()
            } else {
                counts
                    .iter()
                    .map(|(k, v)| format!("{k}={v}"))
                    .collect::<Vec<_>>()
                    .join("  ")
            };
            text.lines.push(Line::from(format!(
                " mode: execution   partial: {}   root: {}",
                partial,
                report["scan_root"].as_str().unwrap_or("")
            )));
            text.lines.push(Line::from(summary));
            text.lines.push(Line::from(""));
            for line in entry_lines {
                text.lines.push(Line::from(line));
            }
        } else {
            text.lines
                .push(Line::from("no report was stored for this run"));
        }
        text.lines.push(Line::from(""));
        text.lines
            .push(Line::from(" any key: return to the suggestion browser"));
        f.render_widget(
            Paragraph::new(text)
                .block(block("Report"))
                .wrap(Wrap { trim: false }),
            area,
        );
    }

    fn draw_config(&self, f: &mut Frame) {
        let area = f.area();
        let text = self
            .config_doc
            .as_ref()
            .and_then(|v| serde_json::to_string_pretty(v).ok())
            .unwrap_or_else(|| "configuration unavailable".to_string());
        let mut lines: Vec<Line> = text
            .lines()
            .map(|line| Line::from(line.to_string()))
            .collect();
        lines.push(Line::from(""));
        lines.push(Line::from(
            "Up/Down scroll  v view cache  C clear cache  Esc/q/c return to suggestions",
        ));
        f.render_widget(
            Paragraph::new(Text::from(lines))
                .block(block("Configuration (read-only)"))
                .scroll((self.config_scroll, 0)),
            area,
        );
    }

    fn draw_cache(&self, f: &mut Frame) {
        let area = f.area();
        let kind = self.cache_kind.as_deref().unwrap_or("all");
        let shown_lo = if self.cache_entries.is_empty() {
            0
        } else {
            self.cache_offset + 1
        };
        let shown_hi = self.cache_offset + self.cache_entries.len();
        let title = format!(
            "Metadata cache (read-only) - kind: {kind}  {shown_lo}-{shown_hi} of {}",
            self.cache_total
        );
        let mut lines: Vec<Line> = Vec::new();
        if self.cache_entries.is_empty() {
            lines.push(Line::from("no cached entries"));
        }
        for r in &self.cache_entries {
            lines.push(Line::from(vec![
                Span::styled(format!("{} ", r.kind), Style::new().fg(Color::Cyan).bold()),
                Span::styled(
                    format!("{} ", r.created_at),
                    Style::new().fg(Color::DarkGray),
                ),
                Span::styled(r.provider.clone(), Style::new().fg(Color::Yellow)),
            ]));
            lines.push(Line::from(format!("   value: {}", r.value)));
            lines.push(Line::from(vec![
                Span::styled("   ", Style::new().fg(Color::DarkGray)),
                Span::styled(
                    format!(
                        "key: {}  input: {}",
                        &r.cache_key[..r.cache_key.len().min(16)],
                        &r.input_hash[..r.input_hash.len().min(16)],
                    ),
                    Style::new().fg(Color::DarkGray),
                ),
            ]));
        }
        lines.push(Line::from(""));
        lines.push(Line::from(
            "Up/Down scroll  n/p page  k kind  r reload  C clear cache  Esc/q/c return",
        ));
        f.render_widget(
            Paragraph::new(Text::from(lines))
                .block(block(&title))
                .scroll((self.cache_scroll, 0))
                .wrap(Wrap { trim: false }),
            area,
        );
    }

    fn draw_foot(&self, f: &mut Frame, area: Rect) {
        let mut lines = vec![Line::from(
            " j/arrows move  space select  a all  A clear  t/k/u/c filters  m re-request  b rebuild  x execute  e export  n scan  g config  ? help  q quit",
        )];
        if let Some(t) = &self.toast {
            lines.push(Line::from(vec![Span::styled(
                t.as_str(),
                Style::new().fg(Color::Red),
            )]));
        }
        f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), area);
    }

    fn draw_prompt(&self, f: &mut Frame, p: &Prompt) {
        let area = f.area();
        let rect = centered_rect(80, 12, area);
        let block = Block::bordered().title(p.title.clone());
        let inner = block.inner(rect);
        f.render_widget(Clear, rect);
        f.render_widget(block, rect);
        f.render_widget(Paragraph::new(p.value.clone()), inner);
        let col = (p.cursor as u16).min(inner.width.saturating_sub(1));
        f.set_cursor_position(Position::new(inner.x.saturating_add(col), inner.y));
    }

    fn draw_confirm(&self, f: &mut Frame, c: &Confirm) {
        let area = f.area();
        let rect = centered_rect(70, 15, area);
        let block = Block::bordered().title(c.title.clone());
        let inner = block.inner(rect);
        f.render_widget(Clear, rect);
        f.render_widget(block, rect);
        let keys = match c.yes {
            ConfirmAction::ProceedStale => " y: proceed anyway    r: rescan    n/Esc: cancel",
            _ => " y: yes    n/Esc: no",
        };
        let text = Text::from(vec![
            Line::from(c.message.clone()),
            Line::from(""),
            Line::from(keys),
        ]);
        f.render_widget(Paragraph::new(text).alignment(Alignment::Center), inner);
    }

    fn draw_help(&self, f: &mut Frame) {
        let area = f.area();
        let text = Text::from(vec![
            Line::from(" homelinter TUI - keys"),
            Line::from(""),
            Line::from(" j or Up/Down         move through suggestions"),
            Line::from(" PageUp/PageDown      page through the list"),
            Line::from(" space               toggle the highlighted suggestion"),
            Line::from(" a                   select all filtered suggestions"),
            Line::from(" A                   clear the selection"),
            Line::from(" t                   cycle the type filter (all/move/delete)"),
            Line::from(" k                   cycle the asset-kind filter"),
            Line::from(" u                   cycle the status filter"),
            Line::from(" c                   clear all filters"),
            Line::from(
                " m                   re-request the highlighted suggestion's provider metadata",
            ),
            Line::from(" x                   execute selection (asks to confirm)"),
            Line::from(" e                   export selection as a shell script"),
            Line::from(" n                   scan another root (Tab toggles hidden)"),
            Line::from(" g                   view the configuration (read-only)"),
            Line::from(" v / l               view the metadata cache (from config)"),
            Line::from(" ?                   show this help"),
            Line::from(" q / Esc / Ctrl-C     quit"),
            Line::from(""),
            Line::from(" Filtering affects navigation and bulk selection only."),
            Line::from(
                " Dependent operations (e.g. symlink repairs) run with their parent suggestion.",
            ),
        ]);
        f.render_widget(Clear, area);
        f.render_widget(Paragraph::new(text).block(block("Help")), area);
    }
}

// ---------------------------------------------------------------------------
// Small rendering helpers
// ---------------------------------------------------------------------------

fn block(title: &str) -> Block<'static> {
    Block::bordered().title(title.to_string())
}

fn centered_rect(width_pct: u16, height_pct: u16, area: Rect) -> Rect {
    let [v] = Layout::vertical([Constraint::Percentage(height_pct)])
        .flex(Flex::Center)
        .areas(area);
    let [h] = Layout::horizontal([Constraint::Percentage(width_pct)])
        .flex(Flex::Center)
        .areas(v);
    h
}

fn shorten(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let keep = max.saturating_sub(3);
    let tail_text: String = s
        .chars()
        .rev()
        .take(keep)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    format!("...{tail_text}")
}

/// One-line label for the suggestions list, ASCII only.
///
/// A suggestion whose provider metadata is still in flight has no stable
/// destination yet, so it is shown as an animated "scanning <filename>"
/// placeholder, using `frame` to pick the spinner glyph.
fn row_label(r: &Row, root: &str, frame: u64) -> Line<'static> {
    let marker = if r.selected { "[x]" } else { "[ ]" };
    let color = if r.selected {
        Color::Green
    } else {
        Color::White
    };
    if r.pending_meta {
        let spinner = "-\\|/".chars().nth((frame % 4) as usize).unwrap_or('-');
        let text = format!("{marker} {spinner} scanning {}", file_name(&r.target));
        return Line::from(Span::styled(text, Style::new().fg(color)));
    }
    let name = path_label(&r.target, root);
    let dest = if r.new_path.is_empty() {
        "delete".to_string()
    } else {
        path_label(&r.new_path, root)
    };
    let status = if r.status == "pending" {
        String::new()
    } else {
        format!(" ({})", r.status)
    };
    let reject = if r.rejection.is_empty() {
        String::new()
    } else {
        " [rejected]".to_string()
    };
    let text = format!(
        "{marker} {:<6} {:<8} {name} -> {dest}{status}{reject}",
        r.typ, r.kind
    );
    Line::from(Span::styled(text, Style::new().fg(color)))
}

/// Last path segment, for the compact "scanning <filename>" placeholder.
fn file_name(path: &str) -> String {
    path.rsplit('/').next().unwrap_or(path).to_string()
}

/// A path as shown in a suggestion row: made relative to the scan root when
/// it lives under it, kept verbatim up to three segments, and truncated to
/// the last three segments with an ellipsis prefix beyond that. Paths
/// outside the scan root (e.g. a destination resolved elsewhere under
/// $HOME) fall back to the absolute path under the same truncation rule.
fn path_label(path: &str, root: &str) -> String {
    let rel = relative_to_scan_root(path, root);
    let parts: Vec<&str> = rel.split('/').filter(|p| !p.is_empty()).collect();
    if parts.len() <= 3 {
        return rel.to_string();
    }
    let last = &parts[parts.len() - 3..];
    format!(".../{}", last.join("/"))
}

/// Strip the scan root prefix when `path` is under it; else the path as-is.
fn relative_to_scan_root(path: &str, root: &str) -> String {
    let base = root.trim_end_matches('/');
    if base.is_empty() || base == "/" {
        return path.to_string();
    }
    if path == base {
        return "/".to_string();
    }
    if let Some(rest) = path.strip_prefix(&format!("{base}/")) {
        return rest.to_string();
    }
    path.to_string()
}

fn detail_text(r: &Row) -> Text<'static> {
    let mk = |label: &str, value: &str| {
        Line::from(vec![
            Span::styled(format!("{label}: "), Style::new().fg(Color::DarkGray)),
            Span::raw(value.to_string()),
        ])
    };
    let mut lines: Vec<Line> = Vec::new();
    lines.push(mk("id", &r.id));
    lines.push(Line::from(vec![
        Span::styled("type: ", Style::new().fg(Color::DarkGray)),
        Span::raw(r.typ.clone()),
        Span::styled("  kind: ", Style::new().fg(Color::DarkGray)),
        Span::raw(r.kind.clone()),
        Span::styled("  status: ", Style::new().fg(Color::DarkGray)),
        Span::styled(r.status.clone(), Style::new().fg(status_color(&r.status))),
    ]));
    lines.push(mk("target", &r.target));
    if !r.new_path.is_empty() {
        lines.push(mk("new path", &r.new_path));
    }
    if !r.reason.is_empty() {
        lines.push(mk("reason", &r.reason));
    }
    for d in &r.dependents {
        lines.push(Line::from(vec![
            Span::styled("dependent: ", Style::new().fg(Color::DarkGray)),
            Span::raw(d.clone()),
        ]));
    }
    if !r.rejection.is_empty() {
        lines.push(Line::from(vec![
            Span::styled("rejected: ", Style::new().fg(Color::Red)),
            Span::raw(r.rejection.clone()),
        ]));
    }
    if r.pending_meta {
        lines.push(Line::from(vec![
            Span::styled("metadata: ", Style::new().fg(Color::DarkGray)),
            Span::styled(
                "pending (m to re-request, b to rebuild)",
                Style::new().fg(Color::Yellow),
            ),
        ]));
    }
    Text::from(lines)
}

fn status_color(status: &str) -> Color {
    match status {
        "executed" => Color::Green,
        "partial" => Color::Yellow,
        "failed" => Color::Red,
        "skipped" => Color::Yellow,
        "rejected" => Color::Red,
        _ => Color::White,
    }
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

/// Run the TUI against an already-healthy embedded service, then return.
/// The caller (driver) performs the graceful shutdown afterwards.
pub async fn run(client: &Client) -> Result<u8, String> {
    if !io::stdin().is_terminal() {
        return Err(
            "the tui requires a terminal; run a plain scan for a summary, or use --execute or --export to act without review"
                .to_string(),
        );
    }
    let (cols, rows) =
        crossterm::terminal::size().map_err(|e| format!("cannot read terminal size: {e}"))?;
    if cols < MIN_COLS || rows < MIN_ROWS {
        return Err(format!(
            "the tui requires at least {MIN_COLS}x{MIN_ROWS}; the terminal is {cols}x{rows}"
        ));
    }

    // Seed the scan form from the service's current settings; the form then
    // writes them back through PUT /config when a scan is started.
    let cfg = client
        .config()
        .await
        .map_err(|e| format!("cannot read settings: {e}"))?;
    let root = cfg["scan"]["root"]
        .as_str()
        .map(PathBuf::from)
        .or_else(homelinter_model::path::home_dir)
        .ok_or_else(|| "cannot determine the scan root".to_string())?;
    let hidden = cfg["scan"]["hidden"].as_bool().unwrap_or(false);
    let target_paths: BTreeMap<String, String> = cfg["target_paths"]
        .as_object()
        .map(|o| {
            o.iter()
                .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                .collect()
        })
        .unwrap_or_default();

    enable_raw_mode().map_err(|e| format!("cannot enable raw mode: {e}"))?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)
        .map_err(|e| format!("cannot enter the alternate screen: {e}"))?;
    let _guard = Restore;

    let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))
        .map_err(|e| format!("cannot start the terminal backend: {e}"))?;

    let mut app = App::new(client.clone(), root, hidden, target_paths);
    app.start_scan()
        .await
        .map_err(|e| format!("cannot start the initial scan: {e}"))?;

    let mut keys = EventStream::new();
    let mut sse: Option<SseReader> = None;
    let mut ticker = tokio::time::interval(Duration::from_millis(120));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        tokio::select! {
            k = keys.next() => {
                match k {
                    Some(Ok(Event::Key(key))) => app.on_key(key).await,
                    Some(Ok(Event::Resize(_, _))) => {}
                    Some(Ok(_)) => {}
                    Some(Err(e)) => app.toast = Some(format!("terminal input error: {e}")),
                    None => {
                        // Terminal closed under us.
                        app.cancel_in_flight().await;
                        app.quit = true;
                    }
                }
            }
            ev = read_sse(&mut sse, client) => {
                if let Some(ev) = ev {
                    app.on_sse(ev).await;
                }
            }
            _ = ticker.tick() => {
                app.poll().await;
            }
        }
        if app.quit {
            break;
        }
        let _ = terminal.draw(|f| app.draw(f));
    }

    let _ = terminal.flush();
    Ok(0)
}

/// Read the next SSE event, reconnecting when the stream drops.
async fn read_sse(reader: &mut Option<SseReader>, client: &Client) -> Option<SseEvent> {
    loop {
        match reader {
            Some(r) => match r.next_event().await {
                Ok(ev) => return Some(ev),
                Err(_) => {
                    *reader = None;
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            },
            None => match client.events().await {
                Ok(r) => *reader = Some(r),
                Err(_) => {
                    tokio::time::sleep(Duration::from_millis(200)).await;
                    return None;
                }
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn sample_row() -> Row {
        Row::from_json(&json!({
            "id": "s-abc",
            "type": "move",
            "asset_kind": "photo",
            "status": "pending",
            "selected": false,
            "target": "/home/a/IMG_1.jpg",
            "new_path": "/home/a/Pictures/2024/IMG_1.jpg",
            "reason": "organize",
            "rejection_reason": null,
            "dependent_operations": [
                {"type": "repair_symlink", "link_path": "/home/a/L", "replacement_target": "/home/a/Pictures/2024"}
            ]
        }))
    }

    #[test]
    fn parses_row_fields_and_dependents() {
        let r = sample_row();
        assert_eq!(r.id, "s-abc");
        assert_eq!(r.typ, "move");
        assert_eq!(r.kind, "photo");
        assert_eq!(r.new_path, "/home/a/Pictures/2024/IMG_1.jpg");
        assert_eq!(r.dependents.len(), 1);
        assert!(r.dependents[0].contains("repair_symlink"));
        assert!(r.is_executable());
    }

    #[test]
    fn pending_metadata_is_not_executable() {
        let mut r = sample_row();
        r.pending_meta = true;
        assert!(!r.is_executable());
        r.pending_meta = false;
        assert!(r.is_executable());
    }

    #[test]
    fn filter_predicate_on_type_kind_status() {
        let r = sample_row();
        let matched = |f: &Filter| -> bool {
            if let Some(t) = &f.typ {
                if r.typ != *t {
                    return false;
                }
            }
            if let Some(k) = &f.kind {
                if r.kind != *k {
                    return false;
                }
            }
            if let Some(s) = &f.status {
                if r.status != *s {
                    return false;
                }
            }
            true
        };
        let f = Filter {
            typ: Some("move".to_string()),
            kind: None,
            status: None,
        };
        assert!(matched(&f));
        let f2 = Filter {
            typ: Some("delete".to_string()),
            ..Default::default()
        };
        assert!(!matched(&f2));
        let f3 = Filter {
            typ: None,
            kind: Some("audio".to_string()),
            status: None,
        };
        assert!(!matched(&f3));
        let f4 = Filter {
            typ: None,
            kind: None,
            status: Some("executed".to_string()),
        };
        assert!(!matched(&f4));
    }

    #[test]
    fn path_label_is_scan_root_relative_and_ellipsis_truncated() {
        let root = "/home/a";
        assert_eq!(path_label("/home/a/IMG_1.jpg", root), "IMG_1.jpg");
        assert_eq!(path_label("/home/a/raw/IMG_1.jpg", root), "raw/IMG_1.jpg");
        assert_eq!(
            path_label("/home/a/Pictures/2024/IMG_1.jpg", root),
            "Pictures/2024/IMG_1.jpg"
        );
        assert_eq!(
            path_label("/home/a/Pictures/2024/03/IMG_1.jpg", root),
            ".../2024/03/IMG_1.jpg"
        );
        assert_eq!(path_label("/home/a", root), "/");
        assert_eq!(path_label("/other/x.jpg", root), "/other/x.jpg");
        assert_eq!(
            path_label("/home/a/Pictures/2024/IMG_1.jpg", ""),
            ".../Pictures/2024/IMG_1.jpg"
        );
    }

    #[test]
    fn shorten_elides_the_middle() {
        let long = "x".repeat(80);
        let s = shorten(&long, 40);
        assert_eq!(s.chars().count(), 40);
        assert!(s.starts_with("..."));
    }

    #[test]
    fn row_label_is_ascii_and_marks_selection() {
        let r = sample_row();
        let label = row_label(&r, "/home/a", 0).to_string();
        assert!(label.starts_with("[ ]"));
        assert!(label.contains("IMG_1.jpg -> Pictures/2024/IMG_1.jpg"));
        assert!(!label.contains('\u{2192}'));
        let mut sel = r.clone();
        sel.selected = true;
        assert!(row_label(&sel, "/home/a", 0).to_string().starts_with("[x]"));
    }

    #[test]
    fn row_label_shows_scanning_placeholder_for_pending_metadata() {
        let mut r = sample_row();
        r.pending_meta = true;
        let label = row_label(&r, "/home/a", 0).to_string();
        assert_eq!(label, "[ ] - scanning IMG_1.jpg");
        let spun = row_label(&r, "/home/a", 1).to_string();
        assert_eq!(spun, "[ ] \\ scanning IMG_1.jpg");
        r.pending_meta = false;
        assert!(row_label(&r, "/home/a", 0).to_string().contains(" -> "));
    }

    #[test]
    fn prompt_editing_moves_cursor() {
        let mut p = Prompt {
            title: "t".to_string(),
            kind: PromptKind::ExportPath,
            value: "abcd".to_string(),
            cursor: 2,
        };
        // Insert in the middle.
        edit_test(
            &mut p,
            KeyEvent::new(KeyCode::Char('Z'), KeyModifiers::NONE),
        );
        assert_eq!(p.value, "abZcd");
        assert_eq!(p.cursor, 3);
        // Backspace deletes the character before the cursor (the inserted Z).
        edit_test(
            &mut p,
            KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE),
        );
        assert_eq!(p.value, "abcd");
        assert_eq!(p.cursor, 2);
        // Home/End.
        edit_test(&mut p, KeyEvent::new(KeyCode::Home, KeyModifiers::NONE));
        assert_eq!(p.cursor, 0);
        edit_test(&mut p, KeyEvent::new(KeyCode::End, KeyModifiers::NONE));
        assert_eq!(p.cursor, p.value.len());
    }

    fn edit_test(p: &mut Prompt, key: KeyEvent) {
        match key.code {
            KeyCode::Char(ch) => {
                if !ch.is_control() {
                    p.value.insert(p.cursor, ch);
                    p.cursor += 1;
                }
            }
            KeyCode::Backspace => {
                if p.cursor > 0 {
                    p.cursor -= 1;
                    p.value.remove(p.cursor);
                }
            }
            KeyCode::Home => p.cursor = 0,
            KeyCode::End => p.cursor = p.value.len(),
            _ => {}
        }
    }
}
