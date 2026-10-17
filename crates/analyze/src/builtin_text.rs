//! In-process document text readers used as the last fallback for the
//! `%s` description provider, when a format has neither a configured
//! `[document_extraction]` command nor a default external tool on the
//! PATH (`pdftotext` for PDF, `pandoc` for office/e-book formats).
//!
//! The readers mirror the external-tool discipline from `run_capture`:
//! text that would exceed the configured stdout cap is a failure
//! (`None`), containers over [`MAX_INPUT_BYTES`] are refused rather than
//! read into memory, and the PDF reader runs under a panic guard so a
//! malformed document can never abort the provider worker. Results are
//! trimmed and blank-line runs are collapsed before returning.
//!
//! PDF text comes from the `pdf-extract` crate (built on `lopdf`). The
//! office/e-book formats are read natively from the text-bearing XML of
//! their zip containers with the same `zip` + `quick-xml` stack as the
//! document metadata reader (`doc_container`): docx body paragraphs
//! (`word/document.xml` `w:p`/`w:t` runs, `w:tab`, `w:br`), odt body
//! paragraphs and headings (`content.xml` `office:text`, including
//! `text:s` space runs), and epub in spine reading order
//! (`META-INF/container.xml` to the OPF package, then each spine item's
//! XHTML with head/navigation/markup stripped). No third-party office
//! crate is used: `docx-rs` is write-oriented and no maintained
//! equivalent exists for odt/epub, so one uniform container reader covers
//! all three.

use std::collections::HashMap;
use std::io::Read;
use std::path::Path;

use quick_xml::events::Event;
use quick_xml::Reader;
use zip::ZipArchive;

/// A zip container archive over an open file, the shared input type of the
/// office/e-book readers.
type Container = ZipArchive<std::fs::File>;

/// Largest single file the in-process readers will open. Text extraction
/// through external tools streams any size, but the built-ins read the
/// container into memory (whole-file for the PDF parser), so a generous
/// cap keeps a pathological input from exhausting memory in the worker.
pub const MAX_INPUT_BYTES: u64 = 128 * 1024 * 1024;

/// Extract plain document text in-process for `ext`, or `None` when the
/// format has no built-in reader, the file is missing or oversized, the
/// text would exceed `max_output_bytes`, or the reader fails. `ext` must
/// be lowercase.
pub fn extract_builtin_text(path: &Path, ext: &str, max_output_bytes: u64) -> Option<String> {
    if max_output_bytes == 0 {
        return None;
    }
    if std::fs::metadata(path).ok()?.len() > MAX_INPUT_BYTES {
        return None;
    }
    let budget = max_output_bytes as usize;
    let raw = match ext {
        "pdf" => pdf_text(path, budget)?,
        "docx" | "odt" | "epub" => container_text(path, ext, budget)?,
        "svg" => svg_text(path, budget)?,
        _ => return None,
    };
    normalize(raw)
}

/// PDF text via `pdf-extract`, under a panic guard. The crate is
/// in-process and does not stream, so a malformed or hostile PDF must not
/// be able to take the worker down; a panic degrades to no text.
fn pdf_text(path: &Path, budget: usize) -> Option<String> {
    let bytes = std::fs::read(path).ok()?;
    if bytes.len() as u64 > MAX_INPUT_BYTES {
        return None;
    }
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        pdf_extract::extract_text_from_mem(&bytes)
    }));
    let text = result.ok()?.ok()?;
    if text.len() > budget {
        return None;
    }
    Some(text)
}

