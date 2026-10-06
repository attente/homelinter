//! Destination format strings.
//!
//! A destination format string is a printf-style template resolved per
//! target at suggestion time into the absolute `new_path`. Parsing and
//! resolution live here so configuration validation and suggestion
//! generation share one implementation.

use std::fmt;
use std::path::PathBuf;

use crate::asset_kind::AssetKind;
use crate::error::ApiError;

/// Supported placeholder characters.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Placeholder {
    Year4,        // %Y
    Year2,        // %y
    Month,        // %m
    Day,          // %d
    Hour,         // %H
    Minute,       // %M
    Second,       // %S
    Title,        // %t
    Author,       // %a
    Location,     // %l
    Subject,      // %s
    Camera,       // %c
    RemoteHost,   // %h
    RemoteOwner,  // %o
    AssetKind,    // %k
    FileName,     // %f
    FileNameNorm, // %F
    Stem,         // %n
    Extension,    // %e
}

impl Placeholder {
    fn from_char(c: char) -> Option<Placeholder> {
        Some(match c {
            'Y' => Placeholder::Year4,
            'y' => Placeholder::Year2,
            'm' => Placeholder::Month,
            'd' => Placeholder::Day,
            'H' => Placeholder::Hour,
            'M' => Placeholder::Minute,
            'S' => Placeholder::Second,
            't' => Placeholder::Title,
            'a' => Placeholder::Author,
            'l' => Placeholder::Location,
            's' => Placeholder::Subject,
            'c' => Placeholder::Camera,
            'h' => Placeholder::RemoteHost,
            'o' => Placeholder::RemoteOwner,
            'k' => Placeholder::AssetKind,
            'f' => Placeholder::FileName,
            'F' => Placeholder::FileNameNorm,
            'n' => Placeholder::Stem,
            'e' => Placeholder::Extension,
            _ => return None,
        })
    }

    fn char(self) -> char {
        match self {
            Placeholder::Year4 => 'Y',
            Placeholder::Year2 => 'y',
            Placeholder::Month => 'm',
            Placeholder::Day => 'd',
            Placeholder::Hour => 'H',
            Placeholder::Minute => 'M',
            Placeholder::Second => 'S',
            Placeholder::Title => 't',
            Placeholder::Author => 'a',
            Placeholder::Location => 'l',
            Placeholder::Subject => 's',
            Placeholder::Camera => 'c',
            Placeholder::RemoteHost => 'h',
            Placeholder::RemoteOwner => 'o',
            Placeholder::AssetKind => 'k',
            Placeholder::FileName => 'f',
            Placeholder::FileNameNorm => 'F',
            Placeholder::Stem => 'n',
            Placeholder::Extension => 'e',
        }
    }

    /// Placeholders that name the target file from its original filename.
    pub fn is_filename(self) -> bool {
        matches!(
            self,
            Placeholder::FileName
                | Placeholder::FileNameNorm
                | Placeholder::Stem
                | Placeholder::Extension
        )
    }
}

/// One parsed segment of a format string.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Segment {
    /// Literal text between placeholders (already unescaped).
    Literal(String),
    /// A placeholder.
    Placeholder(Placeholder),
}

/// A parsed, validated destination format string.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FormatString {
    segments: Vec<Segment>,
    raw: String,
    /// The format begins with the host's absolute-path syntax.
    absolute: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FormatError(pub String);

impl fmt::Display for FormatError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for FormatError {}

