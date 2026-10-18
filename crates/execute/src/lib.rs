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
//!   the hash and confirms that the recorded keeper still exists with that
//!   same content immediately before deletion, following a keeper that was
//!   already moved earlier in the same batch.
//! - Dependent symlink repairs run only after their parent succeeds, never
//!   follow links, and are re-validated immediately before repair.
//!
//! There is no rollback: failures are reported precisely, and a partially
//! completed unit is reported as `partial` without claiming cleanup that
//! did not happen.

mod fsops;
mod script;

pub use script::{export_host, export_script};

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use homelinter_model::hash;
use homelinter_model::suggestion::{
    DependentOperation, ExecutionReport, ExecutionStatus, OperationKind, OperationOutcome,
    OperationStatus, ReportEntry, Suggestion, SuggestionStatus, SuggestionType,
};

/// Execute the given suggestions (already batch-validated and sorted by
/// sort key). `selected_only` restricts execution to `selected == true`.
pub fn execute_batch(
    items: &mut [Suggestion],
    scan_root: &Path,
    selected_only: bool,
) -> ExecutionReport {
    let mut report = ExecutionReport::empty(scan_root);

    // Deterministic order: the caller sorts, but a defensive re-sort keeps
    // validation and dependency order identical to generation.
    items.sort_by_key(|a| a.sort_key());

    // Keeper relocations from moves that already completed in this batch, so
    // a duplicate delete can still find its keeper after the keeper moved.
    let mut relocated: BTreeMap<PathBuf, PathBuf> = BTreeMap::new();

    for s in items.iter_mut() {
        if selected_only && !s.selected {
            continue;
        }
        if s.status != SuggestionStatus::Pending {
            continue;
        }
        let mut entry = execute_one(s, &relocated);
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
        if s.suggestion_type == SuggestionType::Move && s.status == SuggestionStatus::Executed {
            if let Some(to) = s.new_path_path() {
                relocated.insert(s.target_path().to_path_buf(), to.to_path_buf());
            }
        }
        report.entries.push(entry);
    }
    report
}

fn execute_one(s: &mut Suggestion, relocated: &BTreeMap<PathBuf, PathBuf>) -> ReportEntry {
    let operations: Vec<OperationOutcome> = Vec::new();
    let mut entry = ReportEntry {
        suggestion_id: s.id.clone(),
        result: ExecutionStatus::Skipped,
        operations,
    };

    let parent = match s.suggestion_type {
        SuggestionType::Move => run_move(s),
        SuggestionType::Delete => run_delete(s, relocated),
    };

    match parent {
        Ok(mut ops) => {
            entry.operations.append(&mut ops);
            // Dependents run only when the parent succeeded.

            let parent_ok = entry
                .operations
                .iter()
                .all(|o| o.status == OperationStatus::Completed);
            if parent_ok {
                for dep in &s.dependent_operations {
                    entry.operations.push(repair_symlink(dep));
                }
            } else {
                entry
                    .operations
                    .extend(skipped_repairs(s, "parent operation did not complete"));
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
                detail: Some(e.clone()),
            });
            // A failed parent leaves every dependent repair undone. Report
            // them as skipped so the client sees the whole plan's outcome
            // rather than silently losing the repairs.
            entry
                .operations
                .extend(skipped_repairs(s, &format!("parent operation failed: {e}")));
        }
    }
    entry
}

