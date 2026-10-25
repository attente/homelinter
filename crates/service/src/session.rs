//! The single in-memory session owned by a service instance: state machine,
//! suggestion batch, selection, execution report retention, and failure
//! record. Session state is never persisted across restarts.

use std::path::{Path, PathBuf};

use homelinter_model::config::RawConfig;
use homelinter_model::suggestion::{
    ExecutionReport, ScanSummary, SessionFailure, SessionState, Suggestion, SuggestionStatus,
};
use homelinter_suggest::validate_batch;
use serde_json::Value as JsonValue;

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
    /// RFC 3339 timestamp of when the current scan was accepted.
    pub scan_started_at: Option<String>,
    /// Home directory used for the symlink index and relative destinations.
    pub home: PathBuf,
    pub platform: &'static str,
    /// Effective current settings: the config-file chain folded with
    /// launch-time CLI overrides and accumulated runtime writes. Settings
    /// affect future scans only; a running scan keeps its snapshot.
    pub config: RawConfig,
    pub config_path: Option<PathBuf>,
    /// Partial config documents written through `PUT /config` during this
    /// process, accumulated so a runtime write wins over a launch-time CLI
    /// override for the same key. Persisted to the config file as well.
    pub runtime_overrides: JsonValue,
    /// Effective config hash, for change detection in `GET /session`.
    pub config_hash: String,
    /// Hash of the config snapshot the current suggestion batch was scanned
    /// under; `None` until a scan publishes its batch. Clients compare it
    /// against `config_hash` to flag a batch produced under an older
    /// configuration (see "Session Model" in DESIGN.md).
    pub batch_config_hash: Option<String>,
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
            scan_started_at: None,
            home,
            platform: homelinter_model::config::platform_name(),
            config,
            config_path,
            runtime_overrides: JsonValue::Object(Default::default()),
            config_hash,
            batch_config_hash: None,
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
    /// A done suggestion has nothing left to run, and a rejected suggestion
    /// cannot run or be exported, so neither is ever selectable: every
    /// selection path skips them, and clients clear and disable their
    /// checkbox.
    fn selectable(&self, i: usize) -> bool {
        !matches!(
            self.suggestions[i].status,
            SuggestionStatus::Executed | SuggestionStatus::Rejected
        )
    }

    pub fn set_selection(&mut self, ids: &[String]) {
        for s in self.suggestions.iter_mut() {
            s.selected = false;
        }
        for id in ids {
            if let Some(i) = self.suggestion_index(id) {
                if self.selectable(i) {
                    self.suggestions[i].selected = true;
                }
            }
        }
    }

    pub fn add_to_selection(&mut self, ids: &[String]) {
        for id in ids {
            if let Some(i) = self.suggestion_index(id) {
                if self.selectable(i) {
                    self.suggestions[i].selected = true;
                }
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
    /// marked rejected stays rejected. Still-pending suggestions are
    /// deferred (their destinations are provisional); they are re-validated
    /// when their provider requests resolve.
    pub fn validate_ids(&mut self, ids: &[String]) -> Vec<(String, String)> {
        let mut subset: Vec<Suggestion> = ids
            .iter()
            .filter_map(|id| self.suggestion_by_id(id).cloned())
            .collect();
        validate_batch(&mut subset, true);
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
                // A suggestion that becomes rejected leaves the selection:
                // rejected entries are never selectable.
                self.suggestions[i].selected = false;
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

    /// Number of move suggestions whose destination metadata currently
    /// includes at least one manual override. Delete suggestions never carry
    /// editable metadata and are not counted.
    pub fn manual_overrides_total(&self) -> usize {
        self.suggestions
            .iter()
            .filter(|s| !s.manual_overrides.is_empty())
            .count()
    }

    /// Progress of outstanding provider metadata requests as
    /// `(completed, total)`: the number of suggestions that originally had
    /// provider requests and have now had every request resolve, over the
    /// number that ever had any (a re-request does not change the total).
    /// `(0, 0)` when the current batch has no provider-dependent
    /// suggestions.
    pub fn metadata_progress(&self) -> (usize, usize) {
        match &self.provider_ctx {
            Some(ctx) => (
                ctx.total_units
                    .len()
                    .saturating_sub(ctx.pending_units.len()),
                ctx.total_units.len(),
            ),
            None => (0, 0),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use homelinter_model::asset_kind::AssetKind;
    use homelinter_model::suggestion::Provenance;

    fn move_suggestion(target: &str, new_path: &str, selected: bool) -> Suggestion {
        let mut s = Suggestion::new_move(
            AssetKind::Photo,
            Path::new(target),
            Path::new(new_path),
            "organize".into(),
            Default::default(),
            Vec::new(),
            Provenance {
                identity_available: true,
                ..Default::default()
            },
        );
        s.selected = selected;
        s
    }

    fn session_with(suggestions: Vec<Suggestion>) -> Session {
        let mut sess = Session::new(PathBuf::from("/home/alice"), RawConfig::default(), None);
        sess.suggestions = suggestions;
        sess
    }

    #[test]
    fn rejected_suggestions_are_not_selectable() {
        let mut sess = session_with(vec![
            move_suggestion("/home/alice/a.jpg", "/home/alice/dest.jpg", true),
            move_suggestion("/home/alice/b.jpg", "/home/alice/dest.jpg", false),
        ]);
        let ids: Vec<String> = sess.suggestions.iter().map(|s| s.id.clone()).collect();

        // Re-validation rejects the collision set and drops the selected
        // member from the selection.
        let rejected = sess.validate_ids(&ids);
        assert_eq!(rejected.len(), 2);
        assert!(sess.selection_ids().is_empty());

        // Every selection path skips rejected suggestions.
        sess.add_to_selection(&ids);
        assert!(sess.selection_ids().is_empty());
        sess.set_selection(&ids);
        assert!(sess.selection_ids().is_empty());
    }
}