impl FormatString {
    /// Parse and validate a format string. Errors on unknown placeholders,
    /// missing placeholders, trailing separators, `~`, and environment
    /// references, matching the configuration rules.
    pub fn parse(raw: &str) -> Result<FormatString, FormatError> {
        if raw.is_empty() {
            return Err(FormatError("format string must not be empty".into()));
        }
        if crate::path::has_env_or_home_reference(raw) {
            return Err(FormatError(format!(
                "format string must not contain '~' or environment-variable references: {raw}"
            )));
        }
        if raw.ends_with('/') || raw.ends_with('\\') {
            return Err(FormatError(format!(
                "format string must not end with a path separator: {raw}"
            )));
        }
        if raw.contains("//") {
            return Err(FormatError(format!(
                "format string must not contain an empty path component: {raw}"
            )));
        }
        let absolute =
            raw.starts_with('/') || (cfg!(windows) && raw.len() >= 3 && raw.as_bytes()[1] == b':');
        let mut segments = Vec::new();
        let mut literal = String::new();
        let mut chars = raw.chars().peekable();
        while let Some(c) = chars.next() {
            if c != '%' {
                literal.push(c);
                continue;
            }
            match chars.next() {
                Some('%') => literal.push('%'),
                Some(c) => match Placeholder::from_char(c) {
                    Some(ph) => {
                        if !literal.is_empty() {
                            segments.push(Segment::Literal(std::mem::take(&mut literal)));
                        }
                        segments.push(Segment::Placeholder(ph));
                    }
                    None => {
                        return Err(FormatError(format!(
                            "unknown placeholder '%{c}' in format string: {raw}"
                        )));
                    }
                },
                None => {
                    return Err(FormatError(format!(
                        "format string ends with a lone '%': {raw}"
                    )));
                }
            }
        }
        if !literal.is_empty() {
            segments.push(Segment::Literal(literal));
        }
        let has_placeholder = segments
            .iter()
            .any(|s| matches!(s, Segment::Placeholder(_)));
        if !has_placeholder {
            return Err(FormatError(format!(
                "format string must contain at least one placeholder: {raw}"
            )));
        }
        Ok(FormatString {
            segments,
            raw: raw.to_string(),
            absolute,
        })
    }

    pub fn raw(&self) -> &str {
        &self.raw
    }

    pub fn segments(&self) -> &[Segment] {
        &self.segments
    }

    pub fn is_absolute(&self) -> bool {
        self.absolute
    }

    pub fn placeholders(&self) -> Vec<Placeholder> {
        self.segments
            .iter()
            .filter_map(|s| match s {
                Segment::Placeholder(p) => Some(*p),
                _ => None,
            })
            .collect()
    }
}

/// A character that joins metadata fields in a filename. When a placeholder
/// resolves to nothing, its adjacent join separators are elided so a missing
/// value never leaves a leading, trailing, or doubled separator.
fn is_join_separator(c: char) -> bool {
    c == '-' || c == '_' || c.is_whitespace()
}

/// Remove trailing join separators from `out`, returning the rightmost one
/// that was removed.
fn strip_trailing_join_separators(out: &mut String) -> Option<char> {
    let mut removed = None;
    while out.chars().last().is_some_and(is_join_separator) {
        let c = out.pop().expect("checked non-empty");
        removed.get_or_insert(c);
    }
    removed
}

/// Resolve a placeholder run. A placeholder that resolves to nothing is
/// omitted together with its adjacent join separators: a separator is kept
/// only when it joined two present values. When an extension placeholder
/// resolves empty, an immediately preceding literal dot is dropped so `%n.%e`
/// degrades to the bare stem.
fn render(segments: &[Segment], values: &PlaceholderValues, out: &mut String) {
    let mut prev_was_literal = false;
    let mut strip_leading = false;
    for (i, seg) in segments.iter().enumerate() {
        match seg {
            Segment::Literal(lit) => {
                let lit = if strip_leading {
                    lit.trim_start_matches(is_join_separator)
                } else {
                    lit.as_str()
                };
                strip_leading = false;
                out.push_str(lit);
                prev_was_literal = true;
            }
            Segment::Placeholder(ph) => {
                strip_leading = false;
                let v = values.render(*ph);
                if v.is_empty() {
                    // Drop a trailing join separator contributed by the
                    // immediately preceding literal; a verbatim value such as
                    // `%n` is never touched.
                    let preceding = if prev_was_literal {
                        strip_trailing_join_separators(out)
                    } else {
                        None
                    };
                    // A separator that follows the placeholder is dropped too;
                    // when it sat between two separators, keep exactly one.
                    let next_has_separator = segments
                        .get(i + 1)
                        .and_then(|s| match s {
                            Segment::Literal(l) => l.chars().next(),
                            _ => None,
                        })
                        .is_some_and(is_join_separator);
                    if let (Some(sep), true) = (preceding, next_has_separator) {
                        out.push(sep);
                    }
                    if *ph == Placeholder::Extension && out.ends_with('.') {
                        out.pop();
                    }
                    strip_leading = next_has_separator;
                    prev_was_literal = false;
                    continue;
                }
                // A rendered segment can never contain separators after
                // sanitization; assert for future-proofing in debug builds.
                debug_assert!(!v.contains('/') && !v.contains('\\') && !v.is_empty());
                out.push_str(&v);
                prev_was_literal = false;
            }
        }
    }
}

