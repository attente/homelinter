//! Filesystem primitives for the executor: no-replace rename (kernel-atomic
//! on Linux and macOS, hard-link publish or a checked rename elsewhere),
//! verified cross-filesystem moves, and symlink replacement without
//! following.
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
///
/// Linux uses `renameat2(RENAME_NOREPLACE)` and macOS uses
/// `renamex_np(RENAME_EXCL)`, both of which fail atomically when the
/// destination appears. Where the kernel or filesystem cannot express a
/// no-replace rename, a non-directory is published with a hard link (which
/// fails if the destination exists) and then removed from the source. Files
/// on a filesystem that supports neither primitive, and directories
/// everywhere that lack an atomic rename, fall back to checking the
/// destination immediately before a plain `rename`.
///
/// That last fallback is best-effort: `rename` silently replaces an existing
/// destination, so a concurrent creation at the same path in the window
/// between the check and the rename can be replaced. It is used only where
/// the filesystem offers no stronger primitive, and only after the executor's
/// own preflight check of the destination.
///
/// Windows uses `MoveFileExW` without `MOVEFILE_REPLACE_EXISTING`, which
/// fails when the destination exists instead of replacing it.
pub fn rename_no_replace(src: &Path, dst: &Path) -> Result<(), String> {
    match platform_rename_noreplace(src, dst)? {
        RenameAttempt::Renamed => Ok(()),
        RenameAttempt::Unsupported => publish_without_platform_rename(src, dst),
    }
}

/// Outcome of the platform's atomic no-replace rename attempt.
enum RenameAttempt {
    /// The rename happened; the destination did not exist.
    Renamed,
    /// The kernel or filesystem does not support the primitive. Unreachable
    /// on supported platforms, which all provide a native no-replace rename.
    #[cfg_attr(
        any(windows, target_os = "linux", target_os = "macos"),
        allow(dead_code)
    )]
    Unsupported,
}

#[cfg(target_os = "linux")]
fn platform_rename_noreplace(src: &Path, dst: &Path) -> Result<RenameAttempt, String> {
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
        Ok(()) => Ok(RenameAttempt::Renamed),
        Err(rustix::io::Errno::NOSYS)
        | Err(rustix::io::Errno::INVAL)
        | Err(rustix::io::Errno::NOTSUP) => Ok(RenameAttempt::Unsupported),
        Err(e) => Err(format!("cannot rename to {}: {e}", dst.display())),
    }
}

#[cfg(target_os = "macos")]
fn platform_rename_noreplace(src: &Path, dst: &Path) -> Result<RenameAttempt, String> {
    use std::os::unix::ffi::OsStrExt;

    let c_src = std::ffi::CString::new(src.as_os_str().as_bytes())
        .map_err(|_| "source path contains a NUL byte".to_string())?;
    let c_dst = std::ffi::CString::new(dst.as_os_str().as_bytes())
        .map_err(|_| "destination path contains a NUL byte".to_string())?;
    // SAFETY: both pointers come from live CStrings and the flag constant is
    // the documented `RENAME_EXCL` value for `renamex_np`.
    let res = unsafe { libc::renamex_np(c_src.as_ptr(), c_dst.as_ptr(), libc::RENAME_EXCL) };
    if res == 0 {
        return Ok(RenameAttempt::Renamed);
    }
    let e = std::io::Error::last_os_error();
    match e.raw_os_error() {
        Some(libc::ENOTSUP) | Some(libc::EOPNOTSUPP) | Some(libc::EINVAL) | Some(libc::ENOSYS) => {
            Ok(RenameAttempt::Unsupported)
        }
        Some(libc::EEXIST) => Err(format!(
            "destination exists; refusing to overwrite: {}",
            dst.display()
        )),
        _ => Err(format!("cannot rename to {}: {e}", dst.display())),
    }
}

