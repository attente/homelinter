//! Executor integration tests: real filesystem operations in a temp tree.

use homelinter_execute::execute_batch;
use homelinter_model::asset_kind::AssetKind;
use homelinter_model::hash;
use homelinter_model::suggestion::{
    DependentOpType, DependentOperation, ExecutionStatus, OperationKind, OperationStatus,
    Provenance, Suggestion, SuggestionStatus, SuggestionType,
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

fn provenance_for(p: &Path) -> Provenance {
    let m = std::fs::metadata(p).unwrap();
    let id = homelinter_model::identity::file_identity(p).unwrap();
    let mut prov = Provenance {
        dev: id.dev,
        ino: id.ino,
        identity_available: id.available,
        ..Default::default()
    };
    prov.size = m.len();
    if let Ok(d) = m
        .modified()
        .map(|t| t.duration_since(std::time::UNIX_EPOCH).unwrap())
    {
        prov.mtime_ns = d.as_nanos() as i128;
    }
    prov
}

fn keeper_metadata(keeper: &Path) -> std::collections::BTreeMap<String, serde_json::Value> {
    std::collections::BTreeMap::from([(
        "duplicate_of".to_string(),
        serde_json::Value::String(keeper.to_string_lossy().into_owned()),
    )])
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
    let report = execute_batch(&mut items, &base, true);
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
    let report = execute_batch(&mut items, &base, true);
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
fn failed_parent_reports_dependent_repairs_as_skipped() {
    let base = tmpdir("mvdeny-dep");
    let src = base.join("a.txt");
    let dst = base.join("b.txt");
    let link = base.join("link.txt");
    write(&src, b"1");
    write(&dst, b"2"); // occupied, so the move fails
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
    let report = execute_batch(&mut items, &base, true);
    assert_eq!(report.entries[0].result, ExecutionStatus::Failed);
    let ops = &report.entries[0].operations;
    assert_eq!(ops.len(), 2, "parent plus one dependent repair");
    assert_eq!(ops[0].status, OperationStatus::Failed);
    assert_eq!(ops[1].operation, OperationKind::RepairSymlink);
    assert_eq!(ops[1].status, OperationStatus::Skipped);
    assert!(ops[1]
        .detail
        .as_deref()
        .unwrap()
        .contains("parent operation failed"));
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
        AssetKind::Document,
        &b,
        "duplicate".into(),
        keeper_metadata(&a),
        vec![],
        provenance_for(&b),
    );
    s.provenance.full_hash = Some(hash::hash_file_full(&b).unwrap());
    s.provenance.size = std::fs::metadata(&b).unwrap().len();
    s.selected = true;

    let mut items = vec![s];
    let report = execute_batch(&mut items, &base, true);
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
        AssetKind::Document,
        &b,
        "duplicate".into(),
        keeper_metadata(&a),
        vec![],
        provenance_for(&b),
    );
    s.provenance.full_hash = Some(hash::hash_file_full(&b).unwrap());
    s.provenance.size = std::fs::metadata(&b).unwrap().len();
    s.selected = true;

    // Content changes after analysis; identity (dev/ino) is unchanged.
    std::fs::write(&b, b"mutated-content").unwrap();

    let mut items = vec![s];
    let report = execute_batch(&mut items, &base, true);
    assert_eq!(report.entries[0].result, ExecutionStatus::Failed);
    assert!(b.exists(), "changed file must not be deleted");
    std::fs::remove_dir_all(&base).ok();
}

fn failed_delete_detail(report: &homelinter_model::suggestion::ExecutionReport) -> String {
    let ops = &report.entries[0].operations;
    assert_eq!(ops[0].status, OperationStatus::Failed);
    ops[0].detail.clone().unwrap_or_default()
}

