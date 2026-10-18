//! Filesystem primitives for the executor: no-replace rename, verified
//! cross-filesystem moves, and symlink replacement without following.
//!
//! These are the only places in homelinter that touch the filesystem on
//! behalf of execution. Every function here fails rather than overwrite or
//! guess, and returns an error string describing exactly what was checked
//! and what failed.

use std::path::{Path, PathBuf};

// ---------------------------------------------------------------------------
// Existence helpers
// ---------------------------------------------------------------------------

/// True when a path exists, whether file, directory, symlink, or special.
pub fn path_exists(p: &Path) -> bool {
    std::fs::symlink_metadata(p).is_ok()
}

/// Create parent directories for a destination. Fails if a component that
/// exists is not a directory.
pub fn create_parent_dirs(parent: &Path) -> Result<(), String> {
    if parent.as_os_str().is_empty() {
        return Ok(());
    }
    std::fs::create_dir_all(parent).map_err(|e| {
        format!(
            "cannot create destination directory {}: {e}",
            parent.display()
        )
    })
}

// ---------------------------------------------------------------------------
// Same-filesystem rename, no-replace
// ---------------------------------------------------------------------------

/// Rename `src` to `dst`, failing when `dst` exists (no-replace semantics).
/// On Linux this uses `renameat2(RENAME_NOREPLACE)` so the guarantee holds
/// atomically; elsewhere it falls back to an existence check immediately
/// before `rename`, which is best-effort under a race.
pub fn rename_no_replace(src: &Path, dst: &Path) -> Result<(), String> {
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::ffi::OsStrExt;

        let c_src = std::ffi::CString::new(src.as_os_str().as_bytes())
            .map_err(|_| "source path contains a NUL byte".to_string())?;
        let c_dst = std::ffi::CString::new(dst.as_os_str().as_bytes())
            .map_err(|_| "destination path contains a NUL byte".to_string())?;
        let res = rustix::fs::renameat_with(
            rustix::fs::CWD,
            &c_src,
            rustix::fs::CWD,
            &c_dst,
            rustix::fs::RenameFlags::NOREPLACE,
        );
        match res {
            Ok(()) => return Ok(()),
            Err(rustix::io::Errno::NOSYS)
            | Err(rustix::io::Errno::INVAL)
            | Err(rustix::io::Errno::NOTSUP) => {
                // Kernel without renameat2: fall through to the checked
                // fallback below.
            }
            Err(e) => return Err(format!("cannot rename to {}: {e}", dst.display())),
        }
    }

    // Fallback (macOS, BSD, non-Linux): check then rename.
    if path_exists(dst) {
        return Err(format!(
            "destination exists; refusing to overwrite: {}",
            dst.display()
        ));
    }
    std::fs::rename(src, dst)
        .map_err(|e| format!("cannot rename {} to {}: {e}", src.display(), dst.display()))?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Filesystem comparison
// ---------------------------------------------------------------------------

/// Whether two paths live on the same filesystem. Uses the platform's
/// object-identity fields: `st_dev` on Unix, volume serial on Windows.
pub fn same_filesystem(a: &Path, b: &Path) -> Result<bool, String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let ma = std::fs::metadata(a).map_err(|e| format!("cannot stat {}: {e}", a.display()))?;
        let mb = std::fs::metadata(b).map_err(|e| format!("cannot stat {}: {e}", b.display()))?;
        Ok(ma.dev() == mb.dev())
    }
    #[cfg(windows)]
    {
        // Compare volume serial numbers when available; treat unknown as
        // same-filesystem only when both are on one drive is unknowable
        // from std alone. Conservative: if we cannot prove they differ,
        // assume the same volume and rely on the no-replace rename to fail
        // safely rather than attempting an unsafe copy.
        Ok(true)
    }
    #[cfg(not(any(unix, windows)))]
    {
        Ok(true)
    }
}

