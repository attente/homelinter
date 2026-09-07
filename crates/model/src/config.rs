//! Configuration file schema, resolution, deep merge, and validation.
//!
//! Configuration holds persistent preferences only. The scan root and the
//! hidden-file selection are deliberately not part of the schema and are
//! rejected when they appear in a config file. Precedence (highest first):
//! per-scan/per-session settings, config file values, inferred values,
//! opinionated defaults.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use serde_json::Value as JsonValue;
use toml::Value as TomlValue;

use crate::asset_kind;
use crate::error::ApiError;
use crate::format::FormatString;

pub const DEFAULT_LARGE_FILE_THRESHOLD: u64 = 104_857_600;
pub const DEFAULT_MAX_TOKENS: u64 = 512;
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LlmBackend {
    Ollama,
    Openrouter,
    Openai,
    Custom,
}

impl LlmBackend {
    pub fn name(self) -> &'static str {
        match self {
            LlmBackend::Ollama => "ollama",
            LlmBackend::Openrouter => "openrouter",
            LlmBackend::Openai => "openai",
            LlmBackend::Custom => "custom",
        }
    }

    pub fn default_endpoint(self) -> &'static str {
        match self {
            LlmBackend::Ollama => "http://localhost:11434",
            LlmBackend::Openrouter => "https://openrouter.ai/api/v1",
            LlmBackend::Openai => "https://api.openai.com/v1",
            LlmBackend::Custom => "",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
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

#[derive(Debug, Clone, Deserialize, Default)]
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
    /// Minimum interval between requests in seconds, enforced between
    /// request starts on the provider worker. Only meaningful for the
    /// `custom` service: the built-in public services keep their documented
    /// usage pace regardless (see "Provider Plugins" in DESIGN.md).
    /// `0` (the default) means no built-in delay.
    #[serde(default)]
    pub rate_limit_seconds: Option<u64>,
}

impl GeocoderConfig {
    pub fn geocoder_service(&self) -> GeocoderService {
        self.service.unwrap_or(GeocoderService::Nominatim)
    }
}

/// A description provider entry: the base `[description]` table or one
/// per-kind override such as `[description.photo]`. LLM transport settings
/// (`backend`, `endpoint`, `model`, `api_key`, `max_tokens`) live directly
/// on each entry; unset keys fall back to the base entry.
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct DescriptionProvider {
    #[serde(default)]
    pub enabled: Option<bool>,
    #[serde(default)]
    pub backend: Option<LlmBackend>,
    #[serde(default)]
    pub endpoint: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub api_key: Option<String>,
    #[serde(default)]
    pub max_tokens: Option<u64>,
}

/// The effective settings of a description provider entry after per-key
/// fallback to the base entry.
#[derive(Debug, Clone)]
pub struct EffectiveDescription {
    pub enabled: bool,
    pub backend: LlmBackend,
    pub endpoint: Option<String>,
    pub model: Option<String>,
    pub api_key: Option<String>,
    pub max_tokens: u64,
}

impl DescriptionProvider {
    /// Effective settings for this entry, with unset keys inherited from
    /// `base`. For the base entry itself, pass it as `base` as well.
    pub fn effective(&self, base: &DescriptionProvider) -> EffectiveDescription {
        EffectiveDescription {
            enabled: self.enabled.or(base.enabled).unwrap_or(false),
            backend: self.backend.or(base.backend).unwrap_or(LlmBackend::Ollama),
            endpoint: self.endpoint.clone().or_else(|| base.endpoint.clone()),
            model: self.model.clone().or_else(|| base.model.clone()),
            api_key: self.api_key.clone().or_else(|| base.api_key.clone()),
            max_tokens: self
                .max_tokens
                .or(base.max_tokens)
                .unwrap_or(DEFAULT_MAX_TOKENS),
        }
    }
}

impl DescriptionConfig {
    /// Effective settings of the base provider.
    pub fn base_effective(&self) -> EffectiveDescription {
        self.base.effective(&self.base)
    }