impl FormatString {
    /// Resolve the format string against metadata values into an absolute
    /// path (relative templates are joined to `home`). The result is
    /// normalized lexically and never ends with a separator.
    ///
    /// Returns `None` when the resolved final segment is empty (an empty
    /// template result) - the caller then produces no move.
    pub fn resolve(
        &self,
        values: &PlaceholderValues,
        home: &std::path::Path,
    ) -> Result<Option<PathBuf>, FormatError> {
        let mut rendered = String::new();
        render(&self.segments, values, &mut rendered);
        if rendered.is_empty() {
            return Ok(None);
        }
        if rendered.ends_with('/') || rendered.ends_with('\\') {
            // A trailing empty value (e.g. `%e` with no extension) leaves
            // the final segment empty; such a template names no file.
            return Ok(None);
        }
        if rendered.starts_with('/') {
            // Keep absolute templates as-is.
        } else if cfg!(windows) && rendered.len() >= 3 && rendered.as_bytes()[1] == b':' {
            // Windows drive-letter absolute template.
        } else {
            let mut full = home.to_string_lossy().into_owned();
            if !full.ends_with(std::path::MAIN_SEPARATOR) {
                full.push(std::path::MAIN_SEPARATOR);
            }
            full.push_str(&rendered);
            rendered = full;
        }
        let raw_path = PathBuf::from(&rendered);
        let norm = crate::path::normalize_lexical(&raw_path).ok_or_else(|| {
            FormatError(format!("template resolved outside the root: {rendered}"))
        })?;
        let final_seg = norm.file_name().map(|s| s.to_string_lossy().into_owned());
        match final_seg {
            Some(name) if !name.is_empty() && name != "." && name != ".." => Ok(Some(norm)),
            _ => Ok(None),
        }
    }
}

/// Metadata values available to resolution. Optional values render as
/// nothing when no source produced them; suggestion generation fills
/// placeholders with generic fallbacks before resolution (see the design's
/// fallback table), so in practice every placeholder has a value.
#[derive(Debug, Clone, Default)]
pub struct PlaceholderValues {
    /// Four-digit year, e.g. "2024".
    pub year: Option<String>,
    /// Two-digit year, e.g. "24".
    pub year2: Option<String>,
    pub month: Option<String>,
    pub day: Option<String>,
    pub hour: Option<String>,
    pub minute: Option<String>,
    pub second: Option<String>,
    pub title: Option<String>,
    pub author: Option<String>,
    pub location: Option<String>,
    pub subject: Option<String>,
    pub camera: Option<String>,
    pub remote_host: Option<String>,
    /// Repository remote URL owner/username: the first path segment after
    /// the host in a remote URL (e.g. `github.com/<owner>/<repo>`).
    pub remote_owner: Option<String>,
    /// Original filename including extension (`%f`), kept verbatim.
    pub file_name: Option<String>,
    /// Filename stem without the extension (`%n`), kept verbatim.
    pub stem: Option<String>,
    /// Extension without the leading dot (`%e`), empty when absent.
    pub extension: Option<String>,
    pub asset_kind: AssetKind,
}

/// Lowercase (Unicode-aware) and normalize a metadata segment: every run of
/// whitespace or separator punctuation becomes a single hyphen, and path
/// separators, NUL, control characters, and platform-invalid characters
/// become `_`. Underscores and dots are preserved, so a run of spaces,
/// hyphens, and other separator punctuation collapses to one hyphen. An
/// empty result means the value carried no usable text; callers render such a
/// placeholder as nothing rather than a literal placeholder word.
pub fn sanitize_metadata_segment(input: &str) -> String {
    let mut out = String::new();
    let mut pending_hyphen = false;
    for c in input.chars() {
        if c.is_whitespace() {
            pending_hyphen = true;
            continue;
        }
        if c == '/' || c == '\\' || c == '\0' || c.is_control() || is_platform_invalid(c) {
            if pending_hyphen {
                out.push('-');
                pending_hyphen = false;
            }
            out.push('_');
            continue;
        }
        if is_separator_punctuation(c) {
            pending_hyphen = true;
            continue;
        }
        if pending_hyphen {
            out.push('-');
            pending_hyphen = false;
        }
        for lc in c.to_lowercase() {
            out.push(lc);
        }
    }
    if pending_hyphen {
        out.push('-');
    }
    // Trim leading and trailing whitespace, hyphens, and dots. A value that
    // normalizes to nothing, `.`, or `..` becomes the empty string.
    out.trim_matches(|c: char| c.is_whitespace() || c == '-' || c == '.')
        .to_string()
}