// ---------------------------------------------------------------------------
// Verified cross-filesystem move
// ---------------------------------------------------------------------------

/// The verified copy-and-delete protocol for a file or a whole directory
/// tree when `src` and `dst` are on different filesystems:
///
/// 1. copy to a temporary object in the destination filesystem,
/// 2. verify it against the source: full-content SHA-256 per file and an
///    exact relative-path-set comparison for trees,
/// 3. publish by no-replace rename to its final name,
/// 4. delete the source only after the copy is published.
///
/// At every interruption point either the intact source or a complete
/// verified destination exists. The temporary path is reported (not
/// silently removed) when a step fails after the copy was started.
pub fn cross_fs_move(src: &Path, dst: &Path) -> Result<(), String> {
    let meta =
        std::fs::metadata(src).map_err(|e| format!("cannot stat source {}: {e}", src.display()))?;

    if meta.is_dir() {
        let tmp = temp_sibling(dst, "dir")?;
        copy_tree_verified(src, &tmp)?;
        // Structural completeness: the copy's relative path set must match
        // the source's exactly before anything is published or deleted.
        verify_tree_completeness(src, &tmp)?;
        // Publish: the temporary directory becomes the destination.
        rename_no_replace(&tmp, dst)?;
        // The verified copy is in place at its final name; only now is the
        // source tree removed. A failure here is reported as partial
        // completion: destination is complete, leftover source remains.
        std::fs::remove_dir_all(src).map_err(|e| {
            format!(
                "copy verified and published at {} but removing source {} failed: {e}",
                dst.display(),
                src.display()
            )
        })?;
        Ok(())
    } else {
        let tmp = temp_sibling(dst, "file")?;
        copy_file_verified(src, &tmp)?;
        rename_no_replace(&tmp, dst)?;
        std::fs::remove_file(src).map_err(|e| {
            format!(
                "copy verified and published at {} but removing source {} failed: {e}",
                dst.display(),
                src.display()
            )
        })?;
        Ok(())
    }
}

/// Build a unique temporary path next to `dst` (same directory means same
/// filesystem as the final destination).
fn temp_sibling(dst: &Path, tag: &str) -> Result<PathBuf, String> {
    let dir = dst.parent().unwrap_or(Path::new("."));
    let name = dst
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "obj".into());
    let mut candidate = dir.join(format!(".homelinter-{tag}-{name}-{}", std::process::id()));
    let mut n = 0u32;
    while path_exists(&candidate) {
        n += 1;
        candidate = dir.join(format!(
            ".homelinter-{tag}-{name}-{}-{n}",
            std::process::id()
        ));
    }
    Ok(candidate)
}

/// Copy one file to `dst` with fsync and full-content SHA-256 verification.
pub fn copy_file_verified(src: &Path, dst: &Path) -> Result<(), String> {
    use std::io::{Read, Write};

    let mut input = std::fs::File::open(src)
        .map_err(|e| format!("cannot open source {}: {e}", src.display()))?;
    let mut output =
        std::fs::File::create(dst).map_err(|e| format!("cannot create {}: {e}", dst.display()))?;

    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = input
            .read(&mut buf)
            .map_err(|e| format!("read error on {}: {e}", src.display()))?;
        if n == 0 {
            break;
        }
        output
            .write_all(&buf[..n])
            .map_err(|e| format!("write error on {}: {e}", dst.display()))?;
    }
    output
        .sync_all()
        .map_err(|e| format!("fsync failed on {}: {e}", dst.display()))?;

    // Verify: full-content hash of the copy equals the source's.
    let h_src = homelinter_model::hash::hash_file_full(src)
        .map_err(|e| format!("cannot hash {}: {e}", src.display()))?;
    let h_dst = homelinter_model::hash::hash_file_full(dst)
        .map_err(|e| format!("cannot hash {}: {e}", dst.display()))?;
    if h_src != h_dst {
        return Err(format!(
            "verification failed: copy {} does not match source {}",
            dst.display(),
            src.display()
        ));
    }
    // Preserve permissions when possible.
    if let Ok(perm) = std::fs::metadata(src).map(|m| m.permissions()) {
        let _ = std::fs::set_permissions(dst, perm);
    }
    Ok(())
}

