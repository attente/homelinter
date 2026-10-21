//! Exported-script integration tests: run the generated POSIX script against
//! a real temp tree and assert the safety behavior it promises.

#![cfg(unix)]

use homelinter_execute::export_script;
use homelinter_model::asset_kind::AssetKind;
use homelinter_model::hash;
use homelinter_model::suggestion::{Provenance, Suggestion, SuggestionStatus};
use std::path::{Path, PathBuf};
use std::process::Command;

fn tmpdir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "hl-script-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn write(path: &Path, content: &[u8]) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(path, content).unwrap();
}

fn move_suggestion(src: &Path, dst: &Path, content: &[u8]) -> Suggestion {
    let mut s = Suggestion::new_move(
        AssetKind::Document,
        src,
        dst,
        "Moved to documents.".into(),
        Default::default(),
        Vec::new(),
        Default::default(),
    );
    s.status = SuggestionStatus::Pending;
    s.provenance = Provenance {
        full_hash: Some(hash::hash_file_full(src).unwrap()),
        size: content.len() as u64,
        identity_available: true,
        ..Default::default()
    };
    s
}

/// Run the exported script, returning its exit status and stderr.
fn run(script: &str, dir: &Path) -> (bool, String) {
    let path = dir.join("export.sh");
    std::fs::write(&path, script).unwrap();
    let out = Command::new("sh").arg(&path).output().unwrap();
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

#[test]
fn exported_move_runs_when_the_content_still_matches() {
    let dir = tmpdir("move-ok");
    let src = dir.join("in/a.pdf");
    let dst = dir.join("out/nested/a.pdf");
    write(&src, b"pdf bytes");
    let s = move_suggestion(&src, &dst, b"pdf bytes");
    let script = export_script(std::slice::from_ref(&s), "linux").unwrap();

    let (ok, err) = run(&script, &dir);
    assert!(ok, "script failed: {err}\n{script}");
    assert!(!src.exists(), "source must be gone");
    assert_eq!(std::fs::read(&dst).unwrap(), b"pdf bytes");
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn exported_move_aborts_when_the_content_changed_since_analysis() {
    let dir = tmpdir("move-drift");
    let src = dir.join("in/a.pdf");
    let dst = dir.join("out/a.pdf");
    write(&src, b"analyzed bytes");
    let s = move_suggestion(&src, &dst, b"analyzed bytes");
    // Edited between analysis and the script run.
    write(&src, b"edited bytes!!");
    let script = export_script(std::slice::from_ref(&s), "linux").unwrap();

    let (ok, err) = run(&script, &dir);
    assert!(!ok, "changed content must abort");
    assert!(err.contains("content changed since analysis"), "{err}");
    assert!(src.exists(), "source must be untouched");
    assert!(!dst.exists(), "destination must not be created");
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn exported_move_refuses_an_existing_destination() {
    let dir = tmpdir("move-clobber");
    let src = dir.join("in/a.pdf");
    let dst = dir.join("out/a.pdf");
    write(&src, b"source");
    write(&dst, b"destination");
    let s = move_suggestion(&src, &dst, b"source");
    let script = export_script(std::slice::from_ref(&s), "linux").unwrap();

    let (ok, err) = run(&script, &dir);
    assert!(!ok, "existing destination must abort");
    assert!(err.contains("destination exists"), "{err}");
    assert_eq!(std::fs::read(&dst).unwrap(), b"destination");
    assert_eq!(std::fs::read(&src).unwrap(), b"source");
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn exported_delete_aborts_when_the_keeper_vanished() {
    let dir = tmpdir("delete-keeper");
    let keeper = dir.join("keeper.bin");
    let dup = dir.join("dup.bin");
    write(&keeper, b"shared");
    write(&dup, b"shared");
    let hash = hash::hash_file_full(&dup).unwrap();
    let mut s = Suggestion::new_delete(
        AssetKind::Document,
        &dup,
        "duplicate".into(),
        std::collections::BTreeMap::from([(
            "duplicate_of".to_string(),
            serde_json::Value::String(keeper.to_string_lossy().into_owned()),
        )]),
        Vec::new(),
        Default::default(),
    );
    s.status = SuggestionStatus::Pending;
    s.provenance.full_hash = Some(hash);
    let script = export_script(std::slice::from_ref(&s), "linux").unwrap();
    // The keeper is gone by the time the script runs.
    std::fs::remove_file(&keeper).unwrap();

    let (ok, err) = run(&script, &dir);
    assert!(!ok, "a missing keeper must abort the delete");
    assert!(err.contains("keeper missing"), "{err}");
    assert!(dup.exists(), "the duplicate must be kept");
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn exported_delete_removes_the_duplicate_when_the_keeper_matches() {
    let dir = tmpdir("delete-ok");
    let keeper = dir.join("keeper.bin");
    let dup = dir.join("dup.bin");
    write(&keeper, b"shared");
    write(&dup, b"shared");
    let hash = hash::hash_file_full(&dup).unwrap();
    let mut s = Suggestion::new_delete(
        AssetKind::Document,
        &dup,
        "duplicate".into(),
        std::collections::BTreeMap::from([(
            "duplicate_of".to_string(),
            serde_json::Value::String(keeper.to_string_lossy().into_owned()),
        )]),
        Vec::new(),
        Default::default(),
    );
    s.status = SuggestionStatus::Pending;
    s.provenance.full_hash = Some(hash);
    let script = export_script(std::slice::from_ref(&s), "linux").unwrap();

    let (ok, err) = run(&script, &dir);
    assert!(ok, "script failed: {err}\n{script}");
    assert!(!dup.exists(), "duplicate must be gone");
    assert_eq!(std::fs::read(&keeper).unwrap(), b"shared");
    std::fs::remove_dir_all(&dir).ok();
}
