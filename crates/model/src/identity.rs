//! Host-native file identity, used to prove immediately before execution
//! that a path still names the object analysis examined.
//!
//! Unix uses `st_dev` + `st_ino`; Windows uses the volume serial number and
//! file index from `GetFileInformationByHandle`. Reparse points are opened
//! without being followed, so a link's own identity is reported.
//!
//! The reported `available` flag is false when the platform returns a zero
//! identity (for example a FAT-style filesystem), which the planner and the
//! executor treat as unverifiable rather than silently trusting the path.

use std::io;
use std::path::Path;

/// A host object identity: device/volume plus inode/file index.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileIdentity {
    pub dev: u64,
    pub ino: u64,
    /// Whether the platform returned a usable identity.
    pub available: bool,
}

/// Read the identity of `path` without following a final symlink.
#[cfg(unix)]
pub fn file_identity(path: &Path) -> io::Result<FileIdentity> {
    identity_from_metadata(&std::fs::symlink_metadata(path)?)
}

/// Read the identity of the object `path` resolves to, following a final
/// symlink. Used when comparing filesystems, where a link's own device says
/// nothing about where the linked object lives.
#[cfg(unix)]
pub fn file_identity_following(path: &Path) -> io::Result<FileIdentity> {
    identity_from_metadata(&std::fs::metadata(path)?)
}

#[cfg(unix)]
fn identity_from_metadata(meta: &std::fs::Metadata) -> io::Result<FileIdentity> {
    use std::os::unix::fs::MetadataExt;
    Ok(FileIdentity {
        dev: meta.dev(),
        ino: meta.ino(),
        available: meta.dev() != 0 && meta.ino() != 0,
    })
}

/// Read the identity of `path` without following a reparse point.
#[cfg(windows)]
pub fn file_identity(path: &Path) -> io::Result<FileIdentity> {
    windows_identity(path, false)
}

/// Read the identity of the object `path` resolves to, following a final
/// reparse point. Used when comparing filesystems, where a link's own
/// volume says nothing about where the linked object lives.
#[cfg(windows)]
pub fn file_identity_following(path: &Path) -> io::Result<FileIdentity> {
    windows_identity(path, true)
}

#[cfg(windows)]
fn windows_identity(path: &Path, follow: bool) -> io::Result<FileIdentity> {
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION,
        FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_DELETE,
        FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
    };

    let wide = wide_path(path)?;
    // Query attributes only: no read/write data access is needed, and
    // `FILE_FLAG_OPEN_REPARSE_POINT` keeps a link's own identity. Opening a
    // directory requires `FILE_FLAG_BACKUP_SEMANTICS`.
    // SAFETY: `wide` is NUL-terminated and outlives the call; all other
    // arguments are documented null/default values.
    let handle = unsafe {
        CreateFileW(
            wide.as_ptr(),
            0,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            std::ptr::null(),
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS
                | if follow {
                    0
                } else {
                    FILE_FLAG_OPEN_REPARSE_POINT
                },
            std::ptr::null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
    // SAFETY: the handle is a live file handle and `info` is a valid
    // writable structure of the expected type.
    let ok = unsafe { GetFileInformationByHandle(handle, &mut info) };
    let err = io::Error::last_os_error();
    // SAFETY: the handle was returned by CreateFileW and is no longer used.
    unsafe { CloseHandle(handle) };
    if ok == 0 {
        return Err(err);
    }
    Ok(FileIdentity {
        dev: u64::from(info.dwVolumeSerialNumber),
        ino: (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow),
        // A successful file-information query is authoritative even when
        // the volume reports a zero serial: unlike Unix inode zero, the
        // Windows pair comes from an API that proved the object exists.
        available: true,
    })
}

/// Convert a filesystem path to a NUL-terminated UTF-16 buffer for Win32
/// calls, using the extended `\\?\` form for absolute paths so operations
/// keep working beyond `MAX_PATH` just as Rust's own filesystem calls do.
/// Relative paths are passed through unchanged.
#[cfg(windows)]
pub fn wide_path(path: &Path) -> io::Result<Vec<u16>> {
    let raw = raw_wide_path(path)?;
    let mut wide = Vec::with_capacity(raw.len() + 8);
    let is_unc = raw.len() >= 2 && raw[0] == b'\\' as u16 && raw[1] == b'\\' as u16;
    let already_extended = is_unc
        && raw.len() >= 4
        && (raw[2] == b'?' as u16 || raw[2] == b'.' as u16)
        && raw[3] == b'\\' as u16;
    let has_drive = raw.len() >= 2 && raw[1] == b':' as u16;
    if already_extended || (!is_unc && !has_drive) {
        wide.extend_from_slice(&raw);
    } else if is_unc {
        wide.extend("\\\\?\\UNC".encode_utf16());
        wide.extend_from_slice(&raw[2..]);
    } else {
        wide.extend("\\\\?\\".encode_utf16());
        wide.extend_from_slice(&raw);
    }
    wide.push(0);
    Ok(wide)
}

/// Convert a raw value (such as a symlink target) to a NUL-terminated
/// UTF-16 buffer without adding an extended-path prefix.
#[cfg(windows)]
pub fn raw_wide_path(path: &Path) -> io::Result<Vec<u16>> {
    use std::os::windows::ffi::OsStrExt;
    let wide: Vec<u16> = path.as_os_str().encode_wide().collect();
    if wide.contains(&0) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "path contains a NUL byte",
        ));
    }
    Ok(wide)
}

/// What kind of link (if any) a path is, without following it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkKind {
    NotLink,
    /// A symbolic link, which the planner and executor support repairing.
    Symlink,
    /// Another reparse point (a Windows junction, mount point, or
    /// filesystem-specific tag). It is never followed, indexed for repair,
    /// or modified.
    OtherReparsePoint,
}

