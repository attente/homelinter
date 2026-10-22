//! Configuration file schema, resolution, deep merge, and validation.
//!
//! Configuration holds persistent preferences only. The scan root and the
//! hidden-file selection are deliberately not part of the schema and are
//! rejected when they appear in a config file. Precedence (highest first):
//! per-scan/per-session settings, config file values, inferred values,
//! opinionated defaults.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;
use toml::Value as TomlValue;
use toml_edit::{Array, DocumentMut, InlineTable, Item, Table, Value as EditValue};

use crate::asset_kind;
use crate::error::ApiError;
use crate::format::FormatString;

pub const DEFAULT_LARGE_FILE_THRESHOLD: u64 = 104_857_600;
/// Default word cap for a `[description]` entry (and its per-modality
/// overrides) when `max_words` is unset.
pub const DEFAULT_MAX_WORDS: u64 = 4;
/// Default description prompt template; `{words}` is substituted before the
/// request, and the full source path plus any extracted document text are
/// always included (description providers always receive the path, which
/// carries the filename, so neither needs a placeholder). When extracted
/// text exists it is delimited and placed before the prompt, with the
/// rendered prompt placed last so a local model answers the question rather
/// than the document; without text the prompt leads and the path follows.
/// The wording asks for keywords/tags rather than a sentence, because the
/// `%s` value is normalized into a lowercased, hyphenated path segment for a
/// file stem or folder name.
pub const DEFAULT_DESCRIPTION_PROMPT: &str = "Describe the subject matter of this file in at most {words} keywords, for use as a file or folder name. Do not mention the file type, extension, medium, path, or folder names (for example photo, image, video, movie, or document). Reply with only the keywords separated by spaces.";
/// Default description prompt for text documents (the `text` modality).
/// Documents often state their own title, so the model is asked to recover
/// the full original title first and only fall back to keyword extraction
/// when the content carries no title. `{words}` bounds the keyword fallback
/// (a recovered title is used verbatim, however long). As with the generic
/// prompt, extracted text is delimited and placed before the prompt, and the
/// source path is always included.
pub const DEFAULT_DOCUMENT_DESCRIPTION_PROMPT: &str = "Read this document and reply with its full original title if the document states one (for example a title, heading, or subject line), preserving the wording exactly. If the document states no title, reply with a short description of its subject matter in at most {words} keywords instead. Do not mention the file type, extension, medium, path, or folder names (for example document, PDF, or folder names). Reply with only the title or the keywords.";
/// Default longest side (pixels) for photos forwarded to a vision
/// backend; larger photos are downscaled before the request.
pub const DEFAULT_MAX_IMAGE_DIMENSION: u64 = 1024;
/// Default cap on representative frames sampled from a video for the
/// vision description provider. Frames are taken about one per second and
/// evenly distributed across the clip; a longer video is thinned to this
/// many frames.
pub const DEFAULT_MAX_VIDEO_FRAMES: u64 = 8;
/// Default upper bound on an ollama description request's context window
/// (`num_ctx`). The window is sized from the payload (images plus prompt and
/// completion budget), and this cap keeps KV-cache memory bounded. Must be
/// positive; lower it for a small-context model, raise it to fit larger
/// inputs.
pub const DEFAULT_MAX_CONTEXT: u64 = 32768;
/// Default geocoder language (ISO 639-1). Unset resolves to this code so
/// reverse-geocoded locality names read in English regardless of the
/// provider's own locale.
pub const DEFAULT_GEOCODER_LANGUAGE: &str = "en";
pub const DEFAULT_PROVIDER_TIMEOUT_SECONDS: u64 = 300;
pub const DEFAULT_TOOL_TIMEOUT_SECONDS: u64 = 30;

/// Concurrent in-flight provider requests. `1` keeps the historical
/// one-at-a-time behavior (always safe); raise it to let a backend that
/// supports parallel inference (for example a local Ollama with
/// `OLLAMA_NUM_PARALLEL > 1`) work on several units at once. Per-provider
/// rate limits are still enforced across concurrent requests.
pub const DEFAULT_PROVIDER_CONCURRENCY: u64 = 1;
pub const DEFAULT_TOOL_STDOUT_MAX_BYTES: u64 = 16 * 1024 * 1024;

/// Host platform identifier recorded in reports and audits.
pub fn platform_name() -> &'static str {
    if cfg!(target_os = "linux") {
        "linux"
    } else if cfg!(target_os = "macos") {
        "macos"
    } else if cfg!(windows) {
        "windows"
    } else {
        "unknown"
    }
}

/// The repository-root `defaults.toml`, embedded at build time. A writable
/// config file that does not exist yet is seeded from this documented dump
/// before a patch is applied, so a first saved setting produces a
/// self-documenting configuration whose diff from the dump is just the keys
/// the user changed. The dump is asserted to resolve to the built-in defaults
/// (see `crates/homelinter/tests/template.rs`), so seeding either text or
/// nothing is behaviorally the same.
pub const DEFAULT_CONFIG_TEMPLATE: &str =
    include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../defaults.toml"));

/// Default user configuration path for the host platform.
pub fn default_user_config_path() -> Option<PathBuf> {
    dirs::config_dir().map(|d| d.join("homelinter").join("config.toml"))
}

/// System-wide configuration path for the host platform.
pub fn system_config_path() -> Option<PathBuf> {
    #[cfg(unix)]
    {
        Some(PathBuf::from("/etc/homelinter/config.toml"))
    }
    #[cfg(windows)]
    {
        dirs::config_dir().map(|d| {
            d.parent()
                .map(|p| p.join("homelinter").join("config.toml"))
                .unwrap_or(d)
        })
    }
    #[cfg(not(any(unix, windows)))]
    {
        None
    }
}

/// The configuration search chain below the explicitly requested file: the
/// optional system-wide file and the optional per-user file, merged from
/// lowest to highest precedence before the explicit file. Production
/// resolves `ConfigChain::host()`; tests inject an empty chain so
/// config-file behavior stays hermetic regardless of what exists on the
/// host machine.
#[derive(Debug, Clone, Default)]
pub struct ConfigChain {
    pub system: Option<PathBuf>,
    pub user: Option<PathBuf>,
}

impl ConfigChain {
    /// The host platform's chain.
    pub fn host() -> ConfigChain {
        ConfigChain {
            system: system_config_path(),
            user: default_user_config_path(),
        }
    }
}

/// Standard per-user application-data directory: cache database and audit
/// log live here.
pub fn app_data_dir() -> Option<PathBuf> {
    dirs::data_dir().map(|d| d.join("homelinter"))
}

// ---------------------------------------------------------------------------
// Raw schema sections. Every field is optional at the section level; the
// derived defaults carry the documented fallback where the config file is
// silent. Section-level structs deny unknown fields so invalid keys are
// rejected.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LlmBackend {
    Ollama,
    Openrouter,
    Custom,
}