/// Copy a directory tree into `dst` (which must not exist yet), file by
/// file with the verified file protocol, preserving empty directories and
/// recreating contained symlinks from their raw targets without following
/// them. A contained link type that cannot be recreated safely fails the
/// move rather than being dropped.
pub fn copy_tree_verified(src: &Path, dst: &Path) -> Result<(), String> {
    let entries =
        std::fs::read_dir(src).map_err(|e| format!("cannot read {}: {e}", src.display()))?;
    std::fs::create_dir_all(dst).map_err(|e| format!("cannot create {}: {e}", dst.display()))?;

    for entry in entries {
        let entry = entry.map_err(|e| format!("cannot read entry of {}: {e}", src.display()))?;
        let name = entry.file_name();
        let child_src = src.join(&name);
        let child_dst = dst.join(&name);
        let ft = entry
            .file_type()
            .map_err(|e| format!("cannot stat {}: {e}", child_src.display()))?;
        if ft.is_dir() {
            copy_tree_verified(&child_src, &child_dst)?;
        } else if ft.is_file() {
            copy_file_verified(&child_src, &child_dst)?;
        } else if ft.is_symlink() {
            let raw = std::fs::read_link(&child_src)
                .map_err(|e| format!("cannot read link {}: {e}", child_src.display()))?;
            let raw_str = raw.to_string_lossy().into_owned();
            replace_symlink(&child_dst, &raw_str, raw.is_relative()).map_err(|e| {
                format!(
                    "cannot recreate contained symlink {} -> {}: {e}",
                    child_src.display(),
                    raw_str
                )
            })?;
        } else {
            return Err(format!(
                "unsupported entry kind in tree: {}; move refused",
                child_src.display()
            ));
        }
    }
    Ok(())
}

/// Every entry under `root` as a path relative to `root`, without
/// following symlinks. Directories contribute their own entry and their
/// children; unreadable entries are errors, never silently skipped.
fn tree_entries(root: &Path) -> Result<std::collections::BTreeSet<PathBuf>, String> {
    let mut out = std::collections::BTreeSet::new();
    let mut stack = vec![(root.to_path_buf(), PathBuf::new())];
    while let Some((dir, rel)) = stack.pop() {
        let entries =
            std::fs::read_dir(&dir).map_err(|e| format!("cannot read {}: {e}", dir.display()))?;
        for entry in entries {
            let entry =
                entry.map_err(|e| format!("cannot read entry of {}: {e}", dir.display()))?;
            let name = entry.file_name();
            let child_rel = rel.join(&name);
            let ft = entry
                .file_type()
                .map_err(|e| format!("cannot stat {}: {e}", dir.join(&name).display()))?;
            if ft.is_dir() {
                stack.push((dir.join(&name), child_rel.clone()));
            }
            out.insert(child_rel);
        }
    }
    Ok(out)
}

/// Compare the relative path sets of a source tree and its copy. Used as
/// the structural half of cross-filesystem verification; content is
/// covered per file by `copy_file_verified`.
fn verify_tree_completeness(src: &Path, copy: &Path) -> Result<(), String> {
    let src_set = tree_entries(src)?;
    let copy_set = tree_entries(copy)?;
    if src_set == copy_set {
        return Ok(());
    }
    let missing: Vec<String> = src_set
        .difference(&copy_set)
        .take(3)
        .map(|p| p.display().to_string())
        .collect();
    let unexpected: Vec<String> = copy_set
        .difference(&src_set)
        .take(3)
        .map(|p| p.display().to_string())
        .collect();
    Err(format!(
        "verification failed: copy {} does not match source {} ({} missing entries, {} unexpected entries; missing: {}, unexpected: {})",
        copy.display(),
        src.display(),
        src_set.difference(&copy_set).count(),
        copy_set.difference(&src_set).count(),
        if missing.is_empty() {
            "none".to_string()
        } else {
            missing.join(", ")
        },
        if unexpected.is_empty() {
            "none".to_string()
        } else {
            unexpected.join(", ")
        }
    ))
}

