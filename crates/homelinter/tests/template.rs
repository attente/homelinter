//! Guards the versioned `defaults.toml` template at the repo root: it must
//! always load and validate against the current configuration schema, so it
//! cannot drift when the schema changes.

use std::path::PathBuf;

#[test]
fn config_template_loads_and_validates() {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../defaults.toml");
    assert!(
        path.is_file(),
        "missing template: {}",
        path.display()
    );
    let loaded = homelinter_model::config::load_config(Some(&path))
        .expect("repo-root defaults.toml template must load and validate against the current schema");

    assert!(
        !loaded.raw.target_paths.is_empty(),
        "template must set at least one target path"
    );

    // Structural check: every top-level section of the schema must appear
    // in the template so it cannot silently lose a section on a schema
    // change.
    let text = std::fs::read_to_string(&path).expect("template is readable");
    for section in [
        "target_paths",
        "llm",
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
}
