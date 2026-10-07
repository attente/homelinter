//! Bounded external tool execution shared by metadata extractors.
//!
//! Tools run with a bounded wall-clock timeout and a captured-stdout cap,
//! a minimal sanitized environment (only PATH), no stdin, and stderr
//! discarded. Exceeding either bound terminates the child and the call
//! returns `None` (extraction failure), matching the document-extraction
//! discipline in DESIGN.md. A non-zero exit status is a failure like any
//! other, and callers treat every `None` as "no metadata". `run_capture_lenient`
//! relaxes only the non-zero-exit rule for callers whose tool emits useful
//! partial output before failing (a batched `exiftool`), while timeout,
//! stdout-cap, spawn, and read failures remain hard failures. Commands run
//! directly without a shell, so paths and metadata can never inject shell
//! syntax. Stdout is drained on a dedicated reader thread while the child
//! runs, so a tool that produces more output than the OS pipe buffer (for
//! example a batched `exiftool` over many files) can exit instead of
//! blocking on a full pipe; the cap still bounds captured output and
//! triggers termination as soon as it is exceeded.

use std::io::Read;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Run `program` with `args`, capturing stdout. `None` on any failure:
/// spawn error, non-zero exit status, timeout, or stdout over `max_bytes`.
pub fn run_capture(
    program: &str,
    args: &[&str],
    timeout: Duration,
    max_bytes: u64,
) -> Option<Vec<u8>> {
    run_capture_inner(program, args, timeout, max_bytes, false)
}

/// Like `run_capture`, but returns the captured stdout even when the tool
/// exits non-zero, so callers that consume partial output (a batched
/// `exiftool` over many files emits rows for every readable input before
/// reporting an error for one of them) can salvage the readable rows.
/// Still `None` on spawn error, timeout, stdout over `max_bytes`, or a
/// read error, because in those cases the captured output is truncated or
/// unusable.
pub fn run_capture_lenient(
    program: &str,
    args: &[&str],
    timeout: Duration,
    max_bytes: u64,
) -> Option<Vec<u8>> {
    run_capture_inner(program, args, timeout, max_bytes, true)
}

fn run_capture_inner(
    program: &str,
    args: &[&str],
    timeout: Duration,
    max_bytes: u64,
    accept_nonzero: bool,
) -> Option<Vec<u8>> {
    if timeout.is_zero() {
        return None;
    }
    let mut cmd = Command::new(program);
    cmd.args(args);
    cmd.env_clear();
    cmd.env("PATH", std::env::var_os("PATH").unwrap_or_default());
    cmd.stdin(Stdio::null());
    cmd.stderr(Stdio::null());
    cmd.stdout(Stdio::piped());
    let mut child = cmd.spawn().ok()?;
    let stdout = child.stdout.take()?;

    // Drain stdout concurrently so the child never blocks on a full pipe.
    // The reader stops at the cap and flags the overflow so the parent can
    // terminate the child instead of waiting for the deadline.
    let overflow = Arc::new(AtomicBool::new(false));
    let overflow_reader = overflow.clone();
    let reader = std::thread::spawn(move || -> Option<Vec<u8>> {
        let mut out: Vec<u8> = Vec::new();
        let mut buf = [0u8; 64 * 1024];
        let mut stdout = stdout;
        loop {
            match stdout.read(&mut buf) {
                Ok(0) => return Some(out),
                Ok(n) => {
                    out.extend_from_slice(&buf[..n]);
                    if out.len() as u64 > max_bytes {
                        overflow_reader.store(true, Ordering::SeqCst);
                        return None;
                    }
                }
                Err(_) => return None,
            }
        }
    });

    let deadline = Instant::now() + timeout;
    let exited = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) => {
                if overflow.load(Ordering::SeqCst) || Instant::now() >= deadline {
                    break None;
                }
                std::thread::sleep(Duration::from_millis(2));
            }
            Err(_) => break None,
        }
    };

    let status = match exited {
        Some(status) => status,
        None => {
            let _ = child.kill();
            let _ = child.wait();
            let _ = reader.join();
            return None;
        }
    };
    if !accept_nonzero && !status.success() {
        let _ = child.kill();
        let _ = child.wait();
        let _ = reader.join();
        return None;
    }
    let out = reader.join().ok().flatten()?;
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn nonzero_exit_fails_but_lenient_keeps_stdout() {
        let timeout = Duration::from_secs(5);
        let max = 1024;
        assert!(run_capture("/bin/sh", &["-c", "printf hi; exit 3"], timeout, max).is_none());
        let out = run_capture_lenient("/bin/sh", &["-c", "printf hi; exit 3"], timeout, max)
            .expect("lenient run should return stdout");
        assert_eq!(out, b"hi");
    }

    #[cfg(unix)]
    #[test]
    fn timeout_is_none_even_for_lenient() {
        let timeout = Duration::from_millis(50);
        assert!(run_capture_lenient("/bin/sh", &["-c", "sleep 5"], timeout, 1024).is_none());
    }
}