/// SVG text and accessible labels from the XML source. Text extraction is
/// independent of rasterization, so labels remain available when rendering
/// is unsupported or the description backend is text-only.
fn svg_text(path: &Path, budget: usize) -> Option<String> {
    let file = std::fs::File::open(path).ok()?;
    if file.metadata().ok()?.len() > MAX_INPUT_BYTES {
        return None;
    }
    let mut bytes = Vec::new();
    file.take(MAX_INPUT_BYTES + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    if bytes.len() as u64 > MAX_INPUT_BYTES {
        return None;
    }
    let mut reader = Reader::from_reader(bytes.as_slice());
    let mut buf = Vec::new();
    let mut sink = Sink::new(budget);
    let mut capture_depth = 0usize;
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(el)) => {
                if matches!(el.local_name().as_ref(), b"text" | b"title" | b"desc") {
                    if capture_depth == 0 && !sink.out.is_empty() {
                        sink.push(' ');
                    }
                    capture_depth += 1;
                }
                push_svg_labels(&el, reader.decoder(), &mut sink);
            }
            Ok(Event::Empty(el)) => push_svg_labels(&el, reader.decoder(), &mut sink),
            Ok(Event::End(el)) => {
                if matches!(el.local_name().as_ref(), b"text" | b"title" | b"desc") {
                    capture_depth = capture_depth.saturating_sub(1);
                    sink.push(' ');
                }
            }
            Ok(Event::Text(text)) if capture_depth > 0 => {
                if let Ok(value) = text.unescape() {
                    sink.push_str(&value);
                }
            }
            Ok(Event::CData(text)) if capture_depth > 0 => {
                if let Ok(value) = reader.decoder().decode(text.as_ref()) {
                    sink.push_str(&value);
                }
            }
            Ok(Event::Eof) => break,
            Ok(_) => {}
            Err(_) => return None,
        }
        if sink.overflowed {
            return None;
        }
        buf.clear();
    }
    normalize(sink.finish()?)
}

/// Append accessible SVG labels from an element's `aria-label` or editor
/// `label` attribute.
fn push_svg_labels(
    element: &quick_xml::events::BytesStart<'_>,
    decoder: quick_xml::Decoder,
    sink: &mut Sink,
) {
    for attr in element.attributes().with_checks(false).flatten() {
        let key = attr.key.local_name();
        if matches!(key.as_ref(), b"aria-label" | b"label") {
            if let Ok(value) = attr.decode_and_unescape_value(decoder) {
                if !value.trim().is_empty() {
                    if !sink.out.is_empty() {
                        sink.push(' ');
                    }
                    sink.push_str(&value);
                }
            }
        }
    }
}

/// Office/e-book text from the zip container. One XML entry is buffered
/// at a time, bounded by an entry cap derived from the output budget, and
/// the accumulated text is capped by the sink.
fn container_text(path: &Path, ext: &str, budget: usize) -> Option<String> {
    let file = std::fs::File::open(path).ok()?;
    if file.metadata().ok()?.len() > MAX_INPUT_BYTES {
        return None;
    }
    let mut archive = ZipArchive::new(file).ok()?;
    let cap = entry_cap(budget);
    let mut sink = Sink::new(budget);
    match ext {
        "docx" => {
            let xml = read_entry(&mut archive, "word/document.xml", cap)?;
            docx_entry(&xml, &mut sink);
        }
        "odt" => {
            let xml = read_entry(&mut archive, "content.xml", cap)?;
            odt_entry(&xml, &mut sink);
        }
        "epub" => epub_text(&mut archive, &mut sink, cap)?,
        _ => return None,
    }
    sink.finish()
}

/// Read one zip entry fully, refusing an entry that alone exceeds `cap`
/// (an entry that large cannot produce text under the output budget once
/// markup overhead is considered).
fn read_entry(archive: &mut Container, name: &str, cap: u64) -> Option<Vec<u8>> {
    let mut f = archive.by_name(name).ok()?;
    if f.size() > cap {
        return None;
    }
    let mut bytes = Vec::with_capacity(f.size() as usize);
    std::io::Read::read_to_end(&mut f, &mut bytes).ok()?;
    Some(bytes)
}

fn entry_cap(budget: usize) -> u64 {
    (budget as u64).saturating_mul(4).saturating_add(1 << 20)
}