#[test]
fn delete_refuses_when_keeper_is_missing() {
    let base = tmpdir("delkeepergone");
    let a = base.join("a.bin");
    let b = base.join("b.bin");
    write(&a, b"same-content");
    write(&b, b"same-content");

    let mut s = Suggestion::new_delete(
        AssetKind::Document,
        &b,
        "duplicate".into(),
        keeper_metadata(&a),
        vec![],
        provenance_for(&b),
    );
    s.provenance.full_hash = Some(hash::hash_file_full(&b).unwrap());
    s.selected = true;

    // The keeper disappears after analysis; deleting b would lose the content.
    std::fs::remove_file(&a).unwrap();

    let mut items = vec![s];
    let report = execute_batch(&mut items, &base, true);
    assert_eq!(report.entries[0].result, ExecutionStatus::Failed);
    assert!(b.exists(), "delete must not run without its keeper");
    assert!(failed_delete_detail(&report).contains("keeper"));
    std::fs::remove_dir_all(&base).ok();
}

#[test]
fn delete_refuses_when_keeper_content_changed() {
    let base = tmpdir("delkeeperchanged");
    let a = base.join("a.bin");
    let b = base.join("b.bin");
    write(&a, b"same-content");
    write(&b, b"same-content");

    let mut s = Suggestion::new_delete(
        AssetKind::Document,
        &b,
        "duplicate".into(),
        keeper_metadata(&a),
        vec![],
        provenance_for(&b),
    );
    s.provenance.full_hash = Some(hash::hash_file_full(&b).unwrap());
    s.selected = true;

    // Same size keeps the keeper's identity check irrelevant; only its hash
    // can tell that it no longer holds the shared content.
    std::fs::write(&a, b"other-content").unwrap();

    let mut items = vec![s];
    let report = execute_batch(&mut items, &base, true);
    assert_eq!(report.entries[0].result, ExecutionStatus::Failed);
    assert!(b.exists(), "delete must not run when the keeper changed");
    assert!(failed_delete_detail(&report).contains("no longer holds"));
    std::fs::remove_dir_all(&base).ok();
}

#[test]
fn delete_without_recorded_keeper_is_refused() {
    let base = tmpdir("delnokeeper");
    let b = base.join("b.bin");
    write(&b, b"same-content");

    let mut s = Suggestion::new_delete(
        AssetKind::Document,
        &b,
        "duplicate".into(),
        Default::default(),
        vec![],
        provenance_for(&b),
    );
    s.provenance.full_hash = Some(hash::hash_file_full(&b).unwrap());
    s.selected = true;

    let mut items = vec![s];
    let report = execute_batch(&mut items, &base, true);
    assert_eq!(report.entries[0].result, ExecutionStatus::Failed);
    assert!(
        b.exists(),
        "a delete without a recorded keeper must not run"
    );
    assert!(failed_delete_detail(&report).contains("no recorded keeper"));
    std::fs::remove_dir_all(&base).ok();
}

#[test]
fn delete_follows_a_keeper_moved_in_the_same_batch() {
    let base = tmpdir("delkeepermoved");
    let a = base.join("Downloads/alpha.bin");
    let b = base.join("Downloads/beta.bin");
    let keeper_dst = base.join("Pictures/2024/alpha.bin");
    write(&a, b"same-content");
    write(&b, b"same-content");

    let mut mv = Suggestion::new_move(
        AssetKind::Document,
        &a,
        &keeper_dst,
        "test".into(),
        Default::default(),
        vec![],
        provenance_for(&a),
    );
    mv.selected = true;

    let mut del = Suggestion::new_delete(
        AssetKind::Document,
        &b,
        "duplicate".into(),
        keeper_metadata(&a),
        vec![],
        provenance_for(&b),
    );
    del.provenance.full_hash = Some(hash::hash_file_full(&b).unwrap());
    del.selected = true;

    let mut items = vec![mv, del];
    let report = execute_batch(&mut items, &base, true);
    assert_eq!(report.entries[0].result, ExecutionStatus::Executed);
    assert_eq!(report.entries[1].result, ExecutionStatus::Executed);
    assert!(keeper_dst.exists(), "keeper moved to its new path");
    assert!(!a.exists());
    assert!(!b.exists(), "duplicate removed after the keeper moved");
    std::fs::remove_dir_all(&base).ok();
}