#[cfg(windows)]
fn platform_rename_noreplace(src: &Path, dst: &Path) -> Result<RenameAttempt, String> {
    use windows_sys::Win32::Foundation::{ERROR_ALREADY_EXISTS, ERROR_FILE_EXISTS};
    use windows_sys::Win32::Storage::FileSystem::{MoveFileExW, MOVEFILE_WRITE_THROUGH};

    let src_w = homelinter_model::identity::wide_path(src)
        .map_err(|_| "source path contains a NUL byte".to_string())?;
    let dst_w = homelinter_model::identity::wide_path(dst)
        .map_err(|_| "destination path contains a NUL byte".to_string())?;
    // No MOVEFILE_REPLACE_EXISTING: an existing destination makes the call
    // fail instead of being replaced. MOVEFILE_WRITE_THROUGH flushes the
    // rename before returning. Same-volume renames (the only kind routed
    // here) do not need MOVEFILE_COPY_ALLOWED, which would degrade into an
    // unverified copy.
    // SAFETY: both buffers are NUL-terminated and outlive the call.
    let ok = unsafe { MoveFileExW(src_w.as_ptr(), dst_w.as_ptr(), MOVEFILE_WRITE_THROUGH) };
    if ok != 0 {
        return Ok(RenameAttempt::Renamed);
    }
    let err = std::io::Error::last_os_error();
    match err.raw_os_error().map(|e| e as u32) {
        Some(code) if code == ERROR_ALREADY_EXISTS || code == ERROR_FILE_EXISTS => Err(format!(
            "destination exists; refusing to overwrite: {}",
            dst.display()
        )),
        _ => Err(format!("cannot rename to {}: {err}", dst.display())),
    }
}

#[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
fn platform_rename_noreplace(_src: &Path, _dst: &Path) -> Result<RenameAttempt, String> {
    Ok(RenameAttempt::Unsupported)
}