/// Accumulator for extracted text. Appending past the budget marks the
/// extraction as overflowed (a failure, exactly like an external tool
/// whose stdout exceeds the cap) and discards the partial text.
struct Sink {
    out: String,
    budget: usize,
    overflowed: bool,
}

impl Sink {
    fn new(budget: usize) -> Self {
        Sink {
            out: String::new(),
            budget,
            overflowed: false,
        }
    }

    fn push_str(&mut self, s: &str) {
        if self.overflowed || s.is_empty() {
            return;
        }
        if self.out.len().saturating_add(s.len()) > self.budget {
            self.overflowed = true;
            self.out.clear();
        } else {
            self.out.push_str(s);
        }
    }

    fn push(&mut self, ch: char) {
        if self.overflowed {
            return;
        }
        if self.out.len().saturating_add(ch.len_utf8()) > self.budget {
            self.overflowed = true;
            self.out.clear();
        } else {
            self.out.push(ch);
        }
    }

    fn spaces(&mut self, n: usize) {
        for _ in 0..n {
            self.push(' ');
            if self.overflowed {
                return;
            }
        }
    }

    fn finish(self) -> Option<String> {
        if self.overflowed {
            None
        } else {
            Some(self.out)
        }
    }
}

/// Trim the extracted text and collapse runs of three or more newlines to
/// a single blank line; `None` when nothing but whitespace remains.
fn normalize(mut text: String) -> Option<String> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return None;
    }
    text = trimmed.to_string();
    let mut out = String::with_capacity(text.len());
    let mut newlines = 0u32;
    for ch in text.chars() {
        if ch == '\n' {
            newlines += 1;
            if newlines <= 2 {
                out.push('\n');
            }
        } else {
            newlines = 0;
            out.push(ch);
        }
    }
    Some(out)
}

/// docx body text: character data of `w:t` runs inside `w:p` paragraphs,
/// with a newline per paragraph and tabs/breaks mapped from `w:tab` and
/// `w:br`.
fn docx_entry(xml: &[u8], sink: &mut Sink) {
    let mut reader = Reader::from_reader(xml);
    let mut buf = Vec::new();
    let mut in_para = false;
    let mut in_text = false;
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(el)) => match el.local_name().as_ref() {
                b"p" => in_para = true,
                b"t" if in_para => in_text = true,
                _ => {}
            },
            Ok(Event::Empty(el)) => match el.local_name().as_ref() {
                b"tab" => sink.push('\t'),
                b"br" => sink.push('\n'),
                _ => {}
            },
            Ok(Event::Text(t)) => {
                if in_text {
                    if let Ok(s) = t.unescape() {
                        sink.push_str(&s);
                    }
                }
            }
            Ok(Event::End(el)) => match el.local_name().as_ref() {
                b"t" => in_text = false,
                b"p" if in_para => {
                    in_para = false;
                    sink.push('\n');
                }
                _ => {}
            },
            Ok(Event::Eof) => break,
            Ok(_) => {}
            Err(_) => break,
        }
        if sink.overflowed {
            break;
        }
        buf.clear();
    }
}

