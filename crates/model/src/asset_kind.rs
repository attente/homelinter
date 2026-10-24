//! Asset kind classification and destination defaults.

use std::collections::BTreeMap;
use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::error::ApiError;

/// homelinter's file categories. homelinter only operates on files it can
/// name from content metadata (document, photo, video, audio) or relocate
/// safely as an atomic unit (repository, archive); every other file is
/// unmanaged: it is never extracted, hashed, or suggested, and never moved
/// or deduplicated.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default, Serialize, Deserialize,
)]
#[serde(rename_all = "lowercase")]
pub enum AssetKind {
    #[default]
    Document,
    Photo,
    Video,
    Audio,
    Repository,
    Archive,
}

impl AssetKind {
    /// Every kind in canonical order.
    pub const ALL: [AssetKind; 6] = [
        AssetKind::Document,
        AssetKind::Photo,
        AssetKind::Video,
        AssetKind::Audio,
        AssetKind::Repository,
        AssetKind::Archive,
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
        }
    }

    /// The opinionated default destination format string for the kind.
    /// Media and document kinds name the target from metadata; `%e` keeps
    /// the original extension. Archives preserve the original filename via
    /// `%f` (the name carries version/source identity) under a year/month
    /// nest dated from the file's modification time, which is always
    /// available; repositories keep their own name under the remote owner.
    pub fn default_format(self) -> &'static str {
        match self {
            AssetKind::Document => "Documents/%a-%t.%e",
            AssetKind::Photo => "Photos/%Y-%m-%d-%H-%M-%S-%l-%s.%e",
            AssetKind::Video => "Videos/%Y-%m-%d-%H-%M-%S-%l-%s.%e",
            AssetKind::Audio => "Audio/%a-%t.%e",
            AssetKind::Repository => "Projects/%o/%n",
            AssetKind::Archive => "Archives/%Y/%m/%f",
        }
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
            ApiError::validation(format!("unknown asset kind '{s}'; expected one of: document, photo, video, audio, repository, archive"))
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
            _ => None,
        }
    }
}

/// Extension-to-kind mapping. `None` means the file is unmanaged: it is
/// never extracted, hashed, moved, or deduplicated, because the tool has no
/// way to name it from content and no way to relocate it safely (data and
/// source files usually live where their software expects them).
/// The final extension wins over earlier ones so `archive.tar.gz`
/// classifies as an archive by `gz` handling below.
fn extension_kind(ext: &str) -> Option<AssetKind> {
    match ext {
        // Document
        "pdf" | "doc" | "docx" | "odt" | "txt" | "md" | "markdown" | "rtf" | "epub" | "tex"
        | "pages" | "key" | "ppt" | "pptx" | "xls" | "xlsx" => Some(AssetKind::Document),
        // Photo
        "jpg" | "jpeg" | "png" | "gif" | "webp" | "svg" | "bmp" | "tif" | "tiff" | "heic"
        | "raw" | "cr2" | "cr3" | "nef" | "arw" | "dng" | "orf" | "rw2" => Some(AssetKind::Photo),
        // Video. `ts` (MPEG transport stream) is deliberately absent: the
        // extension also names TypeScript sources, and code must stay
        // unmanaged rather than be moved into `Videos/`.
        "mp4" | "mkv" | "avi" | "mov" | "webm" | "flv" | "wmv" | "m4v" | "mpg" | "mpeg"
        | "3gp" => Some(AssetKind::Video),
        // Audio
        "mp3" | "flac" | "ogg" | "m4a" | "wav" | "aac" | "opus" | "wma" | "aiff" | "alac" => {
            Some(AssetKind::Audio)
        }
        // Archive (two-part forms handled by the caller via is_archive_name).
        // Installer packages (dmg, deb, rpm, apk, msi, exe, bin) are treated
        // as archives: they are immutable distributable units that deduplicate
        // safely and keep their original name under a fixed folder. Source
        // and script extensions (py, rs, go, ...) are unmanaged instead.
        "zip" | "tar" | "gz" | "bz2" | "xz" | "7z" | "rar" | "zst" | "tgz" | "tbz2" | "txz"
        | "iso" | "dmg" | "deb" | "rpm" | "apk" | "msi" | "exe" | "bin" => Some(AssetKind::Archive),
        _ => None,
    }
}

/// Compound-archive names resolved before the simple extension mapping so
/// `tar.gz` and friends classify as archives rather than documents by `gz`.
/// `None` means the file is unmanaged: never extracted, hashed, or
/// suggested.
pub fn classify_file_name(name: &str) -> Option<AssetKind> {
    let lower = name.to_lowercase();
    for two in [
        "tar.gz", "tar.bz2", "tar.xz", "tar.zst", "tar.lz", "tar.lz4", "tar.z", "tar.lzo",
    ] {
        if lower.ends_with(two) {
            return Some(AssetKind::Archive);
        }
    }
    // Known lockfiles and other location-bound data are unmanaged: software
    // reads them at a fixed path, so moving or deduplicating them is unsafe.
    if lower == "cargo.lock"
        || lower == "package-lock.json"
        || lower == "pnpm-lock.yaml"
        || lower == "yarn.lock"
    {
        return None;
    }
    let dot = lower.rfind('.');
    match dot {
        Some(i) if i + 1 < lower.len() => extension_kind(&lower[i + 1..]),
        _ => None,
    }
}