/// Checked rename: verify the destination is absent, then rename.
///
/// This is best-effort, because `rename` replaces an existing destination;
/// it is used only where no atomic publish is available.
#[cfg_attr(windows, allow(dead_code))]
fn checked_rename(src: &Path, dst: &Path) -> Result<(), String> {
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

/// Publish `src` at `dst` with the strongest primitive the filesystem
/// offers, then remove the source name.
///
/// `hard_link` creates the destination name atomically and fails when it
/// already exists, so files keep the no-replace guarantee without a platform
/// rename primitive. Directories cannot be hard-linked, and some filesystems
/// reject hard links entirely; in both cases the destination is checked
/// immediately before a plain `rename`, which is best-effort because
/// `rename` replaces an existing destination.
#[cfg_attr(windows, allow(dead_code))]
fn publish_without_platform_rename(src: &Path, dst: &Path) -> Result<(), String> {
    let meta = std::fs::symlink_metadata(src)
        .map_err(|e| format!("cannot stat source {}: {e}", src.display()))?;
    if !meta.is_dir() {
        match std::fs::hard_link(src, dst) {
            Ok(()) => {
                return std::fs::remove_file(src).map_err(|e| {
                    format!(
                        "published {} but could not remove source {}: {e}",
                        dst.display(),
                        src.display()
                    )
                });
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                return Err(format!(
                    "destination exists; refusing to overwrite: {}",
                    dst.display()
                ));
            }
            Err(_) => {
                // No hard links on this filesystem: fall through to the
                // best-effort checked rename.
            }
        }
    }
    checked_rename(src, dst)
}

// ---------------------------------------------------------------------------
// Filesystem comparison
// ---------------------------------------------------------------------------

/// Whether two paths live on the same filesystem. Uses the platform's
/// device field (`st_dev` on Unix, volume serial on Windows) of the object
/// each path resolves to: a destination parent that is a symlink or
/// junction must be compared at its target, not at the link.
pub fn same_filesystem(a: &Path, b: &Path) -> Result<bool, String> {
    let ia = homelinter_model::identity::file_identity_following(a)
        .map_err(|e| format!("cannot stat {}: {e}", a.display()))?;
    let ib = homelinter_model::identity::file_identity_following(b)
        .map_err(|e| format!("cannot stat {}: {e}", b.display()))?;
    Ok(ia.dev == ib.dev)
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
/// silently removed) when a step fails after the copy was started; a
/// failure to remove the source after the publish is a `Partial` outcome,
/// because the destination is already complete and verified.
pub fn cross_fs_move(src: &Path, dst: &Path) -> Result<MoveOutcome, String> {
    let meta =
        std::fs::metadata(src).map_err(|e| format!("cannot stat source {}: {e}", src.display()))?;

    if meta.is_dir() {
        let tmp = temp_sibling(dst, "dir")?;
        copy_tree_verified(src, &tmp)?;
        // fsync the copied files (done per file) and the directory entries
        // of every copied directory before verification and publish.
        fsync_tree_dirs(&tmp)?;
        // Structural completeness: the copy's relative path set must match
        // the source's exactly before anything is published or deleted.
        verify_tree_completeness(src, &tmp)?;
        // Publish: the temporary directory becomes the destination.
        rename_no_replace(&tmp, dst)?;
        fsync_dir(&parent_dir(dst))?;
        // The verified copy is in place at its final name; only now is the
        // source tree removed. A failure here is reported as partial
        // completion: destination is complete, leftover source remains.
        if let Err(e) = std::fs::remove_dir_all(src) {
            return Ok(MoveOutcome::Partial(format!(
                "copy verified and published at {} but removing source {} failed: {e}",
                dst.display(),
                src.display()
            )));
        }
        Ok(MoveOutcome::Complete)
    } else {
        let tmp = temp_sibling(dst, "file")?;
        copy_file_verified(src, &tmp)?;
        rename_no_replace(&tmp, dst)?;
        // Make the published name durable in its directory before the
        // source is removed.
        fsync_dir(&parent_dir(dst))?;
        if let Err(e) = std::fs::remove_file(src) {
            return Ok(MoveOutcome::Partial(format!(
                "copy verified and published at {} but removing source {} failed: {e}",
                dst.display(),
                src.display()
            )));
        }
        Ok(MoveOutcome::Complete)
    }
}

/// How much of a cross-filesystem move completed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MoveOutcome {
    /// The copy is published and the source is gone.
    Complete,
    /// The copy is verified and published, but the source could not be
    /// removed (the destination is complete; the leftover source remains).
    Partial(String),
}

/// The directory that holds `dst`, or `.` for a bare file name.
fn parent_dir(dst: &Path) -> PathBuf {
    match dst.parent() {
        Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
        _ => PathBuf::from("."),
    }
}

/// fsync a directory so the names created in it are durable. Directory
/// handles cannot be synced on Windows, and filesystems that reject the
/// operation with `InvalidInput` or `Unsupported` simply do not offer it,
/// so those cases succeed as a platform limitation; any other error is
/// reported.
fn fsync_dir(dir: &Path) -> Result<(), String> {
    #[cfg(unix)]
    {
        let handle = std::fs::File::open(dir)
            .map_err(|e| format!("cannot open directory {} for fsync: {e}", dir.display()))?;
        match handle.sync_all() {
            Ok(()) => Ok(()),
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::InvalidInput | std::io::ErrorKind::Unsupported
                ) =>
            {
                Ok(())
            }
            Err(e) => Err(format!("fsync failed on directory {}: {e}", dir.display())),
        }
    }
    #[cfg(not(unix))]
    {
        // Windows does not support flushing a directory handle; make the
        // call still fail for a missing directory so callers get an honest
        // error instead of a silent success.
        std::fs::metadata(dir)
            .map_err(|e| format!("cannot open directory {} for fsync: {e}", dir.display()))?;
        Ok(())
    }
}

