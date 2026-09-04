//! Batch validation of a set of suggestions before execution.
//!
//! Validation is deterministic and idempotent over the provided slice. It
//! detects destination collisions (two moves to the same path), moves that
//! would write into a planned delete or another move's target, deletes of
//! objects another suggestion depends on, and objects whose identity could
//! not be captured at scan time. Rejected suggestions are marked
//! `Rejected` with a machine-readable reason; execution skips them without
//! failing the batch.

use std::collections::HashMap;

use homelinter_model::suggestion::{Suggestion, SuggestionStatus, SuggestionType};

/// Validate every suggestion in `items`, marking rejections in place.
/// Returns the rejected suggestion ids. Callers may pass a subset
/// (selection) to re-validate; rules apply only within the given set, so
/// selection narrowing is monotonic: a subset never re-admits a
/// full-batch rejection.
pub fn validate_batch(items: &mut [Suggestion]) -> Vec<String> {
    // Precompute sort keys so later indexing never borrows `items` mutably
    // while a suggestion is being inspected.
    let sort_keys: Vec<String> = items.iter().map(|s| s.sort_key()).collect();
    let mut dest_claims: HashMap<String, usize> = HashMap::new();
    let mut delete_targets: HashMap<String, usize> = HashMap::new();
    let mut source_keys: HashMap<String, usize> = HashMap::new();

    for (i, s) in items.iter().enumerate() {
        match s.suggestion_type {
            SuggestionType::Move => {
                if let Some(new_path) = &s.new_path {
                    dest_claims.insert(new_path.clone(), i);
                }
            }
            SuggestionType::Delete => {
                delete_targets.insert(s.target.clone(), i);
            }
        }
        source_keys.insert(s.target.clone(), i);
    }

    let mut rejected: Vec<String> = Vec::new();
    for (i, s) in items.iter_mut().enumerate() {
        let mut reasons: Vec<String> = Vec::new();
        if !s.provenance.identity_available {
            reasons.push("identity_unavailable".to_string());
        }
        match s.suggestion_type {
            SuggestionType::Move => {
                let Some(new_path) = &s.new_path else {
                    continue;
                };
                let dest = new_path.clone();
                if s.target == dest {
                    reasons.push("noop_move".to_string());
                }
                if let Some(&j) = dest_claims.get(&dest).filter(|&&j| j != i) {
                    reasons.push(format!("destination_collision:also={}", sort_keys[j]));
                }
                if delete_targets.contains_key(&dest) {
                    reasons.push("destination_is_planned_delete".to_string());
                }
                if let Some(&j) = source_keys.get(&dest).filter(|&&j| j != i) {
                    reasons.push(format!(
                        "destination_is_planned_source:also={}",
                        sort_keys[j]
                    ));
                }
            }
            SuggestionType::Delete => {
                let t = s.target.clone();
                if let Some(&j) = source_keys.get(&t).filter(|&&j| j != i) {
                    reasons.push(format!("delete_of_planned_source:also={}", sort_keys[j]));
                }
            }
        }
        if !reasons.is_empty() {
            s.status = SuggestionStatus::Rejected;
            s.rejection_reason = Some(reasons.join("; "));
            rejected.push(s.id.clone());
        }
    }
    rejected
}