#[test]
fn move_refuses_an_mtime_change_without_a_recorded_hash() {
    let base = tmpdir("mtimechanged");
    let src = base.join("a.jpg");
    write(&src, b"content");
    let provenance = provenance_for(&src);
    let mut mv = Suggestion::new_move(
        AssetKind::Photo,
        &src,
        &base.join("dst/a.jpg"),
        "test".into(),
        Default::default(),
        vec![],
        provenance,
    );
    mv.selected = true;
    // Same size and identity, different mtime, no hash on record: analysis
    // cannot be trusted, so the move is refused rather than guessed at.
    let stamp = std::fs::FileTimes::new()
        .set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_600_000_000));
    std::fs::File::options()
        .write(true)
        .open(&src)
        .unwrap()
        .set_times(stamp)
        .unwrap();

    let mut items = vec![mv];
    let report = execute_batch(&mut items, &base, true);
    assert_eq!(report.entries[0].result, ExecutionStatus::Failed);
    assert!(src.exists());
    let detail = format!("{:?}", report.entries[0]);
    assert!(
        detail.contains("re-analysis required"),
        "unexpected detail: {detail}"
    );
    std::fs::remove_dir_all(&base).ok();
}

#[test]
fn move_tolerates_an_mtime_change_when_the_recorded_hash_matches() {
    let base = tmpdir("mtimehash");
    let src = base.join("a.jpg");
    write(&src, b"content");
    let mut provenance = provenance_for(&src);
    provenance.full_hash = Some(hash::hash_file_full(&src).unwrap());
    let mut mv = Suggestion::new_move(
        AssetKind::Photo,
        &src,
        &base.join("dst/a.jpg"),
        "test".into(),
        Default::default(),
        vec![],
        provenance,
    );
    mv.selected = true;
    let stamp = std::fs::FileTimes::new()
        .set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_600_000_000));
    std::fs::File::options()
        .write(true)
        .open(&src)
        .unwrap()
        .set_times(stamp)
        .unwrap();

    let mut items = vec![mv];
    let report = execute_batch(&mut items, &base, true);
    assert_eq!(report.entries[0].result, ExecutionStatus::Executed);
    assert!(base.join("dst/a.jpg").exists());
    std::fs::remove_dir_all(&base).ok();
}

#[test]
fn move_tolerates_an_mtime_change_when_only_the_sampled_hash_is_recorded() {
    let base = tmpdir("mtimefast");
    let src = base.join("a.mov");
    write(&src, b"sampled-content");
    let mut provenance = provenance_for(&src);
    provenance.fast_hash =
        Some(homelinter_model::hash::hash_file_fast(&src, provenance.size).unwrap());
    let mut mv = Suggestion::new_move(
        AssetKind::Video,
        &src,
        &base.join("dst/a.mov"),
        "test".into(),
        Default::default(),
        vec![],
        provenance.clone(),
    );
    mv.selected = true;
    let stamp = std::fs::FileTimes::new()
        .set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_600_000_000));
    std::fs::File::options()
        .write(true)
        .open(&src)
        .unwrap()
        .set_times(stamp)
        .unwrap();

    // Same bytes, touched mtime, sampled hash identical: the drift is only a
    // timestamp, so the move proceeds.
    let mut items = vec![mv];
    let report = execute_batch(&mut items, &base, true);
    assert_eq!(report.entries[0].result, ExecutionStatus::Executed);
    assert!(base.join("dst/a.mov").exists());

    // Different bytes at the same size with the same recorded sample: the
    // sampled hash no longer matches, so the drift is refused.
    let src2 = base.join("b.mov");
    write(&src2, b"sampled-content");
    let mut mv2 = Suggestion::new_move(
        AssetKind::Video,
        &src2,
        &base.join("dst/b.mov"),
        "test".into(),
        Default::default(),
        vec![],
        provenance,
    );
    mv2.selected = true;
    write(&src2, b"SAMPLED-content");
    std::fs::File::options()
        .write(true)
        .open(&src2)
        .unwrap()
        .set_times(stamp)
        .unwrap();
    let mut items = vec![mv2];
    let report = execute_batch(&mut items, &base, true);
    assert_eq!(report.entries[0].result, ExecutionStatus::Failed);
    assert!(src2.exists());
    std::fs::remove_dir_all(&base).ok();
}

