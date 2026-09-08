//! Asset kind classification and destination defaults.

use std::collections::BTreeMap;
use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::error::ApiError;

/// homelinter's file categories. Every discovered unit is classified into
/// exactly one kind; classification drives destination format strings and
/// reporting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AssetKind {
    Document,
    Photo,
    Video,
    Audio,
    Repository,
    Archive,
    Data,
    Executable,
    Other,
}

impl AssetKind {
    /// Every kind in canonical order.
    pub const ALL: [AssetKind; 9] = [
        AssetKind::Document,
        AssetKind::Photo,
        AssetKind::Video,
        AssetKind::Audio,
        AssetKind::Repository,
        AssetKind::Archive,
        AssetKind::Data,
        AssetKind::Executable,
        AssetKind::Other,
    ];

    /// Kinds whose contents may be described by a content provider.
    pub const CONTENT_KINDS: [AssetKind; 4] = [
        AssetKind::Document,
        AssetKind::Photo,
        AssetKind::Video,
        AssetKind::Audio,
    ];

    /// Canonical lowercase name, e.g. `photo`. Safe for `%k` interpolation.
    pub fn name(self) -> &'static str {
        match self {
            AssetKind::Document => "document",
            AssetKind::Photo => "photo",
            AssetKind::Video => "video",
            AssetKind::Audio => "audio",
            AssetKind::Repository => "repository",
            AssetKind::Archive => "archive",
            AssetKind::Data => "data",
            AssetKind::Executable => "executable",
            AssetKind::Other => "other",
        }
    }

    /// The opinionated default destination format string for the kind.
    /// These preserve the original filename via `%f`.
    pub fn default_format(self) -> &'static str {
        match self {
            AssetKind::Document => "Documents/%f",
            AssetKind::Photo => "Pictures/%Y/%m/%f",
            AssetKind::Video => "Videos/%Y/%m/%f",
            AssetKind::Audio => "Music/%f",
            AssetKind::Repository => "Projects/%f",
            AssetKind::Archive => "Archives/%f",
            AssetKind::Data => "Data/%f",
            AssetKind::Executable => "bin/%f",
            AssetKind::Other => "Other/%f",
        }
    }
}

impl Default for AssetKind {
    fn default() -> Self {
        AssetKind::Other
    }
}

impl fmt::Display for AssetKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

impl FromStr for AssetKind {
    type Err = ApiError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::from_name(s).ok_or_else(|| {
            ApiError::validation(format!("unknown asset kind '{s}'; expected one of: document, photo, video, audio, repository, archive, data, executable, other"))
        })
    }
}

impl AssetKind {
    pub fn from_name(s: &str) -> Option<AssetKind> {
        match s {
            "document" => Some(AssetKind::Document),
            "photo" => Some(AssetKind::Photo),
            "video" => Some(AssetKind::Video),
            "audio" => Some(AssetKind::Audio),
            "repository" => Some(AssetKind::Repository),
            "archive" => Some(AssetKind::Archive),
            "data" => Some(AssetKind::Data),
            "executable" => Some(AssetKind::Executable),
            "other" => Some(AssetKind::Other),
            _ => None,
        }
    }
}

/// Extension-to-kind mapping. The final extension wins over earlier ones so
/// `archive.tar.gz` classifies as an archive by `gz` handling below.
fn extension_kind(ext: &str) -> AssetKind {
    match ext {
        // Document
        "pdf" | "doc" | "docx" | "odt" | "txt" | "md" | "markdown" | "rtf" | "epub" | "tex"
        | "pages" | "key" | "ppt" | "pptx" | "xls" | "xlsx" => AssetKind::Document,
        // Photo
        "jpg" | "jpeg" | "png" | "gif" | "webp" | "svg" | "bmp" | "tif" | "tiff" | "heic"
        | "raw" | "cr2" | "cr3" | "nef" | "arw" | "dng" | "orf" | "rw2" => AssetKind::Photo,
        // Video
        "mp4" | "mkv" | "avi" | "mov" | "webm" | "flv" | "wmv" | "m4v" | "mpg" | "mpeg" | "3gp"
        | "ts" => AssetKind::Video,
        // Audio
        "mp3" | "flac" | "ogg" | "m4a" | "wav" | "aac" | "opus" | "wma" | "aiff" | "alac" => {
            AssetKind::Audio
        }
        // Archive (two-part forms handled by the caller via is_archive_name)
        "zip" | "tar" | "gz" | "bz2" | "xz" | "7z" | "rar" | "zst" | "tgz" | "tbz2" | "txz"
        | "iso" => AssetKind::Archive,
        // Data
        "json" | "xml" | "csv" | "yaml" | "yml" | "toml" | "sql" | "db" | "sqlite" | "parquet"
        | "ndjson" | "tsv" | "ini" | "conf" | "cfg" => AssetKind::Data,
        // Executable (source and binary forms)
        "exe" | "app" | "sh" | "py" | "rb" | "js" | "wasm" | "pl" | "php" | "rs" | "go" | "c"
        | "h" | "cpp" | "bin" | "msi" | "dmg" | "deb" | "rpm" | "apk" => AssetKind::Executable,
        _ => AssetKind::Other,
    }
}

