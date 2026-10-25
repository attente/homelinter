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
//! selection, a manual metadata editor dialog, an editable scan-settings
//! form that saves through the configuration API, a live operation log
//! during execution, a report view when a run finishes, and a read-only
//! configuration view with a metadata-cache viewer. Confirmation and
//! text-input dialogs are modal overlays. A terminal of at least 80x24 is
//! required.

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
    /// The retained keeper for a duplicate delete (`metadata.duplicate_of`);
    /// empty when absent.
    duplicate_of: String,
    reason: String,
    rejection: String,
    dependents: Vec<String>,
    pending_meta: bool,
    /// Destination fields backed by saved manual overrides (empty for
    /// deletes and for moves with no saved edits).
    manual_overrides: Vec<String>,
    /// Effective metadata values the manual editor starts from.
    metadata_location: String,
    metadata_subject: String,
    metadata_author: String,
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
            duplicate_of: v["metadata"]["duplicate_of"]
                .as_str()
                .unwrap_or("")
                .to_string(),
            reason: str_field(v, "reason"),
            rejection: v["rejection_reason"].as_str().unwrap_or("").to_string(),
            pending_meta: v["metadata_pending"].as_bool().unwrap_or(false),
            manual_overrides: v["manual_overrides"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|f| f.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default(),
            metadata_location: v["metadata"]["location"].as_str().unwrap_or("").to_string(),
            metadata_subject: v["metadata"]["subject"].as_str().unwrap_or("").to_string(),
            metadata_author: v["metadata"]["author"].as_str().unwrap_or("").to_string(),
            dependents,
        }
    }

    fn is_executable(&self) -> bool {
        (self.status == "pending" || self.status == "executed") && !self.pending_meta
    }

    fn is_selectable(&self) -> bool {
        self.status != "executed" && self.status != "rejected"
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
    /// `Some(true)` keeps only suggestions with saved manual overrides,
    /// `Some(false)` keeps only the rest.
    manual: Option<bool>,
    /// Case-insensitive substring matched against each row's path fields
    /// (source target, move destination, duplicate keeper). Persisted to
    /// `[ui].path_filter` through the settings path, so it survives scans
    /// and TUI restarts; the other dimensions reset with each scan.
    path: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    Scan,
    Browse,
    Settings,
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
    ClearOverrides(String),
}

struct Confirm {
    title: String,
    message: String,
    yes: ConfirmAction,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PromptKind {
    ExportPath,
    PathFilter,
    /// Edit one scan-settings field by index.
    SettingText(usize),
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

/// One persisted scan phase row. A phase returning after another phase ran
/// in between starts a new row, and the service's one-second heartbeat
/// updates the active row in place instead of appending a duplicate.
#[derive(Debug, Clone)]
struct ScanStage {
    phase: String,
    detail: Option<String>,
    scanned: u64,
    total: Option<u64>,
    elapsed_ms: u64,
    done: bool,
}

impl ScanStage {
    fn label(&self) -> String {
        let detail = self
            .detail
            .as_deref()
            .map(|d| format!(" ({d})"))
            .unwrap_or_default();
        let scanned = match self.total {
            Some(t) => format!("{}/{}", self.scanned, t),
            None => format!("{}", self.scanned),
        };
        format!(
            "{}{}  {}, elapsed={}s",
            self.phase,
            detail,
            scanned,
            self.elapsed_ms / 1000
        )
    }
}

/// One row in the scan-settings form.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SettingKind {
    Text,
    Bool,
    Secret,
}

#[derive(Debug, Clone)]
struct SettingField {
    section: &'static str,
    label: &'static str,
    path: String,
    kind: SettingKind,
    value: String,
    original: String,
    bool_value: bool,
    original_bool: bool,
}

/// Editable view of the durable scan configuration. Only fields that changed
/// are written back through `PUT /config`; blank optional fields are sent as
/// null so the key is deleted and the built-in default applies again.
struct SettingsForm {
    fields: Vec<SettingField>,
    cursor: usize,
    offset: usize,
}

impl SettingsForm {
    fn from_config(doc: &Value) -> SettingsForm {
        let mut fields = Vec::new();
        macro_rules! text {
            ($section:expr, $label:expr, $path:expr $(,)?) => {{
                let value = setting_text(doc, $path);
                fields.push(SettingField {
                    section: $section,
                    label: $label,
                    path: $path.to_string(),
                    kind: SettingKind::Text,
                    original: value.clone(),
                    value,
                    bool_value: false,
                    original_bool: false,
                });
            }};
        }
        macro_rules! secret {
            ($section:expr, $label:expr, $path:expr $(,)?) => {{
                fields.push(SettingField {
                    section: $section,
                    label: $label,
                    path: $path.to_string(),
                    kind: SettingKind::Secret,
                    value: String::new(),
                    original: String::new(),
                    bool_value: false,
                    original_bool: false,
                });
            }};
        }
        macro_rules! toggle {
            ($section:expr, $label:expr, $path:expr, $default:expr $(,)?) => {{
                let value = cfg_get(doc, $path)
                    .and_then(Value::as_bool)
                    .unwrap_or($default);
                fields.push(SettingField {
                    section: $section,
                    label: $label,
                    path: $path.to_string(),
                    kind: SettingKind::Bool,
                    value: String::new(),
                    original: String::new(),
                    bool_value: value,
                    original_bool: value,
                });
            }};
        }

        text!("Scan", "root", "scan.root");
        toggle!("Scan", "hidden", "scan.hidden", false);
        text!("Scan", "exclude paths (one per line)", "scan.exclude_paths");
        text!("Scan", "exclude extensions", "scan.exclude_extensions");
        toggle!(
            "Scan",
            "default exclusions",
            "scan.default_exclusions",
            true
        );

        for (kind, label) in [
            ("document", "document destination"),
            ("photo", "photo destination"),
            ("video", "video destination"),
            ("audio", "audio destination"),
            ("repository", "repository destination"),
            ("archive", "archive destination"),
        ] {
            text!("Destination paths", label, &format!("target_paths.{kind}"),);
        }

        toggle!("Geocoder", "enabled", "geocoder.enabled", false);
        text!("Geocoder", "service", "geocoder.service");
        text!("Geocoder", "endpoint", "geocoder.endpoint");
        secret!("Geocoder", "api key (write-only)", "geocoder.api_key");
        text!("Geocoder", "language", "geocoder.language");
        text!("Geocoder", "contact", "geocoder.user_agent_contact");
        text!(
            "Geocoder",
            "neighbourhood cities",
            "geocoder.neighbourhood_cities",
        );
        text!("Geocoder", "rate limit (ms)", "geocoder.rate_limit_ms");

        toggle!("Description", "enabled", "description.enabled", false);
        text!("Description", "service", "description.service");
        text!("Description", "endpoint", "description.endpoint");
        text!("Description", "model", "description.model");
        secret!("Description", "api key (write-only)", "description.api_key");
        text!("Description", "max words", "description.max_words");
        text!(
            "Description",
            "rate limit (ms)",
            "description.rate_limit_ms"
        );

        text!(
            "Limits",
            "large-file threshold",
            "duplication.large_file_threshold",
        );
        text!(
            "Limits",
            "provider timeout (s)",
            "limits.provider_timeout_seconds",
        );
        text!(
            "Limits",
            "provider concurrency",
            "limits.provider_concurrency",
        );
        text!("Limits", "tool timeout (s)", "limits.tool_timeout_seconds");
        text!(
            "Limits",
            "tool stdout max (bytes)",
            "limits.tool_stdout_max_bytes",
        );
        toggle!("Limits", "cache enabled", "cache.enabled", true);

        // Show the built-in service defaults the way the SPA form does, so
        // an untouched field does not look unset when the service applies a
        // fallback anyway.
        for (path, default) in [
            ("geocoder.service", "nominatim"),
            ("description.service", "ollama"),
        ] {
            if let Some(field) = fields.iter_mut().find(|f| f.path == path) {
                if field.value.is_empty() {
                    field.value = default.to_string();
                    field.original = default.to_string();
                }
            }
        }

        for fmt in ["pdf", "docx", "odt", "epub"] {
            let path = format!("document_extraction.{fmt}");
            let value = extraction_text(doc, fmt);
            fields.push(SettingField {
                section: "Document extraction",
                label: match fmt {
                    "pdf" => "pdf (blank = default, - = disabled)",
                    "docx" => "docx (blank = default, - = disabled)",
                    "odt" => "odt (blank = default, - = disabled)",
                    _ => "epub (blank = default, - = disabled)",
                },
                path,
                kind: SettingKind::Text,
                original: value.clone(),
                value,
                bool_value: false,
                original_bool: false,
            });
        }

        SettingsForm {
            fields,
            cursor: 0,
            offset: 0,
        }
    }

