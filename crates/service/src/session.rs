//! The single in-memory session owned by a service instance: state machine,
//! suggestion batch, selection, execution report retention, and failure
//! record. Session state is never persisted across restarts.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use homelinter_model::config::RawConfig;
use homelinter_model::suggestion::{
    ExecutionReport, ScanSummary, SessionFailure, SessionState, Suggestion, SuggestionStatus,
};
use homelinter_suggest::validate_batch;

/// The identity of a validated execution batch: the normalized sorted ids,
/// used for idempotent re-runs and stored-report lookup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BatchIdentity(pub Vec<String>);

impl BatchIdentity {
    pub fn of(ids: &[String]) -> BatchIdentity {
        let mut v: Vec<String> = ids.to_vec();
        v.sort();
        v.dedup();
        BatchIdentity(v)
    }
}

/// Everything the service tracks for the current session.
pub struct Session {
    pub state: SessionState,
    /// Normalized absolute scan root captured when the scan started.
    pub scan_root: Option<PathBuf>,
    /// Home directory used for the symlink index and relative destinations.
    pub home: PathBuf,
    pub platform: &'static str,
    /// Effective merged configuration (config file chain + session-level
    /// values that are not part of the config schema).
    pub config: RawConfig,
    pub config_path: Option<PathBuf>,
    /// Effective config hash, for change detection in `GET /session`.
    pub config_hash: String,
    /// Per-scan `target_paths` overrides from the scan request.
    pub scan_target_paths: BTreeMap<String, String>,
    pub hidden: bool,
    /// Top-level suggestions in deterministic order.
    pub suggestions: Vec<Suggestion>,
    /// Most recent execution report, retained until the next
    /// scan.
    pub report: Option<ExecutionReport>,
    /// Batch identity of the stored report (idempotency key).
    pub report_batch: Option<BatchIdentity>,
    pub failure: Option<SessionFailure>,
    pub summary: Option<ScanSummary>,
    pub degraded_identity: bool,
    pub warnings: Vec<String>,
    /// Context for refreshing suggestions with late provider results; set
    /// when a scan publishes its batch, replaced by the next scan.
    pub provider_ctx: Option<crate::providers::ProviderScanCtx>,
}

impl Session {
    pub fn new(home: PathBuf, config: RawConfig, config_path: Option<PathBuf>) -> Session {
        let config_hash = homelinter_model::config::config_hash(&config);
        Session {
            state: SessionState::Idle,
            scan_root: None,
            home,
            platform: homelinter_model::config::platform_name(),
            config,
            config_path,
            config_hash,
            scan_target_paths: BTreeMap::new(),
            hidden: false,
            suggestions: Vec::new(),
            report: None,
            report_batch: None,
            failure: None,
            summary: None,
            degraded_identity: false,
            warnings: Vec::new(),
            provider_ctx: None,
        }
    }

    /// The scan root, normalized absolute when a scan has been started.
    pub fn root(&self) -> Option<&Path> {
        self.scan_root.as_deref()
    }

    pub fn suggestion_by_id(&self, id: &str) -> Option<&Suggestion> {
        self.suggestions.iter().find(|s| s.id == id)
    }

    fn suggestion_index(&self, id: &str) -> Option<usize> {
        self.suggestions.iter().position(|s| s.id == id)
    }

    /// Select exactly the given top-level suggestion ids. Dependent
    /// operation ids never appear in the batch and are rejected by the
    /// handler before this is called.
    pub fn set_selection(&mut self, ids: &[String]) {
        for s in self.suggestions.iter_mut() {
            s.selected = false;
        }
        for id in ids {
            if let Some(i) = self.suggestion_index(id) {
                self.suggestions[i].selected = true;
            }
        }
    }

    pub fn add_to_selection(&mut self, ids: &[String]) {
        for id in ids {
            if let Some(i) = self.suggestion_index(id) {
                self.suggestions[i].selected = true;
            }
        }
    }

    pub fn remove_from_selection(&mut self, ids: &[String]) {
        for id in ids {
            if let Some(i) = self.suggestion_index(id) {
                self.suggestions[i].selected = false;
            }
        }
    }

    pub fn selection_ids(&self) -> Vec<String> {
        self.suggestions
            .iter()
            .filter(|s| s.selected)
            .map(|s| s.id.clone())
            .collect()
    }

    /// Re-validate the given ids (typically the selection) and return the
    /// rejected entries with reasons. Validation is monotonic: a suggestion
    /// marked rejected stays rejected.
    pub fn validate_ids(&mut self, ids: &[String]) -> Vec<(String, String)> {
        let mut subset: Vec<Suggestion> = ids
            .iter()
            .filter_map(|id| self.suggestion_by_id(id).cloned())
            .collect();
        validate_batch(&mut subset);
        let rejected: Vec<(String, String)> = subset
            .iter()
            .filter(|s| s.status == SuggestionStatus::Rejected)
            .filter_map(|s| {
                s.rejection_reason
                    .as_ref()
                    .map(|r| (s.id.clone(), r.clone()))
            })
            .collect();
        for (id, reason) in &rejected {
            if let Some(i) = self.suggestion_index(id) {
                self.suggestions[i].status = SuggestionStatus::Rejected;
                self.suggestions[i].rejection_reason = Some(reason.clone());
            }
        }
        rejected
    }

    /// Suggestion counts by type and kind plus estimated reclaimed space,
    /// from the current batch.
    pub fn summary_of_batch(&self) -> ScanSummary {
        let mut summary = ScanSummary {
            total_symlinks_indexed: 0,
            ..Default::default()
        };
        for s in &self.suggestions {
            let type_name = match s.suggestion_type {
                homelinter_model::suggestion::SuggestionType::Move => "move",
                homelinter_model::suggestion::SuggestionType::Delete => "delete",
            };
            *summary
                .suggestions_by_type
                .entry(type_name.to_string())
                .or_insert(0) += 1;
            *summary
                .suggestions_by_kind
                .entry(s.asset_kind.name().to_string())
                .or_insert(0) += 1;
            if matches!(
                s.suggestion_type,
                homelinter_model::suggestion::SuggestionType::Delete
            ) {
                if let Some(size) = s.metadata.get("size").and_then(|v| v.as_u64()) {
                    summary.estimated_space_reclaimed += size;
                }
            }
        }
        summary
    }

    /// Executable ids: pending (not rejected / executed / skipped).
    pub fn executable_ids(&self, ids: &[String]) -> Vec<String> {
        ids.iter()
            .filter(|id| {
                self.suggestion_by_id(id)
                    .is_some_and(|s| s.status == SuggestionStatus::Pending && !s.metadata_pending)
            })
            .cloned()
            .collect()
    }
}