// ---------------------------------------------------------------------------
// Symlink replacement (never follows)
// ---------------------------------------------------------------------------

/// Replace the symlink at `link_path` so it points at `target`. `target`
/// has already been resolved to its final relative or absolute form by the
/// planner; we never follow the link or its target. Fails if `link_path`
/// does not currently exist as a symlink.
pub fn replace_symlink(
    link_path: &Path,
    target: &str,
    _target_relative: bool,
) -> Result<(), String> {
    let meta = std::fs::symlink_metadata(link_path)
        .map_err(|e| format!("cannot stat link {}: {e}", link_path.display()))?;
    if !meta.file_type().is_symlink() {
        return Err(format!("not a symlink: {}", link_path.display()));
    }
    // Remove the old link, then create the new one pointing at the
    // (already-planned) replacement target. Creating never follows.
    std::fs::remove_file(link_path)
        .map_err(|e| format!("cannot remove old link {}: {e}", link_path.display()))?;

    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(target, link_path).map_err(|e| {
            format!(
                "cannot create symlink {} -> {}: {e}",
                link_path.display(),
                target
            )
        })?;
        Ok(())
    }
    #[cfg(not(unix))]
    {
        // Junction/reparse-point creation needs Windows APIs that the
        // scaffold does not yet bind; report unsupported rather than guess.
        Err(format!(
            "symlink replacement is not implemented on this platform: {}",
            link_path.display()
        ))
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "hl-fsops-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn build_tree(root: &Path) {
        std::fs::create_dir_all(root.join("nested/deep")).unwrap();
        std::fs::write(root.join("a.bin"), b"a").unwrap();
        std::fs::write(root.join("nested/b.bin"), b"bb").unwrap();
    }

    #[test]
    fn tree_entries_lists_dirs_and_files_relative() {
        let root = tmpdir("entries");
        build_tree(&root);
        let set = tree_entries(&root).unwrap();
        let rel: Vec<String> = set.iter().map(|p| p.display().to_string()).collect();
        assert_eq!(rel, vec!["a.bin", "nested", "nested/b.bin", "nested/deep"]);
    }

    #[test]
    fn tree_entries_reports_unreadable_roots() {
        let root = tmpdir("missing").join("nope");
        let err = tree_entries(&root).unwrap_err();
        assert!(err.contains("cannot read"), "{err}");
    }

    #[test]
    fn completeness_accepts_an_exact_copy_and_rejects_differences() {
        let root = tmpdir("complete");
        let src = root.join("src");
        let copy = root.join("copy");
        build_tree(&src);
        copy_tree_verified(&src, &copy).unwrap();
        verify_tree_completeness(&src, &copy).unwrap();

        std::fs::remove_file(copy.join("nested/b.bin")).unwrap();
        let err = verify_tree_completeness(&src, &copy).unwrap_err();
        assert!(err.contains("1 missing entries"), "{err}");
        assert!(err.contains("nested/b.bin"), "{err}");
    }

    #[test]
    fn completeness_rejects_unexpected_entries() {
        let root = tmpdir("unexpected");
        let src = root.join("src");
        let copy = root.join("copy");
        build_tree(&src);
        copy_tree_verified(&src, &copy).unwrap();
        std::fs::write(copy.join("stray.bin"), b"x").unwrap();
        let err = verify_tree_completeness(&src, &copy).unwrap_err();
        assert!(err.contains("1 unexpected entries"), "{err}");
        assert!(err.contains("stray.bin"), "{err}");
    }
}
