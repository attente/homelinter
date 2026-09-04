//! Native executor: moves, deletes, and dependent symlink repairs.
//!
//! The executor is the authoritative implementation of the safety rules:
//!
//! - Suggestions are ordered deterministically and executed one at a time.
//! - Immediately before each operation the source is re-stat'ed. A changed
//!   size or object identity invalidates the operation and re-queues it for
//!   re-analysis; only an mtime change may still pass when content identity
//!   is unchanged (move verification re-hashes full content anyway).
//! - A destination is never overwritten: moves use rename semantics that
//!   fail when the destination exists (no-replace), and cross-filesystem
//!   moves use the verified copy-and-delete protocol.
//! - Delete only ever removes a file whose duplicate relationship was
//!   confirmed by full-content hash at analysis time; the executor re-verifies
//!   the hash immediately before deletion.
//! - Dependent symlink repairs run only after their parent succeeds, never
//!   follow links, and are re-validated immediately before repair.
//!
//! There is no rollback: failures are reported precisely, and a partially
//! completed unit is reported as `partial` without claiming cleanup that
//! did not happen.

mod fsops;
mod script;

pub use script::{export_host, export_script};

use std::path::{Path, PathBuf};

use homelinter_model::hash;
use homelinter_model::suggestion::{
    DependentOperation, ExecutionReport, ExecutionStatus, OperationKind, OperationOutcome,
    OperationStatus, ReportEntry, Suggestion, SuggestionStatus, SuggestionType,
};

pub struct ExecuteOptions {
    /// Validate and report every operation without touching the filesystem.
    pub dry_run: bool,
}

impl Default for ExecuteOptions {
    fn default() -> Self {
        ExecuteOptions { dry_run: false }
    }
}

/// Execute the given suggestions (already batch-validated and sorted by
/// sort key). `selected_only` restricts execution to `selected == true`.
pub fn execute_batch(
    items: &mut [Suggestion],
    scan_root: &Path,
    selected_only: bool,
    opts: &ExecuteOptions,
) -> ExecutionReport {
    let mut report = ExecutionReport::empty(scan_root);
    report.dry_run = opts.dry_run;

    // Deterministic order: the caller sorts, but a defensive re-sort keeps
    // validation and dependency order identical to generation.
    items.sort_by(|a, b| a.sort_key().cmp(&b.sort_key()));

    for s in items.iter_mut() {
        if selected_only && !s.selected {
            continue;
        }
        if s.status != SuggestionStatus::Pending {
            continue;
        }
        let mut entry = execute_one(s, opts);
        // Classify the entry from its operations: a failed parent is a
        // failure; a completed parent with one or more failed dependent
        // repairs is partial (executed, flag raised); nothing attempted is
        // skipped.
        let parent_failed = entry
            .operations
            .iter()
            .any(|o| o.status == OperationStatus::Failed);
        let anything_done = entry
            .operations
            .iter()
            .any(|o| o.status == OperationStatus::Completed);
        if parent_failed && !anything_done {
            entry.result = ExecutionStatus::Failed;
        } else if !anything_done {
            entry.result = ExecutionStatus::Skipped;
        } else {
            entry.result = ExecutionStatus::Executed;
            let dependent_failed = entry
                .operations
                .iter()
                .skip(1)
                .any(|o| o.status == OperationStatus::Failed);
            if dependent_failed {
                report.partial = true;
            }
        }
        // Mirror the outcome onto the suggestion's own status for the
        // session model.
        s.status = match entry.result {
            ExecutionStatus::Executed => SuggestionStatus::Executed,
            ExecutionStatus::Failed => SuggestionStatus::Failed,
            ExecutionStatus::Skipped => SuggestionStatus::Skipped,
        };
        report.entries.push(entry);
    }
    report
}

