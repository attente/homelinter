//! Rendering of service responses for the three output formats.
//!
//! The final document goes to stdout; progress and notices go to stderr so a
//! machine-readable stdout carries only the final document.

use std::io::Write;

use homelinter_model::config::OutputFormat;
use serde_json::Value;

/// Print a progress/notice line to stderr.
pub fn note(msg: &str) {
    eprintln!("{msg}");
}

/// Convert a JSON value into a TOML value for `toml` output mode.
fn json_to_toml(v: &Value) -> toml::Value {
    match v {
        Value::Null => toml::Value::String(String::new()),
        Value::Bool(b) => toml::Value::Boolean(*b),
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                toml::Value::Integer(i)
            } else if let Some(f) = n.as_f64() {
                toml::Value::Float(f)
            } else {
                toml::Value::Integer(0)
            }
        }
        Value::String(s) => toml::Value::String(s.clone()),
        Value::Array(items) => toml::Value::Array(items.iter().map(json_to_toml).collect()),
        Value::Object(map) => {
            let mut out = toml::map::Map::new();
            for (k, val) in map {
                out.insert(k.clone(), json_to_toml(val));
            }
            toml::Value::Table(out)
        }
    }
}

/// Emit the final machine-readable document.
pub fn emit_document(format: OutputFormat, value: &Value) -> std::io::Result<()> {
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    match format {
        OutputFormat::Json => {
            serde_json::to_writer_pretty(&mut out, value)?;
            writeln!(out)?;
        }
        OutputFormat::Toml => {
            let toml_v = json_to_toml(value);
            let text = toml::to_string_pretty(&toml_v)
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e.to_string()))?;
            write!(out, "{text}")?;
        }
        OutputFormat::Pretty => {
            // Pretty callers use the targeted printers below; the generic
            // fallback emits indented JSON with a header.
            writeln!(out, "{}", serde_json::to_string_pretty(value)?)?;
        }
    }
    out.flush()
}

/// Print an execution report per the selected format.
pub fn emit_report(format: OutputFormat, report: &Value) -> std::io::Result<()> {
    match format {
        OutputFormat::Pretty => {
            let mut out = std::io::stdout().lock();
            out.write_all(report_text(report).as_bytes())?;
            out.flush()
        }
        _ => emit_document(format, report),
    }
}

/// Human-readable byte count using binary units, so a reclaimed-space
/// figure stays readable without losing the exact number in machine output.
pub(crate) fn bytes_label(bytes: u64) -> String {
    const UNITS: [&str; 7] = ["B", "KiB", "MiB", "GiB", "TiB", "PiB", "EiB"];
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    let text = if value >= 10.0 {
        format!("{value:.0}")
    } else {
        format!("{value:.1}")
    };
    format!("{text} {}", UNITS[unit])
}

/// Human-readable execution report: one entry per suggestion with the
/// operation's paths and any failure or partial-completion detail.
fn report_text(report: &Value) -> String {
    use std::fmt::Write as _;

    let mut s = String::new();
    let _ = writeln!(s, "Execution report:");
    let _ = writeln!(
        s,
        "  scan_root: {}",
        report["scan_root"].as_str().unwrap_or("?")
    );
    let _ = writeln!(
        s,
        "  platform:  {}",
        report["platform"].as_str().unwrap_or("?")
    );
    let _ = writeln!(s, "  partial:   {}", report["partial"]);
    let _ = writeln!(
        s,
        "  entries:   {}",
        report["entries"].as_array().map(|a| a.len()).unwrap_or(0)
    );
    let reclaimed = report["reclaimed_bytes"].as_u64().unwrap_or(0);
    if reclaimed > 0 {
        let _ = writeln!(s, "  reclaimed: {}", bytes_label(reclaimed));
    }
    for e in report["entries"].as_array().into_iter().flatten() {
        let _ = writeln!(
            s,
            "    {}: {}",
            e["suggestion_id"].as_str().unwrap_or("?"),
            e["result"].as_str().unwrap_or("?")
        );
        for op in e["operations"].as_array().into_iter().flatten() {
            for key in ["source", "destination", "detail"] {
                if let Some(v) = op[key].as_str() {
                    let _ = writeln!(s, "      {key}={v}");
                }
            }
        }
    }
    s
}

/// Confirm a written export script.
pub fn emit_export(
    format: OutputFormat,
    path: &str,
    platform: &str,
    count: u64,
) -> std::io::Result<()> {
    match format {
        OutputFormat::Pretty => {
            let mut out = std::io::stdout().lock();
            writeln!(
                out,
                "Wrote {count} operations as a {platform} script to {path}"
            )?;
            writeln!(out, "Review the script before running it.")?;
            out.flush()
        }
        _ => emit_document(
            format,
            &serde_json::json!({ "path": path, "platform": platform, "count": count }),
        ),
    }
}

/// Emit the readiness line per the transport contract.
pub fn readiness(port: u16) {
    let mut out = std::io::stdout().lock();
    let _ = writeln!(out, "homelinter: listening on 127.0.0.1:{port}");
    let _ = out.flush();
}
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn report_text_prints_operation_paths_and_string_detail() {
        let report = json!({
            "scan_root": "/home/alice",
            "platform": "linux",
            "partial": true,
            "reclaimed_bytes": 2048,
            "entries": [{
                "suggestion_id": "s1",
                "result": "partial",
                "operations": [{
                    "operation": "move",
                    "source": "/home/alice/a.bin",
                    "destination": "/home/alice/Documents/a.bin",
                    "status": "completed",
                    "detail": "copy verified and published but removing source failed"
                }]
            }]
        });
        let text = report_text(&report);
        assert!(text.contains("s1: partial"), "{text}");
        assert!(text.contains("source=/home/alice/a.bin"), "{text}");
        assert!(
            text.contains("destination=/home/alice/Documents/a.bin"),
            "{text}"
        );
        assert!(text.contains("detail=copy verified"), "{text}");
        assert!(text.contains("reclaimed: 2.0 KiB"), "{text}");
    }

    #[test]
    fn bytes_label_uses_binary_units() {
        assert_eq!(bytes_label(0), "0 B");
        assert_eq!(bytes_label(1023), "1023 B");
        assert_eq!(bytes_label(1024), "1.0 KiB");
        assert_eq!(bytes_label(1536), "1.5 KiB");
        assert_eq!(bytes_label(10 * 1024), "10 KiB");
        assert_eq!(bytes_label(3 * 1024 * 1024), "3.0 MiB");
    }
}
