//! Bounded external tool execution shared by metadata extractors.
//!
//! Tools run with a bounded wall-clock timeout and a captured-stdout cap,
//! a minimal sanitized environment (only PATH), no stdin, and stderr
//! discarded. Exceeding either bound terminates the child and the call
//! returns `None` (extraction failure), matching the document-extraction
//! discipline in DESIGN.md. A non-zero exit status is a failure like any
//! other; callers treat every `None` as "no metadata". Commands run
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

    let ok = matches!(exited, Some(status) if status.success());
    if !ok {
        let _ = child.kill();
        let _ = child.wait();
        let _ = reader.join();
        return None;
    }
    let out = reader.join().ok().flatten()?;
    Some(out)
}
