//! Service API integration tests: session contract, scan -> ready pipeline,
//! selection and validation rules, real execution with idempotency,
//! export, config, cache, audit, and an end-to-end HTTP + SSE + shutdown run.

use std::path::{Path, PathBuf};
use std::time::Duration;

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use tower::ServiceExt;

use homelinter_model::config::ConfigChain;
use homelinter_service::pipeline::AppHandle;
use homelinter_service::{build_service, ServiceOptions};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn unique_dir(tag: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("homelinter-{tag}-{}-{nanos}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

struct Env {
    home: PathBuf,
    data: PathBuf,
    _config: PathBuf,
}

impl Env {
    fn new(tag: &str) -> Env {
        let base = unique_dir(tag);
        let home = base.join("home");
        let data = base.join("data");
        let config = base.join("config.toml");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&data).unwrap();
        std::fs::write(&config, "[output]\nformat = \"json\"\n").unwrap();
        Env {
            home,
            data,
            _config: config,
        }
    }
    fn service(&self) -> AppHandle {
        let opts = ServiceOptions {
            cli_config: Some(self._config.clone()),
            home_override: Some(self.home.clone()),
            data_dir_override: Some(self.data.clone()),
            // Hermetic config chain: never read the host's system or
            // per-user config files, whatever this machine happens to have.
            config_chain_override: Some(ConfigChain::default()),
            web_dir: None,
            ..Default::default()
        };
        build_service(opts).unwrap().app
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(self.home.parent().unwrap());
    }
}

/// Oneshot request helper: returns (status, decoded JSON body).
async fn request(
    app: &AppHandle,
    method: &str,
    uri: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let router = homelinter_service::router(app.clone());
    let mut builder = Request::builder().method(method).uri(uri);
    let b = match body {
        Some(v) => {
            builder = builder.header(header::CONTENT_TYPE, "application/json");
            Body::from(serde_json::to_vec(&v).unwrap())
        }
        None => Body::empty(),
    };
    let resp = router
        .oneshot(builder.body(b).unwrap())
        .await
        .expect("oneshot response");
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let text = String::from_utf8_lossy(&bytes);
    let v = serde_json::from_str(&text).unwrap_or(Value::Null);
    (status, v)
}

async fn request_text(
    app: &AppHandle,
    method: &str,
    uri: &str,
    body: Option<Value>,
) -> (StatusCode, axum::http::HeaderMap, String) {
    let router = homelinter_service::router(app.clone());
    let mut builder = Request::builder().method(method).uri(uri);
    let b = match body {
        Some(v) => {
            builder = builder.header(header::CONTENT_TYPE, "application/json");
            Body::from(serde_json::to_vec(&v).unwrap())
        }
        None => Body::empty(),
    };
    let resp = router.oneshot(builder.body(b).unwrap()).await.unwrap();
    let status = resp.status();
    let headers = resp.headers().clone();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        headers,
        String::from_utf8_lossy(&bytes).into_owned(),
    )
}

async fn session_state(app: &AppHandle) -> String {
    let (_, v) = request(app, "GET", "/api/v1/session", None).await;
    v["state"].as_str().unwrap_or("?").to_string()
}