#[test]
fn relocations_cover_executed_moves_only() {
    let base = tmpdir("relocations");
    let a = base.join("a.bin");
    let b = base.join("b.bin");
    write(&a, b"a");
    write(&b, b"b");
    let mut executed = Suggestion::new_move(
        AssetKind::Document,
        &a,
        &base.join("dst/a.bin"),
        String::new(),
        Default::default(),
        vec![],
        provenance_for(&a),
    );
    executed.status = SuggestionStatus::Executed;
    // A pending move has not relocated anything yet.
    let mut pending = Suggestion::new_move(
        AssetKind::Document,
        &b,
        &base.join("dst/b.bin"),
        String::new(),
        Default::default(),
        vec![],
        provenance_for(&b),
    );
    pending.status = SuggestionStatus::Pending;

    let map = homelinter_execute::relocations_of(&[executed, pending]);
    assert_eq!(map.len(), 1);
    assert_eq!(map.get(&a), Some(&base.join("dst/a.bin")));
    std::fs::remove_dir_all(&base).ok();
}

#[cfg(unix)]
#[test]
fn keeper_move_republishes_the_delete_for_a_later_batch() {
    use std::os::unix::fs::symlink;

    let base = tmpdir("keepermovesplit");
    let keeper = base.join("Downloads/zeta.bin");
    let duplicate = base.join("Downloads/beta.bin");
    let keeper_dst = base.join("Pictures/zeta.bin");
    let link = base.join("Photos/link.bin");
    write(&keeper, b"same-content");
    write(&duplicate, b"same-content");
    std::fs::create_dir_all(link.parent().unwrap()).unwrap();
    symlink(&keeper, &link).unwrap();

    let mut mv = Suggestion::new_move(
        AssetKind::Document,
        &keeper,
        &keeper_dst,
        "test".into(),
        Default::default(),
        vec![],
        provenance_for(&keeper),
    );
    mv.selected = true;

    let mut del = Suggestion::new_delete(
        AssetKind::Document,
        &duplicate,
        "duplicate".into(),
        std::collections::BTreeMap::from([
            (
                "duplicate_of".to_string(),
                serde_json::Value::String(keeper.to_string_lossy().into_owned()),
            ),
            // A recorded size lets the reason text be rewritten with the
            // keeper's current path.
            ("size".to_string(), serde_json::Value::from(12u64)),
        ]),
        vec![homelinter_model::suggestion::DependentOperation {
            op_type: homelinter_model::suggestion::DependentOpType::RepairSymlink,
            link_path: link.to_string_lossy().into_owned(),
            recorded_target: keeper.to_string_lossy().into_owned(),
            replacement_target: keeper.to_string_lossy().into_owned(),
            recorded_target_relative: false,
            provenance: Default::default(),
        }],
        provenance_for(&duplicate),
    );
    del.provenance.full_hash = Some(hash::hash_file_full(&duplicate).unwrap());

    // Batch 1 executes only the keeper's move. The delete was not selected,
    // yet its recorded keeper and its repair must follow the keeper.
    let delete_id = del.id.clone();
    let mut items = vec![mv, del];
    let report = execute_batch(&mut items, &base, true);
    assert_eq!(report.entries.len(), 1, "only the selected move runs");
    assert_eq!(report.entries[0].result, ExecutionStatus::Executed);
    assert!(items[0].status == SuggestionStatus::Executed);
    assert!(items[1].status == SuggestionStatus::Pending);
    let keeper_dst_str = keeper_dst.to_string_lossy().into_owned();
    assert_eq!(
        items[1].metadata["duplicate_of"].as_str(),
        Some(keeper_dst_str.as_str()),
        "the delete's record follows the keeper"
    );
    assert!(
        items[1].reason.contains(&keeper_dst_str),
        "the reason names the keeper's current path: {}",
        items[1].reason
    );
    assert_eq!(
        items[1].dependent_operations[0].replacement_target,
        keeper_dst_str
    );

    // Batch 2 executes the delete on the same session state. Reset the
    // republished record first, which is what a caller that only hands the
    // batch over again leaves behind: the executor seeds its relocations from
    // the batch's already-executed move, so the delete still confirms the
    // keeper at its current path instead of failing.
    items[1].metadata.insert(
        "duplicate_of".to_string(),
        serde_json::Value::String(keeper.to_string_lossy().into_owned()),
    );
    items[1].selected = true;
    let report = execute_batch(&mut items, &base, true);
    let entry = report
        .entries
        .iter()
        .find(|e| e.suggestion_id == delete_id)
        .expect("the delete ran");
    assert_eq!(entry.result, ExecutionStatus::Executed);
    assert!(!duplicate.exists(), "duplicate removed in a later batch");
    assert_eq!(std::fs::read_link(&link).unwrap(), keeper_dst);
    std::fs::remove_dir_all(&base).ok();
}