fn execute_one(s: &mut Suggestion, opts: &ExecuteOptions) -> ReportEntry {
    let operations: Vec<OperationOutcome> = Vec::new();
    let mut entry = ReportEntry {
        suggestion_id: s.id.clone(),
        result: ExecutionStatus::Skipped,
        operations,
        suggestion: None,
    };

    // Dry runs include the full suggestion payload so the report is a
    // self-contained preview (no session state required to render it).
    if opts.dry_run {
        if let Ok(v) = serde_json::to_value(&*s) {
            entry.suggestion = Some(v);
        }
    }

    let parent = match s.suggestion_type {
        SuggestionType::Move => run_move(s, opts),
        SuggestionType::Delete => run_delete(s, opts),
    };

    match parent {
        Ok(mut ops) => {
            entry.operations.append(&mut ops);
            // Dependents run only when the parent succeeded.
            if !opts.dry_run {
                let parent_ok = entry
                    .operations
                    .iter()
                    .all(|o| o.status == OperationStatus::Completed);
                if parent_ok {
                    for dep in &s.dependent_operations {
                        entry.operations.push(repair_symlink(dep, opts.dry_run));
                    }
                } else {
                    for dep in &s.dependent_operations {
                        entry.operations.push(OperationOutcome {
                            operation: OperationKind::RepairSymlink,
                            source: Some(dep.link_path.clone()),
                            destination: Some(dep.replacement_target.clone()),
                            status: OperationStatus::Skipped,
                            detail: Some("parent operation did not complete".into()),
                        });
                    }
                }
            } else {
                for dep in &s.dependent_operations {
                    entry.operations.push(OperationOutcome {
                        operation: OperationKind::RepairSymlink,
                        source: Some(dep.link_path.clone()),
                        destination: Some(dep.replacement_target.clone()),
                        status: OperationStatus::Skipped,
                        detail: Some("dry run: no filesystem changes".into()),
                    });
                }
            }
        }
        Err(e) => {
            entry.operations.push(OperationOutcome {
                operation: match s.suggestion_type {
                    SuggestionType::Move => OperationKind::Move,
                    SuggestionType::Delete => OperationKind::Delete,
                },
                source: Some(s.target.clone()),
                destination: s.new_path.clone(),
                status: OperationStatus::Failed,
                detail: Some(e),
            });
        }
    }
    entry
}

// ---------------------------------------------------------------------------
// Moves
// ---------------------------------------------------------------------------

fn run_move(s: &Suggestion, opts: &ExecuteOptions) -> Result<Vec<OperationOutcome>, String> {
    let src = s.target_path();
    let Some(dst) = s.new_path_path() else {
        return Err("move suggestion missing new_path".into());
    };
    if opts.dry_run {
        return Ok(vec![OperationOutcome {
            operation: OperationKind::Move,
            source: Some(src.to_string_lossy().into_owned()),
            destination: Some(dst.to_string_lossy().into_owned()),
            status: OperationStatus::Completed,
            detail: Some("dry run: would move (preconditions simulated)".into()),
        }]);
    }

    // --- Preflight: the source must still be the analyzed object. ---
    preflight_source(src, s)?;

    // --- Destination must not exist. ---
    if fsops::path_exists(dst) {
        return Err(format!(
            "destination exists; refusing to overwrite: {}",
            dst.display()
        ));
    }

    // Create destination parent directories if needed.
    if let Some(parent) = dst.parent() {
        fsops::create_parent_dirs(parent)?;
    }

    // Compare filesystems of the source and the destination's parent
    // directory (the destination itself does not exist yet).
    let dest_fs_ref = dst.parent().unwrap_or(Path::new("."));
    let same_fs = fsops::same_filesystem(src, dest_fs_ref)
        .map_err(|e| format!("cannot compare filesystems for {}: {e}", dst.display()))?;

    if same_fs {
        fsops::rename_no_replace(src, dst)?;
        // Post-condition sanity: the object must be gone from source.
        if fsops::path_exists(src) {
            return Err(format!(
                "move reported success but source still exists: {}",
                src.display()
            ));
        }
    } else {
        // Cross-filesystem: verified copy-and-delete protocol.
        fsops::cross_fs_move(src, dst)?;
    }

    Ok(vec![OperationOutcome {
        operation: OperationKind::Move,
        source: Some(src.to_string_lossy().into_owned()),
        destination: Some(dst.to_string_lossy().into_owned()),
        status: OperationStatus::Completed,
        detail: None,
    }])
}

// ---------------------------------------------------------------------------
// Deletes
// ---------------------------------------------------------------------------

fn run_delete(s: &Suggestion, opts: &ExecuteOptions) -> Result<Vec<OperationOutcome>, String> {
    let src = s.target_path();
    if opts.dry_run {
        return Ok(vec![OperationOutcome {
            operation: OperationKind::Delete,
            source: Some(src.to_string_lossy().into_owned()),
            destination: None,
            status: OperationStatus::Completed,
            detail: Some("dry run: would delete (preconditions simulated)".into()),
        }]);
    }

    // --- Preflight: identity of the object itself. ---
    preflight_source(src, s)?;

    // A delete removes the file only after the duplicate relationship is
    // re-confirmed: re-hash the full content and compare to the recorded
    // full-content hash captured during analysis.
    let now =
        hash::hash_file_full(src).map_err(|e| format!("cannot hash {}: {e}", src.display()))?;
    match s.provenance.full_hash {
        Some(recorded) if recorded == now => {}
        Some(_) => {
            return Err("content changed since analysis; delete refused (re-analyze)".into());
        }
        None => {
            return Err("no recorded full-content hash; delete refused (re-analyze)".into());
        }
    }

    std::fs::remove_file(src).map_err(|e| format!("cannot delete {}: {e}", src.display()))?;

    Ok(vec![OperationOutcome {
        operation: OperationKind::Delete,
        source: Some(src.to_string_lossy().into_owned()),
        destination: None,
        status: OperationStatus::Completed,
        detail: None,
    }])
}

