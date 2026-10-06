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
pub const DEFAULT_PROVIDER_TIMEOUT_SECONDS: u64 = 120;
pub const DEFAULT_TOOL_TIMEOUT_SECONDS: u64 = 30;
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
// silent. Section-level structs deny unknown fields so invalid keys (such as
// the session-scoped `scan.root` and `scan.hidden`) are rejected.
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
    /// Kinds without a content modality (repository, archive, data,
    /// executable, other) never use it - they have no content source for
    /// `%s` - except location summaries, which always use the text
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

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct ScanConfig {
    #[serde(default)]
    pub exclude_paths: Vec<String>,
    #[serde(default)]
    pub hidden_definition: Option<HiddenDefinition>,
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
    pub tool_timeout_seconds: Option<u64>,
    #[serde(default)]
    pub tool_stdout_max_bytes: Option<u64>,
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

    pub fn provider_timeout(&self) -> u64 {
        self.limits
            .provider_timeout_seconds
            .unwrap_or(DEFAULT_PROVIDER_TIMEOUT_SECONDS)
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
    /// precedence: per-scan overrides, config, then defaults.
    pub fn resolved_target_paths(
        &self,
        overrides: &BTreeMap<String, String>,
    ) -> Result<BTreeMap<asset_kind::AssetKind, FormatString>, ApiError> {
        let mut out = BTreeMap::new();
        let mut seen: Vec<String> = Vec::new();
        for kind in asset_kind::AssetKind::ALL {
            let template = overrides
                .get(kind.name())
                .or_else(|| self.target_paths.get(kind.name()))
                .map(String::as_str)
                .unwrap_or_else(|| kind.default_format());
            let fs = FormatString::parse(template).map_err(|e| {
                ApiError::validation(format!(
                    "invalid destination format string for {}: {e}",
                    kind.name()
                ))
            })?;
            out.insert(kind, fs);
            seen.push(kind.name().to_string());
        }
        // Reject overrides that name an unknown kind (checked by caller too,
        // but keep this function total).
        for k in overrides.keys() {
            if asset_kind::AssetKind::from_name(k).is_none() {
                return Err(ApiError::validation(format!("unknown asset kind '{k}'")));
            }
        }
        let _ = seen;
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

/// Deep merge of two JSON values with the semantics `PUT /config` needs:
/// tables merge by key, scalars and arrays replace, empty tables contribute
/// nothing, and a `null` value deletes the key from the base object (the
/// only way a caller can unset a previously persisted value).
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

/// Total TOML -> JSON conversion for the `PUT /config` persistence path
/// (the writable file is merged in JSON space so `null` deletions work,
/// then converted back for parsing and writing).
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

/// Persist a TOML value atomically to `path`.
pub fn write_config_toml(path: &Path, value: &TomlValue) -> Result<(), ApiError> {
    let text = toml::to_string_pretty(value)
        .map_err(|e| ApiError::internal(format!("cannot serialize config: {e}")))?;
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
        if pat.trim().is_empty() {
            fields.push((
                format!("scan.exclude_paths[{i}]"),
                "pattern must not be empty".into(),
            ));
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

    // Cache: `scan.root` and `scan.hidden` are not valid keys; deny
    // unknown fields on ScanConfig already rejects them at parse time.

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

/// The file content currently at the writable path (the file `PUT /config`
/// writes to), or an empty table when no file exists there.
fn writable_file_toml(chain: &ConfigChain, cli: Option<&Path>) -> Result<TomlValue, ApiError> {
    let path = writable_config_path_chain(chain, cli)?;
    Ok(read_config_file(&path, false)?.unwrap_or_else(|| TomlValue::Table(Default::default())))
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
    let write_path = writable_config_path_chain(chain, cli)?;
    let mut file_json = writable_file_toml(chain, cli).map(|t| toml_to_json(&t))?;
    deep_merge_json(&mut file_json, submitted.clone());
    let merged_toml = json_to_toml(&file_json)
        .ok_or_else(|| ApiError::validation("configuration must be a table"))?;
    let raw: RawConfig = merged_toml
        .clone()
        .try_into()
        .map_err(|e| ApiError::validation(format!("invalid configuration: {e}")))?;
    validate_config(&raw)?;
    write_config_toml(&write_path, &merged_toml)?;
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
    let mut file_json = writable_file_toml(chain, cli).map(|t| toml_to_json(&t))?;
    deep_merge_json(&mut file_json, submitted.clone());
    let merged_toml = json_to_toml(&file_json)
        .ok_or_else(|| ApiError::validation("configuration must be a table"))?;
    let raw: RawConfig = merged_toml
        .try_into()
        .map_err(|e| ApiError::validation(format!("invalid configuration: {e}")))?;
    validate_config(&raw)?;
    Ok(raw)
}

/// Apply a partial per-scan configuration overlay (the `config` member of
/// `POST /session/scan`) on top of the effective configuration. The overlay
/// uses the same shape and merge semantics as `PUT /config` - tables merge
/// by key, scalars and arrays replace, `null` deletes the key so the
/// built-in default applies - but nothing is persisted: the result is the
/// configuration snapshot for that one scan only.
pub fn overlay_scan_config(base: &RawConfig, overlay: &JsonValue) -> Result<RawConfig, ApiError> {
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
            "language": raw.geocoder.language,
            "user_agent_contact": raw.geocoder.user_agent_contact,
            "neighbourhood_cities": raw.geocoder.neighbourhood_cities,
            "rate_limit_ms": raw.geocoder.rate_limit_ms,
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
            "exclude_paths": raw.scan.exclude_paths,
            "hidden_definition": match raw.hidden_definition() {
                HiddenDefinition::Explorer => "explorer",
                HiddenDefinition::Attribute => "attribute",
                HiddenDefinition::Dotfiles => "dotfiles",
            },
        }),
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
            "tool_timeout_seconds": raw.tool_timeout(),
            "tool_stdout_max_bytes": raw.tool_stdout_max_bytes(),
        }),
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
    let json = effective_config_json(raw, None);
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
        assert_eq!(rc.description.base_effective().max_words, DEFAULT_MAX_WORDS);
        assert_eq!(rc.output_format(), OutputFormat::Pretty);
        assert_eq!(rc.hidden_definition(), HiddenDefinition::Explorer);
    }

    #[test]
    fn rejects_scan_root_and_hidden() {
        assert!(parse("[scan]\nroot = \"/x\"\n").is_err());
        assert!(parse("[scan]\nhidden = true\n").is_err());
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
    fn overlay_scan_config_overrides_and_resets() {
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
                "api_key": "per-scan-key",
            },
            "scan": { "exclude_paths": ["new/**"] },
            "target_paths": { "photo": null, "audio": "Music/%f" },
            "limits": { "tool_timeout_seconds": 7 },
        });
        let raw = overlay_scan_config(&base, &overlay).unwrap();
        assert_eq!(raw.geocoder_service(), GeocoderService::Custom);
        assert_eq!(raw.geocoder.language, None);
        assert_eq!(raw.geocoder.api_key.as_deref(), Some("per-scan-key"));
        assert!(raw.geocoder.enabled);
        assert_eq!(raw.scan.exclude_paths, vec!["new/**"]);
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
    fn overlay_scan_config_rejects_bad_documents() {
        let base = RawConfig::default();
        // Non-table overlay.
        assert!(overlay_scan_config(&base, &serde_json::json!(5)).is_err());
        // Session-scoped keys stay rejected.
        assert!(
            overlay_scan_config(&base, &serde_json::json!({ "scan": { "root": "/x" } })).is_err()
        );
        // Unknown keys are rejected by the section schemas.
        assert!(
            overlay_scan_config(&base, &serde_json::json!({ "geocoder": { "bogus": 1 } })).is_err()
        );
        // Invalid target-path template.
        assert!(overlay_scan_config(
            &base,
            &serde_json::json!({ "target_paths": { "photo": "no placeholders" } })
        )
        .is_err());
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
        let overrides = BTreeMap::from([("photo".to_string(), "Gallery/%f".to_string())]);
        let resolved = rc.resolved_target_paths(&overrides).unwrap();
        assert_eq!(resolved[&AssetKind::Photo].raw(), "Gallery/%f");
        assert_eq!(
            resolved[&AssetKind::Audio].raw(),
            AssetKind::Audio.default_format()
        );
        let no_override = rc.resolved_target_paths(&BTreeMap::new()).unwrap();
        assert_eq!(no_override[&AssetKind::Photo].raw(), "Pics/%Y/%f");
    }
}
