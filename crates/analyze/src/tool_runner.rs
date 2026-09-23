//! Bounded external tool execution shared by metadata extractors.
//!
//! Tools run with a bounded wall-clock timeout and a captured-stdout cap,
//! a minimal sanitized environment (only PATH), no stdin, and stderr
//! discarded. Exceeding either bound terminates the child and the call
//! returns `None` (extraction failure), matching the document-extraction
//! discipline in DESIGN.md. A non-zero exit status is a failure like any
//! other; callers treat every `None` as "no metadata". Commands run
//! directly without a shell, so paths and metadata can never inject shell
//! syntax. The timeout is a hard bound on the wait: stdout is drained after
//! the child exits, so a producer that floods more than the OS pipe buffer
//! before exiting is bounded by the cap rather than the deadline (the
//! extractors request a handful of tags, so their output is tiny).

use std::io::Read;
use std::process::{Command, Stdio};
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
    let deadline = Instant::now() + timeout;
    loop {
        if Instant::now() >= deadline {
            let _ = child.kill();
            return None;
        }
        match child.try_wait() {
            Ok(Some(status)) => {
                if !status.success() {
                    let _ = child.kill();
                    return None;
                }
                break;
            }
            Ok(None) => {
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(_) => {
                let _ = child.kill();
                return None;
            }
        }
    }
    // The child exited successfully; drain stdout up to the cap.
    let Some(mut stdout) = child.stdout.take() else {
        return Some(Vec::new());
    };
    let mut out: Vec<u8> = Vec::new();
    let mut buf = [0u8; 8192];
    loop {
        if (out.len() as u64) >= max_bytes {
            let _ = child.kill();
            return None;
        }
        match stdout.read(&mut buf) {
            Ok(0) => return Some(out),
            Ok(n) => {
                out.extend_from_slice(&buf[..n]);
                if (out.len() as u64) > max_bytes {
                    let _ = child.kill();
                    return None;
                }
            }
            Err(_) => {
                let _ = child.kill();
                return None;
            }
        }
    }
}
