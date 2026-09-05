//! Executor integration tests: real filesystem operations in a temp tree.

use homelinter_execute::{execute_batch, ExecuteOptions};
use homelinter_model::asset_kind::AssetKind;
use homelinter_model::hash;
use homelinter_model::suggestion::{
    DependentOpType, DependentOperation, ExecutionStatus, OperationStatus, Provenance, Suggestion,
    SuggestionStatus, SuggestionType,
};
use std::path::{Path, PathBuf};

fn tmpdir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "hl-exec-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[cfg(unix)]
fn provenance_for(p: &Path) -> Provenance {
    use std::os::unix::fs::MetadataExt;
    let m = std::fs::metadata(p).unwrap();
    let mut prov = Provenance::unix_identity(m.dev(), m.ino());
    prov.size = m.len();
    if let Ok(d) = m
        .modified()
        .and_then(|t| Ok(t.duration_since(std::time::UNIX_EPOCH).unwrap()))
    {
        prov.mtime_ns = d.as_nanos() as i128;
    }
    prov
}

fn write(p: &Path, content: &[u8]) {
    if let Some(parent) = p.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(p, content).unwrap();
}

#[test]
fn move_executes_same_fs() {
    let base = tmpdir("mv");
    let src = base.join("Downloads").join("a.jpg");
    let dst = base.join("Pictures").join("2024").join("a.jpg");
    write(&src, b"photo-bytes");

    let mut s = Suggestion::new_move(
        AssetKind::Photo,
        &src,
        &dst,
        "test".into(),
        Default::default(),
        vec![],
        provenance_for(&src),
    );
    s.selected = true;

    let mut items = vec![s];
    let report = execute_batch(&mut items, &base, true, &ExecuteOptions::default());
    assert_eq!(report.entries.len(), 1);
    assert_eq!(report.entries[0].result, ExecutionStatus::Executed);
    assert!(items[0].status == SuggestionStatus::Executed);
    assert!(!src.exists());
    assert_eq!(std::fs::read(&dst).unwrap(), b"photo-bytes");
    std::fs::remove_dir_all(&base).ok();
}

#[test]
fn move_refuses_existing_destination() {
    let base = tmpdir("mvdeny");
    let src = base.join("a.txt");
    let dst = base.join("b.txt");
    write(&src, b"1");
    write(&dst, b"2"); // occupied

    let mut s = Suggestion::new_move(
        AssetKind::Document,
        &src,
        &dst,
        "test".into(),
        Default::default(),
        vec![],
        provenance_for(&src),
    );
    s.selected = true;
    let mut items = vec![s];
    let report = execute_batch(&mut items, &base, true, &ExecuteOptions::default());
    assert_eq!(report.entries[0].result, ExecutionStatus::Failed);
    let op = &report.entries[0].operations[0];
    assert_eq!(op.status, OperationStatus::Failed);
    assert!(op.detail.as_deref().unwrap().contains("exists"));
    // Nothing changed.
    assert_eq!(std::fs::read(&src).unwrap(), b"1");
    assert_eq!(std::fs::read(&dst).unwrap(), b"2");
    std::fs::remove_dir_all(&base).ok();
}

#[test]
fn delete_verifies_hash_then_removes() {
    let base = tmpdir("del");
    let a = base.join("a.bin");
    let b = base.join("b.bin");
    write(&a, b"same-content");
    write(&b, b"same-content");

    let mut s = Suggestion::new_delete(
        AssetKind::Data,
        &b,
        "duplicate".into(),
        Default::default(),
        vec![],
        provenance_for(&b),
    );
    s.provenance.full_hash = Some(hash::hash_file_full(&b).unwrap());
    s.provenance.size = std::fs::metadata(&b).unwrap().len();
    s.selected = true;

    let mut items = vec![s];
    let report = execute_batch(&mut items, &base, true, &ExecuteOptions::default());
    assert_eq!(report.entries[0].result, ExecutionStatus::Executed);
    assert!(!b.exists());
    assert!(a.exists());
    std::fs::remove_dir_all(&base).ok();
}

#[test]
fn delete_refuses_when_content_changed() {
    let base = tmpdir("delchanged");
    let a = base.join("a.bin");
    let b = base.join("b.bin");
    write(&a, b"same-content");
    write(&b, b"same-content");

    let mut s = Suggestion::new_delete(
        AssetKind::Data,
        &b,
        "duplicate".into(),
        Default::default(),
        vec![],
        provenance_for(&b),
    );
    s.provenance.full_hash = Some(hash::hash_file_full(&b).unwrap());
    s.provenance.size = std::fs::metadata(&b).unwrap().len();
    s.selected = true;

    // Content changes after analysis; identity (dev/ino) is unchanged.
    std::fs::write(&b, b"mutated-content").unwrap();

    let mut items = vec![s];
    let report = execute_batch(&mut items, &base, true, &ExecuteOptions::default());
    assert_eq!(report.entries[0].result, ExecutionStatus::Failed);
    assert!(b.exists(), "changed file must not be deleted");
    std::fs::remove_dir_all(&base).ok();
}

#[test]
fn dry_run_touches_nothing() {
    let base = tmpdir("dry");
    let src = base.join("a.txt");
    let dst = base.join("b.txt");
    write(&src, b"hello");

    let mut s = Suggestion::new_move(
        AssetKind::Document,
        &src,
        &dst,
        "test".into(),
        Default::default(),
        vec![],
        provenance_for(&src),
    );
    s.selected = true;
    let mut items = vec![s];
    let report = execute_batch(&mut items, &base, true, &ExecuteOptions { dry_run: true });
    assert_eq!(report.entries[0].result, ExecutionStatus::Executed);
    assert!(report.dry_run);
    assert!(src.exists());
    assert!(!dst.exists());
    assert!(report.entries[0].suggestion.is_some());
    std::fs::remove_dir_all(&base).ok();
}

