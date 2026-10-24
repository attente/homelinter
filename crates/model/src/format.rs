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
use unicode_normalization::char::is_combining_mark;
use unicode_normalization::UnicodeNormalization;

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

/// Convert provider text to printable ASCII before it becomes metadata.
/// Compatibility decomposition transliterates accented and presentation
/// forms; typographic quotes and dashes map to their keyboard equivalents,
/// common symbols map to short ASCII names, and unsupported Unicode
/// characters become spaces so they cannot join neighboring words. The
/// returned string contains only characters from U+0020 through U+007E and
/// has whitespace runs collapsed.
pub fn normalize_provider_text(input: &str) -> String {
    let mut out = String::new();
    let mut pending_space = false;
    for c in input.nfkd() {
        if is_combining_mark(c) {
            continue;
        }
        let replacement = match c {
            '\u{2018}' | '\u{2019}' | '\u{201a}' | '\u{201b}' | '\u{2032}' => "'",
            '\u{201c}' | '\u{201d}' | '\u{201e}' | '\u{201f}' | '\u{2033}' => "\"",
            '\u{2026}' => "...",
            '\u{00a9}' => "(c)",
            '\u{00ae}' => "(R)",
            '\u{2122}' => "TM",
            '\u{00a3}' => "GBP",
            '\u{00a5}' => "JPY",
            '\u{20ac}' => "EUR",
            '\u{20b9}' => "INR",
            '\u{2022}' | '\u{2023}' | '\u{2043}' => "-",
            '\u{00b7}' | '\u{22c5}' => ".",
            '\u{00d7}' => "x",
            '\u{00f7}' => "/",
            '\u{2190}' => "<-",
            '\u{2192}' => "->",
            '\u{2194}' => "<->",
            '\u{2191}' => "up",
            '\u{2193}' => "down",
            '\u{2212}' => "-",
            dash if is_unicode_dash(dash) => "-",
            c if c.is_whitespace() => " ",
            c if c.is_ascii_graphic() => {
                if pending_space {
                    out.push(' ');
                    pending_space = false;
                }
                out.push(c);
                continue;
            }
            _ => " ",
        };
        for ch in replacement.chars() {
            if ch.is_ascii_whitespace() {
                pending_space = !out.is_empty();
            } else {
                if pending_space {
                    out.push(' ');
                    pending_space = false;
                }
                out.push(ch);
            }
        }
    }
    out.trim().to_string()
}

/// Lowercase (Unicode-aware) and normalize a metadata segment. Accented
/// characters are decomposed (NFD) and their combining diacritical marks
/// dropped, so they fall back to their basic ASCII equivalent (`café`
/// becomes `cafe`). Every run of whitespace or separator punctuation becomes
/// a single hyphen, and path separators, NUL, control characters, and
/// platform-invalid characters become `_`. Underscores and dots are
/// preserved, so a run of spaces, hyphens, and other separator punctuation
/// collapses to one hyphen. Possessives are dropped instead of hyphenated:
/// a word-final `'s` loses the apostrophe and the `s`, and a word-final
/// apostrophe on a plural noun loses only the apostrophe. A common
/// contraction whose word is in `CONTRACTIONS` is the exception: it is
/// spelled out (`it's` becomes `it is`), so it keeps its meaning instead of
/// reading as a possessive. An empty result means the value carried no
/// usable text; callers render such a placeholder as nothing rather than a
/// literal placeholder word.
pub fn sanitize_metadata_segment(input: &str) -> String {
    // Collect first so the possessive rules can look ahead past an apostrophe
    // at the character that follows it and, for `'s`, at the boundary after
    // that. Filtering the combining marks here keeps the lookahead aligned
    // with the characters that are actually rendered.
    let chars: Vec<char> = input.nfd().filter(|c| !is_combining_mark(*c)).collect();
    let mut out = String::new();
    let mut pending_hyphen = false;
    // Index in `out` where the word being built starts, so an apostrophe can
    // name the word before it when matching a contraction.
    let mut word_start = 0;
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c.is_whitespace() {
            pending_hyphen = true;
            i += 1;
            continue;
        }
        if c == '/' || c == '\\' || c == '\0' || c.is_control() || is_platform_invalid(c) {
            if pending_hyphen {
                out.push('-');
                pending_hyphen = false;
            }
            out.push('_');
            word_start = out.len();
            i += 1;
            continue;
        }
        if is_apostrophe(c) {
            let next = chars.get(i + 1).copied();
            let possessive_s = matches!(next, Some('s' | 'S'))
                && chars
                    .get(i + 2)
                    .is_none_or(|after| !after.is_alphanumeric());
            if possessive_s {
                // `word's` and `word's,` drop the apostrophe and the `s`;
                // the surrounding separator (if any) supplies the hyphen.
                // A known contraction spells the word out instead.
                let word = &out[word_start..];
                if let Some((_, expansion)) = CONTRACTIONS
                    .iter()
                    .find(|(contracted, _)| *contracted == word)
                {
                    out.push('-');
                    word_start = out.len();
                    out.push_str(expansion);
                }
                i += 2;
                continue;
            }
            if next.is_none_or(|after| !after.is_alphanumeric()) {
                // A plural possessive (`providers' names`) drops only the
                // apostrophe, so the following separator stays a single
                // hyphen rather than becoming a doubled one.
                i += 1;
                continue;
            }
            // An interior apostrophe (`O'Brien`) remains a separator.
        }
        if is_separator_punctuation(c) {
            pending_hyphen = true;
            i += 1;
            continue;
        }
        if pending_hyphen {
            out.push('-');
            pending_hyphen = false;
            word_start = out.len();
        }
        for lc in c.to_lowercase() {
            out.push(lc);
        }
        i += 1;
    }
    if pending_hyphen {
        out.push('-');
    }
    // Trim leading and trailing whitespace, hyphens, and dots. A value that
    // normalizes to nothing, `.`, or `..` becomes the empty string.
    out.trim_matches(|c: char| c.is_whitespace() || c == '-' || c == '.')
        .to_string()
}