// ---------------------------------------------------------------------------
// Dependent symlink repairs
// ---------------------------------------------------------------------------

fn repair_symlink(dep: &DependentOperation, dry_run: bool) -> OperationOutcome {
    let link_path = PathBuf::from(&dep.link_path);
    let outcome = |status, detail| OperationOutcome {
        operation: OperationKind::RepairSymlink,
        source: Some(dep.link_path.clone()),
        destination: Some(dep.replacement_target.clone()),
        status,
        detail,
    };

    if dry_run {
        return outcome(
            OperationStatus::Completed,
            Some("dry run: would repair symlink".into()),
        );
    }

    // Re-validate against the scan-time record, immediately before repair.
    match validate_link_before_repair(&link_path, dep) {
        Ok(()) => {}
        Err(e) => return outcome(OperationStatus::Failed, Some(e)),
    }

    match fsops::replace_symlink(
        &link_path,
        &dep.replacement_target,
        dep.recorded_target_relative,
    ) {
        Ok(()) => outcome(OperationStatus::Completed, None),
        Err(e) => outcome(OperationStatus::Failed, Some(e)),
    }
}

/// A symlink is repaired only when its current state matches the scan-time
/// record: the path is still a symlink and its raw target is unchanged. The
/// resolved-identity check happened at plan time, when the recorded
/// resolved target still existed; immediately before repair the old target
/// has usually already moved, so only the link's own state is re-checked.
/// The parent operation's preflight (object identity + content hash for
/// deletes) guarantees the moved or deleted object was the analyzed one.
fn validate_link_before_repair(link_path: &Path, dep: &DependentOperation) -> Result<(), String> {
    let meta = std::fs::symlink_metadata(link_path)
        .map_err(|e| format!("link missing or unreadable: {e}"))?;
    if !meta.file_type().is_symlink() {
        return Err("path is no longer a symlink; repair refused".into());
    }
    let raw = std::fs::read_link(link_path).map_err(|e| format!("cannot read link: {e}"))?;
    if raw.to_string_lossy() != dep.recorded_target {
        return Err("link target changed since analysis; repair refused".into());
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Shared preflight
// ---------------------------------------------------------------------------

/// Re-stat the source immediately before execution and compare identity and
/// size to the analysis-time provenance. On Unix the identity is
/// (st_dev, st_ino). Only mtime drift is tolerated and only when full
/// content identity is confirmed; moves re-verify by re-hash, deletes by
/// design require the recorded full-content hash.
fn preflight_source(src: &Path, s: &Suggestion) -> Result<(), String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let meta = std::fs::symlink_metadata(src)
            .map_err(|e| format!("source missing or unreadable: {e}"))?;
        let p = &s.provenance;
        if !p.identity_available {
            return Err(
                "source identity unavailable at analysis time; re-analysis required".into(),
            );
        }
        if meta.dev() != p.dev || meta.ino() != p.ino {
            return Err("source identity changed since analysis; re-analysis required".into());
        }
        if meta.len() != p.size {
            return Err("source size changed since analysis; re-analysis required".into());
        }
        // If only mtime moved and full content hash is available, allow the
        // operation; delete re-hashes anyway, and move verification re-hashes
        // full content on cross-filesystem paths.
        if let Some(recorded) = &p.full_hash {
            if meta.len() == p.size {
                let now = hash::hash_file_full(src)
                    .map_err(|e| format!("cannot hash {}: {e}", src.display()))?;
                if now != *recorded {
                    return Err(
                        "source content changed since analysis; re-analysis required".into(),
                    );
                }
            }
        }
        Ok(())
    }
    #[cfg(not(unix))]
    {
        // Degraded-identity platforms re-analyze rather than execute.
        Err("no native object identity on this platform; re-analysis required".into())
    }
}

/// Convenience for callers that already validated suggestions: mark all
/// executed-suggestion outcomes onto the session model.
pub fn status_from_report(items: &mut [Suggestion], report: &ExecutionReport) {
    let by_id: std::collections::HashMap<&str, &ReportEntry> = report
        .entries
        .iter()
        .map(|e| (e.suggestion_id.as_str(), e))
        .collect();
    for s in items.iter_mut() {
        if let Some(entry) = by_id.get(s.id.as_str()) {
            s.status = match entry.result {
                ExecutionStatus::Executed => SuggestionStatus::Executed,
                ExecutionStatus::Failed => SuggestionStatus::Failed,
                ExecutionStatus::Skipped => SuggestionStatus::Skipped,
            };
        }
    }
}
