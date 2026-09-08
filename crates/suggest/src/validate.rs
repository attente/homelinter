//! Batch validation of a set of suggestions before execution.
//!
//! Validation is deterministic and idempotent over the provided slice. It
//! detects destination collisions (two moves to the same path), moves that
//! would write into a planned delete or another move's target, deletes of
//! objects another suggestion depends on, and objects whose identity could
//! not be captured at scan time. Rejected suggestions are marked
//! `Rejected` with a machine-readable reason; execution skips them without
//! failing the batch.
//!
//! Suggestions whose provider metadata is still in flight
//! (`metadata_pending`) may be excluded with `skip_pending`: their
//! destinations are built on fallback metadata and a collision may
//! disappear once the provider result arrives, so they are deferred and
//! re-validated against real metadata when their last request resolves.
//! Excluded suggestions neither receive rejections nor participate in
//! collision detection.

use std::collections::HashMap;

use homelinter_model::suggestion::{Suggestion, SuggestionStatus, SuggestionType};

/// Validate every suggestion in `items`, marking rejections in place.
/// When `skip_pending` is true, suggestions with `metadata_pending` set are
/// left entirely untouched (not evaluated, not participating in collision
/// detection). Returns the rejected suggestion ids. Callers may pass a
/// subset (selection) to re-validate; rules apply only within the given
/// set, so selection narrowing is monotonic: a subset never re-admits a
/// full-batch rejection.
pub fn validate_batch(items: &mut [Suggestion], skip_pending: bool) -> Vec<String> {
    // Precompute sort keys so later indexing never borrows `items` mutably
    // while a suggestion is being inspected.
    let sort_keys: Vec<String> = items.iter().map(|s| s.sort_key()).collect();
    let mut dest_claims: HashMap<String, usize> = HashMap::new();
    let mut delete_targets: HashMap<String, usize> = HashMap::new();
    let mut source_keys: HashMap<String, usize> = HashMap::new();

    for (i, s) in items.iter().enumerate() {
        if skip_pending && s.metadata_pending {
            continue;
        }
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
        if skip_pending && s.metadata_pending {
            continue;
        }
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

#[cfg(test)]
mod tests {
    use super::*;
    use homelinter_model::asset_kind::AssetKind;
    use std::path::Path;

    fn move_suggestion(target: &str, new_path: &str) -> Suggestion {
        let provenance = homelinter_model::suggestion::Provenance {
            identity_available: true,
            ..Default::default()
        };
        Suggestion::new_move(
            AssetKind::Photo,
            Path::new(target),
            Path::new(new_path),
            "organize".into(),
            Default::default(),
            Vec::new(),
            provenance,
        )
    }

    #[test]
    fn skip_pending_defers_colliding_suggestions() {
        // Two moves to the same destination: without skip_pending one is
        // rejected; with skip_pending a pending one is deferred entirely
        // and does not cause its (non-pending) sibling to be rejected
        // either, since its provisional destination may change.
        let mut items = vec![
            move_suggestion("/a/one.jpg", "/d/unknown.jpg"),
            move_suggestion("/a/two.jpg", "/d/unknown.jpg"),
        ];
        items[0].metadata_pending = true;
        let rejected = validate_batch(&mut items, true);
        assert!(rejected.is_empty());
        assert!(items.iter().all(|s| s.status == SuggestionStatus::Pending));

        // Without skip_pending the pending suggestion still collides and is
        // rejected like any other.
        let mut items = vec![
            move_suggestion("/a/one.jpg", "/d/unknown.jpg"),
            move_suggestion("/a/two.jpg", "/d/unknown.jpg"),
        ];
        items[0].metadata_pending = true;
        let rejected = validate_batch(&mut items, false);
        assert_eq!(rejected.len(), 1);
        assert!(items
            .iter()
            .filter(|s| s.status == SuggestionStatus::Rejected)
            .all(|s| s.rejection_reason.is_some()));
    }
}