/// fsync every directory in a copied tree, deepest first. Symlinks are
/// never followed.
fn fsync_tree_dirs(root: &Path) -> Result<(), String> {
    let entries =
        std::fs::read_dir(root).map_err(|e| format!("cannot read {}: {e}", root.display()))?;
    for entry in entries {
        let entry = entry.map_err(|e| format!("cannot read entry of {}: {e}", root.display()))?;
        let child = root.join(entry.file_name());
        if entry
            .file_type()
            .map_err(|e| format!("cannot stat {}: {e}", child.display()))?
            .is_dir()
        {
            fsync_tree_dirs(&child)?;
        }
    }
    fsync_dir(root)
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
    // Preserve mtime/atime where the host supports setting them. Without
    // this a cross-filesystem move would make an old file look new, which
    // would distort later keeper (oldest copy) selection.
    if let Ok(meta) = std::fs::metadata(src) {
        let mut times = std::fs::FileTimes::new();
        let mut any = false;
        if let Ok(t) = meta.modified() {
            times = times.set_modified(t);
            any = true;
        }
        if let Ok(t) = meta.accessed() {
            times = times.set_accessed(t);
            any = true;
        }
        if any {
            let _ = output.set_times(times);
        }
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
        // Classify links before `is_dir`/`is_file`: a directory symlink or
        // junction has the directory attribute set but must never be
        // traversed.
        match homelinter_model::identity::link_kind(&child_src, &ft) {
            homelinter_model::identity::LinkKind::Symlink => {
                // Recreate from the raw target so relative and absolute
                // links stay exactly as recorded and links to missing
                // targets are still reproduced (the copy never follows
                // them).
                let raw = std::fs::read_link(&child_src)
                    .map_err(|e| format!("cannot read link {}: {e}", child_src.display()))?;
                create_symlink(&child_dst, &raw).map_err(|e| {
                    format!(
                        "cannot recreate contained symlink {} -> {}: {e}",
                        child_src.display(),
                        raw.display()
                    )
                })?;
            }
            homelinter_model::identity::LinkKind::OtherReparsePoint => {
                return Err(format!(
                    "unsupported reparse point in tree: {}; move refused",
                    child_src.display()
                ));
            }
            homelinter_model::identity::LinkKind::NotLink => {
                if ft.is_dir() {
                    copy_tree_verified(&child_src, &child_dst)?;
                } else if ft.is_file() {
                    copy_file_verified(&child_src, &child_dst)?;
                } else {
                    return Err(format!(
                        "unsupported entry kind in tree: {}; move refused",
                        child_src.display()
                    ));
                }
            }
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
            match homelinter_model::identity::link_kind(&dir.join(&name), &ft) {
                homelinter_model::identity::LinkKind::NotLink => {
                    if ft.is_dir() {
                        stack.push((dir.join(&name), child_rel.clone()));
                    }
                }
                homelinter_model::identity::LinkKind::Symlink => {}
                homelinter_model::identity::LinkKind::OtherReparsePoint => {
                    return Err(format!(
                        "unsupported reparse point in tree: {}",
                        dir.join(&name).display()
                    ));
                }
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
// Symlink creation and replacement (never follows)
// ---------------------------------------------------------------------------

/// Create a new symlink at `link_path` pointing at `target` verbatim, as
/// used when reproducing contained links in a fresh tree copy. Fails if
/// `link_path` already exists; never follows `target`.
pub fn create_symlink(link_path: &Path, target: &Path) -> Result<(), String> {
    if path_exists(link_path) {
        return Err(format!(
            "refusing to create link over existing path: {}",
            link_path.display()
        ));
    }
    create_link(link_path, &target.to_string_lossy())
}

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
    if homelinter_model::identity::link_kind(link_path, &meta.file_type())
        != homelinter_model::identity::LinkKind::Symlink
    {
        return Err(format!(
            "not a supported symbolic link: {}",
            link_path.display()
        ));
    }
    // Remove the old link, then create the new one pointing at the
    // (already-planned) replacement target. Creating never follows.
    remove_link(link_path)
        .map_err(|e| format!("cannot remove old link {}: {e}", link_path.display()))?;
    create_link(link_path, target)
}

/// Create `link_path` pointing at the raw target string. The caller has
/// already ensured the path does not exist.
#[cfg(unix)]
fn create_link(link_path: &Path, target: &str) -> Result<(), String> {
    std::os::unix::fs::symlink(target, link_path).map_err(|e| {
        format!(
            "cannot create symlink {} -> {}: {e}",
            link_path.display(),
            target
        )
    })
}

/// Create a Windows symbolic link (never a junction or other reparse
/// point). The directory flag is derived from the target when it resolves;
/// a dangling target is recreated as a file link, which still preserves the
/// raw target text. Creating links requires either Developer Mode or the
/// SeCreateSymbolicLinkPrivilege; the error says so instead of guessing.
#[cfg(windows)]
fn create_link(link_path: &Path, target: &str) -> Result<(), String> {
    use windows_sys::Win32::Foundation::ERROR_PRIVILEGE_NOT_HELD;
    use windows_sys::Win32::Storage::FileSystem::{
        CreateSymbolicLinkW, SYMBOLIC_LINK_FLAG_ALLOW_UNPRIVILEGED_CREATE,
        SYMBOLIC_LINK_FLAG_DIRECTORY,
    };

    let target_path = Path::new(target);
    let resolved = if target_path.is_absolute() {
        target_path.to_path_buf()
    } else {
        link_path
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join(target_path)
    };
    let mut flags = SYMBOLIC_LINK_FLAG_ALLOW_UNPRIVILEGED_CREATE;
    if std::fs::metadata(&resolved).is_ok_and(|m| m.is_dir()) {
        flags |= SYMBOLIC_LINK_FLAG_DIRECTORY;
    }

    let link_w = homelinter_model::identity::wide_path(link_path)
        .map_err(|_| "link path contains a NUL byte".to_string())?;
    // The target is passed raw (no extended-path prefix) when relative so
    // the link keeps the exact text the user would see; an absolute target
    // gains the extended prefix, which Windows accepts for link targets.
    let target_w = homelinter_model::identity::wide_path(target_path)
        .map_err(|_| "link target contains a NUL byte".to_string())?;
    // SAFETY: both buffers are NUL-terminated and outlive the call.
    let ok = unsafe { CreateSymbolicLinkW(link_w.as_ptr(), target_w.as_ptr(), flags) };
    if ok != 0 {
        return Ok(());
    }
    let err = std::io::Error::last_os_error();
    if err.raw_os_error().map(|e| e as u32) == Some(ERROR_PRIVILEGE_NOT_HELD) {
        return Err(format!(
            "cannot create symlink {} -> {}: {err} (enable Developer Mode or grant SeCreateSymbolicLinkPrivilege)",
            link_path.display(),
            target
        ));
    }
    Err(format!(
        "cannot create symlink {} -> {}: {err}",
        link_path.display(),
        target
    ))
}

/// Remove a symlink without following it. A Windows directory symlink is
/// removed with `remove_dir`; Unix links use `remove_file` in both cases.
fn remove_link(link_path: &Path) -> std::io::Result<()> {
    match std::fs::remove_file(link_path) {
        Ok(()) => Ok(()),
        Err(first) => std::fs::remove_dir(link_path).map_err(|_| first),
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
    fn copy_preserves_the_modification_time() {
        let root = tmpdir("copy-times");
        let src = root.join("src.bin");
        let dst = root.join("dst.bin");
        std::fs::write(&src, b"payload").unwrap();
        let stamp = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_000_000);
        std::fs::File::options()
            .write(true)
            .open(&src)
            .unwrap()
            .set_times(std::fs::FileTimes::new().set_modified(stamp))
            .unwrap();
        copy_file_verified(&src, &dst).unwrap();
        let copied = std::fs::metadata(&dst)
            .unwrap()
            .modified()
            .unwrap()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap();
        assert_eq!(copied.as_secs(), 1_000_000);
        std::fs::remove_dir_all(&root).ok();
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

    #[cfg(unix)]
    #[test]
    fn tree_copy_recreates_symlinks_without_following() {
        let root = tmpdir("links");
        let src = root.join("src");
        let copy = root.join("copy");
        build_tree(&src);
        std::os::unix::fs::symlink("a.bin", src.join("rel-link")).unwrap();
        std::os::unix::fs::symlink("/does/not/exist/at/all", src.join("abs-link")).unwrap();
        std::os::unix::fs::symlink("nested", src.join("dir-link")).unwrap();

        copy_tree_verified(&src, &copy).unwrap();
        verify_tree_completeness(&src, &copy).unwrap();

        for (name, want) in [
            ("rel-link", "a.bin"),
            ("abs-link", "/does/not/exist/at/all"),
            ("dir-link", "nested"),
        ] {
            let meta = std::fs::symlink_metadata(copy.join(name)).unwrap();
            assert!(meta.file_type().is_symlink(), "{name} is not a symlink");
            assert_eq!(
                std::fs::read_link(copy.join(name)).unwrap().as_os_str(),
                want
            );
            assert_eq!(
                std::fs::read_link(src.join(name)).unwrap().as_os_str(),
                want
            );
        }
        // A link to a directory must not be expanded into a real directory.
        assert!(std::fs::symlink_metadata(copy.join("dir-link"))
            .unwrap()
            .file_type()
            .is_symlink());
    }

    #[cfg(unix)]
    #[test]
    fn create_symlink_refuses_to_overwrite() {
        let root = tmpdir("create-link");
        let existing = root.join("existing");
        std::fs::write(&existing, b"x").unwrap();
        let err = create_symlink(&existing, Path::new("elsewhere")).unwrap_err();
        assert!(
            err.contains("refusing to create link over existing path"),
            "{err}"
        );
        assert_eq!(std::fs::read(&existing).unwrap(), b"x");
    }

    #[test]
    fn fsync_helpers_sync_directories_and_report_missing_ones() {
        let root = tmpdir("fsync");
        build_tree(&root);
        fsync_tree_dirs(&root).unwrap();
        fsync_dir(&root).unwrap();
        let err = fsync_dir(&root.join("missing")).unwrap_err();
        assert!(err.contains("cannot open directory"), "{err}");
    }

    #[cfg(unix)]
    #[test]
    fn hard_link_publish_removes_source_and_keeps_content() {
        let root = tmpdir("link-publish");
        let src = root.join("src.bin");
        let dst = root.join("dst.bin");
        std::fs::write(&src, b"payload").unwrap();
        publish_without_platform_rename(&src, &dst).unwrap();
        assert!(!path_exists(&src));
        assert_eq!(std::fs::read(&dst).unwrap(), b"payload");
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn hard_link_publish_refuses_existing_destination() {
        let root = tmpdir("link-exists");
        let src = root.join("src.bin");
        let dst = root.join("dst.bin");
        std::fs::write(&src, b"source").unwrap();
        std::fs::write(&dst, b"destination").unwrap();
        let err = publish_without_platform_rename(&src, &dst).unwrap_err();
        assert!(err.contains("destination exists"), "{err}");
        assert_eq!(std::fs::read(&dst).unwrap(), b"destination");
        assert_eq!(std::fs::read(&src).unwrap(), b"source");
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn directory_publish_falls_back_to_a_checked_rename() {
        let root = tmpdir("link-dir");
        let src = root.join("src");
        std::fs::create_dir(&src).unwrap();
        let dst = root.join("dst");
        publish_without_platform_rename(&src, &dst).unwrap();
        assert!(!path_exists(&src));
        assert!(dst.is_dir());
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn checked_rename_refuses_existing_destination() {
        let root = tmpdir("checked-exists");
        let src = root.join("src");
        let dst = root.join("dst");
        std::fs::create_dir(&src).unwrap();
        std::fs::create_dir(&dst).unwrap();
        let err = checked_rename(&src, &dst).unwrap_err();
        assert!(err.contains("destination exists"), "{err}");
        assert!(path_exists(&src));
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn hard_link_publish_keeps_contained_symlinks() {
        let root = tmpdir("link-symlink");
        let src = root.join("link");
        let dst = root.join("published");
        std::os::unix::fs::symlink("raw-target", &src).unwrap();
        publish_without_platform_rename(&src, &dst).unwrap();
        assert!(!path_exists(&src));
        assert_eq!(std::fs::read_link(&dst).unwrap().as_os_str(), "raw-target");
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn rename_no_replace_is_atomic_on_macos() {
        let root = tmpdir("renamex");
        let src = root.join("src.bin");
        let dst = root.join("dst.bin");
        std::fs::write(&src, b"source").unwrap();
        rename_no_replace(&src, &dst).unwrap();
        assert!(!path_exists(&src));
        assert_eq!(std::fs::read(&dst).unwrap(), b"source");

        let src2 = root.join("src2.bin");
        std::fs::write(&src2, b"second").unwrap();
        let err = rename_no_replace(&src2, &dst).unwrap_err();
        assert!(err.contains("destination exists"), "{err}");
        assert_eq!(std::fs::read(&dst).unwrap(), b"source");
        assert_eq!(std::fs::read(&src2).unwrap(), b"second");
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn cross_fs_move_reports_partial_when_source_cleanup_fails() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};

        let root = tmpdir("partial");
        let src = root.join("src");
        let dst = root.join("dst");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("keep.bin"), b"payload").unwrap();

        // Root ignores the permission bits, so the failure cannot be induced.
        if std::fs::metadata("/").unwrap().uid() == 0 {
            return;
        }
        std::fs::set_permissions(&src, std::fs::Permissions::from_mode(0o555)).unwrap();
        let outcome = cross_fs_move(&src, &dst).unwrap();
        std::fs::set_permissions(&src, std::fs::Permissions::from_mode(0o755)).unwrap();

        let MoveOutcome::Partial(detail) = outcome else {
            panic!("expected a partial move, got {outcome:?}");
        };
        assert!(detail.contains("removing source"), "{detail}");
        // The verified destination is complete; the source is still there.
        assert_eq!(std::fs::read(dst.join("keep.bin")).unwrap(), b"payload");
        assert!(src.exists());
        std::fs::remove_dir_all(&root).ok();
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

    #[cfg(windows)]
    #[test]
    fn windows_rename_refuses_an_existing_destination() {
        let root = tmpdir("win-rename-exists");
        let src = root.join("a.txt");
        let dst = root.join("b.txt");
        std::fs::write(&src, b"source").unwrap();
        std::fs::write(&dst, b"occupant").unwrap();
        let err = rename_no_replace(&src, &dst).unwrap_err();
        assert!(err.contains("exists"), "{err}");
        assert_eq!(std::fs::read(&src).unwrap(), b"source");
        assert_eq!(std::fs::read(&dst).unwrap(), b"occupant");
        std::fs::remove_dir_all(&root).ok();
    }

    #[cfg(windows)]
    #[test]
    fn windows_rename_moves_a_directory() {
        let root = tmpdir("win-rename-dir");
        let src = root.join("before");
        let dst = root.join("after");
        std::fs::create_dir_all(src.join("nested")).unwrap();
        std::fs::write(src.join("nested/file.txt"), b"x").unwrap();
        rename_no_replace(&src, &dst).unwrap();
        assert!(!src.exists());
        assert_eq!(std::fs::read(dst.join("nested/file.txt")).unwrap(), b"x");
        std::fs::remove_dir_all(&root).ok();
    }

    #[cfg(windows)]
    #[test]
    fn windows_symlink_is_replaced_without_following() {
        let root = tmpdir("win-symlink");
        let a = root.join("a.txt");
        let b = root.join("b.txt");
        std::fs::write(&a, b"a").unwrap();
        std::fs::write(&b, b"b").unwrap();
        let link = root.join("link.txt");
        if let Err(e) = create_symlink(&link, Path::new("a.txt")) {
            // Creating symlinks needs Developer Mode or the symlink
            // privilege; the test is meaningless without either.
            if e.contains("Developer Mode") || e.contains("privilege") {
                std::fs::remove_dir_all(&root).ok();
                return;
            }
            panic!("{e}");
        }
        replace_symlink(&link, "b.txt", true).unwrap();
        assert_eq!(std::fs::read_link(&link).unwrap(), Path::new("b.txt"));
        let meta = std::fs::symlink_metadata(&link).unwrap();
        assert_eq!(
            homelinter_model::identity::link_kind(&link, &meta.file_type()),
            homelinter_model::identity::LinkKind::Symlink
        );
        assert_eq!(std::fs::read(&a).unwrap(), b"a");
        assert_eq!(std::fs::read(&b).unwrap(), b"b");
        std::fs::remove_dir_all(&root).ok();
    }
}