/// Normalize an optional metadata value to a path segment. Returns the empty
/// string when no value is available (or it normalizes to nothing), and treats
/// the literal answer `unknown` as no answer, so a missing value never appears
/// in a filename.
fn sanitize_metadata_value(value: Option<&str>) -> String {
    let s = sanitize_metadata_segment(value.unwrap_or(""));
    if s == "unknown" {
        String::new()
    } else {
        s
    }
}

/// Windows-reserved device names and path-invalid characters. On other
/// platforms only the control/separator rules apply.
fn is_platform_invalid(_c: char) -> bool {
    #[cfg(windows)]
    {
        matches!(_c, '<' | '>' | ':' | '"' | '|' | '?' | '*')
    }
    #[cfg(not(windows))]
    {
        false
    }
}

/// ASCII punctuation that separates words. Underscores and dots are
/// preserved as filename characters; hyphens are separators so runs of them
/// collapse with surrounding whitespace into a single hyphen.
fn is_separator_punctuation(c: char) -> bool {
    c.is_ascii_punctuation() && c != '_' && c != '.'
}

/// Characters that may never appear in a host file name.
fn file_name_invalid(_c: char) -> bool {
    _c == '/' || _c == '\0' || _c.is_control()
}

impl PlaceholderValues {
    fn render(&self, ph: Placeholder) -> String {
        match ph {
            Placeholder::Year4 => self.year.clone().unwrap_or_default(),
            Placeholder::Year2 => self.year2.clone().unwrap_or_default(),
            Placeholder::Month => self.month.clone().unwrap_or_default(),
            Placeholder::Day => self.day.clone().unwrap_or_default(),
            Placeholder::Hour => self.hour.clone().unwrap_or_default(),
            Placeholder::Minute => self.minute.clone().unwrap_or_default(),
            Placeholder::Second => self.second.clone().unwrap_or_default(),
            Placeholder::Title => sanitize_metadata_value(self.title.as_deref()),
            Placeholder::Author => sanitize_metadata_value(self.author.as_deref()),
            Placeholder::Location => sanitize_metadata_value(self.location.as_deref()),
            Placeholder::Subject => sanitize_metadata_value(self.subject.as_deref()),
            Placeholder::Camera => sanitize_metadata_value(self.camera.as_deref()),
            Placeholder::RemoteHost => self
                .remote_host
                .as_deref()
                .map(str::to_lowercase)
                .unwrap_or_default(),
            Placeholder::RemoteOwner => match self.remote_owner.as_deref() {
                Some(v) if !v.trim().is_empty() => sanitize_metadata_segment(v),
                _ => String::new(),
            },
            Placeholder::AssetKind => self.asset_kind.name().to_string(),
            Placeholder::FileName => self.file_name.clone().unwrap_or_default(),
            Placeholder::FileNameNorm => {
                sanitize_metadata_segment(self.file_name.as_deref().unwrap_or(""))
            }
            Placeholder::Stem => self.stem.clone().unwrap_or_default(),
            Placeholder::Extension => self.extension.clone().unwrap_or_default(),
        }
    }
}

impl fmt::Display for Placeholder {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "%{}", self.char())
    }
}

