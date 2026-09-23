//! PDF metadata reader via the poppler `pdfinfo` tool.
//!
//! `pdfinfo` is the same poppler family as `pdftotext`and is detected at
//! runtime like every other optional tool. It reports title, author,
//! creation date, and page count from the PDF info dict,xand it is the only
//! document extractor that supplies a page count:`docx`/`odt`/`epub` do not
//! define one,so no page-count machinery is contorted around them (the
//! built-in container reader in `doc_container.rs` handles those formats).
use std::path::Path;

use homelinter_model::asset_kind::AssetKind;

use crate::dateparse::DateParts;
use crate::extractor::{EmbeddedMeta, ExtractionContext, MetadataExtractor};
use crate::tool_runner::run_capture;

/// Preferred PDF metadata extractor when `pdfinfo` is on the PATH.
pub struct PdfInfoExtractor;

impl MetadataExtractor for PdfInfoExtractor {
    fn name(&self) -> &'static str {
        "pdfinfo"
    }

    fn extract(&self, path: &Path, kind: AssetKind, ctx: &ExtractionContext) -> EmbeddedMeta {
        let mut meta = EmbeddedMeta::default();
        let Some(path_str) = path.to_str() else {
            return meta;
        };
        let is_pdf = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.eq_ignore_ascii_case("pdf"))
            .unwrap_or(false);
        if !matches!(kind, AssetKind::Document) || !is_pdf {
            return meta;
        }
        // Prefer `-rawdates` (the undecoded PDF date string) because the
        // default output localizes dates, which is timezone-dependent and
        // loses the original clock time. Older poppler builds lack the
        // flag, so fall back to the plain output on an empty result.
        let mut bytes = run_capture(
            "pdfinfo",
            &["-rawdates", path_str],
            ctx.tool_timeout,
            ctx.tool_stdout_max_bytes,
        );
        let empty = bytes
            .as_deref()
            .map(|b| parse_pairs(&String::from_utf8_lossy(b)).is_empty())
            .unwrap_or(true);
        if empty {
            bytes = run_capture(
                "pdfinfo",
                &[path_str],
                ctx.tool_timeout,
                ctx.tool_stdout_max_bytes,
            );
        }
        let Some(bytes) = bytes else {
            return meta;
        };
        let text = String::from_utf8_lossy(&bytes);
        for (key, value) in parse_pairs(&text) {
            match key.as_str() {
                "title" => {
                    if meta.title.is_none() && !value.is_empty() {
                        meta.title = Some(cap(&value, 512));
                    }
                }
                "author" => {
                    if meta.author.is_none() && !value.is_empty() {
                        meta.author = Some(cap(&value, 256));
                    }
                }
                "pages" => meta.page_count = value.parse().ok(),
                "creationdate" => {
                    if meta.date_taken.is_none() {
                        meta.date_taken = pdf_date_parts(&value);
                    }
                }
                _ => {}
            }
        }
        meta
    }
}

/// Split `pdfinfo`'s `Key: value` lines, lowercasing keys and trimming
/// values. Unknown keys are dropped.
fn parse_pairs(text: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for line in text.lines() {
        let Some((k, v)) = line.split_once(':') else {
            continue;
        };
        let key = k.trim().to_ascii_lowercase();
        let value = v.trim();
        if value.is_empty() || value == "no" {
            continue;
        }
        out.push((key, value.to_string()));
    }
    out
}

fn cap(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

/// Parse a PDF date string (`D:YYYYMMDDHHmmSS...`) into calendar parts.
/// A value that lacks a month and a day is rejected rather than guessed.
fn pdf_date_parts(s: &str) -> Option<DateParts> {
    let s = s.trim().strip_prefix("D:").unwrap_or(s.trim());
    let digits: String = s
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .take(14)
        .collect();
    if digits.len() < 8 {
        return None;
    }
    let y: i32 = digits[0..4].parse().ok()?;
    let mo: u32 = digits[4..6].parse().ok()?;
    let d: u32 = digits[6..8].parse().ok()?;
    let (h, mi, se) = if digits.len() >= 14 {
        (
            digits[8..10].parse().ok()?,
            digits[10..12].parse().ok()?,
            digits[12..14].parse().ok()?,
        )
    } else {
        (0, 0, 0)
    };
    if h > 23 || mi > 59 || se > 59 {
        return None;
    }
    if !DateParts::valid_year(y) {
        return None;
    }
    chrono::NaiveDate::from_ymd_opt(y, mo, d).map(|_| DateParts {
        year: y,
        month: mo,
        day: d,
        hour: h,
        minute: mi,
        second: se,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    use homelinter_scan::ToolSet;

    /// A tiny single-page PDF with a valid xref table and an Info dict.
    /// Offsets are computed while assembling each object, so the xref is
    /// byte-accurate without hand counting.
    fn build_pdf() -> Vec<u8> {
        let entries = [
            "<< /Type /Catalog /Pages 2 0 R >>",
            "<< /Type /Pages /Kids [3 0 R] /Count 1 >>",
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] >>",
            "<< /Title (My Report) /Author (Jane Doe) /CreationDate (D:20240315120000Z) >>",
        ];
        let mut buf = Vec::new();
        buf.extend_from_slice(b"%PDF-1.4\n");
        let mut offsets = [0usize; 5];
        for (num, body) in entries.iter().enumerate() {
            let num = num + 1;
            offsets[num] = buf.len();
            buf.extend_from_slice(format!("{num} 0 obj\n{body}\nendobj\n").as_bytes());
        }
        let xref_pos = buf.len();
        buf.extend_from_slice(b"xref\n0 5\n");
        buf.extend_from_slice(b"0000000000 65535 f \n");
        for offset in offsets.iter().skip(1) {
            buf.extend_from_slice(format!("{offset:010} 00000 n \n").as_bytes());
        }
        buf.extend_from_slice(b"trailer\n<< /Size 5 /Root 1 0 R /Info  4 0 R >>\nstartxref\n");
        buf.extend_from_slice(format!("{xref_pos}\n").as_bytes());
        buf.extend_from_slice(b"%%EOF\n");
        buf
    }

    #[test]
    fn reads_pdf_title_author_pages_and_date() {
        let ts = ToolSet::detect();
        if !ts.has("pdfinfo") {
            eprintln!("skipping pdfinfo test: tool not found");
            return;
        }
        let dir = std::env::temp_dir().join(format!(
            "hl-pdf-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("report.pdf");
        std::fs::write(&path, build_pdf()).unwrap();
        let ctx = ExtractionContext {
            tool_timeout: Duration::from_secs(10),
            tool_stdout_max_bytes: 1 << 20,
            ..Default::default()
        };
        let meta = PdfInfoExtractor {}.extract(&path, AssetKind::Document, &ctx);
        assert_eq!(meta.title.as_deref(), Some("My Report"));
        assert_eq!(meta.author.as_deref(), Some("Jane Doe"));
        assert_eq!(meta.page_count, Some(1));
        // The raw date is deterministic when `-rawdates` is supported; on
        // older poppler builds the fallback localizes the value, so only
        // the calendar date is asserted and only when a date was read.
        if let Some(dp) = meta.date_taken {
            assert_eq!((dp.year, dp.month, dp.day), (2024, 3, 15));
        }
        std::fs::remove_dir_all(&dir).ok();
    }
}