    fn current(&self) -> Option<&SettingField> {
        self.fields.get(self.cursor)
    }
}

fn cfg_get<'a>(doc: &'a Value, path: &str) -> Option<&'a Value> {
    let mut cur = doc;
    for part in path.split('.') {
        cur = cur.get(part)?;
    }
    Some(cur)
}

fn setting_text(doc: &Value, path: &str) -> String {
    match cfg_get(doc, path) {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Number(n)) => n.to_string(),
        Some(Value::Bool(b)) => b.to_string(),
        Some(Value::Array(items)) => {
            let sep = if path == "scan.exclude_paths" {
                "\n"
            } else {
                ", "
            };
            items
                .iter()
                .map(|v| match v {
                    Value::String(s) => s.clone(),
                    other => other.to_string(),
                })
                .collect::<Vec<_>>()
                .join(sep)
        }
        _ => String::new(),
    }
}

fn extraction_text(doc: &Value, format: &str) -> String {
    match cfg_get(doc, &format!("document_extraction.{format}")) {
        Some(Value::Array(items)) if items.is_empty() => "-".to_string(),
        Some(Value::Array(items)) => items
            .iter()
            .map(|v| v.as_str().unwrap_or_default())
            .collect::<Vec<_>>()
            .join(" "),
        _ => String::new(),
    }
}

/// Insert `value` at a dotted path, creating intermediate objects.
fn set_json_path(root: &mut Value, path: &str, value: Value) {
    let mut parts = path.split('.').peekable();
    let mut cur = root;
    while let Some(part) = parts.next() {
        if parts.peek().is_none() {
            if let Value::Object(map) = cur {
                map.insert(part.to_string(), value);
            }
            return;
        }
        let next = cur
            .as_object_mut()
            .expect("settings path parent is an object");
        cur = next
            .entry(part.to_string())
            .or_insert_with(|| Value::Object(serde_json::Map::new()));
    }
}

/// Convert one text field's form value to the JSON the configuration API
/// expects. Blank optional values become null (delete the key); lists are
/// split on lines or commas; numeric fields are validated locally so a typo
/// does not reach the service.
fn setting_value(path: &str, text: &str) -> Result<Value, String> {
    let trimmed = text.trim();
    if let Some(_format) = path.strip_prefix("document_extraction.") {
        if trimmed.is_empty() {
            return Ok(Value::Null);
        }
        if trimmed == "-" {
            return Ok(serde_json::json!([]));
        }
        return Ok(serde_json::json!(trimmed
            .split_whitespace()
            .collect::<Vec<_>>()));
    }
    if path == "scan.exclude_paths" {
        let items: Vec<String> = trimmed
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .map(str::to_string)
            .collect();
        return Ok(if items.is_empty() {
            Value::Null
        } else {
            serde_json::json!(items)
        });
    }
    if path == "scan.exclude_extensions" || path == "geocoder.neighbourhood_cities" {
        let items: Vec<String> = trimmed
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect();
        return Ok(if items.is_empty() {
            Value::Null
        } else {
            serde_json::json!(items)
        });
    }
    const NUMERIC: [&str; 9] = [
        "geocoder.rate_limit_ms",
        "description.max_words",
        "description.rate_limit_ms",
        "duplication.large_file_threshold",
        "limits.provider_timeout_seconds",
        "limits.provider_concurrency",
        "limits.tool_timeout_seconds",
        "limits.tool_stdout_max_bytes",
        "description.max_image_dimension",
    ];
    if NUMERIC.contains(&path) {
        let n: u64 = trimmed
            .parse()
            .map_err(|_| format!("{path} must be a whole number"))?;
        return Ok(serde_json::json!(n));
    }
    Ok(if trimmed.is_empty() {
        Value::Null
    } else {
        Value::String(trimmed.to_string())
    })
}

/// Manual metadata editor over one move suggestion's location, subject, and
/// author. The service's `PUT /suggestions/{id}/metadata` body is a full
/// replacement, so every field is sent and a blank one clears it.
struct MetadataEditor {
    id: String,
    focus: usize,
    fields: [String; 3],
    cursors: [usize; 3],
}