/// Classify `path` without following it. `file_type` must come from a
/// non-following metadata/readdir call for the same path.
#[cfg(unix)]
pub fn link_kind(_path: &Path, file_type: &std::fs::FileType) -> LinkKind {
    if file_type.is_symlink() {
        LinkKind::Symlink
    } else {
        LinkKind::NotLink
    }
}

#[cfg(windows)]
pub fn link_kind(path: &Path, file_type: &std::fs::FileType) -> LinkKind {
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, FileAttributeTagInfo, GetFileAttributesW, GetFileInformationByHandleEx,
        FILE_ATTRIBUTE_TAG_INFO, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
        FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
    };

    // Rust's `FileType::is_symlink` reports every name-surrogate reparse
    // point, including junctions, so ask for the exact tag instead.
    const IO_REPARSE_TAG_SYMLINK: u32 = 0xA000_000C;
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;
    const INVALID_FILE_ATTRIBUTES: u32 = u32::MAX;

    let Ok(wide) = wide_path(path) else {
        return LinkKind::NotLink;
    };
    // SAFETY: `wide` is NUL-terminated and outlives the call.
    let attributes = unsafe { GetFileAttributesW(wide.as_ptr()) };
    if attributes == INVALID_FILE_ATTRIBUTES || attributes & FILE_ATTRIBUTE_REPARSE_POINT == 0 {
        // Fast path: ordinary files and directories need no handle. This
        // keeps the per-entry scan cost to one attribute query.
        return LinkKind::NotLink;
    }
    if !file_type.is_symlink() {
        // A reparse point outside the name-surrogate set (for example a
        // cloud placeholder) that std does not report as a link.
        return LinkKind::OtherReparsePoint;
    }
    // SAFETY: `wide` is NUL-terminated and outlives the call.
    let handle = unsafe {
        CreateFileW(
            wide.as_ptr(),
            0,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            std::ptr::null(),
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
            std::ptr::null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return LinkKind::NotLink;
    }
    let mut info: FILE_ATTRIBUTE_TAG_INFO = unsafe { std::mem::zeroed() };
    // SAFETY: the handle is live and `info` is a correctly sized writable
    // structure; the size is passed in bytes.
    let ok = unsafe {
        GetFileInformationByHandleEx(
            handle,
            FileAttributeTagInfo,
            (&mut info as *mut FILE_ATTRIBUTE_TAG_INFO).cast(),
            std::mem::size_of::<FILE_ATTRIBUTE_TAG_INFO>() as u32,
        )
    };
    // SAFETY: the handle came from CreateFileW and is no longer used.
    unsafe { CloseHandle(handle) };
    if ok == 0 || info.FileAttributes & FILE_ATTRIBUTE_REPARSE_POINT == 0 {
        return LinkKind::NotLink;
    }
    if info.ReparseTag == IO_REPARSE_TAG_SYMLINK {
        LinkKind::Symlink
    } else {
        LinkKind::OtherReparsePoint
    }
}

#[cfg(not(any(unix, windows)))]
pub fn link_kind(_path: &Path, file_type: &std::fs::FileType) -> LinkKind {
    if file_type.is_symlink() {
        LinkKind::Symlink
    } else {
        LinkKind::NotLink
    }
}

#[cfg(not(any(unix, windows)))]
pub fn file_identity(_path: &Path) -> io::Result<FileIdentity> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "no native object identity on this platform",
    ))
}

#[cfg(not(any(unix, windows)))]
pub fn file_identity_following(_path: &Path) -> io::Result<FileIdentity> {
    file_identity(_path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_is_stable_for_an_unchanged_path() {
        let path = std::env::temp_dir().join(format!(
            "hl-identity-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::write(&path, b"x").unwrap();
        let a = file_identity(&path).unwrap();
        let b = file_identity(&path).unwrap();
        assert_eq!(a, b);
        assert!(a.available, "temporary filesystem should provide identity");
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn link_kind_distinguishes_links_from_files() {
        let dir = std::env::temp_dir().join(format!("hl-link-kind-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("file.txt");
        std::fs::write(&file, b"x").unwrap();
        let meta = std::fs::symlink_metadata(&file).unwrap();
        assert_eq!(link_kind(&file, &meta.file_type()), LinkKind::NotLink);
        #[cfg(unix)]
        {
            let link = dir.join("link.txt");
            std::os::unix::fs::symlink("file.txt", &link).unwrap();
            let meta = std::fs::symlink_metadata(&link).unwrap();
            assert_eq!(link_kind(&link, &meta.file_type()), LinkKind::Symlink);
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    #[cfg(windows)]
    #[test]
    fn wide_paths_use_the_extended_prefix() {
        fn text(path: &str) -> String {
            let wide = wide_path(Path::new(path)).unwrap();
            String::from_utf16(&wide[..wide.len() - 1]).unwrap()
        }
        assert_eq!(text(r"C:\Users\alice\x.txt"), r"\\?\C:\Users\alice\x.txt");
        assert_eq!(text(r"\\server\share\x.txt"), r"\\?\UNC\server\share\x.txt");
        assert_eq!(text(r"\\?\C:\already\x.txt"), r"\\?\C:\already\x.txt");
        assert_eq!(text("relative/x.txt"), "relative/x.txt");
    }

    #[test]
    fn identity_reports_missing_paths() {
        let path = std::env::temp_dir().join("hl-identity-missing-does-not-exist");
        std::fs::remove_file(&path).ok();
        assert!(file_identity(&path).is_err());
    }
}