/// odt body text: character data under `text:p`/`text:h` paragraphs inside
/// the `office:text` body, with `text:s` space runs expanded and a newline
/// per paragraph or heading. Tables and lists nest paragraphs, so walking
/// every paragraph inside the body covers them.
fn odt_entry(xml: &[u8], sink: &mut Sink) {
    let mut reader = Reader::from_reader(xml);
    let mut buf = Vec::new();
    let mut in_body = false;
    let mut para_depth = 0usize;
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(el)) => {
                match el.local_name().as_ref() {
                    b"text" if !in_body => in_body = true,
                    b"p" | b"h" if in_body => para_depth += 1,
                    _ => {}
                }
                if in_body && para_depth > 0 && el.local_name().as_ref() == b"s" {
                    sink.spaces(space_count(&el));
                }
            }
            Ok(Event::Empty(el)) => {
                if in_body && para_depth > 0 && el.local_name().as_ref() == b"s" {
                    sink.spaces(space_count(&el));
                }
            }
            Ok(Event::Text(t)) => {
                if in_body && para_depth > 0 {
                    if let Ok(s) = t.unescape() {
                        sink.push_str(&s);
                    }
                }
            }
            Ok(Event::End(el)) => match el.local_name().as_ref() {
                b"p" | b"h" if para_depth > 0 => {
                    para_depth -= 1;
                    if para_depth == 0 {
                        sink.push('\n');
                    }
                }
                b"text" if in_body => in_body = false,
                _ => {}
            },
            Ok(Event::Eof) => break,
            Ok(_) => {}
            Err(_) => break,
        }
        if sink.overflowed {
            break;
        }
        buf.clear();
    }
}

/// The number of spaces an ODF `text:s` run represents (`text:c` attribute,
/// default one).
fn space_count(el: &quick_xml::events::BytesStart) -> usize {
    el.attributes()
        .flatten()
        .find(|a| a.key.local_name().as_ref() == b"c")
        .and_then(|a| a.unescape_value().ok())
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(1)
        .max(1)
}

/// epub text: follow `META-INF/container.xml` to the OPF package, read
/// the spine in order, and strip markup from each spine item's XHTML.
fn epub_text(archive: &mut Container, sink: &mut Sink, cap: u64) -> Option<()> {
    let container = read_entry(archive, "META-INF/container.xml", cap)?;
    let opf = rootfile_from_container(&container).or_else(|| {
        archive
            .file_names()
            .find(|n| n.ends_with(".opf"))
            .map(|n| n.to_string())
    })?;
    let pkg = read_entry(archive, &opf, cap)?;
    let (manifest, spine) = parse_opf(&pkg)?;
    let by_id: HashMap<&str, &str> = manifest
        .iter()
        .map(|(id, href)| (id.as_str(), href.as_str()))
        .collect();
    if spine.is_empty() {
        for (_, href) in &manifest {
            if sink.overflowed {
                break;
            }
            epub_part(archive, &opf, href, sink, cap);
        }
    } else {
        for idref in spine {
            if sink.overflowed {
                break;
            }
            if let Some(href) = by_id.get(idref.as_str()) {
                epub_part(archive, &opf, href, sink, cap);
            }
        }
    }
    Some(())
}

/// Extract one spine part: resolve its href against the OPF directory,
/// read the XHTML, and append a separator after it.
fn epub_part(archive: &mut Container, opf: &str, href: &str, sink: &mut Sink, cap: u64) {
    let entry = resolve_opf_path(opf, href);
    let Some(bytes) = read_entry(archive, &entry, cap) else {
        return;
    };
    html_entry(&bytes, sink);
    sink.push('\n');
}

/// The OPF root-file path from an EPUB `container.xml` rootfile element.
fn rootfile_from_container(xml: &[u8]) -> Option<String> {
    let mut reader = Reader::from_reader(xml);
    let mut buf = Vec::new();
    loop {
        // rootfile elements are self-closing in every real container.xml.
        let event = reader.read_event_into(&mut buf);
        let name = match &event {
            Ok(Event::Start(el)) | Ok(Event::Empty(el)) => Some(el.local_name()),
            _ => None,
        };
        if let Some(name) = name {
            if name.as_ref() == b"rootfile" {
                if let Ok(Event::Start(el)) | Ok(Event::Empty(el)) = &event {
                    for attr in el.attributes().flatten() {
                        if attr.key.local_name().as_ref() == b"full-path" {
                            let v = attr.unescape_value().ok()?;
                            let v = v.trim();
                            if !v.is_empty() {
                                return Some(v.to_string());
                            }
                        }
                    }
                }
            }
        }
        match event {
            Ok(Event::Eof) => break,
            Ok(_) => {}
            Err(_) => break,
        }
        buf.clear();
    }
    None
}

