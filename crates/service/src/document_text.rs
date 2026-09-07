//! Document text extraction for the `%s` description provider, driven by
//! the `[document_extraction]` configuration section.
//!
//! Extraction runs in the background provider worker, only for a
//! description job that actually needs to invoke the provider (cache
//! misses skip it), so a scan is never blocked on text extraction and a
//! cached description is reused without re-reading the document.
//!
//! The command for a format is the configured argv when present; an empty
//! array disables extraction for that format; otherwise a runtime default
//! is selected when the tool is on the PATH (`pdftotext` for PDF, `pandoc`
//! for docx/odt/epub). The literal `{path}` argument is replaced by the
//! absolute input path; commands run directly without a shell under the
//! `[limits]` discipline (`tool_timeout_seconds`, `tool_stdout_max_bytes`,
//! sanitized environment). Any failure - spawn error, non-zero exit,
//! timeout, output over the cap, missing tool - degrades to no text:
//! the description provider sees the unit path and kind only and `%s`
//! resolves to its fallback while analysis continues.

use std::path::Path;
use std::time::Duration;

use homelinter_analyze::run_capture;
use homelinter_model::config::RawConfig;
use homelinter_scan::ToolSet;

/// Extract plain text from `path` for the description provider. `None`
/// when extraction is disabled for the format, no command is configured
/// and no default tool is available, the file has no extension, or the
/// command fails; never an error.
pub fn extract_document_text(cfg: &RawConfig, path: &Path, tools: &ToolSet) -> Option<String> {
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    let argv = match cfg.document_extraction.get(&ext) {
        // An empty array explicitly disables extraction for this format.
        Some(argv) if argv.is_empty() => return None,
        Some(argv) => argv.clone(),
        None => default_argv(&ext, tools)?,
    };
    let program = argv.first()?;
    let path_str = path.to_str()?.to_string();
    // `{path}` is validated at config time to be a complete argument of
    // its own and to occur exactly once; substitute it verbatim.
    let args: Vec<String> = argv
        .iter()
        .map(|a| {
            if a == "{path}" {
                path_str.clone()
            } else {
                a.clone()
            }
        })
        .collect();
    let arg_refs: Vec<&str> = args.iter().skip(1).map(String::as_str).collect();
    let bytes = run_capture(
        program,
        &arg_refs,
        Duration::from_secs(cfg.tool_timeout()),
        cfg.tool_stdout_max_bytes(),
    )?;
    let text = String::from_utf8_lossy(&bytes).trim().to_string();
    if text.is_empty() {
        None
    } else {
        Some(text)
    }
}

/// The runtime default command for a format, when the tool is on the PATH.
/// Format keys are file extensions (`pdf`, `docx`, `odt`, `epub`, ...).
/// The argv mirrors the shipped `defaults.toml` `[document_extraction]`
/// entries so an unconfigured install behaves identically to the template:
/// `pdftotext -layout` writes to stdout with `-` as the output file, and
/// `pandoc -t plain` renders office/e-book text to stdout. Built-in Rust
/// text-extraction fallbacks (pdf-extract, docx-rs) are future work behind
/// this same seam.
fn default_argv(ext: &str, tools: &ToolSet) -> Option<Vec<String>> {
    match ext {
        "pdf" if tools.has("pdftotext") => Some(vec![
            "pdftotext".into(),
            "-layout".into(),
            "{path}".into(),
            "-".into(),
        ]),
        "docx" | "odt" | "epub" if tools.has("pandoc") => Some(vec![
            "pandoc".into(),
            "{path}".into(),
            "-t".into(),
            "plain".into(),
        ]),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::process::Command;

    fn cfg_with(argv: Option<&[&str]>) -> RawConfig {
        let mut cfg = RawConfig::default();
        if let Some(argv) = argv {
            cfg.document_extraction.insert(
                "txt".to_string(),
                argv.iter().map(|s| s.to_string()).collect(),
            );
        }
        cfg
    }

    fn tools_with(names: &[&str]) -> ToolSet {
        let mut t = ToolSet::detect();
        t.available = names.iter().map(|s| s.to_string()).collect();
        t
    }

    #[test]
    fn extracts_via_configured_command() {
        // cat is coreutils and present wherever this test suite runs;
        // skip when it is not, mirroring the hg adapter integration test.
        if Command::new("cat").arg("--version").output().is_err() {
            return;
        }
        let dir = std::env::temp_dir().join(format!("homelinter-doctext-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("note.txt");
        std::fs::write(&path, "invoice for march\n").unwrap();
        let cfg = cfg_with(Some(&["cat", "{path}"]));
        let text = extract_document_text(&cfg, &path, &tools_with(&["cat"])).unwrap();
        assert_eq!(text, "invoice for march");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn empty_argv_disables_extraction() {
        let cfg = cfg_with(Some(&[]));
        let path = PathBuf::from("/tmp/x.txt");
        // Disabled regardless of tool availability.
        assert_eq!(extract_document_text(&cfg, &path, &ToolSet::detect()), None);
    }

    #[test]
    fn unconfigured_format_without_default_tool_is_none() {
        let cfg = RawConfig::default();
        // rtf has no runtime default, so no tool can satisfy it.
        let path = PathBuf::from("/tmp/x.rtf");
        assert_eq!(
            extract_document_text(&cfg, &path, &tools_with(&["pdftotext", "pandoc"])),
            None
        );
    }

    #[test]
    fn default_pdf_requires_pdftotext() {
        let cfg = RawConfig::default();
        let path = PathBuf::from("/tmp/x.pdf");
        // pdftotext is the only default for pdf; without it there is no
        // command and extraction degrades to None.
        assert_eq!(
            extract_document_text(&cfg, &path, &tools_with(&["pandoc"])),
            None
        );
        let _ = extract_document_text(&cfg, &path, &tools_with(&["pdftotext"]));
    }

    #[test]
    fn missing_file_degrades_to_none() {
        let cfg = cfg_with(Some(&["cat", "{path}"]));
        let path = PathBuf::from("/nonexistent/homelinter-nope.txt");
        assert_eq!(
            extract_document_text(&cfg, &path, &tools_with(&["cat"])),
            None
        );
    }
}