#[cfg(unix)]
#[test]
fn move_repairs_dependent_symlink() {
    use std::os::unix::fs::symlink;

    let base = tmpdir("repair");
    let src = base.join("Downloads").join("photo.jpg");
    let dst = base.join("Pictures").join("photo.jpg");
    write(&src, b"xx");
    let link = base.join("link.jpg");
    symlink(&src, &link).unwrap();

    // The raw target is absolute and the replacement stays absolute.
    let dep = DependentOperation {
        op_type: DependentOpType::RepairSymlink,
        link_path: link.to_string_lossy().into_owned(),
        recorded_target: src.to_string_lossy().into_owned(),
        replacement_target: dst.to_string_lossy().into_owned(),
        recorded_target_relative: false,
        provenance: Provenance {
            link_resolved_target: Some(src.to_string_lossy().into_owned()),
            link_raw_relative: Some(false),
            ..Default::default()
        },
    };

    let mut s = Suggestion::new_move(
        AssetKind::Photo,
        &src,
        &dst,
        "test".into(),
        Default::default(),
        vec![dep],
        provenance_for(&src),
    );
    s.selected = true;
    let mut items = vec![s];
    let report = execute_batch(&mut items, &base, true, &ExecuteOptions::default());
    assert_eq!(report.entries[0].result, ExecutionStatus::Executed);
    // The link now points at the moved file.
    let target = std::fs::read_link(&link).unwrap();
    assert_eq!(target, dst);
    std::fs::remove_dir_all(&base).ok();
}

#[cfg(unix)]
#[test]
fn repair_refused_when_link_changed() {
    use std::os::unix::fs::symlink;

    let base = tmpdir("repairchanged");
    let src = base.join("src.txt");
    let dst = base.join("dst.txt");
    let other = base.join("other.txt");
    write(&src, b"1");
    write(&other, b"2");
    let link = base.join("link.txt");
    symlink(&src, &link).unwrap();

    let dep = DependentOperation {
        op_type: DependentOpType::RepairSymlink,
        link_path: link.to_string_lossy().into_owned(),
        recorded_target: src.to_string_lossy().into_owned(),
        replacement_target: dst.to_string_lossy().into_owned(),
        recorded_target_relative: false,
        provenance: Provenance {
            link_resolved_target: Some(src.to_string_lossy().into_owned()),
            link_raw_relative: Some(false),
            ..Default::default()
        },
    };

    // The link is retargeted after analysis -> repair must refuse.
    std::fs::remove_file(&link).unwrap();
    symlink(&other, &link).unwrap();

    let mut s = Suggestion::new_move(
        AssetKind::Document,
        &src,
        &dst,
        "test".into(),
        Default::default(),
        vec![dep],
        provenance_for(&src),
    );
    s.selected = true;
    let mut items = vec![s];
    let report = execute_batch(&mut items, &base, true, &ExecuteOptions::default());
    // Parent succeeded, dependent failed -> partial.
    assert!(report.partial);
    assert_eq!(report.entries[0].result, ExecutionStatus::Executed);
    let repair_op = &report.entries[0].operations[1];
    assert_eq!(repair_op.status, OperationStatus::Failed);
    // The link still points at `other`, untouched.
    assert_eq!(std::fs::read_link(&link).unwrap(), other);
    std::fs::remove_dir_all(&base).ok();
}

#[test]
fn unselected_and_rejected_are_skipped() {
    let base = tmpdir("skip");
    let src = base.join("a.txt");
    let dst = base.join("b.txt");
    write(&src, b"1");

    let mut s = Suggestion::new_move(
        AssetKind::Document,
        &src,
        &dst,
        "test".into(),
        Default::default(),
        vec![],
        provenance_for(&src),
    );
    s.selected = false; // excluded from this batch

    let mut items = vec![s];
    let report = execute_batch(&mut items, &base, true, &ExecuteOptions::default());
    assert_eq!(report.entries.len(), 0);
    assert!(src.exists());
    std::fs::remove_dir_all(&base).ok();
}

#[test]
fn move_without_selected_only_runs_everything_pending() {
    let base = tmpdir("allpending");
    let src = base.join("a.txt");
    let dst = base.join("b.txt");
    write(&src, b"1");

    let s = Suggestion::new_move(
        AssetKind::Document,
        &src,
        &dst,
        "test".into(),
        Default::default(),
        vec![],
        provenance_for(&src),
    );
    let mut items = vec![s];
    // selected_only = false: every pending suggestion is executed.
    let report = execute_batch(&mut items, &base, false, &ExecuteOptions::default());
    assert_eq!(report.entries.len(), 1);
    assert!(!src.exists());
    assert!(dst.exists());
    std::fs::remove_dir_all(&base).ok();
}

#[test]
fn suggestion_type_serde_roundtrip() {
    let s = Suggestion {
        id: "s-x".into(),
        suggestion_type: SuggestionType::Move,
        asset_kind: AssetKind::Document,
        target: "/a/b.txt".into(),
        new_path: Some("/c/b.txt".into()),
        reason: "r".into(),
        metadata: Default::default(),
        dependent_operations: vec![],
        selected: false,
        status: SuggestionStatus::Pending,
        metadata_pending: false,
        rejection_reason: None,
        provenance: Default::default(),
    };
    let json = serde_json::to_string(&s).unwrap();
    let back: Suggestion = serde_json::from_str(&json).unwrap();
    assert_eq!(back.id, s.id);
    assert_eq!(back.new_path, s.new_path);
    assert_eq!(back.suggestion_type, SuggestionType::Move);
    assert_eq!(back.status, SuggestionStatus::Pending);
}