/// Dependent repairs that are not attempted, reported as skipped with the
/// reason so the whole plan's outcome stays visible to clients.
fn skipped_repairs(s: &Suggestion, detail: &str) -> Vec<OperationOutcome> {
    s.dependent_operations
        .iter()
        .map(|dep| OperationOutcome {
            operation: OperationKind::RepairSymlink,
            source: Some(dep.link_path.clone()),
            destination: Some(dep.replacement_target.clone()),
            status: OperationStatus::Skipped,
            detail: Some(detail.into()),
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Moves
// ---------------------------------------------------------------------------

fn run_move(s: &Suggestion) -> Result<Vec<OperationOutcome>, String> {
    let src = s.target_path();
    let Some(dst) = s.new_path_path() else {
        return Err("move suggestion missing new_path".into());
    };

    // --- Preflight: the source must still be the analyzed object. ---
    preflight_source(src, s, ContentCheck::Optional)?;

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

fn run_delete(
    s: &Suggestion,
    relocated: &BTreeMap<PathBuf, PathBuf>,
) -> Result<Vec<OperationOutcome>, String> {
    let src = s.target_path();

    // --- Preflight: identity of the object itself, plus the re-confirmed
    // duplicate relationship. A delete removes the file only after its full
    // content is re-hashed and matched against the recorded full-content
    // hash captured during analysis; this is the only hash pass, so the
    // preflight does it and no second pass follows. ---
    preflight_source(src, s, ContentCheck::Required)?;

    // --- Confirm the keeper: the duplicate relationship is only valid while
    // the recorded original still exists with the duplicate's content. A
    // keeper that already moved in this batch is looked up at its new path;
    // a keeper that vanished or changed refuses the delete. ---
    confirm_keeper(s, relocated)?;

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

fn repair_symlink(dep: &DependentOperation) -> OperationOutcome {
    let link_path = PathBuf::from(&dep.link_path);
    let outcome = |status, detail| OperationOutcome {
        operation: OperationKind::RepairSymlink,
        source: Some(dep.link_path.clone()),
        destination: Some(dep.replacement_target.clone()),
        status,
        detail,
    };

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
// Keeper confirmation
// ---------------------------------------------------------------------------

/// Confirm that the keeper recorded for a duplicate delete still holds the
/// same full content. The delete's provenance hash is the group's verified
/// content hash, so a keeper with that exact hash still justifies removing
/// the redundant copy. Anything else (missing record, missing keeper, changed
/// keeper, keeper equal to the target) refuses the delete and asks for
/// re-analysis rather than risking the only remaining copy.
fn confirm_keeper(s: &Suggestion, relocated: &BTreeMap<PathBuf, PathBuf>) -> Result<(), String> {
    let recorded = s
        .metadata
        .get("duplicate_of")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "delete has no recorded keeper; re-analysis required".to_string())?;
    let recorded = Path::new(recorded);
    if recorded == s.target_path() {
        return Err("recorded keeper is the delete target; re-analysis required".into());
    }
    // A keeper moved earlier in this batch now lives at its new path; that is
    // the only place its content can still be confirmed.
    let keeper = relocated.get(recorded).map_or(recorded, PathBuf::as_path);
    let expected = s
        .provenance
        .full_hash
        .ok_or_else(|| "no recorded full-content hash; delete refused (re-analyze)".to_string())?;
    let meta = std::fs::metadata(keeper).map_err(|e| {
        format!(
            "recorded keeper {} is missing or unreadable: {e}; re-analysis required",
            keeper.display()
        )
    })?;
    if !meta.is_file() {
        return Err(format!(
            "recorded keeper {} is not a regular file; re-analysis required",
            keeper.display()
        ));
    }
    let now = hash::hash_file_full(keeper)
        .map_err(|e| format!("cannot hash keeper {}: {e}", keeper.display()))?;
    if now != expected {
        return Err(format!(
            "recorded keeper {} no longer holds the duplicate content; re-analysis required",
            keeper.display()
        ));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Shared preflight
// ---------------------------------------------------------------------------

/// What the preflight does about full content.
#[derive(Clone, Copy, PartialEq, Eq)]
enum ContentCheck {
    /// Re-hash only when analysis recorded a full-content hash (moves).
    Optional,
    /// A full-content hash is mandatory and must match (duplicate deletes).
    Required,
}

/// Re-stat the source immediately before execution and compare identity and
/// size to the analysis-time provenance. On Unix the identity is
/// (st_dev, st_ino). Only mtime drift is tolerated and only when full
/// content identity is confirmed; moves re-verify by re-hash when one was
/// recorded, deletes always require the recorded full-content hash. This is
/// the single content-hash pass per operation.
fn preflight_source(src: &Path, s: &Suggestion, check: ContentCheck) -> Result<(), String> {
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
        // operation; delete always requires it, and move verification
        // re-hashes full content on cross-filesystem paths.
        match (&p.full_hash, check) {
            (Some(recorded), _) => {
                let now = hash::hash_file_full(src)
                    .map_err(|e| format!("cannot hash {}: {e}", src.display()))?;
                if now != *recorded {
                    return Err(
                        "source content changed since analysis; re-analysis required".into(),
                    );
                }
            }
            (None, ContentCheck::Required) => {
                return Err("no recorded full-content hash; delete refused (re-analyze)".into());
            }
            (None, ContentCheck::Optional) => {}
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