    /// Effective settings for one per-kind entry, if present.
    pub fn kind_effective(&self, kind: &str) -> Option<EffectiveDescription> {
        self.per_kind.get(kind).map(|p| p.effective(&self.base))
    }
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct DescriptionConfig {
    /// Base provider fields; the base entry is the default provider for
    /// every content kind without an explicit per-kind override.
    #[serde(default, flatten)]
    pub base: DescriptionProvider,
    /// Per-kind overrides, e.g. `[description.photo]`. Serde cannot combine
    /// `flatten` with `deny_unknown_fields`, so unknown per-kind keys are
    /// validated manually by `validate_config`.
    #[serde(default, flatten)]
    pub per_kind: BTreeMap<String, DescriptionProvider>,
}

#[derive(Debug, Clone, Deserialize)]
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HiddenDefinition {
    Explorer,
    Attribute,
    Dotfiles,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct ScanConfig {
    #[serde(default)]
    pub exclude_paths: Vec<String>,
    #[serde(default)]
    pub hidden_definition: Option<HiddenDefinition>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OutputFormat {
    Pretty,
    Json,
    Toml,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct OutputConfig {
    #[serde(default)]
    pub format: Option<OutputFormat>,
}

#[derive(Debug, Clone, Deserialize)]
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

#[derive(Debug, Clone, Deserialize, Default)]
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
#[derive(Debug, Clone, Deserialize, Default)]
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
        for (k, _) in overrides {
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

/// Deep merge of two JSON values with identical semantics, used by
/// `PUT /config` which merges submitted values into the existing file.
pub fn deep_merge_json(base: &mut JsonValue, higher: JsonValue) {
    match base {
        JsonValue::Object(base_o) => {
            let JsonValue::Object(high_o) = &higher else {
                *base = higher;
                return;
            };
            for (key, high_val) in high_o {
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
    let mut merged = TomlValue::Table(Default::default());
    let mut highest_existing: Option<PathBuf> = None;
    let mut had_user = false;

    if let Some(sys) = system_config_path() {
        if let Some(v) = read_config_file(&sys, false)? {
            deep_merge_toml(&mut merged, v);
            highest_existing = Some(sys);
        }
    }
    if let Some(user) = default_user_config_path() {
        if let Some(v) = read_config_file(&user, false)? {
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
    if let Some(p) = cli_config {
        return Ok(p.to_path_buf());
    }
    let user = default_user_config_path()
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

    // target_paths
    crate::format::validate_target_paths(&raw.target_paths)
        .err()
        .map(|e| {
            if let Some(details) = e.body.details {
                fields.extend(
                    details
                        .into_iter()
                        .map(|(k, v)| (k, v.as_str().map(str::to_string).unwrap_or_default())),
                );
            } else {
                fields.push(("target_paths".into(), e.body.message));
            }
        });

    // geocoder
    let geo = &raw.geocoder;
    if geo.enabled {
        let key = geo.api_key.as_deref().map(str::trim).unwrap_or("");
        // The configured rate limit paces only the custom service; the
        // built-in public services always enforce their documented usage pace.
        let rate_warn = || {
            if geo.rate_limit_seconds.is_some() {
                Some((
                    "geocoder.rate_limit_seconds".to_string(),
                    "rate_limit_seconds applies only to the custom service; built-in public services always enforce their documented usage pace"
                        .to_string(),
                ))
            } else {
                None
            }
        };
        match geo.geocoder_service() {
            GeocoderService::Google | GeocoderService::Mapbox => {
                if key.is_empty() {
                    fields.push((
                        "geocoder.api_key".into(),
                        format!(
                            "api_key is required by the {:?} service when enabled",
                            geo.geocoder_service()
                        )
                        .to_lowercase(),
                    ));
                }
                if let Some(w) = rate_warn() {
                    fields.push(w);
                }
            }
            GeocoderService::Nominatim | GeocoderService::Photon => {
                if !key.is_empty() {
                    fields.push((
                        "geocoder.api_key".into(),
                        format!(
                            "api_key is rejected for the {:?} service",
                            geo.geocoder_service()
                        )
                        .to_lowercase(),
                    ));
                }
                if let Some(w) = rate_warn() {
                    fields.push(w);
                }
            }
            GeocoderService::Custom => {
                if geo
                    .endpoint
                    .as_deref()
                    .map(str::trim)
                    .unwrap_or("")
                    .is_empty()
                {
                    fields.push((
                        "geocoder.endpoint".into(),
                        "endpoint is required for the custom service".into(),
                    ));
                }
            }
        }
        if geo
            .endpoint
            .as_deref()
            .map(str::trim)
            .unwrap_or("")
            .is_empty()
        {
            fields.push((
                "geocoder.endpoint".into(),
                "endpoint must not be empty when the geocoder is enabled".into(),
            ));
        }
    }

    // description: every provider entry (base and per-kind) is validated
    // on its effective settings, with unset keys inherited from the base.
    let desc = &raw.description;
    asset_kind::validate_per_kind_description_keys(
        desc.per_kind
            .keys()
            .cloned()
            .collect::<Vec<_>>()
            .into_iter(),
    )
    .err()
    .map(|e| {
        if let Some(details) = e.body.details {
            fields.extend(
                details
                    .into_iter()
                    .map(|(k, v)| (k, v.as_str().map(str::to_string).unwrap_or_default())),
            );
        }
    });
    fn check_provider(prefix: String, eff: EffectiveDescription, fields: &mut Vec<(String, String)>) {
        if let Some(ep) = &eff.endpoint {
            if ep.trim().is_empty() {
                fields.push((
                    format!("{prefix}.endpoint"),
                    "endpoint must not be empty".into(),
                ));
            }
        }
        if eff.max_tokens == 0 {
            fields.push((
                format!("{prefix}.max_tokens"),
                "max_tokens must be positive".into(),
            ));
        }
        if !eff.enabled {
            return;
        }
        if eff.model.as_deref().map(str::trim).unwrap_or("").is_empty() {
            fields.push((
                format!("{prefix}.model"),
                "model is required when the description provider is enabled".into(),
            ));
        }
        match eff.backend {
            LlmBackend::Custom => {
                if eff
                    .endpoint
                    .as_deref()
                    .map(str::trim)
                    .unwrap_or("")
                    .is_empty()
                {
                    fields.push((
                        format!("{prefix}.endpoint"),
                        "endpoint is required for the custom backend".into(),
                    ));
                }
            }
            LlmBackend::Openrouter | LlmBackend::Openai => {
                if eff
                    .api_key
                    .as_deref()
                    .map(str::trim)
                    .unwrap_or("")
                    .is_empty()
                {
                    fields.push((
                        format!("{prefix}.api_key"),
                        format!(
                            "api_key is required by the {:?} backend when enabled",
                            eff.backend
                        )
                        .to_lowercase(),
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
                    fields.push((
                        format!("{prefix}.api_key"),
                        "api_key is rejected for the ollama backend".into(),
                    ));
                }
            }
        }
    }
    check_provider("description".into(), desc.base_effective(), &mut fields);
    for kind in desc.per_kind.keys() {
        let eff = desc
            .kind_effective(kind)
            .expect("per-kind key iterated from the same map");
        check_provider(format!("description.{kind}"), eff, &mut fields);
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
fn writable_file_toml(cli: Option<&Path>) -> Result<TomlValue, ApiError> {
    let path = writable_config_path(cli)?;
    Ok(read_config_file(&path, false)?.unwrap_or_else(|| TomlValue::Table(Default::default())))
}

/// Merge a partial configuration document into the file `PUT /config`
/// writes to, validate the merged result, write it atomically, then reload
/// the full precedence chain. Returns the effective config and its path.
pub fn merge_and_write_config(
    submitted: &JsonValue,
    cli: Option<&Path>,
) -> Result<LoadedConfig, ApiError> {
    let write_path = writable_config_path(cli)?;
    let mut file_toml = writable_file_toml(cli)?;
    let sub_toml = json_to_toml(submitted)
        .ok_or_else(|| ApiError::validation("configuration must be a table"))?;
    deep_merge_toml(&mut file_toml, sub_toml);
    let raw: RawConfig = file_toml
        .clone()
        .try_into()
        .map_err(|e| ApiError::validation(format!("invalid configuration: {e}")))?;
    validate_config(&raw)?;
    write_config_toml(&write_path, &file_toml)?;
    load_config(cli)
}

/// Validate a partial configuration document exactly as `PUT /config`
/// would merge it, without persisting anything. Returns the merged raw
/// config when valid.
pub fn merge_and_validate_config(
    submitted: &JsonValue,
    cli: Option<&Path>,
) -> Result<RawConfig, ApiError> {
    let mut file_toml = writable_file_toml(cli)?;
    let sub_toml = json_to_toml(submitted)
        .ok_or_else(|| ApiError::validation("configuration must be a table"))?;
    deep_merge_toml(&mut file_toml, sub_toml);
    let raw: RawConfig = file_toml
        .try_into()
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
        JsonValue::Number(n) => {
            if let Some(i) = n.as_i64() {
                Some(TomlValue::Integer(i))
            } else if let Some(f) = n.as_f64() {
                Some(TomlValue::Float(f))
            } else {
                None
            }
        }
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
            "rate_limit_seconds": raw.geocoder.rate_limit_seconds,
        }),
    );
    let mut desc = serde_json::Map::new();
    let base_eff = raw.description.base_effective();
    if let Some(e) = desc_base_enabled(&raw.description) {
        desc.insert("enabled".into(), JsonValue::Bool(e));
    }
    desc.insert(
        "backend".into(),
        JsonValue::String(llm_backend_name(base_eff.backend).into()),
    );
    desc.insert(
        "endpoint".into(),
        JsonValue::String(
            base_eff
                .endpoint
                .clone()
                .unwrap_or_else(|| base_eff.backend.default_endpoint().into()),
        ),
    );
    desc.insert("model".into(), serde_json::to_value(&base_eff.model).unwrap_or(JsonValue::Null));
    desc.insert("max_tokens".into(), JsonValue::from(base_eff.max_tokens));
    for (kind, p) in &raw.description.per_kind {
        let eff = p.effective(&raw.description.base);
        desc.insert(
            kind.clone(),
            serde_json::json!({
                "enabled": eff.enabled,
                "backend": llm_backend_name(eff.backend),
                "endpoint": eff.endpoint.clone().unwrap_or_else(|| eff.backend.default_endpoint().into()),
                "model": eff.model,
                "max_tokens": eff.max_tokens,
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
        LlmBackend::Openai => "openai",
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
        assert_eq!(
            rc.description.base_effective().max_tokens,
            DEFAULT_MAX_TOKENS
        );
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
        let ok = parse("[description.photo]\nenabled = true\nmodel = \"m\"\n");
        assert!(ok.is_ok());
    }

    #[test]
    fn description_key_requirements() {
        assert!(parse("[description]\nenabled = true\nmodel = \"m\"\nbackend = \"openai\"\n").is_err());
        assert!(parse(
            "[description]\nenabled = true\nmodel = \"m\"\nbackend = \"openai\"\napi_key = \"k\"\n"
        )
        .is_ok());
        assert!(parse(
            "[description]\nenabled = true\nmodel = \"m\"\nbackend = \"ollama\"\napi_key = \"k\"\n"
        )
        .is_err());
        assert!(parse("[description]\nenabled = true\n").is_err());
    }

    #[test]
    fn description_per_kind_fallback() {
        // A per-kind entry overrides only the keys it sets; enabled and
        // model inherit from the base entry.
        let rc = parse(
            "[description]\nenabled = true\nmodel = \"base\"\n[description.photo]\nmodel = \"vision\"\n",
        )
        .unwrap();
        let photo = rc.description.kind_effective("photo").unwrap();
        assert!(photo.enabled);
        assert_eq!(photo.model.as_deref(), Some("vision"));
        assert_eq!(photo.backend, LlmBackend::Ollama);
        // An explicitly disabled entry suppresses the base for that kind.
        let rc = parse(
            "[description]\nenabled = true\nmodel = \"base\"\n[description.audio]\nenabled = false\n",
        )
        .unwrap();
        assert!(!rc.description.kind_effective("audio").unwrap().enabled);
        // An enabled per-kind entry with no model anywhere is rejected on
        // its effective settings while the disabled base stays valid.
        assert!(parse("[description.photo]\nenabled = true\n").is_err());
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
        let mut base: TomlValue = "[description]\nenabled = false\n[description.photo]\nenabled = false\n[scan]\nexclude_paths = [\".cache/**\"]\n"
            .parse()
            .unwrap();
        let higher: TomlValue =
            "[description.photo]\nenabled = true\n[scan]\nexclude_paths = []\n"
                .parse()
                .unwrap();
        deep_merge_toml(&mut base, higher);
        let t = base.as_table().unwrap();
        assert_eq!(t["description"]["enabled"].as_bool(), Some(false));
        assert_eq!(t["description"]["photo"]["enabled"].as_bool(), Some(true));
        // Arrays are replaced wholesale, not unioned.
        assert_eq!(t["scan"]["exclude_paths"].as_array().unwrap().len(), 0);
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
