//! Host-native absolute path handling.
//!
//! Per the design, every path crossing a client boundary is an absolute,
//! normalized, host-native path. Relative paths, `~`, environment-variable
//! references, and redundant `.`/`..` components are rejected at input
//! validation and are never emitted. Normalization is lexical: it never
//! resolves symlinks, so the lexical path and the filesystem identity it
//! names stay distinct.

use std::path::{Component, Path, PathBuf};

use crate::error::ApiError;

/// True when `s` contains a `~` expansion, a `$`-style environment-variable
/// reference, or a `%VAR%` reference that would make the value depend on the
/// caller's environment. Used by config-file and API path validation.
pub fn has_env_or_home_reference(s: &str) -> bool {
    s.contains('~') || s.contains('$') || s.contains("%HOME%") || s.contains("%USERPROFILE%")
}

/// Normalize a host-native absolute path lexically.
///
/// Removes redundant `.` components and resolves `..` lexically against the
/// path's own components without touching the filesystem. Returns `None`
/// when the result is not absolute or when `..` would escape the root.
pub fn normalize_lexical(p: &Path) -> Option<PathBuf> {
    if !p.is_absolute() {
        return None;
    }
    let mut out = PathBuf::new();
    #[cfg(unix)]
    {
        out.push(Component::RootDir);
        for c in p.components() {
            match c {
                Component::RootDir => {}
                Component::CurDir => {}
                Component::ParentDir => {
                    out.pop();
                }
                Component::Normal(s) => out.push(s),
                Component::Prefix(_) => unreachable!("prefix on unix"),
            }
        }
    }
    #[cfg(not(unix))]
    {
        // Host-native normalization on non-unix platforms keeps the prefix
        // and otherwise applies the same lexical rules as the unix branch.
        for c in p.components() {
            match c {
                Component::Prefix(_) => out.push(c.as_os_str()),
                Component::RootDir | Component::CurDir => {}
                Component::ParentDir => {
                    if !out.pop() {
                        return None;
                    }
                }
                Component::Normal(s) => out.push(s),
            }
        }
    }
    if !out.as_os_str().is_empty() {
        Some(out)
    } else {
        None
    }
}

/// Convert an input path to normalized absolute form.
///
/// Accepts an absolute path directly. A relative path is resolved against
/// `base` (the process working directory for CLI inputs). The result is the
/// lexical normalized absolute path; no symlinks are resolved.
pub fn to_absolute(input: &Path, base: &Path) -> Result<PathBuf, ApiError> {
    if input.as_os_str().is_empty() {
        return Err(ApiError::validation("path must not be empty"));
    }
    let text = input.to_string_lossy();
    if has_env_or_home_reference(&text) {
        return Err(ApiError::validation(format!(
            "path must not contain '~' or environment-variable references: {text}"
        )));
    }
    let joined = if input.is_absolute() {
        input.to_path_buf()
    } else {
        base.join(input)
    };
    normalize_lexical(&joined)
        .ok_or_else(|| ApiError::validation(format!("invalid absolute path: {text}")))
}

/// Validate a request-input path that must already be absolute and
/// normalized. Rejects relative paths, `~`, and env references with a 422.
pub fn require_absolute_normalized(p: &Path) -> Result<(), ApiError> {
    let text = p.to_string_lossy();
    if !p.is_absolute() {
        return Err(ApiError::validation(format!(
            "path must be absolute: {text}"
        )));
    }
    if has_env_or_home_reference(&text) {
        return Err(ApiError::validation(format!(
            "path must not contain '~' or environment-variable references: {text}"
        )));
    }
    // Compare raw OS strings: `Path` equality is component-wise and would
    // treat `/a/./b` and `/a/b` as equal.
    if normalize_lexical(p).as_deref().map(Path::as_os_str) != Some(p.as_os_str()) {
        return Err(ApiError::validation(format!(
            "path must be normalized (no '.' or '..' components): {text}"
        )));
    }
    Ok(())
}

/// Split a path into its normalized parent directory and file name.
/// Returns an error when the path has no file name component.
pub fn parent_and_name(p: &Path) -> Result<(PathBuf, String), ApiError> {
    let name = p
        .file_name()
        .ok_or_else(|| ApiError::validation(format!("path has no file name: {}", p.display())))?;
    let parent = p
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    Ok((parent, name.to_string_lossy().into_owned()))
}

/// The string form of a normalized absolute path used for ordering and as
/// the basis of stable suggestion ids. Host-native; on Windows this is the
/// case-folded form for collision comparison.
pub fn sort_key(p: &Path) -> String {
    let s = p.to_string_lossy();
    #[cfg(windows)]
    {
        s.to_lowercase()
    }
    #[cfg(not(windows))]
    {
        s.into_owned()
    }
}

/// Compare two absolute paths under the host's collision rules (case
/// folding on Windows, exact on Unix).
pub fn paths_collide(a: &Path, b: &Path) -> bool {
    sort_key(a) == sort_key(b)
}

/// Lexically relative path from `from` (a directory) to `to`.
///
/// Both paths are compared component by component; no filesystem access is
/// performed. Returns `"."` when the paths are equal.
pub fn relative_to(from: &Path, to: &Path) -> String {
    let from_comps: Vec<_> = from.components().collect();
    let to_comps: Vec<_> = to.components().collect();
    let mut common = 0;
    while common < from_comps.len()
        && common < to_comps.len()
        && from_comps[common] == to_comps[common]
    {
        common += 1;
    }
    let mut parts: Vec<String> = Vec::new();
    for _ in common..from_comps.len() {
        parts.push("..".to_string());
    }
    for c in &to_comps[common..] {
        parts.push(c.as_os_str().to_string_lossy().into_owned());
    }
    if parts.is_empty() {
        ".".to_string()
    } else {
        parts.join("/")
    }
}

/// Platform home directory resolved at call time from the host
/// environment. `None` when no home can be determined.
pub fn home_dir() -> Option<PathBuf> {
    dirs::home_dir()
}

pub fn default_app_data_dir() -> Option<PathBuf> {
    dirs::data_dir().map(|d| d.join("homelinter"))
}

#[cfg(test)]
mod tests {
    #[cfg(unix)]
    use super::*;

    #[cfg(unix)]
    #[test]
    fn normalizes_lexically() {
        assert_eq!(
            normalize_lexical(Path::new("/a/b/../c/./d")).unwrap(),
            PathBuf::from("/a/c/d")
        );
        // `..` at the root is absorbed: the root cannot be popped.
        assert_eq!(
            normalize_lexical(Path::new("/../a")).unwrap(),
            PathBuf::from("/a")
        );
        assert!(normalize_lexical(Path::new("relative")).is_none());
    }

    #[cfg(unix)]
    #[test]
    fn relative_paths() {
        assert_eq!(relative_to(Path::new("/a/b"), Path::new("/a/b/c")), "c");
        assert_eq!(relative_to(Path::new("/a/b"), Path::new("/a/x")), "../x");
        assert_eq!(relative_to(Path::new("/a/b"), Path::new("/a/b")), ".");
    }

    #[cfg(unix)]
    #[test]
    fn rejects_bad_inputs() {
        assert!(require_absolute_normalized(Path::new("~/x")).is_err());
        assert!(require_absolute_normalized(Path::new("/a/./b")).is_err());
        assert!(require_absolute_normalized(Path::new("relative")).is_err());
        assert!(require_absolute_normalized(Path::new("/a/b")).is_ok());
    }
}
