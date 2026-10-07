//! Guards the versioned `defaults.toml` dump at the repo root: it must load
//! and validate against the current schema, document every key, and be a
//! faithful dump of the built-in defaults, so that loading it is behaviorally
//! equivalent to supplying no configuration file at all.

use std::path::PathBuf;

use homelinter_model::config::{effective_config_json, load_config_chain, ConfigChain, RawConfig};

#[test]
fn config_template_loads_and_validates() {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../defaults.toml");
    assert!(path.is_file(), "missing template: {}", path.display());
    let loaded = load_config_chain(&ConfigChain::default(), Some(&path)).expect(
        "repo-root defaults.toml template must load and validate against the current schema",
    );

    // Structural check: every top-level section of the schema must appear
    // in the template so it cannot silently lose a section on a schema
    // change.
    let text = std::fs::read_to_string(&path).expect("template is readable");
    for section in [
        "target_paths",
        "geocoder",
        "description",
        "document_extraction",
        "duplication",
        "scan",
        "output",
        "cache",
        "limits",
    ] {
        assert!(
            text.contains(&format!("[{section}]")),
            "template is missing the [{section}] section; update it alongside any schema change"
        );
    }

    // Every configurable key must appear (as an active assignment or a
    // commented default), so a new key cannot ship without being documented
    // in the dump. Keys whose default is "unset" (secrets and optional
    // overrides) are commented but still present.
    for key in [
        // [target_paths]: asserted to equal the per-kind built-in default in
        // `assert_defaults_dump_matches_builtin`.
        // [geocoder]
        "enabled",
        "service",
        "endpoint",
        "api_key",
        "language",
        "user_agent_contact",
        "neighbourhood_cities",
        "rate_limit_ms",
        // [description]
        "model",
        "max_words",
        "prompt",
        "max_image_dimension",
        "max_video_frames",
        "max_context",
        // [document_extraction]
        "pdf",
        "docx",
        // [duplication]
        "large_file_threshold",
        // [scan]
        "exclude_paths",
        "exclude_extensions",
        "hidden_definition",
        // [output]
        "format",
        // [cache] reuses `enabled`; [limits]
        "provider_timeout_seconds",
        "provider_concurrency",
        "tool_timeout_seconds",
        "tool_stdout_max_bytes",
    ] {
        assert!(
            text.contains(key),
            "template is missing `{key}`; update it alongside any schema change"
        );
    }

    assert_defaults_dump_matches_builtin(&loaded.raw);
}

/// `defaults.toml` must resolve to exactly the built-in defaults. A change to
/// any default fails here until the dump is updated. The comparison uses the
/// effective (default-resolved) configuration, so an explicit default value
/// equals an omitted one; the only normalizations are the materialized
/// `target_paths` map and the conditionally emitted `description.enabled`.
fn assert_defaults_dump_matches_builtin(dumped: &RawConfig) {
    // Secrets are excluded from the effective JSON, so assert them directly:
    // the dump must leave every API key unset.
    assert_eq!(dumped.geocoder.api_key, None);
    assert_eq!(dumped.description.base.api_key, None);

    let mut dumped_json = effective_config_json(dumped, None);
    let mut builtin_json = effective_config_json(&RawConfig::default(), None);

    // The dump materializes every kind; the built-in map is empty and each
    // kind falls back to `target_path_defaults`.
    assert_eq!(
        dumped_json["target_paths"],
        dumped_json["target_path_defaults"]
    );
    dumped_json.as_object_mut().unwrap().remove("target_paths");
    builtin_json.as_object_mut().unwrap().remove("target_paths");

    // `description.enabled` is only emitted when the base entry sets it; the
    // built-in base leaves it unset, so drop it from both sides.
    for json in [&mut dumped_json, &mut builtin_json] {
        if let Some(desc) = json.get_mut("description").and_then(|v| v.as_object_mut()) {
            desc.remove("enabled");
        }
    }

    assert_eq!(
        dumped_json, builtin_json,
        "defaults.toml must resolve to the built-in defaults"
    );
}