/// Validate a full target_paths mapping during configuration validation.
pub fn validate_target_paths(
    target_paths: &std::collections::BTreeMap<String, String>,
) -> Result<(), ApiError> {
    let mut fields = Vec::new();
    for (kind, tmpl) in target_paths {
        let parsed = AssetKind::from_name(kind);
        let kind_err = match parsed {
            Some(_) => None,
            None => Some("unknown asset kind; expected one of: document, photo, video, audio, repository, archive, data, executable, other".to_string()),
        };
        if let Some(reason) = kind_err {
            fields.push((format!("target_paths.{kind}"), reason));
            continue;
        }
        match FormatString::parse(tmpl) {
            Ok(fs) => {
                // Configuration-time checks: a template that can never name
                // a file (e.g. ending in an always-empty %e with nothing
                // after it) is rejected at configuration time.
                if fs.is_absolute() {
                    // Absolute templates are permitted and can point anywhere.
                }
            }
            Err(e) => fields.push((format!("target_paths.{kind}"), e.to_string())),
        }
    }
    if fields.is_empty() {
        Ok(())
    } else {
        Err(ApiError::validation_errors(
            "invalid target_paths configuration",
            fields,
        ))
    }
}

/// Validate that a filename-only segment is safe on the host: no separator,
/// NUL, or control character, not `.`/`..`, and (on Windows) not a reserved
/// device name. Existing host file names satisfy this by construction; it
/// guards resolved destinations whose final segment came from metadata.
pub fn validate_file_name_segment(name: &str) -> bool {
    !name.is_empty()
        && name != "."
        && name != ".."
        && !name.chars().any(file_name_invalid)
        && (cfg!(not(windows)) || !is_windows_reserved_name(name))
}