#[cfg(unix)]
#[test]
fn delete_repair_follows_a_keeper_moved_later_in_sort_order() {
    use std::os::unix::fs::symlink;

    let base = tmpdir("delrepairkeepermoved");
    let keeper = base.join("Downloads/zeta.bin");
    let duplicate = base.join("Downloads/beta.bin");
    let keeper_dst = base.join("Pictures/zeta.bin");
    let link = base.join("Photos/link.bin");
    write(&keeper, b"same-content");
    write(&duplicate, b"same-content");
    std::fs::create_dir_all(link.parent().unwrap()).unwrap();
    symlink(&keeper, &link).unwrap();

    let mut mv = Suggestion::new_move(
        AssetKind::Document,
        &keeper,
        &keeper_dst,
        "test".into(),
        Default::default(),
        vec![],
        provenance_for(&keeper),
    );
    mv.selected = true;

    let dep = DependentOperation {
        op_type: DependentOpType::RepairSymlink,
        link_path: link.to_string_lossy().into_owned(),
        recorded_target: keeper.to_string_lossy().into_owned(),
        replacement_target: keeper.to_string_lossy().into_owned(),
        recorded_target_relative: false,
        provenance: Provenance {
            link_resolved_target: Some(keeper.to_string_lossy().into_owned()),
            link_raw_relative: Some(false),
            ..Default::default()
        },
    };
    let mut del = Suggestion::new_delete(
        AssetKind::Document,
        &duplicate,
        "duplicate".into(),
        keeper_metadata(&keeper),
        vec![dep],
        provenance_for(&duplicate),
    );
    del.provenance.full_hash = Some(hash::hash_file_full(&duplicate).unwrap());
    del.selected = true;

    // The duplicate sorts before the keeper, so only the executor's
    // moves-first order can publish the keeper before this delete runs.
    let mut items = vec![del, mv];
    let report = execute_batch(&mut items, &base, true);
    assert_eq!(report.entries[0].result, ExecutionStatus::Executed);
    assert_eq!(report.entries[1].result, ExecutionStatus::Executed);
    assert_eq!(std::fs::read_link(&link).unwrap(), keeper_dst);
    assert_eq!(
        report.entries[1].operations[1].destination.as_deref(),
        Some(keeper_dst.to_string_lossy().as_ref())
    );
    std::fs::remove_dir_all(&base).ok();
}