/// Common `'s` contractions, keyed by the word before the apostrophe and
/// mapped to the word the contraction hides. The sanitizer replaces the
/// apostrophe and `s` with a hyphen and this expansion (`it's` becomes
/// `it-is`), so a contraction is not mistaken for a possessive. Only these
/// fixed forms are recognized: any other `word's` is ambiguous between a
/// possessive and a noun plus `is`/`has`, so it is treated as a possessive.
const CONTRACTIONS: [(&str, &str); 13] = [
    ("it", "is"),
    ("that", "is"),
    ("what", "is"),
    ("there", "is"),
    ("where", "is"),
    ("when", "is"),
    ("why", "is"),
    ("how", "is"),
    ("who", "is"),
    ("here", "is"),
    ("he", "is"),
    ("she", "is"),
    ("let", "us"),
];

/// Apostrophe forms that can mark a possessive. Provider text reaches the
/// sanitizer as ASCII after `normalize_provider_text`, but metadata and
/// original filenames may still carry typographic forms.
fn is_apostrophe(c: char) -> bool {
    matches!(
        c,
        '\'' | '\u{2018}' | '\u{2019}' | '\u{02bc}' | '\u{2032}' | '\u{ff07}'
    )
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

/// Punctuation that separates words. ASCII punctuation (other than the
/// underscores and dots preserved as filename characters) qualifies, as do
/// the typographic dash characters such as the en and em dash. Hyphens are
/// separators too, so a run mixing them with surrounding whitespace or
/// typographic dashes collapses into a single hyphen.
fn is_separator_punctuation(c: char) -> bool {
    (c.is_ascii_punctuation() && c != '_' && c != '.') || is_unicode_dash(c)
}

/// The Unicode `Dash` property characters outside ASCII. These are treated
/// exactly like the ASCII hyphen so that a title such as
/// `aws-administration---the-definitive-guide` (with an em dash between the
/// ASCII hyphens) normalizes to a single hyphen instead of preserving the
/// typographic dash in a filename.
fn is_unicode_dash(c: char) -> bool {
    matches!(
        c,
        '\u{058a}'
            | '\u{05be}'
            | '\u{1400}'
            | '\u{1806}'
            | '\u{2010}'
            | '\u{2011}'
            | '\u{2012}'
            | '\u{2013}'
            | '\u{2014}'
            | '\u{2015}'
            | '\u{2e17}'
            | '\u{2e1a}'
            | '\u{2e3a}'
            | '\u{2e3b}'
            | '\u{2e40}'
            | '\u{301c}'
            | '\u{3030}'
            | '\u{30a0}'
            | '\u{fe31}'
            | '\u{fe32}'
            | '\u{fe58}'
            | '\u{fe63}'
            | '\u{ff0d}'
            | '\u{10ead}'
    )
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
            Placeholder::Extension => self
                .extension
                .as_deref()
                .map(str::to_lowercase)
                .unwrap_or_default(),
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
            None => Some("unknown asset kind; expected one of: document, photo, video, audio, repository, archive".to_string()),
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
    fn provider_text_is_ascii_and_normalizes_typographic_punctuation() {
        assert_eq!(
            normalize_provider_text(
                "\u{201c}Cafe\u{301}\u{201d} \u{2014} \u{2018}it\u{2019}s\u{2019} \u{2026} \u{2122} \u{2022} \u{1f680}"
            ),
            "\"Cafe\" - 'it's' ... TM -"
        );
        assert_eq!(normalize_provider_text("\u{6771}\u{4eac}"), "");
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
            "at-t-draft-v1.2"
        );
        // Possessives are removed rather than hyphenated, singular and
        // plural alike, and removing one never leaves a doubled hyphen.
        assert_eq!(
            sanitize_metadata_segment("the company's logo"),
            "the-company-logo"
        );
        assert_eq!(
            sanitize_metadata_segment("the providers' names"),
            "the-providers-names"
        );
        assert_eq!(
            sanitize_metadata_segment("Employees' handbook"),
            "employees-handbook"
        );
        assert_eq!(sanitize_metadata_segment("James's file"), "james-file");
        // A common contraction is spelled out instead of being mistaken for
        // a possessive.
        assert_eq!(sanitize_metadata_segment("it's a draft"), "it-is-a-draft");
        assert_eq!(
            sanitize_metadata_segment("That's the plan"),
            "that-is-the-plan"
        );
        assert_eq!(sanitize_metadata_segment("let's go"), "let-us-go");
        assert_eq!(sanitize_metadata_segment("Who's there?"), "who-is-there");
        // A contraction only matches the whole preceding word, and the
        // possessive pronoun `its` (no apostrophe) is untouched.
        assert_eq!(sanitize_metadata_segment("a bit's worth"), "a-bit-worth");
        assert_eq!(sanitize_metadata_segment("its metadata"), "its-metadata");
        // Interior apostrophes stay separators.
        assert_eq!(
            sanitize_metadata_segment("O'Brien's notes"),
            "o-brien-notes"
        );
        // Typographic apostrophes mark possessives too.
        assert_eq!(
            sanitize_metadata_segment("the company\u{2019}s logo"),
            "the-company-logo"
        );
        assert_eq!(
            sanitize_metadata_segment("the providers\u{2019} names"),
            "the-providers-names"
        );
        // A possessive next to an existing separator still collapses to one
        // hyphen, never two.
        assert_eq!(
            sanitize_metadata_segment("the company's - logo"),
            "the-company-logo"
        );
        assert_eq!(sanitize_metadata_segment("!!!"), "");
        assert_eq!(sanitize_metadata_segment("..."), "");
        assert_eq!(sanitize_metadata_segment(".."), "");
        assert_eq!(sanitize_metadata_segment("naïve Café"), "naive-cafe");
        assert_eq!(sanitize_metadata_segment("Crème Brûlée"), "creme-brulee");
        assert_eq!(sanitize_metadata_segment("München"), "munchen");
        assert_eq!(sanitize_metadata_segment("señor"), "senor");
        // Decomposed input (base + combining mark) normalizes the same way.
        assert_eq!(sanitize_metadata_segment("cafe\u{301}"), "cafe");
        // Typographic dashes are separators: an em dash between hyphens (or
        // spaces) collapses with its neighbors into one hyphen.
        assert_eq!(
            sanitize_metadata_segment("aws-administration-\u{2014}-the-definitive-guide"),
            "aws-administration-the-definitive-guide"
        );
        assert_eq!(
            sanitize_metadata_segment("AWS Administration \u{2013} The Definitive Guide"),
            "aws-administration-the-definitive-guide"
        );
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
    fn extension_placeholder_is_lowercased() {
        let home = std::path::Path::new("/home/alice");
        let fs = FormatString::parse("Pictures/%n.%e").unwrap();
        let mut v = vals();
        v.extension = Some("JPG".into());
        let p = fs.resolve(&v, home).unwrap().unwrap();
        assert_eq!(p, PathBuf::from("/home/alice/Pictures/IMG_0001.jpg"));
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
