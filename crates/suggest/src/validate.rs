//! Batch validation of a set of suggestions before execution.
//!
//! Validation is deterministic and idempotent over the provided slice. It
//! detects destination collisions (every suggestion in a collision set is
//! rejected when multiple moves share a destination), moves that would
//! write into a planned delete or another move's target, deletes of
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
use std::path::Path;

use homelinter_model::path;
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
    // Collision maps are keyed by `path::sort_key`, which applies the host
    // *platform's* collision comparison (case-folded on a Windows build,
    // exact on a Unix build). Per "Platform Scope" this follows the host
    // platform's semantics rather than probing the target filesystem's case
    // sensitivity, so a Unix host scanning a case-insensitive mount (NTFS,
    // exFAT, SMB) does not fold case here.
    let mut dest_claims: HashMap<String, Vec<usize>> = HashMap::new();
    let mut delete_targets: HashMap<String, usize> = HashMap::new();
    let mut source_keys: HashMap<String, usize> = HashMap::new();

    for (i, s) in items.iter().enumerate() {
        if skip_pending && s.metadata_pending {
            continue;
        }
        match s.suggestion_type {
            SuggestionType::Move => {
                if let Some(new_path) = &s.new_path {
                    dest_claims
                        .entry(path::sort_key(Path::new(new_path)))
                        .or_default()
                        .push(i);
                }
            }
            SuggestionType::Delete => {
                delete_targets.insert(path::sort_key(Path::new(&s.target)), i);
            }
        }
        source_keys.insert(path::sort_key(Path::new(&s.target)), i);
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
                let dest_key = path::sort_key(Path::new(&dest));
                if s.target == dest {
                    reasons.push("noop_move".to_string());
                }
                let name = std::path::Path::new(&dest)
                    .file_name()
                    .and_then(|n| n.to_str());
                match name {
                    Some(n) if homelinter_model::format::validate_file_name_segment(n) => {}
                    _ => reasons.push("invalid_destination_segment".to_string()),
                }
                // Every suggestion in a collision set is rejected: no
                // arbitrary survivor is picked, so the user must resolve
                // the conflict. The reason names every partner via its
                // source sort key.
                if let Some(others) = dest_claims.get(&dest_key) {
                    let partners: Vec<&str> = others
                        .iter()
                        .filter(|&&j| j != i)
                        .map(|&j| sort_keys[j].as_str())
                        .collect();
                    if !partners.is_empty() {
                        reasons.push(format!(
                            "destination_collision:also={}",
                            partners.join(",")
                        ));
                    }
                }
                if delete_targets.contains_key(&dest_key) {
                    reasons.push("destination_is_planned_delete".to_string());
                }
                if let Some(&j) = source_keys.get(&dest_key).filter(|&&j| j != i) {
                    reasons.push(format!(
                        "destination_is_planned_source:also={}",
                        sort_keys[j]
                    ));
                }
            }
            SuggestionType::Delete => {
                let t = s.target.clone();
                let t_key = path::sort_key(Path::new(&t));
                if let Some(&j) = source_keys.get(&t_key).filter(|&&j| j != i) {
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
        // Two moves to the same destination: with skip_pending the pending
        // one is deferred entirely and does not cause its (non-pending)
        // sibling to be rejected either, since its provisional destination
        // may change. Without skip_pending every suggestion in the
        // collision set is rejected.
        let mut items = vec![
            move_suggestion("/a/one.jpg", "/d/unknown.jpg"),
            move_suggestion("/a/two.jpg", "/d/unknown.jpg"),
        ];
        items[0].metadata_pending = true;
        let rejected = validate_batch(&mut items, true);
        assert!(rejected.is_empty());
        assert!(items.iter().all(|s| s.status == SuggestionStatus::Pending));

        // Without skip_pending the pending suggestion collides like any
        // other, and both members of the collision set are rejected.
        let mut items = vec![
            move_suggestion("/a/one.jpg", "/d/unknown.jpg"),
            move_suggestion("/a/two.jpg", "/d/unknown.jpg"),
        ];
        items[0].metadata_pending = true;
        let rejected = validate_batch(&mut items, false);
        assert_eq!(rejected.len(), 2);
        assert!(items
            .iter()
            .filter(|s| s.status == SuggestionStatus::Rejected)
            .all(|s| s.rejection_reason.is_some()));
    }

    #[test]
    fn rejects_every_suggestion_in_a_collision_set() {
        // Three moves to one destination: every member is rejected and its
        // reason names the two partners (never itself) by source sort key.
        let mut items = vec![
            move_suggestion("/a/one.jpg", "/d/unknown.jpg"),
            move_suggestion("/b/two.jpg", "/d/unknown.jpg"),
            move_suggestion("/c/three.jpg", "/d/unknown.jpg"),
        ];
        let rejected = validate_batch(&mut items, false);
        assert_eq!(rejected.len(), 3);
        assert!(items.iter().all(|s| {
            s.rejection_reason
                .as_deref()
                .unwrap_or("")
                .contains("destination_collision:also=")
        }));
        let reason = items[0].rejection_reason.as_deref().unwrap();
        assert!(reason.contains("/b/two.jpg"));
        assert!(reason.contains("/c/three.jpg"));
        assert!(!reason.contains("/a/one.jpg"));
    }

    // Compile-time gated: only the branch for the build target runs, so
    // this is exercised only by a Windows test run.
    #[cfg(windows)]
    #[test]
    fn rejects_case_folded_destination_collision() {
        // On the Windows host platform, destinations differing only by case
        // collide and every member of the set is rejected.
        let mut items = vec![
            move_suggestion("/a/one.jpg", "/d/Photo.jpg"),
            move_suggestion("/a/two.jpg", "/d/photo.jpg"),
        ];
        let rejected = validate_batch(&mut items, false);
        assert_eq!(rejected.len(), 2);
        assert!(items.iter().all(|s| s
            .rejection_reason
            .as_deref()
            .unwrap_or("")
            .contains("destination_collision")));
    }

    // Compile-time gated: only the branch for the build target runs, so
    // this is exercised only by a Unix test run.
    #[cfg(unix)]
    #[test]
    fn distinct_case_destinations_do_not_collide_on_unix() {
        // The Unix host platform is case-sensitive: differing case is a
        // distinct destination, so neither suggestion is rejected.
        let mut items = vec![
            move_suggestion("/a/one.jpg", "/d/Photo.jpg"),
            move_suggestion("/a/two.jpg", "/d/photo.jpg"),
        ];
        let rejected = validate_batch(&mut items, false);
        assert!(rejected.is_empty());
    }

    #[test]
    fn rejects_destination_with_invalid_final_segment() {
        // A control character in the resolved filename segment is rejected
        // before execution rather than passed through to a rename.
        let mut items = vec![move_suggestion("/a/x.jpg", "/d/bad\nname.jpg")];
        let rejected = validate_batch(&mut items, false);
        assert_eq!(rejected.len(), 1);
        assert!(items[0]
            .rejection_reason
            .as_deref()
            .unwrap()
            .contains("invalid_destination_segment"));
    }
}