/// A parsed OPF package: manifest items (id, href) in document order and
/// spine itemref ids in reading order.
type OpfPackage = (Vec<(String, String)>, Vec<String>);

/// Parse the OPF package: manifest items (id, href) in document order and
/// spine itemref ids in reading order.
fn parse_opf(xml: &[u8]) -> Option<OpfPackage> {
    let mut reader = Reader::from_reader(xml);
    let mut buf = Vec::new();
    let mut manifest: Vec<(String, String)> = Vec::new();
    let mut spine: Vec<String> = Vec::new();
    let mut in_manifest = false;
    let mut in_spine = false;
    loop {
        let event = reader.read_event_into(&mut buf);
        // Manifest items and spine itemrefs are self-closing in practice.
        if let Ok(Event::Start(el)) | Ok(Event::Empty(el)) = &event {
            match el.local_name().as_ref() {
                b"manifest" => in_manifest = true,
                b"spine" => in_spine = true,
                b"item" if in_manifest => {
                    let mut id = None;
                    let mut href = None;
                    for attr in el.attributes().flatten() {
                        match attr.key.local_name().as_ref() {
                            b"id" => id = attr.unescape_value().ok().map(|c| c.into_owned()),
                            b"href" => href = attr.unescape_value().ok().map(|c| c.into_owned()),
                            _ => {}
                        }
                    }
                    if let (Some(id), Some(href)) = (id, href) {
                        manifest.push((id, href));
                    }
                }
                b"itemref" if in_spine => {
                    for attr in el.attributes().flatten() {
                        if attr.key.local_name().as_ref() == b"idref" {
                            if let Ok(v) = attr.unescape_value() {
                                spine.push(v.into_owned());
                            }
                        }
                    }
                }
                _ => {}
            }
        }
        match event {
            Ok(Event::End(el)) => match el.local_name().as_ref() {
                b"manifest" => in_manifest = false,
                b"spine" => in_spine = false,
                _ => {}
            },
            Ok(Event::Eof) => break,
            Ok(_) => {}
            Err(_) => return None,
        }
        buf.clear();
    }
    Some((manifest, spine))
}

/// Resolve an OPF `href` (relative to the OPF file's directory, or
/// absolute from the archive root when it starts with `/`) to a zip entry
/// path, normalizing `.` and `..` segments.
fn resolve_opf_path(opf: &str, href: &str) -> String {
    let joined = if let Some(stripped) = href.strip_prefix('/') {
        stripped.to_string()
    } else {
        match opf.rsplit_once('/') {
            Some((dir, _)) => format!("{dir}/{href}"),
            None => href.to_string(),
        }
    };
    let mut segs: Vec<&str> = Vec::new();
    for seg in joined.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                segs.pop();
            }
            s => segs.push(s),
        }
    }
    segs.join("/")
}