/// Poll `GET /session` until the state machine reaches `want` or we time out.
async fn wait_state(app: &AppHandle, want: &str) {
    for _ in 0..600 {
        if session_state(app).await == want {
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!(
        "session never reached {want}; last state: {}",
        session_state(app).await
    );
}

#[tokio::test]
async fn launch_overrides_seed_scan_settings() {
    let env = Env::new("scan_defaults");
    let opts = ServiceOptions {
        cli_config: Some(env._config.clone()),
        home_override: Some(env.home.clone()),
        data_dir_override: Some(env.data.clone()),
        config_chain_override: Some(ConfigChain::default()),
        web_dir: None,
        initial_overrides: Some(json!({
            "scan": { "root": env.home.to_string_lossy().into_owned(), "hidden": true },
            "target_paths": { "photo": "Pictures/%Y/%m/%f" },
        })),
    };
    let app = build_service(opts).unwrap().app;
    // CLI flags are launch-time settings overrides, visible in the effective
    // settings and editable through the API.
    let (status, v) = request(&app, "GET", "/api/v1/config", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["scan"]["root"], env.home.to_string_lossy().as_ref());
    assert_eq!(v["scan"]["hidden"], true);
    assert_eq!(v["target_paths"]["photo"], "Pictures/%Y/%m/%f");
    // The session no longer advertises per-scan dialog defaults.
    let (_, sess) = request(&app, "GET", "/api/v1/session", None).await;
    assert!(sess.get("scan_defaults").is_none());
}

#[tokio::test]
async fn scan_settings_default_to_home_hidden_off() {
    let env = Env::new("scan_defaults_off");
    let app = env.service();
    let (_, v) = request(&app, "GET", "/api/v1/config", None).await;
    assert!(v["scan"]["root"].is_null());
    assert_eq!(v["scan"]["hidden"], false);
    assert_eq!(v["target_paths"], json!({}));
    assert!(v["default_exclude_paths"]
        .as_array()
        .unwrap()
        .iter()
        .any(|x| x == "node_modules"));
}

/// Create two identical files with distinct mtimes; the older becomes the
/// keeper, the newer the delete target.
fn seed_duplicates(home: &Path) -> (PathBuf, PathBuf) {
    let dl = home.join("Downloads");
    std::fs::create_dir_all(&dl).unwrap();
    let older = dl.join("alpha.bin");
    let newer = dl.join("beta.bin");
    std::fs::write(&older, b"same-content-xyz").unwrap();
    std::fs::write(&newer, b"same-content-xyz").unwrap();
    let old_time = std::fs::FileTimes::new()
        .set_modified(std::time::UNIX_EPOCH + Duration::from_secs(1_600_000_000));
    let new_time = std::fs::FileTimes::new()
        .set_modified(std::time::UNIX_EPOCH + Duration::from_secs(1_600_001_000));
    std::fs::File::open(&older)
        .unwrap()
        .set_times(old_time)
        .unwrap();
    std::fs::File::open(&newer)
        .unwrap()
        .set_times(new_time)
        .unwrap();
    (older, newer)
}

async fn scan_to_ready(app: &AppHandle, root: &Path) {
    // The scan root is an ordinary runtime setting; persist it, then start
    // a scan with no per-scan parameters.
    let (status, _) = request(
        app,
        "PUT",
        "/api/v1/config",
        Some(json!({ "scan": { "root": root.to_string_lossy() } })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, v) = request(app, "POST", "/api/v1/session/scan", Some(json!({}))).await;
    assert_eq!(status, StatusCode::ACCEPTED);
    assert_eq!(v["state"], "scanning");
    wait_state(app, "ready").await;
}

async fn suggestion_ids(app: &AppHandle, status_filter: &str) -> Vec<String> {
    let (_, v) = request(app, "GET", "/api/v1/suggestions", None).await;
    let mut ids = Vec::new();
    if let Some(items) = v["items"].as_array() {
        for it in items {
            if let Some(st) = it["status"].as_str() {
                if status_filter.is_empty() || st == status_filter {
                    if let Some(id) = it["id"].as_str() {
                        ids.push(id.to_string());
                    }
                }
            }
        }
    }
    ids
}

// ---------------------------------------------------------------------------
// Session & health contract
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn health_and_session_start_idle() {
    let env = Env::new("idle");
    let app = env.service();
    let (status, v) = request(&app, "GET", "/api/v1/health", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["state"], "idle");
    let (status, v) = request(&app, "GET", "/api/v1/session", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["state"], "idle");
    assert!(v["config_hash"].is_string());
    assert_eq!(
        v["metadata_progress"],
        json!({ "completed": 0, "total": 0 })
    );
    // Config GET carries the merged effective config.
    let (status, v) = request(&app, "GET", "/api/v1/config", None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(v.get("config").is_some() || v.get("output").is_some());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn settings_validation_rules() {
    let env = Env::new("paths");
    let app = env.service();
    // Relative scan root -> 422 through the settings API.
    let (status, v) = request(
        &app,
        "PUT",
        "/api/v1/config",
        Some(json!({ "scan": { "root": "relative/x" } })),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(v["error"]["code"], "validation_failed");
    // Unknown asset kind in target_paths -> 422.
    let (status, _) = request(
        &app,
        "PUT",
        "/api/v1/config",
        Some(json!({ "target_paths": { "nope": "X/%f" } })),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    // Malformed template -> 422.
    let (status, _) = request(
        &app,
        "PUT",
        "/api/v1/config",
        Some(json!({ "target_paths": { "photo": "%" } })),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    // Cancel when idle is a no-op 200.
    let (status, v) = request(&app, "POST", "/api/v1/cancel", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["state"], "idle");
    // Execute from idle conflicts.
    let (status, _) = request(&app, "POST", "/api/v1/execute", Some(json!({}))).await;
    assert_eq!(status, StatusCode::CONFLICT);
    // Every rejected write left the session untouched.
    assert_eq!(session_state(&app).await, "idle");
}

// ---------------------------------------------------------------------------
// Scan -> ready -> suggestions
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn scan_produces_validated_batch() {
    let env = Env::new("scan");
    let app = env.service();
    seed_duplicates(&env.home);
    scan_to_ready(&app, &env.home).await;

    let (_, v) = request(&app, "GET", "/api/v1/session", None).await;
    assert!(v["suggestions_total"].as_u64().unwrap() >= 1);
    assert_eq!(v["scan_root"], env.home.to_string_lossy().as_ref());

    // Every suggestion is top-level pending (post-validation) and has an id.
    let ids = suggestion_ids(&app, "pending").await;
    assert!(!ids.is_empty());
    let (status, v) = request(
        &app,
        "POST",
        "/api/v1/validate",
        Some(json!({ "ids": ids })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["valid"], true);

    // Report summary reflects files scanned and suggestion counts.
    let (status, v) = request(&app, "GET", "/api/v1/report/summary", None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(v["total_files_scanned"].as_u64().unwrap() >= 1);

    // Unknown suggestion id is rejected by validation/selection.
    let (status, _) = request(
        &app,
        "PUT",
        "/api/v1/selection",
        Some(json!({ "ids": ["bogus-id"] })),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);

    // Empty selection: execute without ids -> 400.
    let (status, _) = request(&app, "POST", "/api/v1/execute", Some(json!({}))).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn settings_writes_persist_and_affect_future_scans() {
    let env = Env::new("scan_settings");
    let app = env.service();
    seed_duplicates(&env.home);

    let (_, initial_cfg) = request(&app, "GET", "/api/v1/config", None).await;
    assert_eq!(initial_cfg["scan"]["default_exclusions"], json!(true));
    assert_eq!(initial_cfg["scan"]["exclude_paths"], json!([]));
    // Built-in exclusions are reported by the settings endpoint.
    assert!(initial_cfg["default_exclude_paths"]
        .as_array()
        .unwrap()
        .iter()
        .any(|v| v == "node_modules"));

    // Persist an exclusion of the Downloads tree, then scan under it.
    let (status, _) = request(
        &app,
        "PUT",
        "/api/v1/config",
        Some(json!({
            "scan": {
                "root": env.home.to_string_lossy(),
                "exclude_paths": ["Downloads"],
            },
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = request(&app, "POST", "/api/v1/session/scan", Some(json!({}))).await;
    assert_eq!(status, StatusCode::ACCEPTED);
    wait_state(&app, "ready").await;
    let (_, sess) = request(&app, "GET", "/api/v1/session", None).await;
    assert_eq!(sess["suggestions_total"], 0);

    // The setting persisted: the effective config still shows it.
    let (_, cfg) = request(&app, "GET", "/api/v1/config", None).await;
    assert_eq!(cfg["scan"]["exclude_paths"], json!(["Downloads"]));

    // Null resets the setting; the next scan finds the duplicates again.
    let (status, _) = request(
        &app,
        "PUT",
        "/api/v1/config",
        Some(json!({ "scan": { "exclude_paths": null } })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (_, cfg) = request(&app, "GET", "/api/v1/config", None).await;
    assert_eq!(cfg["scan"]["exclude_paths"], json!([]));
    let (status, _) = request(&app, "POST", "/api/v1/session/scan", Some(json!({}))).await;
    assert_eq!(status, StatusCode::ACCEPTED);
    wait_state(&app, "ready").await;
    let (_, sess) = request(&app, "GET", "/api/v1/session", None).await;
    assert!(sess["suggestions_total"].as_u64().unwrap() >= 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn invalid_settings_write_leaves_session_untouched() {
    let env = Env::new("scan_settings_bad");
    let app = env.service();
    seed_duplicates(&env.home);

    // Relative scan root is rejected as a setting.
    let (status, _) = request(
        &app,
        "PUT",
        "/api/v1/config",
        Some(json!({ "scan": { "root": "relative/x" } })),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    // Unknown section key.
    let (status, _) = request(
        &app,
        "PUT",
        "/api/v1/config",
        Some(json!({ "geocoder": { "bogus": 1 } })),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    // Non-table document.
    let (status, _) = request(&app, "PUT", "/api/v1/config", Some(json!(5))).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);

    // Nothing changed: the session is still idle and a normal scan works.
    assert_eq!(session_state(&app).await, "idle");
    scan_to_ready(&app, &env.home).await;
    let (_, sess) = request(&app, "GET", "/api/v1/session", None).await;
    assert!(sess["suggestions_total"].as_u64().unwrap() >= 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn real_execute_moves_keeper_deletes_duplicate_and_idempotent_rerun() {
    let env = Env::new("exec");
    let app = env.service();
    let (older, newer) = seed_duplicates(&env.home);
    scan_to_ready(&app, &env.home).await;

    let ids = suggestion_ids(&app, "pending").await;
    assert_eq!(ids.len(), 2, "expected keeper move + duplicate delete");

    let (status, _) = request(&app, "POST", "/api/v1/execute", Some(json!({ "ids": ids }))).await;
    assert_eq!(status, StatusCode::ACCEPTED);
    wait_state(&app, "done").await;

    // Newer duplicate deleted; older keeper moved out of Downloads.
    assert!(!newer.exists());
    assert!(!older.exists());

    let (_, report) = request(&app, "GET", "/api/v1/execution", None).await;
    let entries = report["entries"].as_array().unwrap();
    assert_eq!(entries.len(), 2);
    assert!(entries.iter().all(|e| e["result"] == "executed"));
    assert_eq!(report["scan_root"], env.home.to_string_lossy().as_ref());

    // Suggestion statuses now executed.
    assert!(suggestion_ids(&app, "executed").await.len() >= 2);

    // Audit log has an entry per executed suggestion.
    let (status, audit) = request(&app, "GET", "/api/v1/audit", None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(audit.as_array().unwrap().len() >= 2);

    // Idempotent re-run of the same batch returns 202 with the stored report.
    let (status, v) = request(&app, "POST", "/api/v1/execute", Some(json!({ "ids": ids }))).await;
    assert_eq!(status, StatusCode::ACCEPTED);
    assert_eq!(v["state"], "done");
    wait_state(&app, "done").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn export_script_contract() {
    let env = Env::new("export");
    let app = env.service();
    seed_duplicates(&env.home);
    scan_to_ready(&app, &env.home).await;

    let ids = suggestion_ids(&app, "pending").await;
    assert!(!ids.is_empty());
    let (status, headers, body) =
        request_text(&app, "POST", "/api/v1/export", Some(json!({ "ids": ids }))).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        headers["x-homelinter-count"].to_str().unwrap(),
        ids.len().to_string(),
        "export headers mismatch ids={ids:?}"
    );
    assert!(headers.contains_key("x-homelinter-platform"));
    assert!(body.contains("#!/bin/sh") || body.contains("Generated by homelinter"));

    // Export to an explicit absolute path persists the script.
    let dest = env.data.join("out.sh");
    let (status, v) = request(
        &app,
        "POST",
        "/api/v1/export",
        Some(json!({ "ids": ids, "path": dest.to_string_lossy() })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["path"], dest.to_string_lossy().as_ref());
    assert!(dest.exists());
    // Relative export path -> 422.
    let (status, _) = request(
        &app,
        "POST",
        "/api/v1/export",
        Some(json!({ "ids": ids, "path": "rel.sh" })),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cache_clear_and_config_put() {
    let env = Env::new("conf");
    let app = env.service();

    // Config PUT deep-merges and persists.
    let (status, v) = request(
        &app,
        "PUT",
        "/api/v1/config",
        Some(json!({ "duplication": { "large_file_threshold": 42 } })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["duplication"]["large_file_threshold"], 42);

    // Config validate accepts a valid candidate and rejects an invalid one.
    let (status, v) = request(
        &app,
        "POST",
        "/api/v1/config/validate",
        Some(json!({ "description": { "backend": "bogus" } })),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(v["error"]["code"], "validation_failed");

    // Cache clear is a 204 and emits cache.cleared.
    let (status, _) = request(&app, "DELETE", "/api/v1/cache", None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    // Audit when empty is an empty array.
    let (status, v) = request(&app, "GET", "/api/v1/audit", None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(v.as_array().unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cache_list_contract() {
    use homelinter_service::cache::{cache_key, KIND_DESCRIPTION, KIND_GEOCODE};

    let env = Env::new("cache-list");
    let app = env.service();

    {
        let cache = app.cache.lock().await;
        let geo = cache_key(
            KIND_GEOCODE,
            "nominatim:openstreetmap",
            &[1u8; 32],
            &[2u8; 32],
        );
        cache
            .put(
                &geo,
                KIND_GEOCODE,
                "nominatim:openstreetmap",
                &[2u8; 32],
                "Berlin",
            )
            .unwrap();
        let desc = cache_key(
            KIND_DESCRIPTION,
            "description:ollama:mistral",
            &[3u8; 32],
            &[4u8; 32],
        );
        cache
            .put(
                &desc,
                KIND_DESCRIPTION,
                "description:ollama:mistral",
                &[4u8; 32],
                "a sunset over the sea",
            )
            .unwrap();
    }

    // Unfiltered listing reports both entries and per-kind counts.
    let (status, v) = request(&app, "GET", "/api/v1/cache", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["total"], 2);
    assert_eq!(v["offset"], 0);
    let entries = v["entries"].as_array().unwrap();
    assert_eq!(entries.len(), 2);
    let geo_entry = entries
        .iter()
        .find(|e| e["kind"] == KIND_GEOCODE)
        .expect("geocode entry");
    assert_eq!(geo_entry["provider"], "nominatim:openstreetmap");
    assert_eq!(geo_entry["value"], "Berlin");
    assert_eq!(geo_entry["cache_key"].as_str().unwrap().len(), 64);
    assert_eq!(geo_entry["input_hash"].as_str().unwrap().len(), 64);
    assert_eq!(v["kind_counts"].as_array().unwrap().len(), 2);

    // A kind filter narrows both the entries and the reported total.
    let (status, v) = request(&app, "GET", "/api/v1/cache?kind=description", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["total"], 1);
    assert_eq!(v["entries"].as_array().unwrap().len(), 1);
    assert_eq!(v["entries"][0]["value"], "a sunset over the sea");

    // A kind with no entries is an empty, well-formed response.
    let (status, v) = request(&app, "GET", "/api/v1/cache?kind=author", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["total"], 0);
    assert!(v["entries"].as_array().unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn config_put_null_deletes_and_get_reports_defaults() {
    let env = Env::new("conf-null");
    let app = env.service();

    // GET /config carries the built-in target-path defaults and the
    // geocoder contact field.
    let (_, v) = request(&app, "GET", "/api/v1/config", None).await;
    assert_eq!(
        v["target_path_defaults"]["photo"],
        "Photos/%Y-%m-%d-%H-%M-%S-%l-%s.%e"
    );
    assert_eq!(v["target_path_defaults"]["archive"], "Archives/%Y/%m/%f");
    assert_eq!(v["geocoder"]["user_agent_contact"], serde_json::Value::Null);

    // Set two values...
    let (status, v) = request(
        &app,
        "PUT",
        "/api/v1/config",
        Some(json!({
            "geocoder": { "language": "fr" },
            "target_paths": { "photo": "Gallery/%f" },
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["geocoder"]["language"], "fr");
    assert_eq!(v["target_paths"]["photo"], "Gallery/%f");

    // ...then delete them again with nulls.
    let (status, v) = request(
        &app,
        "PUT",
        "/api/v1/config",
        Some(json!({
            "geocoder": { "language": null },
            "target_paths": { "photo": null },
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    // Deleting the key restores the built-in default language, `en`.
    assert_eq!(v["geocoder"]["language"], "en");
    assert!(v["target_paths"].get("photo").is_none());
    assert_eq!(
        v["target_path_defaults"]["photo"],
        "Photos/%Y-%m-%d-%H-%M-%S-%l-%s.%e"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn session_exposes_batch_config_staleness() {
    let env = Env::new("stale");
    let app = env.service();
    seed_duplicates(&env.home);
    scan_to_ready(&app, &env.home).await;

    // A freshly published batch carries the hash of the config it was
    // scanned under; nothing changed yet, so it equals the live hash.
    let (_, v) = request(&app, "GET", "/api/v1/session", None).await;
    let batch_hash = v["batch_config_hash"].as_str().unwrap().to_string();
    assert_eq!(batch_hash, v["config_hash"].as_str().unwrap());

    // A config save after the scan moves config_hash while the batch keeps
    // the hash of the config that produced it: the batch is stale.
    let (status, _) = request(
        &app,
        "PUT",
        "/api/v1/config",
        Some(json!({ "duplication": { "large_file_threshold": 42 } })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (_, v) = request(&app, "GET", "/api/v1/session", None).await;
    assert_eq!(v["batch_config_hash"].as_str().unwrap(), batch_hash);
    assert_ne!(v["config_hash"].as_str().unwrap(), batch_hash);

    // A new scan regenerates the batch under the current config: fresh again.
    scan_to_ready(&app, &env.home).await;
    let (_, v) = request(&app, "GET", "/api/v1/session", None).await;
    assert_eq!(
        v["batch_config_hash"].as_str().unwrap(),
        v["config_hash"].as_str().unwrap()
    );
}

// ---------------------------------------------------------------------------
// End-to-end HTTP + SSE + graceful shutdown on a real listener
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sse_snapshot_and_shutdown_over_http() {
    let env = Env::new("sse");
    let built = build_service(ServiceOptions {
        cli_config: Some(env._config.clone()),
        home_override: Some(env.home.clone()),
        data_dir_override: Some(env.data.clone()),
        web_dir: None,
        ..Default::default()
    })
    .unwrap();
    let app = built.app.clone();

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let serve = tokio::spawn(async move {
        let _ = homelinter_service::serve(listener, app.clone(), built.shutdown_rx).await;
    });

    // GET /health over a real socket.
    let health = raw_http_get(addr, "/api/v1/health").await;
    assert!(health.starts_with("HTTP/1.1 200"));
    assert!(health.contains("\"state\":\"idle\"") || health.contains("\"state\": \"idle\""));

    // SSE: connect and read the initial snapshot (state idle).
    let mut sse = tokio::net::TcpStream::connect(addr).await.unwrap();
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    sse.write_all(
        b"GET /api/v1/events HTTP/1.1\r\nHost: localhost\r\nAccept: text/event-stream\r\n\r\n",
    )
    .await
    .unwrap();
    let mut buf = Vec::new();
    let mut chunk = [0u8; 512];
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            break;
        }
        let n = tokio::time::timeout(remaining, sse.read(&mut chunk))
            .await
            .unwrap_or(Ok(0))
            .unwrap_or(0);
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
        if buf
            .windows(b"event: session.state".len())
            .any(|w| w == b"event: session.state")
        {
            break;
        }
    }
    let text = String::from_utf8_lossy(&buf);
    assert!(
        text.contains("event: session.state"),
        "SSE snapshot missing session.state event: {text:?}"
    );

    // Close the SSE connection before requesting shutdown so the graceful
    // drain is not held open by the event stream.
    drop(sse);

    // POST shutdown; server stops gracefully.
    raw_http_post(addr, "/api/v1/shutdown", "").await;
    let _ = tokio::time::timeout(Duration::from_secs(5), serve)
        .await
        .expect("server shut down");
}

async fn raw_http_get(addr: std::net::SocketAddr, path: &str) -> String {
    raw_http_request(addr, "GET", path, "").await
}

async fn raw_http_post(addr: std::net::SocketAddr, path: &str, body: &str) -> String {
    raw_http_request(addr, "POST", path, body).await
}

async fn raw_http_request(
    addr: std::net::SocketAddr,
    method: &str,
    path: &str,
    body: &str,
) -> String {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut s = tokio::net::TcpStream::connect(addr).await.unwrap();
    let req = format!(
        "{method} {path} HTTP/1.1\r\nHost: localhost\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    s.write_all(req.as_bytes()).await.unwrap();
    let mut out = Vec::new();
    s.read_to_end(&mut out).await.unwrap();
    String::from_utf8_lossy(&out).into_owned()
}

// ---------------------------------------------------------------------------
// Static SPA serving
// ---------------------------------------------------------------------------

struct WebEnv {
    home: PathBuf,
    data: PathBuf,
    web: PathBuf,
    _config: PathBuf,
}

impl WebEnv {
    fn new(tag: &str) -> WebEnv {
        let base = unique_dir(tag);
        let home = base.join("home");
        let data = base.join("data");
        let web = base.join("web");
        let config = base.join("config.toml");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&data).unwrap();
        std::fs::create_dir_all(web.join("assets")).unwrap();
        std::fs::write(&config, "[output]\nformat = \"json\"\n").unwrap();
        std::fs::write(
            web.join("index.html"),
            "<!doctype html><html><body>homelinter</body></html>",
        )
        .unwrap();
        std::fs::write(web.join("assets/app.js"), "export const ok = 1;").unwrap();
        WebEnv {
            home,
            data,
            web,
            _config: config,
        }
    }

    fn service(&self) -> AppHandle {
        let opts = ServiceOptions {
            cli_config: Some(self._config.clone()),
            home_override: Some(self.home.clone()),
            data_dir_override: Some(self.data.clone()),
            web_dir: Some(self.web.clone()),
            ..Default::default()
        };
        build_service(opts).unwrap().app
    }
}

impl Drop for WebEnv {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(self.home.parent().unwrap());
    }
}

/// Raw response helper that keeps headers for static file assertions.
async fn request_raw(
    app: &AppHandle,
    method: &str,
    uri: &str,
) -> (StatusCode, axum::http::HeaderMap, Vec<u8>) {
    let router = homelinter_service::router(app.clone());
    let resp = router
        .oneshot(
            Request::builder()
                .method(method)
                .uri(uri)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .expect("oneshot response");
    let status = resp.status();
    let headers = resp.headers().clone();
    let bytes = resp
        .into_body()
        .collect()
        .await
        .unwrap()
        .to_bytes()
        .to_vec();
    (status, headers, bytes)
}

#[tokio::test]
async fn spa_serves_index_with_csp() {
    let env = WebEnv::new("spa-index");
    let app = env.service();
    let (status, headers, bytes) = request_raw(&app, "GET", "/").await;
    assert_eq!(status, StatusCode::OK);
    assert!(headers
        .get("content-type")
        .unwrap()
        .to_str()
        .unwrap()
        .contains("text/html"));
    assert_eq!(
        headers.get("content-security-policy").unwrap().to_str().unwrap(),
        "default-src 'self'; script-src 'self'; style-src 'self'; img-src 'self' data:; font-src 'self'; connect-src 'self'; object-src 'none'; frame-src 'self' https://www.openstreetmap.org; base-uri 'none'; form-action 'none'; frame-ancestors 'none'"
    );
    assert_eq!(
        headers.get("cache-control").unwrap().to_str().unwrap(),
        "no-cache"
    );
    assert!(String::from_utf8_lossy(&bytes).contains("homelinter"));
}

#[tokio::test]
async fn spa_serves_hashed_assets_immutable() {
    let env = WebEnv::new("spa-assets");
    let app = env.service();
    let (status, headers, bytes) = request_raw(&app, "GET", "/assets/app.js").await;
    assert_eq!(status, StatusCode::OK);
    assert!(headers
        .get("content-type")
        .unwrap()
        .to_str()
        .unwrap()
        .contains("javascript"));
    assert_eq!(
        headers.get("cache-control").unwrap().to_str().unwrap(),
        "public, max-age=31536000, immutable"
    );
    assert_eq!(String::from_utf8_lossy(&bytes), "export const ok = 1;");
}

#[tokio::test]
async fn spa_falls_back_to_index_for_routes() {
    let env = WebEnv::new("spa-route");
    let app = env.service();
    // Deep link with no file extension renders the SPA shell.
    let (status, headers, bytes) = request_raw(&app, "GET", "/some/deep/route").await;
    assert_eq!(status, StatusCode::OK);
    assert!(headers
        .get("content-type")
        .unwrap()
        .to_str()
        .unwrap()
        .contains("text/html"));
    assert!(String::from_utf8_lossy(&bytes).contains("homelinter"));
}

#[tokio::test]
async fn spa_missing_real_file_is_json_404() {
    let env = WebEnv::new("spa-missing");
    let app = env.service();
    let (status, _, bytes) = request_raw(&app, "GET", "/assets/does-not-exist.js").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let v: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(v["error"]["code"], "not_found");
}

#[tokio::test]
async fn spa_rejects_path_traversal() {
    // Literal dot segments are normalized away by hyper before the handler
    // runs, so the real attack surface is percent-encoded traversal and
    // dot segments sent over a raw socket. Both must never read outside
    // web_dir.

    let env = WebEnv::new("spa-traversal");
    let app = env.service();
    let (status, _, _) = request_raw(&app, "GET", "/%2e%2e/etc/passwd").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, _, _) = request_raw(&app, "GET", "/assets/%2e%2e/app.js").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, _, _) = request_raw(&app, "GET", "/%2e/etc/passwd").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // Over a real socket, a crafted request line must not serve files from
    // outside web_dir (hyper normalizes /../x to /x; /x does not exist).
    let secret = env.home.parent().unwrap().join("secret.txt");
    std::fs::write(&secret, "TOP-SECRET-DO-NOT-SERVE").unwrap();
    let built = build_service(ServiceOptions {
        cli_config: Some(env._config.clone()),
        home_override: Some(env.home.clone()),
        data_dir_override: Some(env.data.clone()),
        web_dir: Some(env.web.clone()),
        ..Default::default()
    })
    .unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let serve = tokio::spawn(async move {
        let _ = homelinter_service::serve(listener, built.app.clone(), built.shutdown_rx).await;
    });
    let resp = raw_http_get(addr, "/../secret.txt").await;
    assert!(
        !resp.contains("TOP-SECRET"),
        "traversal served outside file: {resp}"
    );
    let resp2 = raw_http_get(addr, "/%2e%2e/secret.txt").await;
    assert!(
        !resp2.contains("TOP-SECRET"),
        "encoded traversal served outside file: {resp2}"
    );
    serve.abort();
}

#[tokio::test]
async fn spa_unknown_api_route_keeps_json_envelope() {
    let env = WebEnv::new("spa-apiroute");
    let app = env.service();
    let (status, _, bytes) = request_raw(&app, "GET", "/api/v1/does-not-exist").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let v: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(v["error"]["code"], "not_found");
}

#[tokio::test]
async fn spa_without_web_dir_serves_no_ui() {
    let env = Env::new("spa-noweb");
    let app = env.service(); // ServiceOptions without web_dir
    let (status, _, bytes) = request_raw(&app, "GET", "/").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let v: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(v["error"]["code"], "not_found");
}

#[tokio::test]
async fn spa_rejects_non_get_methods() {
    let env = WebEnv::new("spa-method");
    let app = env.service();
    let (status, _, _) = request_raw(&app, "POST", "/").await;
    assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED);
}