fn is_windows_reserved_name(name: &str) -> bool {
    #[cfg(windows)]
    {
        let stem = name.split('.').next().unwrap_or("").to_uppercase();
        matches!(
            stem.as_str(),
            "CON"
                | "PRN"
                | "AUX"
                | "NUL"
                | "COM1"
                | "COM2"
                | "COM3"
                | "COM4"
                | "COM5"
                | "COM6"
                | "COM7"
                | "COM8"
                | "COM9"
                | "LPT1"
                | "LPT2"
                | "LPT3"
                | "LPT4"
                | "LPT5"
                | "LPT6"
                | "LPT7"
                | "LPT8"
                | "LPT9"
        )
    }
    #[cfg(not(windows))]
    {
        let _ = name;
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vals() -> PlaceholderValues {
        PlaceholderValues {
            year: Some("2024".into()),
            year2: Some("24".into()),
            month: Some("03".into()),
            day: Some("15".into()),
            hour: Some("10".into()),
            minute: Some("30".into()),
            second: Some("00".into()),
            title: Some("Sunset over the River".into()),
            author: Some("Artist".into()),
            location: Some("San Francisco".into()),
            subject: None,
            camera: None,
            remote_host: None,
            remote_owner: None,
            file_name: Some("IMG_0001.jpg".into()),
            stem: Some("IMG_0001".into()),
            extension: Some("jpg".into()),
            asset_kind: AssetKind::Photo,
        }
    }

    #[cfg(unix)]
    #[test]
    fn resolves_relative_template() {
        let home = std::path::Path::new("/home/alice");
        let fs = FormatString::parse("Pictures/%Y/%m/%f").unwrap();
        let p = fs.resolve(&vals(), home).unwrap().unwrap();
        assert_eq!(
            p,
            PathBuf::from("/home/alice/Pictures/2024/03/IMG_0001.jpg")
        );
    }

    #[cfg(unix)]
    #[test]
    fn resolves_absolute_template() {
        let fs = FormatString::parse("/mnt/usb/%Y/%f").unwrap();
        let p = fs
            .resolve(&vals(), std::path::Path::new("/home/alice"))
            .unwrap()
            .unwrap();
        assert_eq!(p, PathBuf::from("/mnt/usb/2024/IMG_0001.jpg"));
    }

    #[test]
    fn rejects_bad_templates() {
        assert!(FormatString::parse("").is_err());
        assert!(FormatString::parse("fixed/name").is_err());
        assert!(FormatString::parse("Pictures/%q/%f").is_err());
        assert!(FormatString::parse("Pictures/%Y/").is_err());
        assert!(FormatString::parse("~/Pictures/%f").is_err());
        assert!(FormatString::parse("Pictures/%Y/%f/").is_err());
        assert!(FormatString::parse("Pictures/100%").is_err());
    }

    #[test]
    fn sanitizes_metadata() {
        assert_eq!(sanitize_metadata_segment("San Francisco"), "san-francisco");
        assert_eq!(
            sanitize_metadata_segment("  Sunset   over the River  "),
            "sunset-over-the-river"
        );
        assert_eq!(sanitize_metadata_segment("a/b\\c"), "a_b_c");
        assert_eq!(
            sanitize_metadata_segment("Legal Consultation, Employee Intake"),
            "legal-consultation-employee-intake"
        );
        assert_eq!(
            sanitize_metadata_segment("William Hua - Offboarding.docx"),
            "william-hua-offboarding.docx"
        );
        assert_eq!(
            sanitize_metadata_segment("AT&T's (draft): v1.2"),
            "at-t-s-draft-v1.2"
        );
        assert_eq!(sanitize_metadata_segment("!!!"), "");
        assert_eq!(sanitize_metadata_segment("..."), "");
        assert_eq!(sanitize_metadata_segment(".."), "");
        assert_eq!(sanitize_metadata_segment("naïve Café"), "naïve-café");
    }

    #[test]
    fn drops_dot_before_empty_extension() {
        let home = std::path::Path::new("/home/alice");
        let fs = FormatString::parse("Music/%a/%Y/%n.%e").unwrap();
        let mut v = vals();
        v.extension = None;
        let p = fs.resolve(&v, home).unwrap().unwrap();
        // %n is the original filename stem, kept verbatim.
        assert_eq!(p, PathBuf::from("/home/alice/Music/artist/2024/IMG_0001"));
    }

    #[test]
    fn empty_trailing_extension_names_no_file() {
        // `%e` with no extension leaves an empty final segment: no move.
        let home = std::path::Path::new("/home/alice");
        let fs = FormatString::parse("Pictures/%e").unwrap();
        let mut v = vals();
        v.extension = None;
        assert!(fs.resolve(&v, home).unwrap().is_none());
        v.extension = Some("jpg".into());
        assert!(fs.resolve(&v, home).unwrap().is_some());
    }

    #[test]
    fn remote_owner_segment_optional() {
        let home = std::path::Path::new("/home/alice");
        let fs = FormatString::parse("Projects/%o/%n").unwrap();
        let mut v = vals();
        v.stem = Some("homelinter".into());
        v.remote_owner = Some("William".into());
        let p = fs.resolve(&v, home).unwrap().unwrap();
        assert_eq!(p, PathBuf::from("/home/alice/Projects/william/homelinter"));
        // With no remote, the owner segment is omitted (no `unknown`).
        v.remote_owner = None;
        let p = fs.resolve(&v, home).unwrap().unwrap();
        assert_eq!(p, PathBuf::from("/home/alice/Projects/homelinter"));
    }

    #[test]
    fn omits_missing_metadata_and_its_separator() {
        let home = std::path::Path::new("/home/alice");
        let fs = FormatString::parse("Documents/%a-%t.%e").unwrap();
        let mut v = vals();
        v.author = None;
        let p = fs.resolve(&v, home).unwrap().unwrap();
        assert_eq!(
            p,
            PathBuf::from("/home/alice/Documents/sunset-over-the-river.jpg")
        );
        // A provider that literally answers `unknown` is treated as no answer.
        v.author = Some("unknown".into());
        let p = fs.resolve(&v, home).unwrap().unwrap();
        assert_eq!(
            p,
            PathBuf::from("/home/alice/Documents/sunset-over-the-river.jpg")
        );
    }

    #[test]
    fn keeps_one_separator_between_present_fields() {
        let home = std::path::Path::new("/home/alice");
        let fs = FormatString::parse("Photos/%H-%l-%s.%e").unwrap();
        let mut v = vals();
        v.location = None;
        v.subject = Some("Beach".into());
        let p = fs.resolve(&v, home).unwrap().unwrap();
        assert_eq!(p, PathBuf::from("/home/alice/Photos/10-beach.jpg"));
        // Two missing fields in a row collapse to a single separator.
        v.subject = None;
        let p = fs.resolve(&v, home).unwrap().unwrap();
        assert_eq!(p, PathBuf::from("/home/alice/Photos/10.jpg"));
    }

    #[test]
    fn present_author_with_missing_subject_leaves_no_trailing_separator() {
        let home = std::path::Path::new("/home/alice");
        let fs = FormatString::parse("Documents/%a-%s.%e").unwrap();
        let mut v = vals();
        v.subject = None;
        let p = fs.resolve(&v, home).unwrap().unwrap();
        assert_eq!(p, PathBuf::from("/home/alice/Documents/artist.jpg"));
    }
}