/// XHTML text: character data outside `head`/`nav`/`script`/`style`/`svg`
/// and friends, one line break per block element. Whitespace-only text
/// nodes (pretty-print indentation) are dropped; the common HTML named
/// entities decode to their characters.
fn html_entry(xml: &[u8], sink: &mut Sink) {
    const SKIP_ELEMENTS: &[&[u8]] = &[
        b"head", b"script", b"style", b"nav", b"svg", b"object", b"title",
    ];
    const BLOCK_ELEMENTS: &[&[u8]] = &[
        b"p",
        b"div",
        b"section",
        b"article",
        b"aside",
        b"blockquote",
        b"pre",
        b"figure",
        b"figcaption",
        b"ul",
        b"ol",
        b"li",
        b"dl",
        b"dt",
        b"dd",
        b"table",
        b"tr",
        b"td",
        b"th",
        b"caption",
        b"header",
        b"footer",
        b"main",
        b"h1",
        b"h2",
        b"h3",
        b"h4",
        b"h5",
        b"h6",
    ];
    let mut reader = Reader::from_reader(xml);
    let mut buf = Vec::new();
    let mut skip = 0usize;
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(el)) => {
                if skip == 0 && SKIP_ELEMENTS.contains(&el.local_name().as_ref()) {
                    skip += 1;
                }
            }
            Ok(Event::Empty(el)) => {
                let name = el.local_name();
                if skip == 0 && name.as_ref() == b"br" {
                    sink.push('\n');
                }
            }
            Ok(Event::Text(t)) => {
                if skip == 0 {
                    if let Ok(cow) = t.unescape_with(html_entity) {
                        if !cow.trim().is_empty() {
                            sink.push_str(&cow);
                        }
                    }
                }
            }
            Ok(Event::End(el)) => {
                let name = el.local_name();
                if skip > 0 {
                    if SKIP_ELEMENTS.contains(&name.as_ref()) {
                        skip -= 1;
                    }
                } else if BLOCK_ELEMENTS.contains(&name.as_ref()) {
                    sink.push('\n');
                }
            }
            Ok(Event::Eof) => break,
            Ok(_) => {}
            Err(_) => break,
        }
        if sink.overflowed {
            break;
        }
        buf.clear();
    }
}

const HTML_ENTITIES: &[(&str, &str)] = &[
    ("nbsp", " "),
    ("amp", "&"),
    ("lt", "<"),
    ("gt", ">"),
    ("quot", "\""),
    ("apos", "'"),
    ("ndash", "-"),
    ("mdash", "-"),
    ("hellip", "..."),
    ("rsquo", "'"),
    ("lsquo", "'"),
    ("rdquo", "\""),
    ("ldquo", "\""),
    ("bull", "*"),
    ("copy", "(c)"),
    ("reg", "(r)"),
    ("trade", "(tm)"),
    ("times", "x"),
    ("divide", "/"),
    ("middot", "*"),
];

