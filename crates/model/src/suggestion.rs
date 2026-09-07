//! The suggestion model: suggestions, dependent operations, execution
//! reports, and session state.

use std::collections::BTreeMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::asset_kind::AssetKind;
use crate::path;

/// One top-level proposed filesystem change.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Suggestion {
    /// `s-<sha256 of the normalized absolute target>`.
    pub id: String,
    #[serde(rename = "type")]
    pub suggestion_type: SuggestionType,
    pub asset_kind: AssetKind,
    pub target: String,
    /// Present only for `move`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub new_path: Option<String>,
    pub reason: String,
    #[serde(skip_serializing_if = "BTreeMap::is_empty", default)]
    pub metadata: BTreeMap<String, serde_json::Value>,
    pub dependent_operations: Vec<DependentOperation>,
    pub selected: bool,
    pub status: SuggestionStatus,
    /// True while one or more provider requests (geocoder/description)
    /// that this suggestion depends on are still in flight. Such
    /// suggestions are visible but not executable until the requests
    /// resolve.
    #[serde(default)]
    pub metadata_pending: bool,
    /// Machine-readable reason when `status == rejected` (batch validation).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rejection_reason: Option<String>,
    /// Internal execution facts (identity, hashes) never serialized to
    /// clients but required for immediate pre-execution validation.
    #[serde(skip)]
    pub provenance: Provenance,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SuggestionType {
    Move,
    Delete,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SuggestionStatus {
    Pending,
    Rejected,
    Executed,
    Failed,
    Skipped,
}

/// Facts recorded at analysis time and re-verified immediately before
/// execution. Never serialized to API clients.
#[derive(Debug, Clone, Default)]
pub struct Provenance {
    /// Recorded full-content SHA-256 for delete targets (duplicate
    /// confirmation) and for move verification bookkeeping.
    pub full_hash: Option<[u8; 32]>,
    pub size: u64,
    pub mtime_ns: i128,
    /// Unix device + inode identity, when the platform provides one.
    #[cfg(unix)]
    pub dev: u64,
    #[cfg(unix)]
    pub ino: u64,
    /// Degraded identity fallback (path + metadata signature) is implicit:
    /// when no identity fields are set the executor re-analyzes rather than
    /// executing, per the design.
    #[cfg(unix)]
    pub identity_available: bool,
    #[cfg(not(unix))]
    pub identity_available: bool,
    /// The resolved absolute target of the link (for repair validation).
    pub link_resolved_target: Option<String>,
    /// Whether the recorded link target was relative or absolute.
    pub link_raw_relative: Option<bool>,
}

impl Provenance {
    #[cfg(unix)]
    pub fn unix_identity(dev: u64, ino: u64) -> Provenance {
        Provenance {
            dev,
            ino,
            identity_available: dev != 0 && ino != 0,
            ..Default::default()
        }
    }
}

/// A dependent filesystem operation attached to a parent suggestion.
/// Dependent operations are mandatory members of the parent execution unit,
/// are never independently selectable, and run only after the parent
/// operation succeeds.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DependentOperation {
    #[serde(rename = "type")]
    pub op_type: DependentOpType,
    pub link_path: String,
    /// The raw symlink target recorded during analysis.
    pub recorded_target: String,
    /// The target to write after the parent operation.
    pub replacement_target: String,
    /// Whether the recorded target was relative (repair must preserve the
    /// relative form, recalculated from the link's parent directory).
    pub recorded_target_relative: bool,
    /// Scan-time resolved identity facts for pre-repair validation.
    #[serde(skip)]
    pub provenance: Provenance,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DependentOpType {
    RepairSymlink,
}

impl Suggestion {
    /// Build the stable suggestion id from a normalized absolute target.
    pub fn id_for_target(target: &Path) -> String {
        use sha2::{Digest, Sha256};
        let mut hasher = Sha256::new();
        hasher.update(target.as_os_str().as_encoded_bytes());
        format!("s-{}", hex::encode(hasher.finalize()))
    }

    pub fn new_move(
        kind: AssetKind,
        target: &Path,
        new_path: &Path,
        reason: String,
        metadata: BTreeMap<String, serde_json::Value>,
        dependents: Vec<DependentOperation>,
        provenance: Provenance,
    ) -> Suggestion {
        Suggestion {
            id: Self::id_for_target(target),
            suggestion_type: SuggestionType::Move,
            asset_kind: kind,
            target: target.to_string_lossy().into_owned(),
            new_path: Some(new_path.to_string_lossy().into_owned()),
            reason,
            metadata,
            dependent_operations: dependents,
            selected: false,
            status: SuggestionStatus::Pending,
            metadata_pending: false,
            rejection_reason: None,
            provenance,
        }
    }

    pub fn new_delete(
        kind: AssetKind,
        target: &Path,
        reason: String,
        metadata: BTreeMap<String, serde_json::Value>,
        dependents: Vec<DependentOperation>,
        provenance: Provenance,
    ) -> Suggestion {
        Suggestion {
            id: Self::id_for_target(target),
            suggestion_type: SuggestionType::Delete,
            asset_kind: kind,
            target: target.to_string_lossy().into_owned(),
            new_path: None,
            reason,
            metadata,
            dependent_operations: dependents,
            selected: false,
            status: SuggestionStatus::Pending,
            metadata_pending: false,
            rejection_reason: None,
            provenance,
        }
    }

    pub fn target_path(&self) -> &Path {
        Path::new(&self.target)
    }

    pub fn new_path_path(&self) -> Option<&Path> {
        self.new_path.as_deref().map(Path::new)
    }

    pub fn is_move(&self) -> bool {
        self.suggestion_type == SuggestionType::Move
    }

    /// Deterministic ordering for display and batch validation: normalized
    /// absolute source path.
    pub fn sort_key(&self) -> String {
        path::sort_key(self.target_path())
    }
}

/// One entry in the execution report, one per top-level suggestion.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReportEntry {
    pub suggestion_id: String,
    pub result: ExecutionStatus,
    /// Per-operation outcomes including dependent repair steps.
    pub operations: Vec<OperationOutcome>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ExecutionStatus {
    Executed,
    Failed,
    Skipped,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OperationOutcome {
    pub operation: OperationKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub destination: Option<String>,
    pub status: OperationStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OperationKind {
    Move,
    Delete,
    RepairSymlink,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OperationStatus {
    Completed,
    Failed,
    Skipped,
}

/// The report for the most recent execution.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecutionReport {
    pub entries: Vec<ReportEntry>,
    /// True when a parent succeeded but a dependent repair failed.
    pub partial: bool,
    pub scan_root: String,
    pub platform: String,
}

impl ExecutionReport {
    pub fn empty(scan_root: &Path) -> ExecutionReport {
        ExecutionReport {
            entries: Vec::new(),
            partial: false,
            scan_root: scan_root.to_string_lossy().into_owned(),
            platform: crate::config::platform_name().to_string(),
        }
    }
}

/// Summary of a completed scan, exposed by `GET /report/summary` and the
/// CLI machine-readable document.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ScanSummary {
    pub total_files_scanned: u64,
    pub total_symlinks_indexed: u64,
    /// Suggestions by type: move/delete counts.
    pub suggestions_by_type: BTreeMap<String, u64>,
    /// Suggestions by asset kind.
    pub suggestions_by_kind: BTreeMap<String, u64>,
    /// Estimated bytes reclaimed by deduplication (delete suggestions).
    pub estimated_space_reclaimed: u64,
}

/// Session state machine states.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SessionState {
    Idle,
    Scanning,
    Analyzing,
    Ready,
    Executing,
    Done,
    Failed,
}

impl SessionState {
    pub fn in_progress(self) -> bool {
        matches!(
            self,
            SessionState::Scanning | SessionState::Analyzing | SessionState::Executing
        )
    }
}

/// Machine-code of a failure or cancellation reason attached to `failed`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionFailure {
    pub reason: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cancelled: Option<bool>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn stable_ids_from_targets() {
        let a = Path::new("/home/alice/x.jpg");
        let b = Path::new("/home/alice/x.jpg");
        let c = Path::new("/home/alice/y.jpg");
        assert_eq!(Suggestion::id_for_target(a), Suggestion::id_for_target(b));
        assert_ne!(Suggestion::id_for_target(a), Suggestion::id_for_target(c));
        assert!(Suggestion::id_for_target(a).starts_with("s-"));
    }
}