#[cfg(unix)]
#[test]
fn delete_repair_keeps_the_original_keeper_path_when_the_keeper_stays() {
    use std::os::unix::fs::symlink;

    let base = tmpdir("delrepairkeeperstays");
    let keeper = base.join("a.bin");
    let duplicate = base.join("b.bin");
    let link = base.join("link.bin");
    write(&keeper, b"same-content");
    write(&duplicate, b"same-content");
    symlink(&keeper, &link).unwrap();

    let dep = DependentOperation {
        op_type: DependentOpType::RepairSymlink,
        link_path: link.to_string_lossy().into_owned(),
        recorded_target: keeper.to_string_lossy().into_owned(),
        replacement_target: keeper.to_string_lossy().into_owned(),
        recorded_target_relative: false,
        provenance: Provenance {
            link_resolved_target: Some(keeper.to_string_lossy().into_owned()),
            link_raw_relative: Some(false),
            ..Default::default()
        },
    };
    let mut del = Suggestion::new_delete(
        AssetKind::Document,
        &duplicate,
        "duplicate".into(),
        keeper_metadata(&keeper),
        vec![dep],
        provenance_for(&duplicate),
    );
    del.provenance.full_hash = Some(hash::hash_file_full(&duplicate).unwrap());
    del.selected = true;

    let mut items = vec![del];
    let report = execute_batch(&mut items, &base, true);
    assert_eq!(report.entries[0].result, ExecutionStatus::Executed);
    assert_eq!(std::fs::read_link(&link).unwrap(), keeper);
    std::fs::remove_dir_all(&base).ok();
}

#[cfg(unix)]
#[test]
fn delete_repair_rewrites_a_relative_link_for_the_moved_keeper() {
    use std::os::unix::fs::symlink;

    let base = tmpdir("delrepairrelative");
    let keeper = base.join("Downloads/zeta.bin");
    let duplicate = base.join("Downloads/beta.bin");
    let keeper_dst = base.join("Pictures/zeta.bin");
    let link = base.join("Photos/link.bin");
    write(&keeper, b"same-content");
    write(&duplicate, b"same-content");
    std::fs::create_dir_all(link.parent().unwrap()).unwrap();
    symlink("../Downloads/zeta.bin", &link).unwrap();

    let mut mv = Suggestion::new_move(
        AssetKind::Document,
        &keeper,
        &keeper_dst,
        "test".into(),
        Default::default(),
        vec![],
        provenance_for(&keeper),
    );
    mv.selected = true;

    let dep = DependentOperation {
        op_type: DependentOpType::RepairSymlink,
        link_path: link.to_string_lossy().into_owned(),
        recorded_target: "../Downloads/zeta.bin".into(),
        replacement_target: "../Downloads/zeta.bin".into(),
        recorded_target_relative: true,
        provenance: Provenance {
            link_resolved_target: Some(keeper.to_string_lossy().into_owned()),
            link_raw_relative: Some(true),
            ..Default::default()
        },
    };
    let mut del = Suggestion::new_delete(
        AssetKind::Document,
        &duplicate,
        "duplicate".into(),
        keeper_metadata(&keeper),
        vec![dep],
        provenance_for(&duplicate),
    );
    del.provenance.full_hash = Some(hash::hash_file_full(&duplicate).unwrap());
    del.selected = true;

    let mut items = vec![del, mv];
    let report = execute_batch(&mut items, &base, true);
    assert_eq!(report.entries[1].result, ExecutionStatus::Executed);
    assert_eq!(
        std::fs::read_link(&link).unwrap(),
        PathBuf::from("../Pictures/zeta.bin")
    );
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
    let report = execute_batch(&mut items, &base, true);
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
    let report = execute_batch(&mut items, &base, true);
    // Parent succeeded, dependent failed -> partial.
    assert!(report.partial);
    assert_eq!(report.entries[0].result, ExecutionStatus::Partial);
    assert_eq!(items[0].status, SuggestionStatus::Partial);
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
    let report = execute_batch(&mut items, &base, true);
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
    let report = execute_batch(&mut items, &base, false);
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
        manual_overrides: vec![],
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