fn html_entity(name: &str) -> Option<&'static str> {
    HTML_ENTITIES
        .iter()
        .find(|(k, _)| *k == name)
        .map(|(_, v)| *v)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::path::PathBuf;
    use zip::write::{SimpleFileOptions, ZipWriter};

    fn write_zip(path: &Path, files: &[(&str, &str)]) {
        let f = std::fs::File::create(path).unwrap();
        let mut zw = ZipWriter::new(f);
        for (name, content) in files {
            zw.start_file(name, SimpleFileOptions::default()).unwrap();
            zw.write_all(content.as_bytes()).unwrap();
        }
        zw.finish().unwrap();
    }

    fn tmp_dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "hl-text-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn extract(path: &Path, ext: &str, budget: u64) -> Option<String> {
        extract_builtin_text(path, ext, budget)
    }

    /// A tiny single-page PDF with text in a content stream and a
    /// byte-accurate xref table. Object offsets are located after assembly
    /// by searching for each unique `N 0 obj` marker, so the fixture needs
    /// no hand-counted offsets.
    fn build_pdf() -> Vec<u8> {
        let stream = b"BT /F1 12 Tf 72 720 Td (Hello world) Tj ET";
        let mut buf = Vec::new();
        buf.extend_from_slice(b"%PDF-1.4\n");
        push_obj(&mut buf, 1, b"<< /Type /Catalog /Pages 2 0 R >>");
        push_obj(&mut buf, 2, b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>");
        push_obj(
            &mut buf,
            3,
            b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Resources << /Font << /F1 4 0 R >> >> /Contents 5 0 R >>",
        );
        push_obj(
            &mut buf,
            4,
            b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica /Encoding /WinAnsiEncoding >>",
        );
        push_stream_obj(&mut buf, 5, stream);
        let xref_pos = buf.len();
        buf.extend_from_slice(b"xref\n0 6\n");
        buf.extend_from_slice(b"0000000000 65535 f \n");
        for num in 1..=5 {
            buf.extend_from_slice(format!("{:010} 00000 n \n", obj_offset(&buf, num)).as_bytes());
        }
        buf.extend_from_slice(b"trailer\n<< /Size 6 /Root 1 0 R >>\nstartxref\n");
        buf.extend_from_slice(format!("{xref_pos}\n").as_bytes());
        buf.extend_from_slice(b"%%EOF\n");
        buf
    }

    fn push_obj(buf: &mut Vec<u8>, num: usize, body: &[u8]) {
        buf.extend_from_slice(format!("\n{num} 0 obj\n").as_bytes());
        buf.extend_from_slice(body);
        buf.extend_from_slice(b"\nendobj");
    }

    fn push_stream_obj(buf: &mut Vec<u8>, num: usize, stream: &[u8]) {
        buf.extend_from_slice(format!("\n{num} 0 obj\n<< /Length ").as_bytes());
        buf.extend_from_slice(format!("{}\n", stream.len()).as_bytes());
        buf.extend_from_slice(b">>\nstream\n");
        buf.extend_from_slice(stream);
        buf.extend_from_slice(b"\nendstream\nendobj");
    }

    /// Byte offset of the `N 0 obj` marker written by `push_obj`/`push_stream_obj`.
    fn obj_offset(buf: &[u8], num: usize) -> usize {
        let marker = format!("\n{num} 0 obj\n");
        buf.windows(marker.len())
            .position(|w| w == marker.as_bytes())
            .map(|p| p + 1)
            .unwrap_or(0)
    }

    #[test]
    fn svg_text_and_accessible_labels_are_extracted() {
        let dir = tmp_dir("svg");
        let path = dir.join("diagram.svg");
        std::fs::write(
            &path,
            r#"<svg xmlns="http://www.w3.org/2000/svg" aria-label="Claim diagram"><title>Claim title</title><desc>Overview &amp; details</desc><text>Claim <tspan>four</tspan></text><path aria-label="Evidence node"/></svg>"#,
        )
        .unwrap();
        let text = extract(&path, "svg", 1 << 20).unwrap();
        assert!(text.contains("Claim diagram"), "got: {text:?}");
        assert!(text.contains("Claim title"), "got: {text:?}");
        assert!(text.contains("Overview & details"), "got: {text:?}");
        assert!(text.contains("Claim four"), "got: {text:?}");
        assert!(text.contains("Evidence node"), "got: {text:?}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn pdf_text_from_content_stream() {
        let dir = tmp_dir("pdf");
        let path = dir.join("doc.pdf");
        std::fs::write(&path, build_pdf()).unwrap();
        let text = extract(&path, "pdf", 1 << 20).unwrap();
        assert!(text.contains("Hello world"), "got: {text:?}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn docx_runs_entities_and_paragraphs() {
        let dir = tmp_dir("docx");
        let path = dir.join("letter.docx");
        write_zip(
            &path,
            &[(
                "word/document.xml",
                r#"<?xml version="1.0"?>
<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body>
<w:p><w:r><w:t>Alpha &amp; beta</w:t></w:r><w:r><w:t xml:space="preserve"> run two</w:t></w:r></w:p>
<w:p><w:r><w:t>Second line</w:t></w:r></w:p>
</w:body></w:document>"#,
            )],
        );
        let text = extract(&path, "docx", 1 << 20).unwrap();
        assert_eq!(text, "Alpha & beta run two\nSecond line");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn odt_paragraphs_headings_and_spaces() {
        let dir = tmp_dir("odt");
        let path = dir.join("notes.odt");
        write_zip(
            &path,
            &[(
                "content.xml",
                r#"<?xml version="1.0"?>
<office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0">
<office:body><office:text>
<text:p>First paragraph</text:p>
<text:h text:outline-level="1">Chapter</text:h>
<text:p>Spaces<text:s text:c="3"/>here and A &amp; B</text:p>
</office:text></office:body></office:document-content>"#,
            )],
        );
        let text = extract(&path, "odt", 1 << 20).unwrap();
        assert_eq!(text, "First paragraph\nChapter\nSpaces   here and A & B");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn epub_follows_spine_and_strips_markup() {
        let dir = tmp_dir("epub");
        let path = dir.join("book.epub");
        write_zip(
            &path,
            &[
                (
                    "META-INF/container.xml",
                    r#"<?xml version="1.0"?><container xmlns="urn:oasis:names:tc:opendocument:xmlns:container"><rootfiles><rootfile full-path="OEBPS/content.opf"/></rootfiles></container>"#,
                ),
                (
                    "OEBPS/content.opf",
                    r#"<?xml version="1.0"?><package xmlns="http://www.idpf.org/2007/opf"><manifest><item id="c1" href="ch1.xhtml" media-type="application/xhtml+xml"/><item id="cover" href="cover.svg" media-type="image/svg+xml"/><item id="c2" href="text/ch2.xhtml" media-type="application/xhtml+xml"/></manifest><spine><itemref idref="c1"/><itemref idref="c2"/></spine></package>"#,
                ),
                (
                    "OEBPS/ch1.xhtml",
                    r#"<?xml version="1.0"?><html xmlns="http://www.w3.org/1999/xhtml"><head><title>Ignored Title</title></head><body><p>Once upon a <em>time</em></p><nav><p>TOC line</p></nav><p>Ampersand &amp; more</p></body></html>"#,
                ),
                (
                    "OEBPS/text/ch2.xhtml",
                    r#"<?xml version="1.0"?><html xmlns="http://www.w3.org/1999/xhtml"><body><h1>Chapter Two</h1><p>Second part</p></body></html>"#,
                ),
            ],
        );
        let text = extract(&path, "epub", 1 << 20).unwrap();
        for expect in [
            "Once upon a time",
            "Ampersand & more",
            "Chapter Two",
            "Second part",
        ] {
            assert!(text.contains(expect), "missing {expect:?} in {text:?}");
        }
        assert!(!text.contains("TOC line"), "nav text leaked: {text:?}");
        assert!(!text.contains("Ignored"), "head text leaked: {text:?}");
        let first = text.find("Once upon").unwrap();
        let second = text.find("Chapter Two").unwrap();
        assert!(first < second, "spine order not preserved: {text:?}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn over_budget_output_is_a_failure() {
        let dir = tmp_dir("budget");
        let path = dir.join("big.docx");
        let para = "<w:p><w:r><w:t>wordy sentence here</w:t></w:r></w:p>";
        let mut body = String::from(
            r#"<?xml version="1.0"?><w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body>"#,
        );
        for _ in 0..200 {
            body.push_str(para);
        }
        body.push_str("</w:body></w:document>");
        write_zip(&path, &[("word/document.xml", &body)]);
        // The real text is roughly 5000 bytes; a 256-byte budget fails.
        assert_eq!(extract(&path, "docx", 256), None);
        // A generous budget succeeds.
        assert!(extract(&path, "docx", 1 << 20).is_some());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn unsupported_and_missing_inputs_are_none() {
        let dir = tmp_dir("none");
        let path = dir.join("plain.txt");
        std::fs::write(&path, "hello").unwrap();
        assert_eq!(extract(&path, "rtf", 1 << 20), None);
        assert_eq!(extract(&dir.join("missing.pdf"), "pdf", 1 << 20), None);
        assert_eq!(extract(&dir.join("missing.docx"), "docx", 1 << 20), None);
        // A corrupt container degrades to no text, never an error.
        let bad = dir.join("bad.epub");
        std::fs::write(&bad, "not a zip at all").unwrap();
        assert_eq!(extract(&bad, "epub", 1 << 20), None);
        std::fs::remove_dir_all(&dir).ok();
    }
}
