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

/// One suggestion as rendered by the interactive pager listing.
fn suggestion_line(s: &Value) -> String {
    let id = s["id"].as_str().unwrap_or("?");
    let ty = s["type"].as_str().unwrap_or("?");
    let kind = s["asset_kind"].as_str().unwrap_or("?");
    let target = s["target"].as_str().unwrap_or("?");
    let mut line = format!("[{ty}] {kind}: {target}");
    if let Some(np) = s["new_path"].as_str() {
        line.push_str(&format!("\n      -> {np}"));
    }
    if let Some(reason) = s["reason"].as_str() {
        line.push_str(&format!("\n      {reason}"));
    }
    if let Some(st) = s["status"].as_str() {
        line.push_str(&format!("\n      status: {st}"));
    }
    line.push_str(&format!("\n      id: {id}"));
    line
}

/// Print the suggestion list for interactive review.
pub fn print_suggestions(items: &[Value]) {
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    for (i, s) in items.iter().enumerate() {
        let _ = writeln!(out, "--- {} ---\n{}", i + 1, suggestion_line(s));
    }
    let _ = out.flush();
}

/// Print an execution report per the selected format.
pub fn emit_report(format: OutputFormat, report: &Value) -> std::io::Result<()> {
    match format {
        OutputFormat::Pretty => {
            let mut out = std::io::stdout().lock();
            writeln!(out, "Execution report:")?;
            writeln!(
                out,
                "  scan_root: {}",
                report["scan_root"].as_str().unwrap_or("?")
            )?;
            writeln!(
                out,
                "  platform:  {}",
                report["platform"].as_str().unwrap_or("?")
            )?;
            writeln!(out, "  partial:   {}", report["partial"])?;
            writeln!(
                out,
                "  entries:   {}",
                report["entries"].as_array().map(|a| a.len()).unwrap_or(0)
            )?;
            if let Some(entries) = report["entries"].as_array() {
                for e in entries {
                    let op = e["operations"]
                        .as_array()
                        .and_then(|a| a.first())
                        .cloned()
                        .unwrap_or(Value::Null);
                    writeln!(
                        out,
                        "    {}: {}",
                        e["suggestion_id"].as_str().unwrap_or("?"),
                        e["result"].as_str().unwrap_or("?")
                    )?;
                    if op.is_object() {
                        let detail = op["detail"].as_object();
                        let keys = detail.map(|m| {
                            m.keys()
                                .filter(|k| {
                                    matches!(
                                        k.as_str(),
                                        "source" | "destination" | "reason" | "error"
                                    )
                                })
                                .map(|k| format!("{k}={}", op["detail"][k]))
                                .collect::<Vec<_>>()
                        });
                        if let Some(ks) = keys {
                            for k in ks {
                                writeln!(out, "      {k}")?;
                            }
                        }
                    }
                }
            }
            out.flush()
        }
        _ => emit_document(format, report),
    }
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
