//! Content hashing. SHA-256 is the only hash algorithm; the full-content
//! hash is the only hash that determines duplicate-group membership, and the
//! fast path (first 1 MiB + last 1 MiB + size) is a shortlist mechanism only.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use sha2::{Digest, Sha256};

pub const FAST_PATH_BYTES: u64 = 1024 * 1024;

/// Hash the complete file contents.
pub fn hash_file_full(path: &Path) -> std::io::Result<[u8; 32]> {
    let mut f = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hasher.finalize().into())
}

/// The fast-path hash: SHA-256 over the first 1 MiB, the last 1 MiB, and the
/// total size as an 8-byte little-endian integer, concatenated in that order
/// with no separator. For files at or below the fast-path size the whole
/// content is hashed with no size byte appended, so the fast-path hash equals
/// the full-content hash there; the size byte separates large-file head and
/// tail chunks only.
pub fn hash_file_fast(path: &Path, size: u64) -> std::io::Result<[u8; 32]> {
    let mut f = File::open(path)?;
    let mut hasher = Sha256::new();
    let head_len = size.min(FAST_PATH_BYTES);
    let mut buf = vec![0u8; head_len as usize];
    f.read_exact(&mut buf)?;
    hasher.update(&buf);
    if size > FAST_PATH_BYTES {
        let tail_len = (size - head_len).min(FAST_PATH_BYTES);
        f.seek(SeekFrom::End(-(tail_len as i64)))?;
        let mut tail = vec![0u8; tail_len as usize];
        f.read_exact(&mut tail)?;
        hasher.update(&tail);
        hasher.update(size.to_le_bytes());
    }
    Ok(hasher.finalize().into())
}

pub fn hex_hash(h: &[u8; 32]) -> String {
    hex::encode(h)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn full_hash_is_stable() {
        let dir = std::env::temp_dir().join(format!("hl-hash-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("f.bin");
        std::fs::write(&p, b"hello world").unwrap();
        assert_eq!(hash_file_full(&p).unwrap(), hash_file_full(&p).unwrap());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn fast_hash_matches_full_on_small_file() {
        let dir = std::env::temp_dir().join(format!("hl-hash2-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("f.bin");
        std::fs::write(&p, b"small").unwrap();
        let size = std::fs::metadata(&p).unwrap().len();
        assert_eq!(
            hash_file_fast(&p, size).unwrap(),
            hash_file_full(&p).unwrap()
        );
        std::fs::remove_dir_all(&dir).ok();
    }
}