const METADATA_LABELS: [&str; 3] = ["Location (%l)", "Subject (%s, %t)", "Author (%a)"];

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
    scan_stages: Vec<ScanStage>,
    metadata_completed: usize,
    metadata_total: usize,
    manual_total: usize,
    /// Provider cache lookups for the current batch, as reported by the
    /// service's `cache_hits`/`cache_misses` session counters.
    cache_hits: u64,
    cache_misses: u64,
    filter: Filter,
    cursor: usize,
    list_offset: usize,
    settings: Option<SettingsForm>,
    editor: Option<MetadataEditor>,
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
    warnings: Vec<String>,
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
            scan_stages: Vec::new(),
            metadata_completed: 0,
            metadata_total: 0,
            manual_total: 0,
            cache_hits: 0,
            cache_misses: 0,
            filter: Filter::default(),
            cursor: 0,
            list_offset: 0,
            settings: None,
            editor: None,
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
            warnings: Vec::new(),
            stale_action: None,
        }
    }

    // ------------------------------------------------------------------
    // API actions
    // ------------------------------------------------------------------

    /// Start a scan against the service's current settings. Durable settings
    /// are saved first by the scan-settings form, never silently here.
    async fn start_scan(&mut self) -> Result<(), String> {
        self.phase = Phase::Scan;
        self.state = "scanning".to_string();
        self.rows.clear();
        self.kinds.clear();
        self.cursor = 0;
        self.list_offset = 0;
        self.scan_stages.clear();
        self.editor = None;
        // The type/kind/status/manual filters reset with each scan; the path
        // filter persists (live and via [ui].path_filter) until cleared.
        self.filter.typ = None;
        self.filter.kind = None;
        self.filter.status = None;
        self.filter.manual = None;
        self.metadata_completed = 0;
        self.metadata_total = 0;
        self.manual_total = 0;
        self.cache_hits = 0;
        self.cache_misses = 0;
        self.client
            .start_scan(serde_json::json!({}))
            .await
            .map_err(|e| format!("cannot start scan: {e}"))?;
        Ok(())
    }

    /// Open the editable scan-settings form, seeded from the service's
    /// effective configuration so another client's changes are never
    /// clobbered by a stale local copy.
    async fn open_settings(&mut self) {
        match self.client.config().await {
            Ok(doc) => {
                self.settings = Some(SettingsForm::from_config(&doc));
                self.config_doc = Some(doc);
                self.phase = Phase::Settings;
            }
            Err(e) => self.toast = Some(format!("cannot read settings: {e}")),
        }
    }

    /// Build the minimal partial configuration for the fields the user
    /// changed, plus the scan root and hidden-file setting (always persisted
    /// with a scan, as the SPA does).
    fn settings_partial(&self) -> Result<Value, String> {
        let form = self
            .settings
            .as_ref()
            .ok_or_else(|| "the settings form is not open".to_string())?;
        let mut body = Value::Object(serde_json::Map::new());
        let mut root = String::new();
        for f in &form.fields {
            match f.kind {
                SettingKind::Bool => {
                    if f.path == "scan.hidden" || f.bool_value != f.original_bool {
                        set_json_path(&mut body, &f.path, Value::Bool(f.bool_value));
                    }
                }
                SettingKind::Secret => {
                    if !f.value.trim().is_empty() {
                        set_json_path(
                            &mut body,
                            &f.path,
                            Value::String(f.value.trim().to_string()),
                        );
                    }
                }
                SettingKind::Text => {
                    if f.path == "scan.root" {
                        root = f.value.clone();
                        continue;
                    }
                    if f.value == f.original {
                        continue;
                    }
                    let value = setting_value(&f.path, &f.value)?;
                    set_json_path(&mut body, &f.path, value);
                }
            }
        }
        let resolved = if root.trim().is_empty() {
            homelinter_model::path::home_dir()
                .ok_or_else(|| "cannot determine home directory".to_string())?
        } else {
            self.resolve_path(&root)?
        };
        let root = resolved.to_string_lossy().into_owned();
        set_json_path(&mut body, "scan.root", Value::String(root));
        Ok(body)
    }

    /// Persist the settings form and, when the save succeeds, start the scan
    /// under the configuration that was just saved.
    async fn save_settings_and_scan(&mut self) {
        let partial = match self.settings_partial() {
            Ok(p) => p,
            Err(e) => {
                self.toast = Some(e);
                return;
            }
        };
        if let Err(e) = self.client.config_put(partial).await {
            self.toast = Some(format!("cannot save scan settings: {e}"));
            return;
        }
        // Re-read the saved configuration so the header and target-path
        // count reflect what the server actually stored.
        if let Ok(doc) = self.client.config().await {
            if let Some(root) = doc["scan"]["root"].as_str() {
                self.root = root.to_string();
            }
            self.hidden = doc["scan"]["hidden"].as_bool().unwrap_or(self.hidden);
            self.target_paths = doc["target_paths"]
                .as_object()
                .map(|o| {
                    o.iter()
                        .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                        .collect()
                })
                .unwrap_or_default();
            self.config_doc = Some(doc);
        }
        self.settings = None;
        if let Err(e) = self.start_scan().await {
            self.toast = Some(e);
            self.phase = Phase::Browse;
        }
    }

    /// Apply one scan progress event to the persisted stage list.
    fn push_scan_stage(
        &mut self,
        phase: String,
        detail: Option<String>,
        scanned: u64,
        total: Option<u64>,
        elapsed_ms: u64,
    ) {
        if let Some(last) = self.scan_stages.last_mut() {
            if last.phase == phase {
                last.detail = detail;
                last.scanned = scanned;
                last.total = total;
                last.elapsed_ms = elapsed_ms;
                return;
            }
            last.done = true;
        }
        self.scan_stages.push(ScanStage {
            phase,
            detail,
            scanned,
            total,
            elapsed_ms,
            done: false,
        });
    }

    /// Cancel an in-progress scan or run but stay in the TUI, returning to
    /// the suggestion browser.
    async fn cancel_and_stay(&mut self) {
        self.cancel_in_flight().await;
        self.phase = Phase::Browse;
        self.toast = Some("cancelled in-flight work".to_string());
        if let Err(e) = self.refresh_rows().await {
            self.toast = Some(format!("cancelled; cannot refresh suggestions: {e}"));
        }
    }

    /// Open the manual metadata editor for the highlighted move suggestion.
    fn open_metadata_editor(&mut self) {
        let Some(idx) = self.filtered_index(self.cursor) else {
            return;
        };
        let row = &self.rows[idx];
        if row.typ != "move" {
            self.toast = Some("manual metadata edits apply to move suggestions".to_string());
            return;
        }
        if row.pending_meta {
            self.toast = Some("wait for the provider metadata to resolve first".to_string());
            return;
        }
        let fields = [
            row.metadata_location.clone(),
            row.metadata_subject.clone(),
            row.metadata_author.clone(),
        ];
        let cursors = [fields[0].len(), fields[1].len(), fields[2].len()];
        self.editor = Some(MetadataEditor {
            id: row.id.clone(),
            focus: 0,
            fields,
            cursors,
        });
    }

    async fn save_metadata_editor(&mut self) {
        let Some(editor) = self.editor.take() else {
            return;
        };
        let blank = |s: &str| {
            let t = s.trim();
            if t.is_empty() {
                Value::Null
            } else {
                Value::String(t.to_string())
            }
        };
        let body = serde_json::json!({
            "location": blank(&editor.fields[0]),
            "subject": blank(&editor.fields[1]),
            "author": blank(&editor.fields[2]),
        });
        match self.client.set_suggestion_metadata(&editor.id, body).await {
            Ok(v) => {
                if v["item"].is_null() {
                    self.rows.retain(|r| r.id != editor.id);
                    self.kinds = self.unique_kinds();
                    if self.cursor >= self.rows.len() {
                        self.cursor = self.rows.len().saturating_sub(1);
                    }
                    self.toast =
                        Some("metadata saved; the suggestion no longer applies".to_string());
                } else {
                    self.upsert_row(&v["item"]);
                    self.toast = Some("metadata saved".to_string());
                }
            }
            Err(e) => {
                // Keep the user's edits so a failed save can be retried.
                self.editor = Some(editor);
                self.toast = Some(format!("cannot save metadata: {e}"));
            }
        }
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
        if !self.rows[idx].is_selectable() {
            return Ok(Vec::new());
        }
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
        if !self.rows[idx].is_selectable() {
            self.toast = Some(format!(
                "{} suggestions cannot be selected",
                self.rows[idx].status
            ));
            return;
        }
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
        let row = &self.rows[idx];
        if row.typ != "move" {
            self.toast = Some("provider metadata does not shape a delete suggestion".to_string());
            return;
        }
        if !row.manual_overrides.is_empty() {
            let fields = row.manual_overrides.join(", ");
            self.confirm = Some(Confirm {
                title: "Clear manual metadata overrides?".to_string(),
                message: format!(
                    "Fetching metadata clears the manual overrides for {fields} and re-requests the provider values."
                ),
                yes: ConfirmAction::ClearOverrides(row.id.clone()),
            });
            return;
        }
        let id = row.id.clone();
        self.refresh_metadata_id(&id, false).await;
    }

    async fn refresh_metadata_id(&mut self, id: &str, clear_overrides: bool) {
        match self.client.refresh_metadata_with(id, clear_overrides).await {
            Ok(_) => {
                let note = if clear_overrides {
                    "manual overrides cleared; provider metadata requested"
                } else {
                    "provider metadata requested"
                };
                self.refresh_after_mutation().await;
                self.toast = Some(note.to_string());
            }
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

    /// Clear the selected suggestions that match the active filter, like the
    /// SPA's `clear filtered`; a selection made under another filter survives.
    async fn clear_selection(&mut self) {
        let ids: Vec<String> = self
            .filtered()
            .into_iter()
            .map(|i| &self.rows[i])
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
        filter_matches_row(&self.filter, r)
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
                    self.push_scan_stage(
                        str_field(&v, "phase"),
                        v["detail"].as_str().map(|s| s.to_string()),
                        v["scanned"].as_u64().unwrap_or(0),
                        v["total"].as_u64(),
                        v["elapsed_ms"].as_u64().unwrap_or(0),
                    );
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
            self.manual_total = sess["manual_overrides_total"].as_u64().unwrap_or(0) as usize;
            self.cache_hits = sess["cache_hits"].as_u64().unwrap_or(0);
            self.cache_misses = sess["cache_misses"].as_u64().unwrap_or(0);
            self.warnings = sess["warnings"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default();
        }
        match self.phase {
            Phase::Scan => {
                if self.state == "ready" {
                    if let Some(last) = self.scan_stages.last_mut() {
                        last.done = true;
                    }
                    if self.rows.is_empty() {
                        let _ = self.refresh_rows().await;
                    }
                    self.phase = Phase::Browse;
                } else if self.state == "failed" {
                    if let Some(last) = self.scan_stages.last_mut() {
                        last.done = true;
                    }
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
        if self.editor.is_some() {
            self.on_editor_key(key).await;
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
                KeyCode::Char('c') => self.cancel_and_stay().await,
                KeyCode::Esc | KeyCode::Char('q') => self.ask_quit(),
                _ => {}
            },
            Phase::Browse => self.on_browse_key(key).await,
            Phase::Settings => self.on_settings_key(key).await,
            Phase::Run => match key.code {
                // A run continues in the background only after a cancel;
                // quitting cancels it first.
                KeyCode::Char('c') => self.cancel_and_stay().await,
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
            KeyCode::Char('f') => {
                self.filter.manual = match self.filter.manual {
                    None => Some(true),
                    Some(true) => Some(false),
                    Some(false) => None,
                };
            }
            KeyCode::Char('c') => {
                self.filter = Filter::default();
                self.save_path_filter(None).await;
            }
            KeyCode::Char('/') => {
                let value = self.filter.path.clone().unwrap_or_default();
                self.prompt = Some(Prompt {
                    title: "Path filter (empty = all)".to_string(),
                    kind: PromptKind::PathFilter,
                    value,
                    cursor: self
                        .filter
                        .path
                        .as_deref()
                        .map(str::len)
                        .unwrap_or_default(),
                });
            }
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
            KeyCode::Char('M') => self.open_metadata_editor(),
            KeyCode::Char('b') => self.rebuild_current().await,
            KeyCode::Char('n') => self.open_settings().await,
            KeyCode::Char('?') => self.help = true,
            KeyCode::Esc | KeyCode::Char('q') => self.ask_quit(),
            _ => {}
        }
    }

    async fn on_settings_key(&mut self, key: KeyEvent) {
        let Some(form) = self.settings.as_mut() else {
            self.phase = Phase::Browse;
            return;
        };
        match key.code {
            KeyCode::Up => form.cursor = form.cursor.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => {
                if form.cursor + 1 < form.fields.len() {
                    form.cursor += 1;
                }
            }
            KeyCode::Home => form.cursor = 0,
            KeyCode::End => form.cursor = form.fields.len().saturating_sub(1),
            KeyCode::Enter | KeyCode::Char(' ') => {
                let Some(f) = form.current() else { return };
                let (idx, kind, title, value) = (
                    form.cursor,
                    f.kind,
                    format!("{}: {}", f.section, f.label),
                    f.value.clone(),
                );
                if kind == SettingKind::Bool {
                    form.fields[idx].bool_value = !form.fields[idx].bool_value;
                } else {
                    self.prompt = Some(Prompt {
                        title,
                        kind: PromptKind::SettingText(idx),
                        cursor: value.len(),
                        value,
                    });
                }
            }
            KeyCode::Char('s') => self.save_settings_and_scan().await,
            KeyCode::Char('?') => self.help = true,
            KeyCode::Esc | KeyCode::Char('q') => self.phase = Phase::Browse,
            _ => {}
        }
    }

    async fn on_editor_key(&mut self, key: KeyEvent) {
        if key.code == KeyCode::Esc {
            self.editor = None;
            return;
        }
        if key.code == KeyCode::Enter {
            self.save_metadata_editor().await;
            return;
        }
        let Some(editor) = self.editor.as_mut() else {
            return;
        };
        match key.code {
            KeyCode::Tab | KeyCode::Down => editor.focus = (editor.focus + 1) % 3,
            KeyCode::BackTab | KeyCode::Up => editor.focus = (editor.focus + 2) % 3,
            KeyCode::Char(ch) => {
                if !ch.is_control() {
                    let focus = editor.focus;
                    let text = &mut editor.fields[focus];
                    let cursor = &mut editor.cursors[focus];
                    if text.is_char_boundary(*cursor) {
                        text.insert(*cursor, ch);
                        *cursor += ch.len_utf8();
                    }
                }
            }
            KeyCode::Backspace => {
                let focus = editor.focus;
                let text = &mut editor.fields[focus];
                let cursor = &mut editor.cursors[focus];
                if *cursor > 0 && text.is_char_boundary(*cursor) {
                    if let Some(prev) = text[..*cursor].char_indices().last() {
                        text.remove(prev.0);
                        *cursor = prev.0;
                    }
                }
            }
            KeyCode::Delete => {
                let focus = editor.focus;
                let text = &mut editor.fields[focus];
                let cursor = &mut editor.cursors[focus];
                if *cursor < text.len() && text.is_char_boundary(*cursor) {
                    text.remove(*cursor);
                }
            }
            KeyCode::Left => {
                let focus = editor.focus;
                let text = &editor.fields[focus];
                let cursor = &mut editor.cursors[focus];
                if let Some(prev) = text[..*cursor].char_indices().last() {
                    *cursor = prev.0;
                }
            }
            KeyCode::Right => {
                let focus = editor.focus;
                let text = &editor.fields[focus];
                let cursor = &mut editor.cursors[focus];
                if let Some(next) = text[*cursor..].chars().next() {
                    *cursor += next.len_utf8();
                }
            }
            KeyCode::Home => {
                let focus = editor.focus;
                editor.cursors[focus] = 0;
            }
            KeyCode::End => {
                let focus = editor.focus;
                editor.cursors[focus] = editor.fields[focus].len();
            }
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
                ConfirmAction::ClearOverrides(id) => {
                    self.refresh_metadata_id(&id, true).await;
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
                PromptKind::ExportPath | PromptKind::PathFilter | PromptKind::SettingText(_) => {}
            },
            KeyCode::Enter => {
                let kind = p.kind;
                let value = p.value;
                match kind {
                    PromptKind::ExportPath => self.export_selected(value).await,
                    PromptKind::SettingText(idx) => {
                        if let Some(form) = self.settings.as_mut() {
                            if let Some(f) = form.fields.get_mut(idx) {
                                f.value = value;
                            }
                        }
                    }
                    PromptKind::PathFilter => {
                        // Empty applies no filter; the committed value is
                        // persisted to [ui].path_filter for the next launch.
                        let saved = if value.is_empty() {
                            None
                        } else {
                            Some(value.clone())
                        };
                        self.filter.path = saved.clone();
                        self.save_path_filter(saved.as_deref()).await;
                    }
                }
            }
            KeyCode::Esc => {}
            _ => self.prompt = Some(p),
        }
    }

    /// Persist the path filter through the settings path (`[ui].path_filter`).
    /// Purely client UI state: the save is excluded from the config hash, so
    /// it never makes the running batch look stale. A failed save keeps the
    /// live filter and only loses the value for the next launch.
    async fn save_path_filter(&mut self, value: Option<&str>) {
        let body = serde_json::json!({ "ui": { "path_filter": value } });
        if let Err(e) = self.client.config_put(body).await {
            self.toast = Some(format!("cannot save the path filter: {e}"));
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
            Phase::Settings => self.draw_settings(f),
            Phase::Run => self.draw_run(f),
            Phase::Report => self.draw_report(f),
            Phase::Config => self.draw_config(f),
            Phase::Cache => self.draw_cache(f),
        }
        if let Some(e) = &self.editor {
            self.draw_metadata_editor(f, e);
        } else if let Some(p) = &self.prompt {
            self.draw_prompt(f, p);
        } else if let Some(c) = &self.confirm {
            self.draw_confirm(f, c);
        } else if self.help {
            self.draw_help(f);
        }
    }

    /// Metadata progress and the provider-cache lookups behind it share one
    /// label: a hit or a miss only means something next to the progress it
    /// advanced, and one label keeps the header from growing a counter per
    /// phase. Empty when there is nothing to report yet.
    fn metadata_label(&self) -> Option<String> {
        let mut parts: Vec<String> = Vec::new();
        if self.metadata_total > 0 {
            parts.push(format!(
                "metadata: {}/{}",
                self.metadata_completed, self.metadata_total
            ));
        }
        if self.cache_hits + self.cache_misses > 0 {
            parts.push(format!(
                "{} {}, {} {}",
                self.cache_hits,
                plural(self.cache_hits, "hit", "hits"),
                self.cache_misses,
                plural(self.cache_misses, "miss", "misses")
            ));
        }
        if parts.is_empty() {
            None
        } else {
            Some(parts.join(", "))
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
                "batch: {}  shown: {}  selected: {}  manual: {}",
                self.rows.len(),
                self.filtered_len(),
                selected,
                self.manual_total
            )),
        ];
        if let Some(label) = self.metadata_label() {
            spans.push(Span::styled(
                format!("  {label}"),
                Style::new().fg(Color::Yellow),
            ));
        }
        if !self.warnings.is_empty() {
            spans.push(Span::styled(
                format!("  warnings: {}", self.warnings.len()),
                Style::new().fg(Color::Yellow),
            ));
        }
        Line::from(spans)
    }

    fn filter_line(&self) -> Line<'static> {
        let t = self.filter.typ.as_deref().unwrap_or("all");
        let k = self.filter.kind.as_deref().unwrap_or("all");
        let s = self.filter.status.as_deref().unwrap_or("all");
        let m = match self.filter.manual {
            Some(true) => "manual",
            Some(false) => "auto",
            None => "all",
        };
        let p = match &self.filter.path {
            Some(p) => {
                let mut text: String = p.chars().take(21).collect();
                if p.chars().count() > 21 {
                    text.push_str("...");
                }
                text
            }
            None => "all".to_string(),
        };
        Line::from(vec![
            Span::styled(" filter ", Style::new().fg(Color::Black).bg(Color::Blue)),
            Span::raw(format!(
                " t:type={t}  k:kind={k}  u:status={s}  f:metadata={m}  /:path={p}  c:clear  "
            )),
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
        let lines = vec![
            Line::from(vec![Span::styled(
                " Scanning files",
                Style::new().fg(Color::Cyan).bold(),
            )]),
            Line::from(""),
            Line::from(format!(" state: {}", self.state)),
            Line::from(format!(" root:  {}", self.root)),
            Line::from(format!(
                " hidden: {}   target overrides: {}",
                if self.hidden { "yes" } else { "no" },
                self.target_paths.len()
            )),
            Line::from(""),
        ];
        let mut text = Text::from(lines);
        if self.scan_stages.is_empty() {
            text.lines.push(Line::from(format!(
                " {} {}",
                progress_meter(0, None, self.frame * 250),
                "waiting for the first phase"
            )));
        } else {
            for stage in &self.scan_stages {
                let clock = stage.elapsed_ms;
                let meter = if stage.done {
                    progress_meter(
                        stage.total.unwrap_or(1),
                        Some(stage.total.unwrap_or(1)),
                        clock,
                    )
                } else {
                    progress_meter(stage.scanned, stage.total, clock)
                };
                let style = if stage.done {
                    Style::new().fg(Color::DarkGray)
                } else {
                    Style::new().fg(Color::White)
                };
                text.lines.push(Line::from(Span::styled(
                    format!(" {meter} {}", stage.label()),
                    style,
                )));
            }
        }
        if let Some(label) = self.metadata_label() {
            text.lines.push(Line::from(format!(" {label}")));
        }
        for w in &self.warnings {
            text.lines.push(Line::from(vec![Span::styled(
                format!(" warning: {w}"),
                Style::new().fg(Color::Yellow),
            )]));
        }
        text.lines.push(Line::from(""));
        text.lines
            .push(Line::from(" c: cancel   q/Esc: cancel and quit   ?: help"));
        f.render_widget(
            Paragraph::new(text)
                .block(block("Scanning"))
                .wrap(Wrap { trim: false }),
            area,
        );
    }

    fn draw_browse(&mut self, f: &mut Frame) {
        let area = f.area();
        let [header, filter_row, list_area, detail_area, foot] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(4),
            Constraint::Length(9),
            Constraint::Length(4),
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

    fn draw_settings(&mut self, f: &mut Frame) {
        let area = f.area();
        let [head, list_area, foot] = Layout::vertical([
            Constraint::Length(2),
            Constraint::Min(4),
            Constraint::Length(3),
        ])
        .areas(area);
        f.render_widget(
            Paragraph::new(Line::from(vec![Span::styled(
                " Scan settings",
                Style::new().fg(Color::Cyan).bold(),
            )])),
            head,
        );
        let Some(form) = self.settings.as_mut() else {
            return;
        };
        let mut lines: Vec<Line> = Vec::new();
        let mut cursor_line = 0usize;
        let mut last_section = "";
        for (i, field) in form.fields.iter().enumerate() {
            if field.section != last_section {
                lines.push(Line::from(Span::styled(
                    format!(" {}", field.section),
                    Style::new().fg(Color::DarkGray).bold(),
                )));
                last_section = field.section;
            }
            if i == form.cursor {
                cursor_line = lines.len();
            }
            let value = match field.kind {
                SettingKind::Bool => {
                    if field.bool_value {
                        "yes".to_string()
                    } else {
                        "no".to_string()
                    }
                }
                SettingKind::Secret => {
                    if field.value.trim().is_empty() {
                        "(unset)".to_string()
                    } else {
                        "(hidden)".to_string()
                    }
                }
                SettingKind::Text => {
                    if field.value.trim().is_empty() {
                        "(default)".to_string()
                    } else {
                        field.value.replace('\n', " | ")
                    }
                }
            };
            let marker = if i == form.cursor { ">" } else { " " };
            let style = if i == form.cursor {
                Style::new().fg(Color::Yellow).bold()
            } else {
                Style::new().fg(Color::White)
            };
            lines.push(Line::from(Span::styled(
                format!("{marker} {:<28} {}", field.label, value),
                style,
            )));
        }
        let vis = list_area.height.max(1) as usize;
        if cursor_line < form.offset {
            form.offset = cursor_line;
        }
        if cursor_line >= form.offset + vis {
            form.offset = cursor_line + 1 - vis;
        }
        f.render_widget(
            Paragraph::new(Text::from(lines))
                .block(block("Configuration (changes apply to the next scan)"))
                .scroll((form.offset as u16, 0)),
            list_area,
        );
        let foot_text = Text::from(vec![
            Line::from(" Up/Down move  Enter edit/toggle  s save and scan  Esc cancel  ? help"),
            Line::from(
                " Blank optional values clear the key and restore the built-in default; a blank extraction command does the same and '-' disables that format.",
            ),
            Line::from(" API keys are write-only: type one to store it; leave blank to keep the saved key."),
        ]);
        f.render_widget(Paragraph::new(foot_text).wrap(Wrap { trim: false }), foot);
    }

    fn draw_metadata_editor(&self, f: &mut Frame, editor: &MetadataEditor) {
        let area = f.area();
        let rect = centered_rect(80, 16, area);
        let block = Block::bordered().title("Edit metadata");
        let inner = block.inner(rect);
        f.render_widget(Clear, rect);
        f.render_widget(block, rect);
        let mut lines = vec![
            Line::from("Edit the destination metadata for this suggestion."),
            Line::from("A blank field clears the override and falls back to the provider or embedded value."),
            Line::from(""),
        ];
        for (i, label) in METADATA_LABELS.iter().enumerate() {
            lines.push(Line::from(Span::styled(
                format!(" {label}"),
                Style::new().fg(Color::DarkGray),
            )));
            let marker = if i == editor.focus { ">" } else { " " };
            let style = if i == editor.focus {
                Style::new().fg(Color::Yellow).bold()
            } else {
                Style::new().fg(Color::White)
            };
            lines.push(Line::from(Span::styled(
                format!("{marker} {}", editor.fields[i]),
                style,
            )));
        }
        lines.push(Line::from(""));
        lines.push(Line::from(
            " Tab/Up/Down: next field   Enter: save   Esc: cancel",
        ));
        f.render_widget(Paragraph::new(Text::from(lines)), inner);
        let row = 3 + editor.focus * 2;
        if row < inner.height as usize {
            let col = (2 + editor.cursors[editor.focus]) as u16;
            f.set_cursor_position(Position::new(
                inner
                    .x
                    .saturating_add(col.min(inner.width.saturating_sub(1))),
                inner.y.saturating_add(row as u16),
            ));
        }
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
            " j/arrows move  space select  a all  A clear  t/k/u/f/c filters  / path  m re-request  M metadata  b rebuild  x execute  e export  n settings+scan  g config  ? help  q quit",
        )];
        if let Some(w) = self.warnings.first() {
            let more = self.warnings.len().saturating_sub(1);
            let text = if more > 0 {
                format!(" warning: {w} (+{more} more)")
            } else {
                format!(" warning: {w}")
            };
            lines.push(Line::from(vec![Span::styled(
                text,
                Style::new().fg(Color::Yellow),
            )]));
        }
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
            Line::from(" A                   clear the selected suggestions matching the filter"),
            Line::from(" t                   cycle the type filter (all/move/delete)"),
            Line::from(" k                   cycle the asset-kind filter"),
            Line::from(" u                   cycle the status filter"),
            Line::from(" f                   cycle the metadata filter (all/manual/auto)"),
            Line::from(
                " /                   edit the path filter (matches source, destination, keeper)",
            ),
            Line::from(" c                   clear all filters"),
            Line::from(
                " m                   re-request provider metadata (clears overrides after confirming)",
            ),
            Line::from(" M                   edit the move's location, subject, and author"),
            Line::from(" b                   rebuild a move's destination from current metadata"),
            Line::from(" x                   execute selection (asks to confirm)"),
            Line::from(" e                   export selection as a shell script"),
            Line::from(" n                   edit scan settings, then scan"),
            Line::from(" g                   view the configuration (read-only)"),
            Line::from(" v / l               view the metadata cache (from config)"),
            Line::from(" ?                   show this help"),
            Line::from(" c                   cancel in-flight work and stay in the browser"),
            Line::from(" C                   clear the metadata cache (asks to confirm)"),
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

/// Singular for exactly one, plural otherwise ("0 hits", "1 hit").
fn plural<'a>(n: u64, one: &'a str, many: &'a str) -> &'a str {
    if n == 1 {
        one
    } else {
        many
    }
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
    let manual = if r.manual_overrides.is_empty() {
        String::new()
    } else {
        " [manual]".to_string()
    };
    let text = format!(
        "{marker} {:<6} {:<8} {name} -> {dest}{status}{reject}{manual}",
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
    if !r.metadata_location.is_empty() {
        lines.push(mk("location", &r.metadata_location));
    }
    if !r.metadata_subject.is_empty() {
        lines.push(mk("subject", &r.metadata_subject));
    }
    if !r.metadata_author.is_empty() {
        lines.push(mk("author", &r.metadata_author));
    }
    if !r.manual_overrides.is_empty() {
        lines.push(Line::from(vec![
            Span::styled("manual: ", Style::new().fg(Color::DarkGray)),
            Span::styled(
                format!(
                    "{} (M to edit, m to clear and re-request)",
                    r.manual_overrides.join(", ")
                ),
                Style::new().fg(Color::Yellow),
            ),
        ]));
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
    // Re-apply the saved path filter ([ui].path_filter). It persists across
    // scans, so the auto-started first scan is already filtered.
    app.filter.path = cfg["ui"]["path_filter"]
        .as_str()
        .map(str::to_string)
        .filter(|s| !s.is_empty());
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

/// The filter predicate, shared by the app state and the tests so the two
/// copies cannot diverge. Empty string fields mean "absent".
fn filter_matches_row(f: &Filter, r: &Row) -> bool {
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
    if let Some(manual) = f.manual {
        if r.manual_overrides.is_empty() == manual {
            return false;
        }
    }
    if let Some(p) = &f.path {
        let new_path = if r.new_path.is_empty() {
            None
        } else {
            Some(r.new_path.as_str())
        };
        let duplicate_of = if r.duplicate_of.is_empty() {
            None
        } else {
            Some(r.duplicate_of.as_str())
        };
        if !homelinter_model::suggestion::path_filter_matches(&r.target, new_path, duplicate_of, p)
        {
            return false;
        }
    }
    true
}

/// Width of the scan progress meter, in cells.
const METER_WIDTH: usize = 32;
/// Cells of the sliding segment shown while a phase cannot know its total.
const METER_SEGMENT: usize = 8;

/// ASCII progress meter for the scanning screen: a track with the percentage
/// once the phase knows its total, and a segment travelling back and forth
/// along the track while it does not (the discovery and symlinks walks, which
/// only learn their length when they stop). The position is derived from the
/// phase's elapsed time, so it moves with the one-second progress heartbeat
/// instead of needing its own timer, and it reverses at each end rather than
/// wrapping, so it never implies a direction or a completion.
fn progress_meter(scanned: u64, total: Option<u64>, elapsed_ms: u64) -> String {
    match total {
        Some(total) if total > 0 => {
            let done = (scanned.min(total) as u128 * METER_WIDTH as u128 / total as u128) as usize;
            let percent = (scanned.min(total) as u128 * 100 / total as u128) as usize;
            format!(
                "[{}{}] {:>3}%",
                "#".repeat(done),
                "-".repeat(METER_WIDTH - done),
                percent
            )
        }
        _ => {
            let span = METER_WIDTH - METER_SEGMENT;
            let step = (elapsed_ms / 250) as usize % (2 * span);
            let pos = if step <= span { step } else { 2 * span - step };
            let mut cells = vec!['-'; METER_WIDTH];
            for cell in cells.iter_mut().skip(pos).take(METER_SEGMENT) {
                *cell = '#';
            }
            format!("[{}]", cells.into_iter().collect::<String>())
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
    fn progress_meter_fills_to_the_known_fraction() {
        assert_eq!(
            progress_meter(0, Some(4000), 0),
            format!("[{}] {:>3}%", "-".repeat(METER_WIDTH), 0)
        );
        let half = progress_meter(2000, Some(4000), 5_000);
        assert_eq!(half.matches('#').count(), METER_WIDTH / 2);
        assert!(half.starts_with('[') && half.ends_with(" 50%"), "{half}");
        assert_eq!(
            progress_meter(4000, Some(4000), 9_000),
            format!("[{}] 100%", "#".repeat(METER_WIDTH))
        );
        // A count that runs past its total never overflows the track, and a
        // zero total is treated as unknown rather than dividing by zero.
        assert_eq!(
            progress_meter(5000, Some(4000), 0),
            format!("[{}] 100%", "#".repeat(METER_WIDTH))
        );
        assert_eq!(
            progress_meter(7, Some(0), 0).matches('#').count(),
            METER_SEGMENT
        );
    }

    #[test]
    fn progress_meter_slides_while_the_phase_has_no_total() {
        let at = |elapsed_ms| progress_meter(929_280, None, elapsed_ms);
        let start = at(0);
        assert_eq!(start.len(), METER_WIDTH + 2);
        assert_eq!(start.matches('#').count(), METER_SEGMENT);
        assert!(start.starts_with("[#"), "{start}");
        // No percentage is claimed for a phase whose length is unknown.
        assert!(!start.contains('%'), "{start}");
        // The segment moves with the phase's clock, so a long walk keeps moving
        // even while its visited count stands still between reports.
        assert_ne!(at(0), at(250));
        assert_ne!(at(250), at(1000));
        // It bounces: one traverse to the far end, then back, repeating, instead
        // of jumping to the left edge like a sweep would.
        let span = METER_WIDTH - METER_SEGMENT;
        let far_end = format!("{}]", "#".repeat(METER_SEGMENT));
        assert!(at(250 * span as u64).ends_with(&far_end));
        assert_eq!(at(250 * (2 * span) as u64), at(0));
        for step in 1..span {
            assert_eq!(
                at(250 * step as u64),
                at(250 * (2 * span - step) as u64),
                "step {step} should mirror on the way back"
            );
            assert_eq!(
                at(250 * (2 * span + step) as u64),
                at(250 * step as u64),
                "the next traverse repeats the first"
            );
        }
        assert!(at(0)
            .split('#')
            .all(|s| s.chars().all(|c| c == '-' || c == '[' || c == ']')));
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
    fn metadata_and_cache_counters_share_one_label() {
        let mut app = App::new(
            Client::new(1),
            PathBuf::from("/home/a"),
            false,
            BTreeMap::new(),
        );
        assert_eq!(app.metadata_label(), None);
        app.metadata_completed = 4;
        app.metadata_total = 9;
        assert_eq!(app.metadata_label().as_deref(), Some("metadata: 4/9"));
        app.cache_hits = 2;
        app.cache_misses = 1;
        assert_eq!(
            app.metadata_label().as_deref(),
            Some("metadata: 4/9, 2 hits, 1 miss")
        );
        app.metadata_completed = 0;
        app.metadata_total = 0;
        assert_eq!(app.metadata_label().as_deref(), Some("2 hits, 1 miss"));
        app.cache_hits = 1;
        app.cache_misses = 0;
        assert_eq!(app.metadata_label().as_deref(), Some("1 hit, 0 misses"));
        app.cache_hits = 0;
        app.cache_misses = 0;
        assert_eq!(app.metadata_label(), None);
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
    fn filter_predicate_on_type_kind_status_and_path() {
        let r = sample_row();
        let f = Filter {
            typ: Some("move".to_string()),
            ..Default::default()
        };
        assert!(filter_matches_row(&f, &r));
        let f2 = Filter {
            typ: Some("delete".to_string()),
            ..Default::default()
        };
        assert!(!filter_matches_row(&f2, &r));
        let f3 = Filter {
            kind: Some("audio".to_string()),
            ..Default::default()
        };
        assert!(!filter_matches_row(&f3, &r));
        let f4 = Filter {
            status: Some("executed".to_string()),
            ..Default::default()
        };
        assert!(!filter_matches_row(&f4, &r));

        // The path filter matches the source target, the move destination,
        // and a duplicate delete's keeper, case-insensitively; an empty
        // filter is unconstrained.
        let f = Filter {
            path: Some("IMG_1".to_string()),
            ..Default::default()
        };
        assert!(filter_matches_row(&f, &r));
        let f = Filter {
            path: Some("pictures/2024".to_string()),
            ..Default::default()
        };
        assert!(filter_matches_row(&f, &r));
        let f = Filter {
            path: Some("VIDEOS".to_string()),
            ..Default::default()
        };
        assert!(!filter_matches_row(&f, &r));
        let keeper = Row::from_json(&json!({
            "id": "s-del",
            "type": "delete",
            "asset_kind": "photo",
            "status": "pending",
            "selected": false,
            "target": "/home/a/copy.jpg",
            "metadata": {"duplicate_of": "/home/a/Original.jpg"},
        }));
        let f = Filter {
            path: Some("original".to_string()),
            ..Default::default()
        };
        assert!(filter_matches_row(&f, &keeper));
        let f = Filter {
            path: Some("copy.jpg".to_string()),
            ..Default::default()
        };
        assert!(filter_matches_row(&f, &keeper));
        let f = Filter {
            path: Some("".to_string()),
            ..Default::default()
        };
        assert!(filter_matches_row(&f, &keeper));
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

    #[test]
    fn manual_filter_selects_overridden_and_plain_rows() {
        let mut overridden = sample_row();
        overridden.manual_overrides = vec!["subject".to_string()];
        let plain = sample_row();
        let manual = Filter {
            manual: Some(true),
            ..Default::default()
        };
        assert!(filter_matches_row(&manual, &overridden));
        assert!(!filter_matches_row(&manual, &plain));
        let auto = Filter {
            manual: Some(false),
            ..Default::default()
        };
        assert!(!filter_matches_row(&auto, &overridden));
        assert!(filter_matches_row(&auto, &plain));
        assert!(filter_matches_row(&Filter::default(), &overridden));
        let label = row_label(&overridden, "/home/a", 0).to_string();
        assert!(label.ends_with(" [manual]"), "{label}");
    }

    #[test]
    fn scan_stage_rows_merge_heartbeats_and_advance() {
        let mut app = App::new(
            Client::new(1),
            PathBuf::from("/home/a"),
            false,
            BTreeMap::new(),
        );
        app.push_scan_stage("walking".to_string(), None, 0, Some(10), 0);
        app.push_scan_stage(
            "walking".to_string(),
            Some("sub".to_string()),
            5,
            Some(10),
            1000,
        );
        assert_eq!(app.scan_stages.len(), 1);
        assert_eq!(app.scan_stages[0].scanned, 5);
        app.push_scan_stage("analyzing".to_string(), None, 1, None, 2000);
        assert_eq!(app.scan_stages.len(), 2);
        assert!(app.scan_stages[0].done);
        assert!(!app.scan_stages[1].done);
        // A phase that returns starts a fresh row rather than editing the
        // older completed one.
        app.push_scan_stage("walking".to_string(), None, 0, Some(2), 3000);
        assert_eq!(app.scan_stages.len(), 3);
        assert!(app.scan_stages[1].done);
    }

    #[test]
    fn setting_values_are_typed_and_blank_clears() {
        assert_eq!(
            setting_value("scan.exclude_paths", "\n/home/a/b\n  /home/c  \n").unwrap(),
            json!(["/home/a/b", "/home/c"])
        );
        assert_eq!(
            setting_value("scan.exclude_paths", "   ").unwrap(),
            Value::Null
        );
        assert_eq!(
            setting_value("scan.exclude_extensions", "tmp, ,part").unwrap(),
            json!(["tmp", "part"])
        );
        assert_eq!(
            setting_value("document_extraction.pdf", "").unwrap(),
            Value::Null
        );
        assert_eq!(
            setting_value("document_extraction.pdf", "pdftotext -layout {path} -").unwrap(),
            json!(["pdftotext", "-layout", "{path}", "-"])
        );
        assert_eq!(
            setting_value("document_extraction.pdf", "-").unwrap(),
            json!([])
        );
        assert_eq!(
            setting_value("geocoder.rate_limit_ms", " 250 ").unwrap(),
            json!(250)
        );
        assert!(setting_value("geocoder.rate_limit_ms", "soon").is_err());
        assert_eq!(
            setting_value("target_paths.photo", " /p/%Y ").unwrap(),
            json!("/p/%Y")
        );
        assert_eq!(
            setting_value("geocoder.endpoint", "  ").unwrap(),
            Value::Null
        );
    }

    #[test]
    fn settings_form_reads_effective_config_and_writes_only_changes() {
        let doc = json!({
            "scan": {
                "root": "/home/a",
                "hidden": true,
                "exclude_paths": ["/home/a/skip"],
            },
            "target_paths": {"photo": "/home/a/Photos"},
            "geocoder": {"enabled": false, "rate_limit_ms": 1000},
            "document_extraction": {"pdf": null, "docx": []},
        });
        let mut form = SettingsForm::from_config(&doc);
        assert_eq!(setting_text(&doc, "scan.hidden"), "true");
        assert_eq!(extraction_text(&doc, "docx"), "-");
        let root = form.fields.iter().find(|f| f.path == "scan.root").unwrap();
        assert_eq!(root.value, "/home/a");

        let mut app = App::new(
            Client::new(1),
            PathBuf::from("/home/a"),
            true,
            BTreeMap::new(),
        );
        // Only the rate limit changes; unchanged fields stay out of the body.
        for field in &mut form.fields {
            if field.path == "geocoder.rate_limit_ms" {
                field.value = "500".to_string();
            }
        }
        app.settings = Some(form);
        let body = app.settings_partial().unwrap();
        assert_eq!(body["geocoder"]["rate_limit_ms"], json!(500));
        assert_eq!(body["scan"]["root"], json!("/home/a"));
        assert_eq!(body["scan"]["hidden"], json!(true));
        assert!(body["target_paths"].get("photo").is_none());
        assert!(body["scan"].get("exclude_paths").is_none());
        assert!(body["document_extraction"].is_null());
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