impl LlmBackend {
    pub fn name(self) -> &'static str {
        match self {
            LlmBackend::Ollama => "ollama",
            LlmBackend::Openrouter => "openrouter",
            LlmBackend::Custom => "custom",
        }
    }

    pub fn default_endpoint(self) -> &'static str {
        match self {
            LlmBackend::Ollama => "http://localhost:11434",
            LlmBackend::Openrouter => "https://openrouter.ai/api/v1",
            LlmBackend::Custom => "",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum GeocoderService {
    Nominatim,
    Photon,
    Google,
    Mapbox,
    Custom,
}

impl GeocoderService {
    pub fn default_endpoint(self) -> &'static str {
        match self {
            GeocoderService::Nominatim => "https://nominatim.openstreetmap.org",
            GeocoderService::Photon => "https://photon.komoot.io",
            GeocoderService::Google => "https://maps.googleapis.com/maps/api",
            GeocoderService::Mapbox => "https://api.mapbox.com",
            GeocoderService::Custom => "",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct GeocoderConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub service: Option<GeocoderService>,
    #[serde(default)]
    pub endpoint: Option<String>,
    #[serde(default)]
    pub api_key: Option<String>,
    /// ISO 639-1 code passed to the provider to shape resolved locality
    /// names. Unset falls back to [`DEFAULT_GEOCODER_LANGUAGE`].
    #[serde(default)]
    pub language: Option<String>,
    #[serde(default)]
    pub user_agent_contact: Option<String>,
    /// Cities whose `%l` uses the finer neighbourhood name instead of the
    /// settlement name; everywhere else (and when the provider supplies no
    /// city) resolves at the city level. Matched case-insensitively against
    /// the provider's `city` component. Empty (the default) uses the city
    /// everywhere.
    #[serde(default)]
    pub neighbourhood_cities: Vec<String>,
    /// Minimum interval between requests in milliseconds, enforced between
    /// request starts on the provider worker. An additional floor on top of
    /// the service's built-in pacing (see "Provider Plugins" in DESIGN.md):
    /// the effective interval is `max(built-in, rate_limit_ms)`.
    /// `0` (the default) means no additional delay.
    #[serde(default)]
    pub rate_limit_ms: Option<u64>,
}

impl GeocoderConfig {
    pub fn geocoder_service(&self) -> GeocoderService {
        self.service.unwrap_or(GeocoderService::Nominatim)
    }
}

/// A description provider entry: the base `[description]` table or one
/// per-modality override such as `[description.image]`. LLM transport settings
/// (`service`, `endpoint`, `model`, `api_key`, `max_words`, `rate_limit_ms`)
/// live directly on each entry; unset keys fall back to the base entry.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct DescriptionProvider {
    #[serde(default)]
    pub enabled: Option<bool>,
    #[serde(default)]
    pub service: Option<LlmBackend>,
    #[serde(default)]
    pub endpoint: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub api_key: Option<String>,
    #[serde(default)]
    pub max_words: Option<u64>,
    /// Prompt template for description requests. Placeholders: `{name}`
    /// (file name), `{path}` (full source path), and `{words}` (the
    /// effective word budget). Extracted document text is appended verbatim
    /// after the rendered template when an extractor produced it, so it needs
    /// no placeholder and always reaches the provider for text documents.
    /// Unknown placeholders are rejected at config time; `{{` and `}}`
    /// escape literal braces. Unset inherits the base entry, then the
    /// built-in template (wording identical to before this key existed).
    #[serde(default)]
    pub prompt: Option<String>,
    /// Larger photos are downscaled (preserving aspect ratio) before the
    /// request so the model processes far fewer image tokens; the value
    /// joins the cache key and the vision settings hash. `0` disables the
    /// cap and forwards the original bytes.
    #[serde(default)]
    pub max_image_dimension: Option<u64>,
    /// Cap on representative frames sampled from a video: about one frame
    /// per second, evenly distributed across the clip, thinned to at most
    /// this many. The value joins the settings hash so a different cap
    /// re-describes. Must be positive.
    #[serde(default)]
    pub max_video_frames: Option<u64>,
    /// Upper bound on the context window (`num_ctx`) requested from an
    /// ollama description backend. The window is sized from the request
    /// payload, so a multi-frame vision request is not truncated by ollama's
    /// small default (4096), and this cap bounds KV-cache memory. Unset
    /// inherits the base entry, then [`DEFAULT_MAX_CONTEXT`]. Must be
    /// positive; lower it for a small-context model. The value does not join
    /// the settings hash because it does not change the model's input.
    #[serde(default)]
    pub max_context: Option<u64>,
    /// Minimum interval between requests in milliseconds, enforced between
    /// request starts on the provider worker. Description backends have no
    /// built-in pacing; the value only ever adds delay, and requests remain
    /// serialized one-in-flight on the worker regardless (see "Provider
    /// Plugins" in DESIGN.md). `0` (the default) means no delay.
    #[serde(default)]
    pub rate_limit_ms: Option<u64>,
}

/// The effective settings of a description provider entry after per-key
/// fallback to the base entry.
#[derive(Debug, Clone)]
pub struct EffectiveDescription {
    pub enabled: bool,
    pub service: LlmBackend,
    pub endpoint: Option<String>,
    pub model: Option<String>,
    pub api_key: Option<String>,
    pub max_words: u64,
    pub max_image_dimension: u64,
    pub max_video_frames: u64,
    pub max_context: u64,
    pub rate_limit_ms: u64,
    pub prompt: String,
}

impl DescriptionProvider {
    /// Effective settings for this entry, with unset keys inherited from
    /// `base`. For the base entry itself, pass it as `base` as well.
    pub fn effective(&self, base: &DescriptionProvider) -> EffectiveDescription {
        EffectiveDescription {
            enabled: self.enabled.or(base.enabled).unwrap_or(false),
            service: self.service.or(base.service).unwrap_or(LlmBackend::Ollama),
            endpoint: self.endpoint.clone().or_else(|| base.endpoint.clone()),
            model: self.model.clone().or_else(|| base.model.clone()),
            api_key: self.api_key.clone().or_else(|| base.api_key.clone()),
            max_words: self
                .max_words
                .or(base.max_words)
                .unwrap_or(DEFAULT_MAX_WORDS),
            max_image_dimension: self
                .max_image_dimension
                .or(base.max_image_dimension)
                .unwrap_or(DEFAULT_MAX_IMAGE_DIMENSION),
            max_video_frames: self
                .max_video_frames
                .or(base.max_video_frames)
                .unwrap_or(DEFAULT_MAX_VIDEO_FRAMES),
            max_context: self
                .max_context
                .or(base.max_context)
                .unwrap_or(DEFAULT_MAX_CONTEXT),
            rate_limit_ms: self.rate_limit_ms.or(base.rate_limit_ms).unwrap_or(0),
            prompt: self
                .prompt
                .clone()
                .or_else(|| base.prompt.clone())
                .unwrap_or_else(|| DEFAULT_DESCRIPTION_PROMPT.to_string()),
        }
    }
}

impl DescriptionConfig {
    /// Effective settings of the base provider.
    pub fn base_effective(&self) -> EffectiveDescription {
        self.base.effective(&self.base)
    }

    /// Effective settings for one per-modality entry, if present.
    pub fn modality_effective(&self, modality: &str) -> Option<EffectiveDescription> {
        self.per_modality
            .get(modality)
            .map(|p| p.effective(&self.base))
    }

    /// The effective settings for one content modality, applying the
    /// modality's built-in default prompt when neither the per-modality
    /// override nor the base entry set one. `None` selects the base entry.
    /// Unlike [`Self::modality_effective`], this returns settings even when
    /// no per-modality override exists, so a caller can address the base
    /// entry as the fallback for a specific modality.
    pub fn effective_for_modality(&self, modality: Option<&str>) -> EffectiveDescription {
        let Some(m) = modality else {
            return self.base_effective();
        };
        let mut eff = match self.per_modality.get(m) {
            Some(p) => p.effective(&self.base),
            None => self.base.effective(&self.base),
        };
        let explicit = self.base.prompt.is_some()
            || self.per_modality.get(m).is_some_and(|p| p.prompt.is_some());
        if !explicit {
            eff.prompt = default_description_prompt(m).to_string();
        }
        eff
    }
}

/// The built-in prompt for a content modality when neither the base entry
/// nor a per-modality override sets one. Documents (`text`) recover their
/// original title and fall back to keywords; every other modality uses the
/// generic keyword prompt.
pub fn default_description_prompt(modality: &str) -> &'static str {
    match modality {
        "text" => DEFAULT_DOCUMENT_DESCRIPTION_PROMPT,
        _ => DEFAULT_DESCRIPTION_PROMPT,
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct DescriptionConfig {
    /// Base provider fields; the base entry is the default provider for
    /// every content kind that lacks an explicit per-modality override.
    /// Kinds without a content modality (repository and archive) never use
    /// it - they have no content source for `%s` - except location
    /// summaries, which always use the text
    /// provider (`[description.text]`, else the base).
    #[serde(default, flatten)]
    pub base: DescriptionProvider,
    /// Per-modality overrides, e.g. `[description.text]` (keys are
    /// `text`, `image`, `video`, `audio`). Serde cannot combine
    /// `flatten` with `deny_unknown_fields`, so unknown per-modality keys are
    /// validated manually by `validate_config`.
    #[serde(default, flatten)]
    pub per_modality: BTreeMap<String, DescriptionProvider>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DuplicationConfig {
    #[serde(default = "default_large_file_threshold")]
    pub large_file_threshold: u64,
}

impl Default for DuplicationConfig {
    fn default() -> Self {
        Self {
            large_file_threshold: default_large_file_threshold(),
        }
    }
}

fn default_large_file_threshold() -> u64 {
    DEFAULT_LARGE_FILE_THRESHOLD
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HiddenDefinition {
    Explorer,
    Attribute,
    Dotfiles,
}

/// Conventional dependency, cache, and generated-output directories excluded
/// from primary discovery in addition to the user's `scan.exclude_paths`.
/// Set `scan.default_exclusions = false` to drop these and use only the
/// user's patterns.
pub const DEFAULT_EXCLUDE_PATHS: &[&str] = &[
    "node_modules",
    "vendor",
    ".venv",
    "venv",
    "__pycache__",
    ".pytest_cache",
    ".mypy_cache",
    ".ruff_cache",
    "target",
    "build",
    "dist",
    ".gradle",
    ".m2",
    ".next",
    ".nuxt",
    ".terraform",
    "Pods",
    "DerivedData",
];

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct ScanConfig {
    /// Extra directory patterns to exclude. They are merged with
    /// `DEFAULT_EXCLUDE_PATHS` unless `default_exclusions` is `false`, so a
    /// user who adds one pattern keeps the built-in ones.
    #[serde(default)]
    pub exclude_paths: Vec<String>,
    /// Extensions excluded from primary discovery, without a leading dot.
    /// Matched case-insensitively against the final extension at any depth,
    /// in addition to `exclude_paths`. Entries are normalized (trimmed,
    /// dot-stripped, lowercased) before matching; a wildcard or path
    /// separator is a structural error.
    #[serde(default)]
    pub exclude_extensions: Vec<String>,
    /// Whether `DEFAULT_EXCLUDE_PATHS` apply. Absent means `true`; set it to
    /// `false` to drop every built-in exclusion and use only `exclude_paths`.
    #[serde(default)]
    pub default_exclusions: Option<bool>,
    #[serde(default)]
    pub hidden_definition: Option<HiddenDefinition>,
    /// Absolute, normalized filesystem root the scan covers. Absent means
    /// the effective home directory. Runtime-mutable: this is an ordinary
    /// setting, not a per-scan parameter.
    #[serde(default)]
    pub root: Option<String>,
    /// Whether hidden entries are included in discovery. Absent means
    /// `false`. Runtime-mutable: an ordinary setting, not a per-scan
    /// parameter.
    #[serde(default)]
    pub hidden: Option<bool>,
}

impl ScanConfig {
    /// Whether the built-in `DEFAULT_EXCLUDE_PATHS` apply (default `true`).
    pub fn default_exclusions_enabled(&self) -> bool {
        self.default_exclusions != Some(false)
    }

    /// Built-in exclusion names the user re-enabled with `!name` entries.
    fn opted_out_defaults(&self) -> std::collections::HashSet<&str> {
        self.exclude_paths
            .iter()
            .filter_map(|pattern| pattern.trim().strip_prefix('!'))
            .collect()
    }

    /// Effective directory exclusions: the built-in defaults (unless disabled
    /// or re-enabled by name with `!name`) followed by the user's positive
    /// patterns, de-duplicated in first-seen order. A `!name` entry cancels
    /// only a built-in, never a positive pattern.
    pub fn effective_exclude_paths(&self) -> Vec<String> {
        let opted_out = self.opted_out_defaults();
        let mut out: Vec<String> = Vec::new();
        if self.default_exclusions_enabled() {
            out.extend(
                DEFAULT_EXCLUDE_PATHS
                    .iter()
                    .filter(|name| !opted_out.contains(**name))
                    .map(|name| (*name).to_string()),
            );
        }
        out.extend(
            self.exclude_paths
                .iter()
                .filter(|pattern| !pattern.trim().starts_with('!'))
                .cloned(),
        );
        let mut seen = std::collections::HashSet::new();
        out.retain(|pattern| seen.insert(pattern.clone()));
        out
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OutputFormat {
    Pretty,
    Json,
    Toml,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct OutputConfig {
    #[serde(default)]
    pub format: Option<OutputFormat>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CacheConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
}

impl Default for CacheConfig {
    fn default() -> Self {
        Self { enabled: true }
    }
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct LimitsConfig {
    #[serde(default)]
    pub provider_timeout_seconds: Option<u64>,
    #[serde(default)]
    pub provider_concurrency: Option<u64>,
    #[serde(default)]
    pub tool_timeout_seconds: Option<u64>,
    #[serde(default)]
    pub tool_stdout_max_bytes: Option<u64>,
}

/// Client-side UI state persisted through the settings path. Holds no scan
/// or planning behavior: it is excluded from the config hash, so saving a
/// filter never makes a running batch look stale.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct UiConfig {
    /// The TUI suggestion browser's saved path filter (case-insensitive
    /// substring over the suggestion's path fields). Absent or empty means
    /// no filter. The SPA keeps its own filter in URL parameters instead.
    #[serde(default)]
    pub path_filter: Option<String>,
}

/// The complete merged configuration file content. Each section defaults to
/// an absent section; accessor methods apply the documented defaults.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct RawConfig {
    #[serde(default)]
    pub target_paths: BTreeMap<String, String>,
    #[serde(default)]
    pub geocoder: GeocoderConfig,
    #[serde(default)]
    pub description: DescriptionConfig,
    #[serde(default)]
    pub document_extraction: BTreeMap<String, Vec<String>>,
    #[serde(default)]
    pub duplication: DuplicationConfig,
    #[serde(default)]
    pub scan: ScanConfig,
    #[serde(default)]
    pub output: OutputConfig,
    #[serde(default)]
    pub cache: CacheConfig,
    #[serde(default)]
    pub limits: LimitsConfig,
    #[serde(default)]
    pub ui: UiConfig,
}

impl RawConfig {
    pub fn geocoder_service(&self) -> GeocoderService {
        self.geocoder.service.unwrap_or(GeocoderService::Nominatim)
    }

    pub fn output_format(&self) -> OutputFormat {
        self.output.format.unwrap_or(OutputFormat::Pretty)
    }

    pub fn hidden_definition(&self) -> HiddenDefinition {
        self.scan
            .hidden_definition
            .unwrap_or(HiddenDefinition::Explorer)
    }

    /// The configured scan root, when one is set. Callers fall back to the
    /// effective home directory when this is `None`.
    pub fn scan_root(&self) -> Option<&str> {
        self.scan.root.as_deref()
    }

    /// Whether hidden entries are scanned (default `false`).
    pub fn scan_hidden(&self) -> bool {
        self.scan.hidden.unwrap_or(false)
    }

    pub fn provider_timeout(&self) -> u64 {
        self.limits
            .provider_timeout_seconds
            .unwrap_or(DEFAULT_PROVIDER_TIMEOUT_SECONDS)
    }

    /// Concurrent in-flight provider requests, clamped to at least one so a
    /// misconfigured zero can never stall the worker.
    pub fn provider_concurrency(&self) -> usize {
        self.limits
            .provider_concurrency
            .unwrap_or(DEFAULT_PROVIDER_CONCURRENCY)
            .max(1) as usize
    }

    pub fn tool_timeout(&self) -> u64 {
        self.limits
            .tool_timeout_seconds
            .unwrap_or(DEFAULT_TOOL_TIMEOUT_SECONDS)
    }

    pub fn tool_stdout_max_bytes(&self) -> u64 {
        self.limits
            .tool_stdout_max_bytes
            .unwrap_or(DEFAULT_TOOL_STDOUT_MAX_BYTES)
    }

    /// The effective destination template for a kind, when configured.
    pub fn configured_target_path(&self, kind: asset_kind::AssetKind) -> Option<&str> {
        self.target_paths.get(kind.name()).map(String::as_str)
    }

    /// The resolved set of per-kind destination format strings honoring
    /// precedence: config, then defaults. `target_paths` is a runtime
    /// setting; there are no per-scan overrides.
    pub fn resolved_target_paths(
        &self,
    ) -> Result<BTreeMap<asset_kind::AssetKind, FormatString>, ApiError> {
        let mut out = BTreeMap::new();
        for kind in asset_kind::AssetKind::ALL {
            let template = self
                .target_paths
                .get(kind.name())
                .map(String::as_str)
                .unwrap_or_else(|| kind.default_format());
            let fs = FormatString::parse(template).map_err(|e| {
                ApiError::validation(format!(
                    "invalid destination format string for {}: {e}",
                    kind.name()
                ))
            })?;
            out.insert(kind, fs);
        }
        Ok(out)
    }
}

// ---------------------------------------------------------------------------
// Deep merge
// ---------------------------------------------------------------------------

/// Merge `higher` into `base` recursively: nested tables merge by key;
/// scalars and arrays are replaced; an empty table contributes nothing.
pub fn deep_merge_toml(base: &mut TomlValue, higher: TomlValue) {
    match base {
        TomlValue::Table(base_t) => {
            let TomlValue::Table(high_t) = &higher else {
                *base = higher;
                return;
            };
            for (key, high_val) in high_t {
                if high_val.as_table().is_some_and(|t| t.is_empty()) {
                    // Empty tables contribute nothing; do not wipe the base.
                    continue;
                }
                let entry = base_t
                    .entry(key.clone())
                    .or_insert_with(|| TomlValue::Table(Default::default()));
                deep_merge_toml(entry, high_val.clone());
            }
        }
        _ => *base = higher,
    }
}

/// Deep merge of two JSON values with the settings-API semantics: tables
/// merge by key, scalars and arrays replace, empty tables contribute
/// nothing, and a `null` value deletes the key from the base object (the
/// only way a caller can unset a previously persisted value).
///
/// This applies overrides in memory (the launch-time CLI fold and the
/// runtime-settings overlay); persisting a `PUT /config` document is a
/// separate, formatting-preserving document edit, see
/// `apply_patch_to_document`.
pub fn deep_merge_json(base: &mut JsonValue, higher: JsonValue) {
    match base {
        JsonValue::Object(base_o) => {
            let JsonValue::Object(high_o) = &higher else {
                *base = higher;
                return;
            };
            for (key, high_val) in high_o {
                if high_val.is_null() {
                    base_o.remove(key.as_str());
                    continue;
                }
                let empty_object = high_val.as_object().is_some_and(|o| o.is_empty());
                if empty_object {
                    continue;
                }
                let entry = base_o
                    .entry(key.clone())
                    .or_insert(JsonValue::Object(Default::default()));
                deep_merge_json(entry, high_val.clone());
            }
        }
        _ => *base = higher,
    }
}

/// Total TOML -> JSON conversion (the shape `GET /config` and the settings
/// API speak). Persistence itself edits the TOML document in place, so no
/// conversion is involved in a write.
pub fn toml_to_json(v: &TomlValue) -> JsonValue {
    match v {
        TomlValue::Table(t) => {
            let mut o = serde_json::Map::new();
            for (k, val) in t {
                o.insert(k.clone(), toml_to_json(val));
            }
            JsonValue::Object(o)
        }
        TomlValue::Array(a) => JsonValue::Array(a.iter().map(toml_to_json).collect()),
        TomlValue::String(s) => JsonValue::String(s.clone()),
        TomlValue::Integer(i) => JsonValue::from(*i),
        TomlValue::Float(f) => JsonValue::from(*f),
        TomlValue::Boolean(b) => JsonValue::Bool(*b),
        TomlValue::Datetime(d) => JsonValue::String(d.to_string()),
    }
}

// ---------------------------------------------------------------------------
// Loading and file resolution
// ---------------------------------------------------------------------------

pub struct LoadedConfig {
    pub raw: RawConfig,
    /// The resolved config file path (highest-precedence existing file), or
    /// null when no config file exists.
    pub config_path: Option<PathBuf>,
}

fn read_config_file(path: &Path, required: bool) -> Result<Option<TomlValue>, ApiError> {
    let content = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound && !required => return Ok(None),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(ApiError::validation(format!(
                "explicitly requested config file does not exist: {}",
                path.display()
            )));
        }
        Err(e) => {
            return Err(ApiError::validation(format!(
                "cannot read config file {}: {e}",
                path.display()
            )));
        }
    };
    let value: TomlValue = content.parse().map_err(|e| {
        ApiError::validation(format!("malformed config file {}: {e}", path.display()))
    })?;
    Ok(Some(value))
}

/// Load the config chain for the host: system, per-user, then the
/// explicitly requested file, merged from lowest to highest precedence.
/// Missing default files are ignored; a missing or malformed explicitly
/// requested file is an error.
pub fn load_config(cli_config: Option<&Path>) -> Result<LoadedConfig, ApiError> {
    load_config_chain(&ConfigChain::host(), cli_config)
}

/// `load_config` over an explicit search chain: the same precedence rules,
/// with the system and per-user files supplied by the caller instead of
/// resolved from the host.
pub fn load_config_chain(
    chain: &ConfigChain,
    cli_config: Option<&Path>,
) -> Result<LoadedConfig, ApiError> {
    let mut merged = TomlValue::Table(Default::default());
    let mut highest_existing: Option<PathBuf> = None;
    let mut had_user = false;

    if let Some(sys) = &chain.system {
        if let Some(v) = read_config_file(sys, false)? {
            deep_merge_toml(&mut merged, v);
            highest_existing = Some(sys.clone());
        }
    }
    if let Some(user) = &chain.user {
        if let Some(v) = read_config_file(user, false)? {
            deep_merge_toml(&mut merged, v);
            highest_existing = Some(user.clone());
            had_user = true;
        }
    }
    if let Some(explicit) = cli_config {
        let v = read_config_file(explicit, true)?.expect("required file read");
        deep_merge_toml(&mut merged, v);
        highest_existing = Some(explicit.to_path_buf());
    }

    let raw: RawConfig = merged
        .try_into()
        .map_err(|e| ApiError::validation(format!("invalid configuration: {e}")))?;

    if !had_user && highest_existing.is_none() {
        // No config file exists at all (system paths are optional).
        // Keep config_path null per the API contract.
    }

    validate_config(&raw)?;
    Ok(LoadedConfig {
        raw,
        config_path: highest_existing,
    })
}

/// The path `PUT /config` writes to: the explicitly requested path, else
/// the per-user default path (creating parent directories as needed).
pub fn writable_config_path(cli_config: Option<&Path>) -> Result<PathBuf, ApiError> {
    writable_config_path_chain(&ConfigChain::host(), cli_config)
}

/// `writable_config_path` over an explicit search chain.
pub fn writable_config_path_chain(
    chain: &ConfigChain,
    cli_config: Option<&Path>,
) -> Result<PathBuf, ApiError> {
    if let Some(p) = cli_config {
        return Ok(p.to_path_buf());
    }
    let user = chain
        .user
        .clone()
        .ok_or_else(|| ApiError::internal("no per-user config directory available"))?;
    if let Some(parent) = user.parent() {
        std::fs::create_dir_all(parent).map_err(|e| {
            ApiError::internal(format!(
                "cannot create config directory {}: {e}",
                parent.display()
            ))
        })?;
    }
    Ok(user)
}

/// Persist a TOML value atomically to `path`. This renders the whole value
/// tree, so it rewrites the file from scratch; `PUT /config` does not use it
/// (it edits the document in place, see `merge_and_write_config_chain`).
pub fn write_config_toml(path: &Path, value: &TomlValue) -> Result<(), ApiError> {
    let text = toml::to_string_pretty(value)
        .map_err(|e| ApiError::internal(format!("cannot serialize config: {e}")))?;
    write_config_text(path, &text)
}

/// Persist config text atomically to `path` (write a sibling temp file, then
/// rename). Nothing is written when the file already holds exactly `text`, so
/// a no-op write never touches the file's mtime.
fn write_config_text(path: &Path, text: &str) -> Result<(), ApiError> {
    if let Ok(current) = std::fs::read_to_string(path) {
        if current == text {
            return Ok(());
        }
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| {
            ApiError::internal(format!(
                "cannot create config directory {}: {e}",
                parent.display()
            ))
        })?;
    }
    let tmp = path.with_extension("toml.tmp");
    std::fs::write(&tmp, text)
        .map_err(|e| ApiError::validation(format!("cannot write {}: {e}", path.display())))?;
    std::fs::rename(&tmp, path)
        .map_err(|e| ApiError::validation(format!("cannot write {}: {e}", path.display())))?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Validation
// ---------------------------------------------------------------------------

/// Validate every rule in the schema section of the design. Returns a
/// structured 422-style error with per-field reasons.
pub fn validate_config(raw: &RawConfig) -> Result<(), ApiError> {
    let mut fields: Vec<(String, String)> = Vec::new();

    // target_paths: a malformed destination cannot be planned, so it stays
    // a hard error.
    if let Some(e) = crate::format::validate_target_paths(&raw.target_paths).err() {
        if let Some(details) = e.body.details {
            fields.extend(
                details
                    .into_iter()
                    .map(|(k, v)| (k, v.as_str().map(str::to_string).unwrap_or_default())),
            );
        } else {
            fields.push(("target_paths".into(), e.body.message));
        }
    }

    // geocoder: structural rules only. Cross-field readiness (enabled but
    // missing a companion value, or a key that a service ignores) is
    // reported as a warning by `config_warnings` instead, so partial edits
    // persisted one field at a time never fail validation.
    if raw
        .geocoder
        .endpoint
        .as_deref()
        .is_some_and(|ep| ep.trim().is_empty())
    {
        fields.push((
            "geocoder.endpoint".into(),
            "endpoint must not be empty".into(),
        ));
    }

    // geocoder: the address-component selection is structural (no keys to
    // validate); readiness gaps are warnings instead.
    let desc = &raw.description;
    if let Some(e) = asset_kind::validate_per_modality_description_keys(
        desc.per_modality
            .keys()
            .cloned()
            .collect::<Vec<_>>()
            .into_iter(),
    )
    .err()
    {
        if let Some(details) = e.body.details {
            fields.extend(
                details
                    .into_iter()
                    .map(|(k, v)| (k, v.as_str().map(str::to_string).unwrap_or_default())),
            );
        }
    }
    fn check_provider(
        prefix: String,
        eff: EffectiveDescription,
        fields: &mut Vec<(String, String)>,
    ) {
        if let Some(ep) = &eff.endpoint {
            if ep.trim().is_empty() {
                fields.push((
                    format!("{prefix}.endpoint"),
                    "endpoint must not be empty".into(),
                ));
            }
        }
        if eff.max_words == 0 {
            fields.push((
                format!("{prefix}.max_words"),
                "max_words must be positive".into(),
            ));
        }
        if eff.max_video_frames == 0 {
            fields.push((
                format!("{prefix}.max_video_frames"),
                "max_video_frames must be positive".into(),
            ));
        }
        if eff.max_context == 0 {
            fields.push((
                format!("{prefix}.max_context"),
                "max_context must be positive".into(),
            ));
        }
        // `{path}` and `{name}` are intentionally not placeholders: the
        // full source path (which carries the filename) and any extracted
        // text are always included around the rendered template, so a custom
        // prompt cannot omit them and only the word budget is
        // template-driven.
        if let Some(bad) = crate::prompt::prompt_template_error(&eff.prompt, &["words"]) {
            fields.push((
                format!("{prefix}.prompt"),
                format!("unsupported placeholder {bad} in prompt template (supported: {{words}}; the path and any content are always included, so {{name}} and {{path}} are not placeholders; escape literal braces by doubling them)"),
            ));
        }
    }
    check_provider("description".into(), desc.base_effective(), &mut fields);
    for modality in desc.per_modality.keys() {
        let eff = desc
            .modality_effective(modality)
            .expect("per-modality key iterated from the same map");
        check_provider(format!("description.{modality}"), eff, &mut fields);
    }

    // document_extraction
    for (fmt, argv) in &raw.document_extraction {
        if argv.is_empty() {
            continue; // empty array disables extraction for that format
        }
        if argv[0].trim().is_empty() {
            fields.push((
                format!("document_extraction.{fmt}"),
                "command must name an executable".into(),
            ));
        }
        let mut path_occurrences = 0usize;
        for arg in argv {
            if arg.contains("{path}") {
                path_occurrences += 1;
                if arg != "{path}" {
                    fields.push((
                        format!("document_extraction.{fmt}"),
                        "'{path}' must be a complete argument of its own".into(),
                    ));
                }
            }
            if arg.contains('{') && !arg.contains("{path}") {
                fields.push((
                    format!("document_extraction.{fmt}"),
                    format!("unsupported placeholder in argument: {arg}"),
                ));
            }
        }
        if path_occurrences == 0 {
            fields.push((
                format!("document_extraction.{fmt}"),
                "command must reference '{path}' exactly once".into(),
            ));
        } else if path_occurrences > 1 {
            fields.push((
                format!("document_extraction.{fmt}"),
                "'{path}' must occur exactly once".into(),
            ));
        }
    }

    // duplication
    // large_file_threshold is u64 by construction (non-negative).

    // scan.exclude_paths
    for (i, pat) in raw.scan.exclude_paths.iter().enumerate() {
        let trimmed = pat.trim();
        if trimmed.is_empty() {
            fields.push((
                format!("scan.exclude_paths[{i}]"),
                "pattern must not be empty".into(),
            ));
        } else if let Some(name) = trimmed.strip_prefix('!') {
            // `!name` re-includes one built-in directory; anything else is a
            // typo that would silently do nothing, so reject it.
            if name.is_empty() {
                fields.push((
                    format!("scan.exclude_paths[{i}]"),
                    "'!' must be followed by a built-in exclusion name".into(),
                ));
            } else if !DEFAULT_EXCLUDE_PATHS.contains(&name) {
                fields.push((
                    format!("scan.exclude_paths[{i}]"),
                    format!(
                        "'!{name}' does not match a built-in exclusion; use '!' only to \
                         re-include one of the built-in directories"
                    ),
                ));
            }
        }
    }

    // scan.exclude_extensions
    for (i, ext) in raw.scan.exclude_extensions.iter().enumerate() {
        let normalized = ext.trim().trim_start_matches('.');
        if normalized.is_empty() {
            fields.push((
                format!("scan.exclude_extensions[{i}]"),
                "extension must not be empty".into(),
            ));
        } else if normalized.contains('/') || normalized.contains('\\') {
            fields.push((
                format!("scan.exclude_extensions[{i}]"),
                "extension must be a bare extension, not a path".into(),
            ));
        } else if normalized.contains(['*', '?', '[', ']']) {
            fields.push((
                format!("scan.exclude_extensions[{i}]"),
                "extension must be a literal, not a glob".into(),
            ));
        }
    }

    // scan.root: an absolute, normalized path with no `~` or
    // environment-variable references, so it can be used verbatim as the
    // discovery root.
    if let Some(root) = raw.scan.root.as_deref() {
        if let Err(e) = crate::path::require_absolute_normalized(std::path::Path::new(root)) {
            fields.push(("scan.root".into(), e.body.message));
        }
    }

    // limits: timeouts and output caps must be positive so limits are never
    // infinite by configuration.
    if raw.limits.provider_timeout_seconds == Some(0) {
        fields.push((
            "limits.provider_timeout_seconds".into(),
            "must be positive".into(),
        ));
    }
    if raw.limits.provider_concurrency == Some(0) {
        fields.push((
            "limits.provider_concurrency".into(),
            "must be positive".into(),
        ));
    }
    if raw.limits.tool_timeout_seconds == Some(0) {
        fields.push((
            "limits.tool_timeout_seconds".into(),
            "must be positive".into(),
        ));
    }
    if raw.limits.tool_stdout_max_bytes == Some(0) {
        fields.push((
            "limits.tool_stdout_max_bytes".into(),
            "must be positive".into(),
        ));
    }

    // Cache: no structural rules beyond the parse-time field checks.

    if fields.is_empty() {
        Ok(())
    } else {
        Err(ApiError::validation_errors(
            "configuration is invalid",
            fields,
        ))
    }
}

/// Readiness advisories for the effective configuration: cross-field
/// conditions under which an enabled provider will not work until another
/// value is set, or a set value is ignored by the selected service. These
/// are warnings, never errors: the configuration loads and scans proceed,
/// and the affected placeholder falls back (omitted for `%l`, the filename
/// stem for `%s`) while the condition holds. Surfaced in `GET /config`,
/// `PUT /config`, and the `config.updated` event so clients can guide the
/// user without blocking field-at-a-time edits.
pub fn config_warnings(raw: &RawConfig) -> Vec<(String, String)> {
    let mut warnings: Vec<(String, String)> = Vec::new();

    let geo = &raw.geocoder;
    if geo.enabled {
        let key = geo.api_key.as_deref().map(str::trim).unwrap_or("");
        let endpoint_empty = geo
            .endpoint
            .as_deref()
            .map(str::trim)
            .unwrap_or("")
            .is_empty();
        match geo.geocoder_service() {
            GeocoderService::Google | GeocoderService::Mapbox => {
                if key.is_empty() {
                    warnings.push((
                        "geocoder.api_key".into(),
                        format!(
                            "api_key is required by the {:?} service; geocoding fails and %l is omitted until it is set",
                            geo.geocoder_service()
                        )
                        .to_lowercase(),
                    ));
                }
            }
            GeocoderService::Nominatim | GeocoderService::Photon => {
                if !key.is_empty() {
                    warnings.push((
                        "geocoder.api_key".into(),
                        format!(
                            "api_key is ignored by the {:?} service",
                            geo.geocoder_service()
                        )
                        .to_lowercase(),
                    ));
                }
            }
            GeocoderService::Custom => {
                if endpoint_empty {
                    warnings.push((
                        "geocoder.endpoint".into(),
                        "endpoint is required for the custom service; geocoding fails and %l is omitted until it is set"
                            .into(),
                    ));
                }
            }
        }
    }

    fn provider_warnings(
        prefix: String,
        eff: EffectiveDescription,
        warnings: &mut Vec<(String, String)>,
    ) {
        if !eff.enabled {
            return;
        }
        if eff.model.as_deref().map(str::trim).unwrap_or("").is_empty() {
            warnings.push((
                format!("{prefix}.model"),
                "model is required when the description provider is enabled; %s falls back to the filename stem until it is set"
                    .into(),
            ));
        }
        match eff.service {
            LlmBackend::Custom => {
                if eff
                    .endpoint
                    .as_deref()
                    .map(str::trim)
                    .unwrap_or("")
                    .is_empty()
                {
                    warnings.push((
                        format!("{prefix}.endpoint"),
                        "endpoint is required for the custom backend; %s falls back to the filename stem until it is set"
                            .into(),
                    ));
                }
            }
            LlmBackend::Openrouter => {
                if eff
                    .api_key
                    .as_deref()
                    .map(str::trim)
                    .unwrap_or("")
                    .is_empty()
                {
                    warnings.push((
                        format!("{prefix}.api_key"),
                        "api_key is required by the openrouter backend; %s falls back to the filename stem until it is set"
                            .into(),
                    ));
                }
            }
            LlmBackend::Ollama => {
                if eff
                    .api_key
                    .as_deref()
                    .map(str::trim)
                    .is_some_and(|s| !s.is_empty())
                {
                    warnings.push((
                        format!("{prefix}.api_key"),
                        "api_key is ignored by the ollama backend".into(),
                    ));
                }
            }
        }
    }
    let desc = &raw.description;
    provider_warnings("description".into(), desc.base_effective(), &mut warnings);
    for modality in desc.per_modality.keys() {
        let eff = desc
            .modality_effective(modality)
            .expect("per-modality key iterated from the same map");
        provider_warnings(format!("description.{modality}"), eff, &mut warnings);
    }

    warnings
}

/// Deserialize and validate a candidate configuration document (JSON body
/// of `POST /config/validate` or `PUT /config`). Returns the raw merged
/// config when valid.
pub fn parse_and_validate_json(value: JsonValue) -> Result<RawConfig, ApiError> {
    let toml_value = json_to_toml(&value)
        .ok_or_else(|| ApiError::validation("configuration must be a table"))?;
    let raw: RawConfig = toml_value
        .try_into()
        .map_err(|e| ApiError::validation(format!("invalid configuration: {e}")))?;
    validate_config(&raw)?;
    Ok(raw)
}

// ---------------------------------------------------------------------------
// Persistence: in-place document editing
// ---------------------------------------------------------------------------

/// The document at the writable config path, the path it is written back to,
/// and whether the file already existed. A missing file starts from the
/// embedded `defaults.toml` dump, so the first saved setting lands in a
/// documented file; an existing file is used exactly as it is on disk.
fn writable_file_document(
    chain: &ConfigChain,
    cli: Option<&Path>,
) -> Result<(PathBuf, DocumentMut, bool), ApiError> {
    let path = writable_config_path_chain(chain, cli)?;
    let (text, existed) = match std::fs::read_to_string(&path) {
        Ok(text) => (text, true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            (DEFAULT_CONFIG_TEMPLATE.to_string(), false)
        }
        Err(e) => {
            return Err(ApiError::validation(format!(
                "cannot read config file {}: {e}",
                path.display()
            )))
        }
    };
    let doc = text.parse::<DocumentMut>().map_err(|e| {
        ApiError::validation(format!("malformed config file {}: {e}", path.display()))
    })?;
    Ok((path, doc, existed))
}

/// Apply a submitted patch document to `doc` in place.
///
/// The write edits the parsed TOML document instead of re-rendering a value
/// tree, so the file's comments, blank lines, key order, dotted keys, inline
/// tables, and multi-line strings survive, and the on-disk diff is limited to
/// the keys the patch actually changes: tables patch key by key, scalars and
/// arrays replace their value while keeping that item's own formatting (an
/// inline comment on the line stays put), and a `null` value deletes the key.
/// A table the patch leaves empty is removed so a file never accumulates stub
/// sections. An empty table in the patch contributes nothing, matching the
/// file-chain merge rule.
fn apply_patch_to_document(doc: &mut DocumentMut, patch: &JsonValue) -> Result<(), ApiError> {
    let JsonValue::Object(fields) = patch else {
        return Err(ApiError::validation("configuration must be a table"));
    };
    patch_table(doc.as_table_mut(), fields)
}

fn patch_table(
    table: &mut Table,
    patch: &serde_json::Map<String, JsonValue>,
) -> Result<(), ApiError> {
    for (key, value) in patch {
        if value.is_null() {
            table.remove(key);
            continue;
        }
        if value.as_object().is_some_and(|o| o.is_empty()) {
            continue;
        }
        match value {
            JsonValue::Object(fields) => patch_child_table(table, key, fields)?,
            value => patch_value_item(table, key, value)?,
        }
    }
    Ok(())
}

/// Patch one key whose submitted value is a table: an existing table (a
/// `[section]` header or a dotted-key table, both modeled as a `Table`) is
/// patched in place, an existing inline table keeps its inline form, and a
/// missing key (or a scalar where a table belongs) gets a fresh table.
fn patch_child_table(
    table: &mut Table,
    key: &str,
    patch: &serde_json::Map<String, JsonValue>,
) -> Result<(), ApiError> {
    let patched_in_place = match table.get_mut(key) {
        Some(Item::Table(child)) => {
            patch_table(child, patch)?;
            true
        }
        Some(Item::Value(EditValue::InlineTable(child))) => {
            patch_inline_table(child, patch)?;
            true
        }
        _ => false,
    };
    if patched_in_place {
        let empty = match table.get(key) {
            Some(Item::Table(child)) => child.is_empty(),
            Some(Item::Value(EditValue::InlineTable(child))) => child.is_empty(),
            _ => false,
        };
        if empty {
            table.remove(key);
        }
        return Ok(());
    }
    let mut child = Table::new();
    patch_table(&mut child, patch)?;
    let mut item = Item::Table(child);
    if let Some(Item::Table(existing)) = table.get(key) {
        *item.as_table_mut().expect("table").decor_mut() = existing.decor().clone();
    }
    insert_item(table, key, item);
    Ok(())
}

/// Patch one key whose submitted value is a scalar or an array.
fn patch_value_item(table: &mut Table, key: &str, value: &JsonValue) -> Result<(), ApiError> {
    // An array is rebuilt element by element so a one-per-line list stays
    // one-per-line (see `patch_array`).
    if let JsonValue::Array(elements) = value {
        if let Some(Item::Value(EditValue::Array(existing))) = table.get_mut(key) {
            patch_array(existing, elements)?;
            return Ok(());
        }
    }
    let mut item = Item::Value(json_to_edit_value(value)?);
    if let Some(Item::Value(existing)) = table.get(key) {
        *item.as_value_mut().expect("value").decor_mut() = existing.decor().clone();
    }
    insert_item(table, key, item);
    Ok(())
}

/// Store `item` under `key`, replacing the existing slot in place when the
/// key is already there.
///
/// `Table::insert` reformats the key it replaces (`Key::fmt`), which discards
/// the comment and indentation attached to a dotted key's line, so an existing
/// slot is overwritten directly and `insert` is only used for a new key.
fn insert_item(table: &mut Table, key: &str, item: Item) {
    match table.get_mut(key) {
        Some(slot) => *slot = item,
        None => {
            table.insert(key, item);
        }
    }
}

/// Patch an inline table in place (the inline form the user wrote is kept).
fn patch_inline_table(
    table: &mut InlineTable,
    patch: &serde_json::Map<String, JsonValue>,
) -> Result<(), ApiError> {
    for (key, value) in patch {
        if value.is_null() {
            table.remove(key);
            continue;
        }
        if value.as_object().is_some_and(|o| o.is_empty()) {
            continue;
        }
        match value {
            JsonValue::Object(fields) => {
                if let Some(EditValue::InlineTable(child)) = table.get_mut(key) {
                    patch_inline_table(child, fields)?;
                    let empty = child.is_empty();
                    if empty {
                        table.remove(key);
                    }
                } else {
                    let mut child = InlineTable::new();
                    patch_inline_table(&mut child, fields)?;
                    insert_inline_value(table, key, EditValue::InlineTable(child));
                }
            }
            value => {
                let mut new_value = json_to_edit_value(value)?;
                if let Some(existing) = table.get(key) {
                    *new_value.decor_mut() = existing.decor().clone();
                }
                insert_inline_value(table, key, new_value);
            }
        }
    }
    Ok(())
}

/// Store a value in an inline table, replacing an existing slot in place (see
/// `insert_item` for why `insert` is reserved for new keys).
fn insert_inline_value(table: &mut InlineTable, key: &str, value: EditValue) {
    match table.get_mut(key) {
        Some(slot) => *slot = value,
        None => {
            table.insert(key, value);
        }
    }
}

/// Rebuild an existing array from the submitted elements, reusing each
/// element's own formatting by index.
///
/// In TOML the separator in front of an element (and, in a multi-line array,
/// the newline and indentation) lives in that element's decor, so rebuilding
/// index by index keeps the user's layout: an edited element keeps the
/// separator and comment of the element it replaces, and an appended element
/// copies the array's plain separator (see `separator_decor`) so a
/// one-per-line list stays one-per-line without duplicating a comment. The
/// array's own trailing whitespace, comments, and trailing comma are
/// preserved.
fn patch_array(existing: &mut Array, submitted: &[JsonValue]) -> Result<(), ApiError> {
    let decor: Vec<toml_edit::Decor> = existing.iter().map(|v| v.decor().clone()).collect();
    let separator = separator_decor(existing);
    let mut rebuilt = Array::new();
    for (index, element) in submitted.iter().enumerate() {
        let mut value = json_to_edit_value(element)?;
        if let Some(d) = decor.get(index).cloned().or_else(|| separator.clone()) {
            *value.decor_mut() = d;
        }
        rebuilt.push_formatted(value);
    }
    rebuilt.set_trailing(existing.trailing().clone());
    rebuilt.set_trailing_comma(existing.trailing_comma());
    *existing = rebuilt;
    Ok(())
}

/// The decor an appended array element should use: the separator between two
/// existing elements (or the first element's own decor for a one-element
/// array), with any comment removed so an appended element never repeats the
/// comment that belongs to the line above it. The newline and indentation of a
/// multi-line array are kept, so an appended element lands on its own line.
fn separator_decor(existing: &Array) -> Option<toml_edit::Decor> {
    let sample = existing.get(if existing.len() > 1 { 1 } else { 0 })?;
    let prefix = sample
        .decor()
        .prefix()
        .and_then(|p| p.as_str())
        .unwrap_or("");
    let stripped = match (prefix.find('#'), prefix.rfind('\n')) {
        // Multi-line: keep from the newline that follows the comment.
        (Some(_), Some(_)) => format!("\n{}", prefix.rsplit('\n').next().unwrap_or("")),
        // A comment with no newline: a plain single-line separator.
        (Some(_), None) => String::new(),
        (None, _) => prefix.to_string(),
    };
    let mut decor = toml_edit::Decor::default();
    decor.set_prefix(stripped);
    Some(decor)
}

/// Convert one JSON value to a TOML value for the in-place patch. `null` is
/// rejected here: it is only meaningful as an object key, where it deletes the
/// key, and TOML has no null value.
fn json_to_edit_value(value: &JsonValue) -> Result<EditValue, ApiError> {
    match value {
        JsonValue::Null => Err(ApiError::validation(
            "null is only valid as a table key, where it deletes the key",
        )),
        JsonValue::Bool(b) => Ok(EditValue::from(*b)),
        JsonValue::String(s) => Ok(EditValue::from(s.clone())),
        JsonValue::Number(n) => match (n.as_i64(), n.as_f64()) {
            (Some(i), _) => Ok(EditValue::from(i)),
            (None, Some(f)) => Ok(EditValue::from(f)),
            (None, None) => Err(ApiError::validation(format!(
                "unsupported numeric value: {n}"
            ))),
        },
        JsonValue::Array(items) => {
            let mut array = Array::new();
            for item in items {
                array.push(json_to_edit_value(item)?);
            }
            Ok(EditValue::Array(array))
        }
        JsonValue::Object(fields) => {
            let mut table = InlineTable::new();
            for (key, field) in fields {
                table.insert(key, json_to_edit_value(field)?);
            }
            Ok(EditValue::InlineTable(table))
        }
    }
}

/// Deserialize and validate the patched document exactly as it will be
/// written.
fn document_to_validated_config(doc: &DocumentMut) -> Result<RawConfig, ApiError> {
    let raw: RawConfig = toml::from_str(&doc.to_string())
        .map_err(|e| ApiError::validation(format!("invalid configuration: {e}")))?;
    validate_config(&raw)?;
    Ok(raw)
}

/// Render the patched document, ensuring the file ends with a newline.
fn render_config_document(doc: &DocumentMut) -> String {
    let mut text = doc.to_string();
    if !text.is_empty() && !text.ends_with('\n') {
        text.push('\n');
    }
    text
}

/// Merge a partial configuration document into the file `PUT /config`
/// writes to, validate the merged result, write it atomically, then reload
/// the full precedence chain. Returns the effective config and its path.
pub fn merge_and_write_config(
    submitted: &JsonValue,
    cli: Option<&Path>,
) -> Result<LoadedConfig, ApiError> {
    merge_and_write_config_chain(&ConfigChain::host(), submitted, cli)
}

/// `merge_and_write_config` over an explicit search chain.
pub fn merge_and_write_config_chain(
    chain: &ConfigChain,
    submitted: &JsonValue,
    cli: Option<&Path>,
) -> Result<LoadedConfig, ApiError> {
    let (write_path, mut doc, existed) = writable_file_document(chain, cli)?;
    apply_patch_to_document(&mut doc, submitted)?;
    document_to_validated_config(&doc)?;
    // A file that does not exist yet is only created when the patch changes
    // the embedded dump: a no-op first save leaves no file behind, just as a
    // no-op save of an existing file leaves it untouched. An explicitly
    // requested path is different: it must exist after the save for the
    // reload that follows to succeed, so it is always written.
    let text = render_config_document(&doc);
    if existed || cli.is_some() || text != DEFAULT_CONFIG_TEMPLATE {
        write_config_text(&write_path, &text)?;
    }
    load_config_chain(chain, cli)
}

/// Validate a partial configuration document exactly as `PUT /config`
/// would merge it, without persisting anything. Returns the merged raw
/// config when valid.
pub fn merge_and_validate_config(
    submitted: &JsonValue,
    cli: Option<&Path>,
) -> Result<RawConfig, ApiError> {
    merge_and_validate_config_chain(&ConfigChain::host(), submitted, cli)
}

/// `merge_and_validate_config` over an explicit search chain.
pub fn merge_and_validate_config_chain(
    chain: &ConfigChain,
    submitted: &JsonValue,
    cli: Option<&Path>,
) -> Result<RawConfig, ApiError> {
    let (_, mut doc, _) = writable_file_document(chain, cli)?;
    apply_patch_to_document(&mut doc, submitted)?;
    document_to_validated_config(&doc)
}

/// Apply a partial configuration override document on top of a base
/// configuration. Used for the launch-time fold of CLI options over the
/// config-file chain: the overlay uses the same shape and merge semantics
/// as `PUT /config` - tables merge by key, scalars and arrays replace,
/// `null` deletes the key so the built-in default applies - but nothing is
/// persisted by this call. The resulting config is validated before it is
/// returned.
pub fn apply_config_overrides(
    base: &RawConfig,
    overlay: &JsonValue,
) -> Result<RawConfig, ApiError> {
    if !overlay.is_object() {
        return Err(ApiError::validation("configuration must be a table"));
    }
    let mut json = serde_json::to_value(base)
        .map_err(|e| ApiError::validation(format!("cannot serialize configuration: {e}")))?;
    deep_merge_json(&mut json, overlay.clone());
    let raw: RawConfig = serde_json::from_value(json)
        .map_err(|e| ApiError::validation(format!("invalid configuration: {e}")))?;
    validate_config(&raw)?;
    Ok(raw)
}

/// Fold launch-time CLI overrides and accumulated runtime-settings writes
/// over a base config-file chain. Precedence is base < launch < runtime, so
/// a runtime write wins over a launch flag for the same key while launch
/// flags keep overriding untouched file values.
pub fn fold_config_overrides(
    base: &RawConfig,
    launch: &JsonValue,
    runtime: &JsonValue,
) -> Result<RawConfig, ApiError> {
    let launched = apply_config_overrides(base, launch)?;
    apply_config_overrides(&launched, runtime)
}

/// Lossy JSON -> TOML conversion for the PUT / config persistence path
/// (JSON booleans/strings/integers map onto TOML cleanly).
pub fn json_to_toml(v: &JsonValue) -> Option<TomlValue> {
    match v {
        JsonValue::Object(map) => {
            let mut t = toml::map::Map::new();
            for (k, val) in map {
                t.insert(k.clone(), json_to_toml(val)?);
            }
            Some(TomlValue::Table(t))
        }
        JsonValue::Array(arr) => {
            let items: Option<Vec<TomlValue>> = arr.iter().map(json_to_toml).collect();
            Some(TomlValue::Array(items?))
        }
        JsonValue::String(s) => Some(TomlValue::String(s.clone())),
        JsonValue::Bool(b) => Some(TomlValue::Boolean(*b)),
        JsonValue::Number(n) => n
            .as_i64()
            .map(TomlValue::Integer)
            .or_else(|| n.as_f64().map(TomlValue::Float)),
        JsonValue::Null => None,
    }
}

/// Convert the effective merged config back to a JSON document for
/// `GET /config` and the `config.updated` SSE event. Secrets (API keys) are
/// never included.
pub fn effective_config_json(raw: &RawConfig, config_path: Option<&Path>) -> JsonValue {
    let mut out = serde_json::Map::new();
    out.insert(
        "target_paths".into(),
        serde_json::to_value(&raw.target_paths).unwrap_or(JsonValue::Null),
    );
    // Built-in default destination formats for every kind; clients use
    // them as placeholders and to show what an unset key resolves to.
    let mut defaults = serde_json::Map::new();
    for kind in asset_kind::AssetKind::ALL {
        defaults.insert(
            kind.name().to_string(),
            JsonValue::String(kind.default_format().to_string()),
        );
    }
    out.insert("target_path_defaults".into(), JsonValue::Object(defaults));
    out.insert(
        "geocoder".into(),
        serde_json::json!({
            "enabled": raw.geocoder.enabled,
            "service": match raw.geocoder_service() {
                GeocoderService::Nominatim => "nominatim",
                GeocoderService::Photon => "photon",
                GeocoderService::Google => "google",
                GeocoderService::Mapbox => "mapbox",
                GeocoderService::Custom => "custom",
            },
            "endpoint": raw.geocoder.endpoint.as_deref().unwrap_or(raw.geocoder_service().default_endpoint()),
            "language": raw
                .geocoder
                .language
                .as_deref()
                .unwrap_or(DEFAULT_GEOCODER_LANGUAGE),
            "user_agent_contact": raw.geocoder.user_agent_contact,
            "neighbourhood_cities": raw.geocoder.neighbourhood_cities,
            "rate_limit_ms": raw.geocoder.rate_limit_ms.unwrap_or(0),
        }),
    );
    let mut desc = serde_json::Map::new();
    let base_eff = raw.description.base_effective();
    if let Some(e) = desc_base_enabled(&raw.description) {
        desc.insert("enabled".into(), JsonValue::Bool(e));
    }
    desc.insert(
        "service".into(),
        JsonValue::String(llm_backend_name(base_eff.service).into()),
    );
    desc.insert(
        "endpoint".into(),
        JsonValue::String(
            base_eff
                .endpoint
                .clone()
                .unwrap_or_else(|| base_eff.service.default_endpoint().into()),
        ),
    );
    desc.insert(
        "model".into(),
        serde_json::to_value(&base_eff.model).unwrap_or(JsonValue::Null),
    );
    desc.insert("max_words".into(), JsonValue::from(base_eff.max_words));
    desc.insert("prompt".into(), JsonValue::from(base_eff.prompt.clone()));
    desc.insert(
        "max_image_dimension".into(),
        JsonValue::from(base_eff.max_image_dimension),
    );
    desc.insert(
        "max_video_frames".into(),
        JsonValue::from(base_eff.max_video_frames),
    );
    desc.insert("max_context".into(), JsonValue::from(base_eff.max_context));
    desc.insert(
        "rate_limit_ms".into(),
        JsonValue::from(base_eff.rate_limit_ms),
    );
    for (modality, p) in &raw.description.per_modality {
        let eff = p.effective(&raw.description.base);
        desc.insert(
            modality.clone(),
            serde_json::json!({
                "enabled": eff.enabled,
                "service": llm_backend_name(eff.service),
                "endpoint": eff.endpoint.clone().unwrap_or_else(|| eff.service.default_endpoint().into()),
                "model": eff.model,
                "max_words": eff.max_words,
                "prompt": eff.prompt,
                "max_image_dimension": eff.max_image_dimension,
                "max_video_frames": eff.max_video_frames,
                "max_context": eff.max_context,
                "rate_limit_ms": eff.rate_limit_ms,
            }),
        );
    }
    out.insert("description".into(), JsonValue::Object(desc));
    out.insert(
        "document_extraction".into(),
        serde_json::to_value(&raw.document_extraction).unwrap_or(JsonValue::Null),
    );
    out.insert(
        "duplication".into(),
        serde_json::json!({ "large_file_threshold": raw.duplication.large_file_threshold }),
    );
    out.insert(
        "scan".into(),
        serde_json::json!({
            "root": raw.scan.root,
            "hidden": raw.scan_hidden(),
            "exclude_paths": raw.scan.exclude_paths,
            "exclude_extensions": raw.scan.exclude_extensions,
            "default_exclusions": raw.scan.default_exclusions_enabled(),
            "hidden_definition": match raw.hidden_definition() {
                HiddenDefinition::Explorer => "explorer",
                HiddenDefinition::Attribute => "attribute",
                HiddenDefinition::Dotfiles => "dotfiles",
            },
        }),
    );
    out.insert(
        "default_exclude_paths".into(),
        serde_json::to_value(DEFAULT_EXCLUDE_PATHS).unwrap_or(JsonValue::Null),
    );
    out.insert(
        "output".into(),
        serde_json::json!({ "format": match raw.output_format() {
            OutputFormat::Pretty => "pretty",
            OutputFormat::Json => "json",
            OutputFormat::Toml => "toml",
        }}),
    );
    out.insert(
        "cache".into(),
        serde_json::json!({ "enabled": raw.cache.enabled }),
    );
    out.insert(
        "limits".into(),
        serde_json::json!({
            "provider_timeout_seconds": raw.provider_timeout(),
            "provider_concurrency": raw.provider_concurrency(),
            "tool_timeout_seconds": raw.tool_timeout(),
            "tool_stdout_max_bytes": raw.tool_stdout_max_bytes(),
        }),
    );
    out.insert(
        "ui".into(),
        serde_json::json!({ "path_filter": raw.ui.path_filter }),
    );
    out.insert(
        "warnings".into(),
        serde_json::to_value(config_warnings(raw).into_iter().collect::<BTreeMap<_, _>>())
            .unwrap_or(JsonValue::Null),
    );
    out.insert(
        "config_path".into(),
        config_path
            .map(|p| JsonValue::String(p.to_string_lossy().into_owned()))
            .unwrap_or(JsonValue::Null),
    );
    JsonValue::Object(out)
}

fn desc_base_enabled(d: &DescriptionConfig) -> Option<bool> {
    d.base.enabled
}

fn llm_backend_name(b: LlmBackend) -> &'static str {
    match b {
        LlmBackend::Ollama => "ollama",
        LlmBackend::Openrouter => "openrouter",
        LlmBackend::Custom => "custom",
    }
}

/// Config hash used by `GET /session` for change detection.
pub fn config_hash(raw: &RawConfig) -> String {
    use sha2::{Digest, Sha256};
    let mut json = effective_config_json(raw, None);
    // The [ui] section is client-side view state, not scan or planning
    // behavior: saving a TUI filter must not make the running batch look
    // stale, so it never contributes to the hash.
    if let Some(obj) = json.as_object_mut() {
        obj.remove("ui");
    }
    let text = serde_json::to_string(&json).unwrap_or_default();
    hex::encode(Sha256::digest(text.as_bytes()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::asset_kind::AssetKind;

    fn parse(raw: &str) -> Result<RawConfig, ApiError> {
        let v: TomlValue = raw.parse().expect("test toml parses");
        let rc: RawConfig = v
            .try_into()
            .map_err(|e| ApiError::validation(format!("{e}")))?;
        validate_config(&rc)?;
        Ok(rc)
    }

    #[test]
    fn empty_config_is_valid_and_defaulted() {
        let rc = parse("").unwrap();
        assert_eq!(
            rc.duplication.large_file_threshold,
            DEFAULT_LARGE_FILE_THRESHOLD
        );
        assert_eq!(rc.provider_timeout(), DEFAULT_PROVIDER_TIMEOUT_SECONDS);
        assert_eq!(
            rc.provider_concurrency(),
            DEFAULT_PROVIDER_CONCURRENCY as usize
        );
        assert_eq!(rc.description.base_effective().max_words, DEFAULT_MAX_WORDS);
        assert_eq!(rc.output_format(), OutputFormat::Pretty);
        assert_eq!(rc.hidden_definition(), HiddenDefinition::Explorer);
        // The user's list starts empty while the built-in defaults apply.
        assert!(rc.scan.exclude_paths.is_empty());
        assert!(rc.scan.default_exclusions_enabled());
        let effective = rc.scan.effective_exclude_paths();
        assert_eq!(
            effective.len(),
            DEFAULT_EXCLUDE_PATHS.len(),
            "built-in defaults apply when nothing is configured"
        );
    }

    #[test]
    fn scan_exclusions_merge_with_defaults_and_can_be_disabled() {
        // A single user pattern keeps the built-in defaults.
        let rc = parse("[scan]\nexclude_paths = [\"my-cache\"]\n").unwrap();
        let effective = rc.scan.effective_exclude_paths();
        assert!(effective.contains(&"my-cache".to_string()));
        assert!(effective.contains(&"node_modules".to_string()));
        assert_eq!(effective.len(), DEFAULT_EXCLUDE_PATHS.len() + 1);

        // Re-listing a default does not duplicate it.
        let rc = parse("[scan]\nexclude_paths = [\"node_modules\"]\n").unwrap();
        assert_eq!(
            rc.scan.effective_exclude_paths().len(),
            DEFAULT_EXCLUDE_PATHS.len()
        );

        // Disabling defaults leaves only the user's patterns.
        let rc =
            parse("[scan]\nexclude_paths = [\"my-cache\"]\ndefault_exclusions = false\n").unwrap();
        assert!(!rc.scan.default_exclusions_enabled());
        assert_eq!(
            rc.scan.effective_exclude_paths(),
            vec!["my-cache".to_string()]
        );

        // A `!name` entry re-includes one built-in while the rest stay.
        let rc = parse("[scan]\nexclude_paths = [\"!build\", \"my-cache\"]\n").unwrap();
        let effective = rc.scan.effective_exclude_paths();
        assert!(!effective.contains(&"build".to_string()));
        assert!(effective.contains(&"node_modules".to_string()));
        assert!(effective.contains(&"my-cache".to_string()));
        assert_eq!(effective.len(), DEFAULT_EXCLUDE_PATHS.len());

        // `!name` cancels only the built-in, not a positive pattern for the
        // same directory.
        let rc = parse("[scan]\nexclude_paths = [\"build\", \"!build\"]\n").unwrap();
        assert!(rc
            .scan
            .effective_exclude_paths()
            .contains(&"build".to_string()));

        // An exception naming a non-built-in is a structural error.
        assert!(parse("[scan]\nexclude_paths = [\"!buidl\"]\n").is_err());
        assert!(parse("[scan]\nexclude_paths = [\"!\"]\n").is_err());
    }

    #[test]
    fn provider_concurrency_defaults_and_validates() {
        let rc = parse("[limits]\nprovider_concurrency = 4\n").unwrap();
        assert_eq!(rc.provider_concurrency(), 4);
        // Zero can never stall the worker.
        assert!(parse("[limits]\nprovider_concurrency = 0\n").is_err());
    }

    #[test]
    fn accepts_and_validates_scan_root_and_hidden() {
        // Root and hidden are ordinary settings, not rejected keys.
        let rc = parse("[scan]\nroot = \"/home/user\"\nhidden = true\n").unwrap();
        assert_eq!(rc.scan_root(), Some("/home/user"));
        assert!(rc.scan_hidden());
        // Absent hidden defaults to false.
        assert!(!parse("").unwrap().scan_hidden());
        // Root must be absolute, normalized, and free of `~`/env refs.
        assert!(parse("[scan]\nroot = \"relative/x\"\n").is_err());
        assert!(parse("[scan]\nroot = \"/a/../b\"\n").is_err());
        assert!(parse("[scan]\nroot = \"~/Pictures\"\n").is_err());
    }

    #[test]
    fn scan_exclude_extensions_validate_and_survive_normalization() {
        // A leading dot and mixed case are accepted; matching normalizes them.
        let rc = parse("[scan]\nexclude_extensions = [\".PNG\", \"jpg\"]\n").unwrap();
        assert_eq!(rc.scan.exclude_extensions, vec![".PNG", "jpg"]);
        // Empty, path-like, and glob entries are structural errors.
        assert!(parse("[scan]\nexclude_extensions = [\"\"]\n").is_err());
        assert!(parse("[scan]\nexclude_extensions = [\"a/png\"]\n").is_err());
        assert!(parse("[scan]\nexclude_extensions = [\"*\"]\n").is_err());
    }

    #[test]
    fn rejects_unknown_placeholders() {
        let err = parse("target_paths = { photo = \"Pictures/%q/%f\" }");
        assert!(err.is_err());
    }

    #[test]
    fn rejects_bad_description_kind() {
        let err = parse("[description.other]\nenabled = true\n");
        assert!(err.is_err());
        let ok = parse("[description.image]\nenabled = true\nmodel = \"m\"\n");
        assert!(ok.is_ok());
    }

    #[test]
    fn max_video_frames_defaults_falls_back_and_must_be_positive() {
        let rc = parse("").unwrap();
        assert_eq!(
            rc.description.base_effective().max_video_frames,
            DEFAULT_MAX_VIDEO_FRAMES
        );
        // A per-modality override inherits the base entry's cap.
        let rc = parse(
            "[description]\nmax_video_frames = 3\n[description.video]\nenabled = true\nmodel = \"m\"\n",
        )
        .unwrap();
        assert_eq!(rc.description.base_effective().max_video_frames, 3);
        assert_eq!(
            rc.description
                .effective_for_modality(Some("video"))
                .max_video_frames,
            3
        );
        // Zero would sample no frames, so it is a structural error.
        assert!(parse("[description]\nmax_video_frames = 0\n").is_err());
    }

    #[test]
    fn max_context_defaults_falls_back_and_must_be_positive() {
        let rc = parse("").unwrap();
        assert_eq!(
            rc.description.base_effective().max_context,
            DEFAULT_MAX_CONTEXT
        );
        // A per-modality override inherits the base entry's cap.
        let rc = parse(
            "[description]\nmax_context = 8192\n[description.video]\nenabled = true\nmodel = \"m\"\n",
        )
        .unwrap();
        assert_eq!(rc.description.base_effective().max_context, 8192);
        assert_eq!(
            rc.description
                .effective_for_modality(Some("video"))
                .max_context,
            8192
        );
        // A zero context window could not hold a request, so it is a
        // structural error.
        assert!(parse("[description]\nmax_context = 0\n").is_err());
    }

    #[test]
    fn prompt_templates_validate_and_default() {
        // Unknown placeholders are hard errors, per provider entry and per
        // override; known ones and literal-brace escapes pass.
        let err = parse(
            "[description]\nenabled = true\nmodel = \"m\"\nprompt = \"Describe {nme} with {words}\"\n"
        );
        assert!(err.is_err());
        // `{name}` and `{path}` are not placeholders: the full source path
        // (which carries the filename) is always forwarded verbatim after the
        // rendered template, so referencing either is a hard error rather
        // than a no-op.
        let err = parse(
            "[description]\nenabled = true\nmodel = \"m\"\nprompt = \"Describe {name} at {path}\"\n"
        );
        assert!(err.is_err());
        let ok = parse(
            "[description]\nenabled = true\nmodel = \"m\"\nprompt = \"Describe with {words}, literal {{braces}}\"\n"
        );
        assert!(ok.is_ok());
        // Geocoder: `neighbourhood_cities` is an optional string list and
        // an unknown key is rejected; the default (unset) is empty.
        let err = parse("[geocoder]\nenabled = true\ngranularity = \"city\"\n");
        assert!(err.is_err());
        let ok = parse(
            "[geocoder]\nenabled = true\nneighbourhood_cities = [\"Toronto\", \"Shibuya\"]\n",
        );
        assert!(ok.is_ok());

        // [ui]: optional client-side filter state. An unknown key inside the
        // section is rejected like every other section.
        let err = parse("[ui]\ntheme = \"dark\"\n");
        assert!(err.is_err());
        let rc = parse("").unwrap();
        assert_eq!(rc.ui.path_filter, None);
        let rc = parse("[ui]\npath_filter = \"photos\"\n").unwrap();
        assert_eq!(rc.ui.path_filter.as_deref(), Some("photos"));
        // The ui section is visible in the effective document but excluded
        // from the config hash, so saving a filter never marks a running
        // batch stale.
        let with = parse("[ui]\npath_filter = \"photos\"\n").unwrap();
        assert_eq!(
            effective_config_json(&with, None)["ui"]["path_filter"],
            serde_json::json!("photos")
        );
        assert_eq!(config_hash(&with), config_hash(&parse("").unwrap()));

        // Unset resolves to the built-in templates (the default wording). An
        // override inherits the base entry's prompt when it sets none.
        let rc = parse("").unwrap();
        assert_eq!(
            rc.description.base_effective().prompt,
            DEFAULT_DESCRIPTION_PROMPT
        );
        assert!(rc.geocoder.neighbourhood_cities.is_empty());
        let rc = parse(
            "[description]\nenabled = true\nmodel = \"m\"\nprompt = \"Custom {words}\"\n\n[description.image]\nmodel = \"v\"\n"
        )
        .unwrap();
        let base = rc.description.base_effective();
        let image = rc.description.modality_effective("image").unwrap();
        assert_eq!(base.prompt, "Custom {words}");
        assert_eq!(image.prompt, "Custom {words}");
    }

    #[test]
    fn text_modality_defaults_to_the_document_title_prompt() {
        let rc = parse("").unwrap();
        // Documents recover their original title; other modalities keep the
        // generic keyword wording. The base entry (no modality) stays
        // generic.
        assert_eq!(
            rc.description.effective_for_modality(Some("text")).prompt,
            DEFAULT_DOCUMENT_DESCRIPTION_PROMPT
        );
        assert_eq!(
            rc.description.effective_for_modality(Some("image")).prompt,
            DEFAULT_DESCRIPTION_PROMPT
        );
        assert_eq!(
            rc.description.effective_for_modality(None).prompt,
            DEFAULT_DESCRIPTION_PROMPT
        );
        // An explicit base prompt is inherited by every modality, so the
        // document default only applies when nothing was configured.
        let rc = parse("[description]\nprompt = \"Base {words}\"\n").unwrap();
        assert_eq!(
            rc.description.effective_for_modality(Some("text")).prompt,
            "Base {words}"
        );
        // A per-modality override wins over the document default.
        let rc = parse("[description.text]\nprompt = \"Text {words}\"\n").unwrap();
        assert_eq!(
            rc.description.effective_for_modality(Some("text")).prompt,
            "Text {words}"
        );
    }

    #[test]
    fn description_key_requirements() {
        // Structural rules stay hard errors: an unknown service name and an
        // unknown per-modality key can never become valid.
        assert!(
            parse("[description]\nenabled = true\nmodel = \"m\"\nservice = \"openrouter\"\n")
                .is_ok()
        );
        assert!(parse("[description]\nenabled = true\nservice = \"ssh\"\n").is_err());
        assert!(parse("[description.ssh]\nenabled = true\n").is_err());
        // Readiness conditions are warnings, not errors: an enabled entry
        // without a key or model loads and degrades at scan time.
        let rc = parse("[description]\nenabled = true\nmodel = \"m\"\nservice = \"openrouter\"\n")
            .unwrap();
        assert!(config_warnings(&rc)
            .iter()
            .any(|(f, _)| f == "description.api_key"));
        let rc = parse(
            "[description]\nenabled = true\nmodel = \"m\"\nservice = \"ollama\"\napi_key = \"k\"\n",
        )
        .unwrap();
        assert!(config_warnings(&rc)
            .iter()
            .any(|(f, _)| f == "description.api_key"));
        let rc = parse("[description]\nenabled = true\n").unwrap();
        assert!(config_warnings(&rc)
            .iter()
            .any(|(f, _)| f == "description.model"));
    }

    #[test]
    fn geocoder_warnings_are_soft() {
        // Enabling a geocoder without its companion values is a warning, so
        // field-at-a-time edits never fail validation mid-edit.
        let rc = parse(
            "[geocoder]\nenabled = true\nservice = \"google\"\nendpoint = \"https://maps.googleapis.com/maps/api\"\n",
        )
        .unwrap();
        assert!(config_warnings(&rc)
            .iter()
            .any(|(f, _)| f == "geocoder.api_key"));
        // Built-in services fall back to their bundled endpoint at runtime,
        // so enabling nominatim without an explicit endpoint is complete;
        // only a service/keyset contradiction (or the endpoint-less custom
        // service) warrants a warning.
        let rc = parse("[geocoder]\nenabled = true\nservice = \"nominatim\"\n").unwrap();
        assert!(config_warnings(&rc).is_empty());
        let rc = parse("[geocoder]\nenabled = true\nservice = \"nominatim\"\napi_key = \"k\"\n")
            .unwrap();
        assert!(config_warnings(&rc)
            .iter()
            .any(|(f, _)| f == "geocoder.api_key"));
        // The custom service has no built-in endpoint and must be told one.
        let rc = parse("[geocoder]\nenabled = true\nservice = \"custom\"\n").unwrap();
        assert!(config_warnings(&rc)
            .iter()
            .any(|(f, _)| f == "geocoder.endpoint"));
        // An explicitly empty endpoint stays a hard error (it can never
        // become valid without editing the same field again).
        assert!(parse("[geocoder]\nenabled = false\nendpoint = \" \"\n").is_err());
    }

    #[test]
    fn description_per_modality_fallback() {
        // A per-modality entry overrides only the keys it sets; enabled and
        // model inherit from the base entry.
        let rc = parse(
            "[description]\nenabled = true\nmodel = \"base\"\n[description.image]\nmodel = \"vision\"\n",
        )
        .unwrap();
        let img = rc.description.modality_effective("image").unwrap();
        assert!(img.enabled);
        assert_eq!(img.model.as_deref(), Some("vision"));
        assert_eq!(img.service, LlmBackend::Ollama);
        // An explicitly disabled entry suppresses the base for that modality.
        let rc = parse(
            "[description]\nenabled = true\nmodel = \"base\"\n[description.audio]\nenabled = false\n",
        )
        .unwrap();
        assert!(!rc.description.modality_effective("audio").unwrap().enabled);
        // An enabled per-modality entry with no model anywhere warns on its
        // effective settings while the disabled base stays silent.
        let rc = parse("[description.image]\nenabled = true\n").unwrap();
        let ws = config_warnings(&rc);
        assert!(ws.iter().any(|(f, _)| f == "description.image.model"));
        assert!(!ws.iter().any(|(f, _)| f == "description.model"));
    }

    #[test]
    fn doc_extraction_rules() {
        assert!(parse(
            "document_extraction = { pdf = [\"pdftotext\", \"-layout\", \"{path}\", \"-\"] }"
        )
        .is_ok());
        assert!(parse("document_extraction = { pdf = [\"pdftotext\", \"pre{path}\"] }").is_err());
        assert!(parse("document_extraction = { pdf = [\"pdftotext\"] }").is_err());
        assert!(parse("document_extraction = { pdf = [] }").is_ok());
    }

    #[test]
    fn deep_merge_semantics() {
        let mut base: TomlValue = "[description]\nenabled = false\n[description.image]\nenabled = false\n[scan]\nexclude_paths = [\".cache/**\"]\n"
            .parse()
            .unwrap();
        let higher: TomlValue = "[description.image]\nenabled = true\n[scan]\nexclude_paths = []\n"
            .parse()
            .unwrap();
        deep_merge_toml(&mut base, higher);
        let t = base.as_table().unwrap();
        assert_eq!(t["description"]["enabled"].as_bool(), Some(false));
        assert_eq!(t["description"]["image"]["enabled"].as_bool(), Some(true));
        // Arrays are replaced wholesale, not unioned.
        assert_eq!(t["scan"]["exclude_paths"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn deep_merge_json_null_deletes() {
        let mut base = serde_json::json!({
            "geocoder": { "language": "fr", "enabled": true },
            "target_paths": { "photo": "Pics/%f" },
            "scan": { "exclude_paths": ["a/**"] },
        });
        deep_merge_json(
            &mut base,
            serde_json::json!({
                "geocoder": { "language": null, "enabled": false },
                "target_paths": { "photo": null, "audio": "Music/%f" },
                "scan": { "exclude_paths": [] },
            }),
        );
        assert_eq!(base["geocoder"]["language"], serde_json::Value::Null);
        assert!(base["geocoder"].get("language").is_none());
        assert_eq!(base["geocoder"]["enabled"], serde_json::json!(false));
        assert!(base["target_paths"].get("photo").is_none());
        assert_eq!(base["target_paths"]["audio"], serde_json::json!("Music/%f"));
        assert_eq!(base["scan"]["exclude_paths"], serde_json::json!([]));
    }

    #[test]
    fn apply_config_overrides_overrides_and_resets() {
        let mut base = RawConfig::default();
        base.geocoder.enabled = true;
        base.geocoder.language = Some("fr".into());
        base.geocoder.api_key = Some("stored-key".into());
        base.scan.exclude_paths = vec!["old/**".into()];
        base.target_paths
            .insert("photo".into(), "Pics/%Y/%f".into());

        let overlay = serde_json::json!({
            "geocoder": {
                "service": "custom",
                "endpoint": "http://geo.local",
                "language": null,
                "api_key": "launch-key",
            },
            "scan": { "exclude_paths": ["new/**"], "hidden": true },
            "target_paths": { "photo": null, "audio": "Music/%f" },
            "limits": { "tool_timeout_seconds": 7 },
        });
        let raw = apply_config_overrides(&base, &overlay).unwrap();
        assert_eq!(raw.geocoder_service(), GeocoderService::Custom);
        assert_eq!(raw.geocoder.language, None);
        assert_eq!(raw.geocoder.api_key.as_deref(), Some("launch-key"));
        assert!(raw.geocoder.enabled);
        assert_eq!(raw.scan.exclude_paths, vec!["new/**"]);
        assert!(raw.scan_hidden());
        assert!(!raw.target_paths.contains_key("photo"));
        assert_eq!(
            raw.target_paths.get("audio").map(String::as_str),
            Some("Music/%f")
        );
        assert_eq!(raw.limits.tool_timeout_seconds, Some(7));
        // The base is untouched.
        assert_eq!(base.geocoder.api_key.as_deref(), Some("stored-key"));
    }

    #[test]
    fn apply_config_overrides_rejects_bad_documents() {
        let base = RawConfig::default();
        // Non-table overlay.
        assert!(apply_config_overrides(&base, &serde_json::json!(5)).is_err());
        // A relative scan root is invalid as a setting.
        assert!(
            apply_config_overrides(&base, &serde_json::json!({ "scan": { "root": "/x" } })).is_ok()
        );
        assert!(apply_config_overrides(
            &base,
            &serde_json::json!({ "scan": { "root": "relative/x" } })
        )
        .is_err());
        // Unknown keys are rejected by the section schemas.
        assert!(
            apply_config_overrides(&base, &serde_json::json!({ "geocoder": { "bogus": 1 } }))
                .is_err()
        );
        // Invalid target-path template.
        assert!(apply_config_overrides(
            &base,
            &serde_json::json!({ "target_paths": { "photo": "no placeholders" } })
        )
        .is_err());
    }

    /// A throwaway writable config path for the persistence tests, plus the
    /// hermetic search chain that points at it.
    fn write_temp_config(prefix: &str, text: &str) -> (ConfigChain, PathBuf, PathBuf) {
        let chain = ConfigChain::default();
        let d = std::env::temp_dir().join(format!(
            "hl-cfg-{prefix}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&d).unwrap();
        let file = d.join("config.toml");
        std::fs::write(&file, text).unwrap();
        (chain, d, file)
    }

    #[test]
    fn first_write_seeds_the_documented_defaults() {
        // No user config yet: the first saved setting seeds the embedded
        // `defaults.toml` dump and edits it in place, so the file is
        // self-documenting and differs from the dump only in that setting.
        let (chain, dir, file) = write_temp_config("seed", "");
        std::fs::remove_file(&file).unwrap();

        let loaded = merge_and_write_config_chain(
            &chain,
            &serde_json::json!({"duplication": {"large_file_threshold": 42}}),
            Some(&file),
        )
        .unwrap();
        assert_eq!(loaded.raw.duplication.large_file_threshold, 42);
        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            DEFAULT_CONFIG_TEMPLATE.replace(
                "large_file_threshold = 104857600",
                "large_file_threshold = 42"
            )
        );

        // Every later save edits the file that now exists: a key the dump
        // already documents (scan.hidden) is rewritten in place, and a key it
        // does not document yet (scan.root) joins its own section. Nothing
        // else moves, and no key is ever duplicated.
        merge_and_write_config_chain(
            &chain,
            &serde_json::json!({"scan": {"hidden": true}}),
            Some(&file),
        )
        .unwrap();
        let after_hidden = std::fs::read_to_string(&file).unwrap();
        assert_eq!(
            after_hidden,
            DEFAULT_CONFIG_TEMPLATE
                .replace(
                    "large_file_threshold = 104857600",
                    "large_file_threshold = 42"
                )
                .replace("hidden = false\n", "hidden = true\n")
        );
        assert_eq!(after_hidden.matches("hidden =").count(), 1);

        merge_and_write_config_chain(
            &chain,
            &serde_json::json!({"scan": {"root": "/tmp/hl-scan-root"}}),
            Some(&file),
        )
        .unwrap();
        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            DEFAULT_CONFIG_TEMPLATE
                .replace(
                    "large_file_threshold = 104857600",
                    "large_file_threshold = 42"
                )
                .replace("hidden = false\n", "hidden = true\n")
                .replace(
                    "hidden_definition = \"explorer\"\n",
                    "hidden_definition = \"explorer\"\nroot = \"/tmp/hl-scan-root\"\n"
                )
        );

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn first_no_op_save_creates_no_config_file() {
        // A save that leaves the effective configuration alone writes nothing
        // when no config file exists yet, just as it leaves an existing file
        // alone. The per-user path is optional, so it can stay absent.
        let (_, dir, file) = write_temp_config("seed-noop", "");
        std::fs::remove_file(&file).unwrap();
        let chain = ConfigChain {
            system: None,
            user: Some(file.clone()),
        };

        merge_and_write_config_chain(&chain, &serde_json::json!({}), None).unwrap();
        assert!(
            !file.exists(),
            "an empty patch must not create a config file"
        );

        // Restating a documented default is not a change either.
        merge_and_write_config_chain(
            &chain,
            &serde_json::json!({"output": {"format": "pretty"}}),
            None,
        )
        .unwrap();
        assert!(
            !file.exists(),
            "restating a default must not create a config file"
        );

        // An explicitly requested path must exist after the save, so a no-op
        // first save to `--config` writes the dump and the reload succeeds.
        let loaded =
            merge_and_write_config_chain(&chain, &serde_json::json!({}), Some(&file)).unwrap();
        assert_eq!(loaded.config_path.as_deref(), Some(file.as_path()));
        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            DEFAULT_CONFIG_TEMPLATE
        );

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn put_config_edits_only_the_changed_keys() {
        // A hand-written file: comments, blank lines, a deliberate section
        // order, a dotted key, an inline table, a multi-line string, and a
        // one-per-line array.
        let before = concat!(
            "# my personal homelinter config\n",
            "\n",
            "# scanning preferences, written as dotted keys\n",
            "scan.hidden = false\n",
            "scan.root = \"/home/me\"\n",
            "\n",
            "# where photos and documents go\n",
            "[target_paths]\n",
            "photo = \"Pics/%Y/%m/%f\"   # keep months\n",
            "document = \"Docs/%t\"\n",
            "\n",
            "[duplication]\n",
            "large_file_threshold = 1048576\n",
            "\n",
            "[geocoder]\n",
            "endpoint = \"https://nominatim.openstreetmap.org\"\n",
            "language = \"fr\"\n",
            "\n",
            "[description]\n",
            "service = \"ollama\"\n",
            "prompt = \"\"\"\n",
            "Name this in {words} words.\n",
            "Keep it lowercase.\n",
            "\"\"\"\n",
        );
        let (chain, dir, file) = write_temp_config("minimal", before);

        // The UI flips one scan toggle and edits one array entry.
        merge_and_write_config_chain(
            &chain,
            &serde_json::json!({"scan": {"hidden": true}}),
            Some(&file),
        )
        .unwrap();
        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            before.replace("scan.hidden = false", "scan.hidden = true")
        );

        // An array patch rewrites only the entries that changed, keeping the
        // one-per-line layout and each entry's own comment.
        let array_before = concat!(
            "[scan]\n",
            "exclude_paths = [\n",
            "  \"node_modules\",\n",
            "  \"target\",   # rust build dir\n",
            "  \".cache\",\n",
            "]\n",
        );
        let (chain, array_dir, array_file) = write_temp_config("minimal-array", array_before);
        merge_and_write_config_chain(
            &chain,
            &serde_json::json!({"scan": {"exclude_paths": ["node_modules", "target", ".cache", "dist"]}}),
            Some(&array_file),
        )
        .unwrap();
        assert_eq!(
            std::fs::read_to_string(&array_file).unwrap(),
            concat!(
                "[scan]\n",
                "exclude_paths = [\n",
                "  \"node_modules\",\n",
                "  \"target\",   # rust build dir\n",
                "  \".cache\",\n",
                "  \"dist\",\n",
                "]\n",
            )
        );

        std::fs::remove_dir_all(&dir).unwrap();
        std::fs::remove_dir_all(&array_dir).unwrap();
    }

    #[test]
    fn put_config_leaves_the_file_alone_when_nothing_changes() {
        let before = "# keep me\n[duplication]\nlarge_file_threshold = 1048576\n";
        let (chain, dir, file) = write_temp_config("noop", before);
        let stamp = std::fs::metadata(&file).unwrap().modified().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));

        // The same value the file already holds, and an empty patch.
        merge_and_write_config_chain(
            &chain,
            &serde_json::json!({"duplication": {"large_file_threshold": 1048576}}),
            Some(&file),
        )
        .unwrap();
        merge_and_write_config_chain(&chain, &serde_json::json!({}), Some(&file)).unwrap();

        assert_eq!(std::fs::read_to_string(&file).unwrap(), before);
        assert_eq!(
            std::fs::metadata(&file).unwrap().modified().unwrap(),
            stamp,
            "a no-op write must not touch the file"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn put_config_is_a_minimal_diff_for_a_hand_written_file() {
        // Deletion plus pruning: removing the only key of a section drops the
        // now-empty section header, and the rest of the file is untouched.
        let before = concat!(
            "# comment kept\n",
            "[geocoder]\n",
            "language = \"fr\"\n",
            "endpoint = \"https://example.invalid\"\n",
            "\n",
            "[duplication]\n",
            "large_file_threshold = 2048\n",
        );
        let (chain, dir, file) = write_temp_config("delete", before);
        merge_and_write_config_chain(
            &chain,
            &serde_json::json!({"geocoder": {"language": null}}),
            Some(&file),
        )
        .unwrap();
        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            concat!(
                "# comment kept\n",
                "[geocoder]\n",
                "endpoint = \"https://example.invalid\"\n",
                "\n",
                "[duplication]\n",
                "large_file_threshold = 2048\n",
            )
        );

        // A new key is appended to its existing section rather than
        // re-rendering the file.
        merge_and_write_config_chain(
            &chain,
            &serde_json::json!({"duplication": {"large_file_threshold": 4096}}),
            Some(&file),
        )
        .unwrap();
        assert!(std::fs::read_to_string(&file)
            .unwrap()
            .contains("large_file_threshold = 4096\n"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn put_config_preserves_section_and_key_order() {
        // Sections are not alphabetical, and a key sits in the middle
        // section: patching must not reorder anything, and a new key must
        // join the end of its own section rather than the end of the file.
        let before = concat!(
            "[duplication]\n",
            "large_file_threshold = 1\n",
            "\n",
            "[geocoder]\n",
            "endpoint = \"https://example.invalid\"\n",
            "\n",
            "# destinations below\n",
            "[target_paths]\n",
            "video = \"Videos/%f\"\n",
            "photo = \"Pics/%f\"\n",
            "\n",
            "# footer, hand maintained\n",
        );
        let (chain, dir, file) = write_temp_config("order", before);
        merge_and_write_config_chain(
            &chain,
            &serde_json::json!({
                "geocoder": {"language": "fr"},
                "target_paths": {"video": "Clips/%f"},
                "scan": {"hidden": true},
            }),
            Some(&file),
        )
        .unwrap();
        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            concat!(
                "[duplication]\n",
                "large_file_threshold = 1\n",
                "\n",
                "[geocoder]\n",
                "endpoint = \"https://example.invalid\"\n",
                "language = \"fr\"\n",
                "\n",
                "# destinations below\n",
                "[target_paths]\n",
                "video = \"Clips/%f\"\n",
                "photo = \"Pics/%f\"\n",
                "\n",
                "[scan]\n",
                "hidden = true\n",
                "\n",
                "# footer, hand maintained\n",
            )
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn put_config_prunes_a_removed_section_without_leaving_a_blank_line() {
        let before = concat!(
            "[duplication]\n",
            "large_file_threshold = 1\n",
            "\n",
            "[target_paths]\n",
            "photo = \"Archive/%f\"\n",
        );
        let (chain, dir, file) = write_temp_config("prune", before);
        merge_and_write_config_chain(
            &chain,
            &serde_json::json!({"target_paths": {"photo": null}}),
            Some(&file),
        )
        .unwrap();
        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            "[duplication]\nlarge_file_threshold = 1\n"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn put_config_adds_keys_in_place_and_keeps_section_order() {
        let before = concat!(
            "# header comment\n",
            "[target_paths]   # destinations\n",
            "photo = \"Pics/%f\"\n",
            "\n",
            "[duplication]\n",
            "large_file_threshold = 1048576\n",
        );
        let (chain, dir, file) = write_temp_config("insert", before);

        // A new key joins its own section, and a new section is appended
        // after the existing ones; nothing already in the file moves.
        merge_and_write_config_chain(
            &chain,
            &serde_json::json!({
                "target_paths": {"photo": "Pics/%f", "document": "Docs/%t"},
                "scan": {"hidden": true},
            }),
            Some(&file),
        )
        .unwrap();
        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            concat!(
                "# header comment\n",
                "[target_paths]   # destinations\n",
                "photo = \"Pics/%f\"\n",
                "document = \"Docs/%t\"\n",
                "\n",
                "[duplication]\n",
                "large_file_threshold = 1048576\n",
                "\n",
                "[scan]\n",
                "hidden = true\n",
            )
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn put_config_updates_an_inline_table_in_place() {
        let before =
            "# inline style\ntarget_paths = { photo = \"Pics/%f\", document = \"Docs/%t\" }\n";
        let (chain, dir, file) = write_temp_config("inline", before);
        merge_and_write_config_chain(
            &chain,
            &serde_json::json!({"target_paths": {"photo": "Photos/%f"}}),
            Some(&file),
        )
        .unwrap();
        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            "# inline style\ntarget_paths = { photo = \"Photos/%f\", document = \"Docs/%t\" }\n"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn put_null_deletes_persisted_key() {
        // An empty chain: the test stays hermetic even when the host has a
        // real per-user config file (which would otherwise leak into every
        // reload below the explicit file).
        let chain = ConfigChain::default();
        let d = std::env::temp_dir().join(format!(
            "hl-cfg-null-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&d).unwrap();
        let file = d.join("config.toml");
        std::fs::write(
            &file,
            "[geocoder]\nenabled = true\nendpoint = \"https://nominatim.openstreetmap.org\"\nlanguage = \"fr\"\n[target_paths]\nphoto = \"Pics/%f\"\n",
        )
        .unwrap();
        let cli = file.clone();
        let loaded =
            merge_and_write_config_chain(&chain, &serde_json::json!({}), Some(&cli)).unwrap();
        assert_eq!(loaded.raw.geocoder.language.as_deref(), Some("fr"));
        let cleared = merge_and_write_config_chain(
            &chain,
            &serde_json::json!({ "geocoder": { "language": null }, "target_paths": { "photo": null } }),
            Some(&cli),
        )
        .unwrap();
        assert_eq!(cleared.raw.geocoder.language, None);
        assert!(!cleared.raw.target_paths.contains_key("photo"));
        // The deletion is persisted: reloading the file sees it too.
        let reloaded = load_config_chain(&chain, Some(&cli)).unwrap();
        assert_eq!(reloaded.raw.geocoder.language, None);
        assert_eq!(reloaded.raw.target_paths.get("photo"), None);
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn resolved_target_paths_precedence() {
        let rc = parse("target_paths = { photo = \"Pics/%Y/%f\" }").unwrap();
        let resolved = rc.resolved_target_paths().unwrap();
        assert_eq!(resolved[&AssetKind::Photo].raw(), "Pics/%Y/%f");
        assert_eq!(
            resolved[&AssetKind::Audio].raw(),
            AssetKind::Audio.default_format()
        );
        let defaults = RawConfig::default().resolved_target_paths().unwrap();
        assert_eq!(
            defaults[&AssetKind::Photo].raw(),
            AssetKind::Photo.default_format()
        );
    }
}