/// Compound-archive names resolved before the simple extension mapping so
/// `tar.gz` and friends classify as archives rather than documents by `gz`.
pub fn classify_file_name(name: &str) -> AssetKind {
    let lower = name.to_lowercase();
    for two in [
        "tar.gz", "tar.bz2", "tar.xz", "tar.zst", "tar.lz", "tar.lz4", "tar.z", "tar.lzo",
    ] {
        if lower.ends_with(two) {
            return AssetKind::Archive;
        }
    }
    if lower == "cargo.lock"
        || lower == "package-lock.json"
        || lower == "pnpm-lock.yaml"
        || lower == "yarn.lock"
    {
        return AssetKind::Data;
    }
    let dot = lower.rfind('.');
    match dot {
        Some(i) if i + 1 < lower.len() => extension_kind(&lower[i + 1..]),
        _ => AssetKind::Other,
    }
}

/// Classification for a full path: use the file name only (the parent
/// directories never influence the kind).
pub fn classify_path(p: &std::path::Path) -> AssetKind {
    match p.file_name().and_then(|n| n.to_str()) {
        Some(name) => classify_file_name(name),
        None => AssetKind::Other,
    }
}

/// Content-description modality names, in canonical order. These are
/// the per-modality keys accepted under `[description]` overrides
/// (e.g. `[description.text]`, `[description.image]`), one per content
/// kind (see `description_modality`).
pub const DESCRIPTION_MODALITIES: [&str; 4] = ["text", "image", "video", "audio"];

/// The content-description modality for an asset kind, if any. Kinds
/// without a modality (repository, archive, data, executable, other)
/// have no content source for `%s`, so no description provider is ever
/// planned for them and `%s` resolves to `unknown`.
pub fn description_modality(kind: AssetKind) -> Option<&'static str> {
    match kind {
        AssetKind::Document => Some("text"),
        AssetKind::Photo => Some("image"),
        AssetKind::Video => Some("video"),
        AssetKind::Audio => Some("audio"),
        _ => None,
    }
}

/// Content-description per-modality keys are restricted to the content
/// modalities (`DESCRIPTION_MODALITIES`).
pub fn validate_per_modality_description_keys(
    keys: impl Iterator<Item = String>,
) -> Result<(), ApiError> {
    let mut fields = Vec::new();
    for k in keys {
        if !DESCRIPTION_MODALITIES.contains(&k.as_str()) {
            fields.push((
                format!("description.{k}"),
                "per-modality description keys must be one of: text, image, video, audio"
                    .to_string(),
            ));
        }
    }
    if fields.is_empty() {
        Ok(())
    } else {
        Err(ApiError::validation_errors(
            "invalid description per-modality configuration",
            fields,
        ))
    }
}

/// The default `target_paths` mapping used when configuration and inference
/// supply nothing for a kind.
pub fn default_target_paths() -> BTreeMap<AssetKind, String> {
    AssetKind::ALL
        .iter()
        .map(|k| (*k, k.default_format().to_string()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_extensions() {
        assert_eq!(classify_file_name("a.pdf"), AssetKind::Document);
        assert_eq!(classify_file_name("IMG_0001.JPG"), AssetKind::Photo);
        assert_eq!(classify_file_name("clip.mov"), AssetKind::Video);
        assert_eq!(classify_file_name("song.FLAC"), AssetKind::Audio);
        assert_eq!(classify_file_name("bundle.tar.gz"), AssetKind::Archive);
        assert_eq!(classify_file_name("data.json"), AssetKind::Data);
        assert_eq!(classify_file_name("run.sh"), AssetKind::Executable);
        assert_eq!(classify_file_name("notes"), AssetKind::Other);
    }

    #[test]
    fn default_formats_preserve_name() {
        for k in AssetKind::ALL {
            assert!(k.default_format().contains("%f"));
        }
    }
}