/// Classification for a full path: use the file name only (the parent
/// directories never influence the kind). `None` means unmanaged.
pub fn classify_path(p: &std::path::Path) -> Option<AssetKind> {
    match p.file_name().and_then(|n| n.to_str()) {
        Some(name) => classify_file_name(name),
        None => None,
    }
}

/// Strip a trailing, duplicated document extension from a recovered title or
/// filename stem. The stem of `Name.docx.pdf` is `Name.docx`, and a provider
/// that echoes the path may return `Name.docx.pdf`; both should reduce to
/// `Name` before the real extension is re-appended by `%e`. Repeated known
/// document extensions are removed, but a hidden name (`.pdf`) or one that is
/// entirely an extension is left unchanged.
pub fn strip_trailing_document_extension(name: &str) -> &str {
    let mut current = name;
    loop {
        let Some(dot) = current.rfind('.') else {
            break;
        };
        if dot == 0 {
            break;
        }
        let ext = &current[dot + 1..];
        if ext.is_empty() || extension_kind(&ext.to_lowercase()) != Some(AssetKind::Document) {
            break;
        }
        let trimmed = current[..dot].trim_end();
        if trimmed.is_empty() {
            break;
        }
        current = trimmed;
    }
    current
}

/// Content-description modality names, in canonical order. These are
/// the per-modality keys accepted under `[description]` overrides
/// (e.g. `[description.text]`, `[description.image]`), one per content
/// kind (see `description_modality`).
pub const DESCRIPTION_MODALITIES: [&str; 4] = ["text", "image", "video", "audio"];

/// The content-description modality for an asset kind, if any. Kinds
/// without a modality (repository and archive, plus every unmanaged file)
/// have no content source for `%s`, so no description provider is ever
/// planned for them and `%s` falls back to the filename stem.
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
        assert_eq!(classify_file_name("a.pdf"), Some(AssetKind::Document));
        assert_eq!(classify_file_name("IMG_0001.JPG"), Some(AssetKind::Photo));
        assert_eq!(classify_file_name("clip.mov"), Some(AssetKind::Video));
        assert_eq!(classify_file_name("song.FLAC"), Some(AssetKind::Audio));
        assert_eq!(
            classify_file_name("bundle.tar.gz"),
            Some(AssetKind::Archive)
        );
        assert_eq!(classify_file_name("app.dmg"), Some(AssetKind::Archive));
        assert_eq!(classify_file_name("pkg.deb"), Some(AssetKind::Archive));
        assert_eq!(classify_file_name("tool.exe"), Some(AssetKind::Archive));
        // Data, source, script, and unknown files are unmanaged: no kind,
        // so they are never moved, hashed, or deduplicated.
        assert_eq!(classify_file_name("data.json"), None);
        assert_eq!(classify_file_name("config.toml"), None);
        assert_eq!(classify_file_name("app.db"), None);
        assert_eq!(classify_file_name("run.sh"), None);
        assert_eq!(classify_file_name("main.rs"), None);
        assert_eq!(classify_file_name("main.ts"), None);
        assert_eq!(classify_file_name("App.TSX"), None);
        assert_eq!(classify_file_name("notes"), None);
        assert_eq!(classify_file_name("cargo.lock"), None);
        assert_eq!(classify_file_name("package-lock.json"), None);
    }

    #[test]
    fn strips_duplicated_document_extensions() {
        assert_eq!(
            strip_trailing_document_extension("William Hua - Offboarding.docx"),
            "William Hua - Offboarding"
        );
        assert_eq!(
            strip_trailing_document_extension("William Hua - Offboarding.docx.pdf"),
            "William Hua - Offboarding"
        );
        // Non-document extensions and extension-free names are untouched.
        assert_eq!(
            strip_trailing_document_extension("IMG_0001.jpg"),
            "IMG_0001.jpg"
        );
        assert_eq!(strip_trailing_document_extension("report"), "report");
        // A hidden name or a bare extension is not stripped.
        assert_eq!(strip_trailing_document_extension(".pdf"), ".pdf");
        assert_eq!(strip_trailing_document_extension("pdf"), "pdf");
    }

    #[test]
    fn default_formats_parse() {
        for k in AssetKind::ALL {
            let raw = k.default_format();
            let parsed = crate::format::FormatString::parse(raw)
                .unwrap_or_else(|e| panic!("default format for {} is invalid: {e}", k.name()));
            assert!(!parsed.segments().is_empty());
        }
    }
}
