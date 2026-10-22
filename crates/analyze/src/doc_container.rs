//! Built-in document metadata readers for the zip-based office/ePub
//! containers: docx (`docProps/core.xml`), odt (`meta.xml`), and epub
//! (the OPF package named by `META-INF/container.xml`). Each metadata
//! file carries the Dublin Core fields we need in a small, well-known XML
//! shape (`dc:title`, `dc:creator`, `dc:date`).
//!
//! The reader is deliberately shallow: it walks the metadata file for text
//! under the local element names `title`, `creator`, and `date`, ignores
//! namespaces and everything else, and never validates the rest of the
//! archive, so a malformed or hostile container degrades to no metadata
//! rather than an error.
use std::path::Path;

use homelinter_model::asset_kind::AssetKind;
use quick_xml::events::Event;
use quick_xml::Reader;
use zip::ZipArchive;

use crate::dateparse::parse_tag_date_parts;
use crate::extractor::{EmbeddedMeta, ExtractionContext, FieldSet, MetadataExtractor};

/// Built-in fallback document extractor (docx/odt/epub). Runs after the
/// `pdfinfo` preferred tool,so its fields never override poppler's.
pub struct DocumentContainerExtractor;

impl MetadataExtractor for DocumentContainerExtractor {
    fn name(&self) -> &'static str {
        "document-container"
    }

    fn provides(&self, kind: AssetKind) -> FieldSet {
        if kind == AssetKind::Document {
            FieldSet::DOCUMENT
        } else {
            FieldSet::EMPTY
        }
    }

    fn extract(&self, path: &Path, kind: AssetKind, _ctx: &ExtractionContext) -> EmbeddedMeta {
        let mut meta = EmbeddedMeta::default();
        if kind != AssetKind::Document {
            return meta;
        }
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        let (entry, is_epub) = match ext.as_deref() {
            Some("docx") => (Some("docProps/core.xml"), false),
            Some("odt") => (Some("meta.xml"), false),
            Some("epub") => (Some("META-INF/container.xml"), true),
            _ => (None, false),
        };
        let Some(entry) = entry else {
            return meta;
        };
        let file = match std::fs::File::open(path) {
            Ok(f) => f,
            Err(_) => return meta,
        };
        let mut archive = match ZipArchive::new(file) {
            Ok(z) => z,
            Err(_) => return meta,
        };
        let xml = if is_epub {
            let root = read_entry(&mut archive, entry)
                .and_then(|c| rootfile_from_container(&c))
                .or_else(|| {
                    archive
                        .file_names()
                        .find(|n| n.ends_with(".opf"))
                        .map(|n| n.to_string())
                });
            match root {
                Some(root) => match read_entry(&mut archive, &root) {
                    Some(x) => x,
                    None => return meta,
                },
                None => return meta,
            }
        } else {
            match read_entry(&mut archive, entry) {
                Some(x) => x,
                None => return meta,
            }
        };
        let fields = parse_meta_xml(&xml);
        // The container XML was read, so the document fields are answered
        // whether or not it declares them.
        meta.settled = FieldSet::DOCUMENT;
        meta.title = fields.title;
        meta.author = fields.author;
        meta.date_taken = fields.date.as_deref().and_then(parse_tag_date_parts);
        meta
    }
}

/// Read one entry from the zip archive as raw bytes; a missing entry
/// degrades to `None`.
fn read_entry(archive: &mut ZipArchive<std::fs::File>, name: &str) -> Option<Vec<u8>> {
    let mut f = archive.by_name(name).ok()?;
    let mut bytes = Vec::with_capacity(f.size() as usize);
    std::io::Read::read_to_end(&mut f, &mut bytes).ok()?;
    Some(bytes)
}

/// The OPF root-file path from an EPUB `container.xml` rootfile element.
fn rootfile_from_container(xml: &[u8]) -> Option<String> {
    let mut reader = Reader::from_reader(xml);
    let mut buf = Vec::new();
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(el)) => {
                if el.local_name().as_ref() == b"rootfile" {
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
            Ok(Event::Eof) => break,
            Ok(_) => {}
            Err(_) => break,
        }
        buf.clear();
    }
    None
}

#[derive(Default)]
struct DocFields {
    title: Option<String>,
    author: Option<String>,
    date: Option<String>,
}

/// Walk the metadata XML for character data under the local element names
/// `title`, `creator`, and `date`, ignoring namespaces (the Dublin Core
/// fields appear under the `dc:` prefix in every supported container type).
fn parse_meta_xml(xml: &[u8]) -> DocFields {
    let mut reader = Reader::from_reader(xml);
    let mut buf = Vec::new();
    let mut stack: Vec<Vec<u8>> = Vec::new();
    let mut out = DocFields::default();
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(el)) => {
                stack.push(el.local_name().as_ref().to_vec());
            }
            Ok(Event::Text(t)) => {
                if let Some(cur) = stack.last() {
                    let text = String::from_utf8_lossy(t.as_ref()).trim().to_string();
                    if text.is_empty() {
                        buf.clear();
                        continue;
                    }
                    let capped = cap(&text, 512);
                    match cur.as_slice() {
                        b"title" => {
                            if out.title.is_none() {
                                out.title = Some(capped);
                            }
                        }
                        b"creator" => {
                            if out.author.is_none() {
                                out.author = Some(cap(&text, 256));
                            }
                        }
                        b"date" => {
                            if out.date.is_none() {
                                out.date = Some(cap(&text, 64));
                            }
                        }
                        _ => {}
                    }
                }
            }
            Ok(Event::End(_)) => {
                stack.pop();
            }
            Ok(Event::Eof) => break,
            Ok(_) => {}
            Err(_) => break,
        }
        buf.clear();
    }
    out
}

fn cap(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
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

    fn tmp_dir(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!(
            "hl-doc-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn docx_odt_and_epub_metadata() {
        let dir = tmp_dir("mix");

        let docx = dir.join("letter.docx");
        write_zip(
            &docx,
            &[
                ("word/document.xml", "<w:document/>"),
                (
                    "docProps/core.xml",
                    r#"<?xml version="1.0"?><cp:coreProperties xmlns:cp="http://schemas.openxmlformats.org/package/2006/metadata/core-properties" xmlns:dc="http://purl.org/dc/elements/1.1/"><dc:title>A Letter</dc:title><dc:creator>Jane Doe</dc:creator><dc:date>2024-03-15T10:00:00Z</dc:date></cp:coreProperties>"#,
                ),
            ],
        );
        let meta = DocumentContainerExtractor {}.extract(
            &docx,
            AssetKind::Document,
            &ExtractionContext::default(),
        );
        assert_eq!(meta.title.as_deref(), Some("A Letter"));
        assert_eq!(meta.author.as_deref(), Some("Jane Doe"));
        let dp = meta.date_taken.unwrap();
        assert_eq!((dp.year, dp.month, dp.day), (2024, 3, 15));

        let odt = dir.join("notes.odt");
        write_zip(
            &odt,
            &[
                ("content.xml", "<office:document/>"),
                (
                    "meta.xml",
                    r#"<?xml version="1.0"?><office:document-meta xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:dc="http://purl.org/dc/elements/1.1/"><office:meta><dc:title>Notes</dc:title><dc:creator>Pat</dc:creator><dc:date>2024-06-01</dc:date></office:meta></office:document-meta>"#,
                ),
            ],
        );
        let meta = DocumentContainerExtractor {}.extract(
            &odt,
            AssetKind::Document,
            &ExtractionContext::default(),
        );
        assert_eq!(meta.title.as_deref(), Some("Notes"));
        assert_eq!(meta.author.as_deref(), Some("Pat"));

        let epub = dir.join("book.epub");
        write_zip(
            &epub,
            &[
                (
                    "META-INF/container.xml",
                    r#"<?xml version="1.0"?><container version="1.0" xmlns="urn:oasis:names:tc:opendocument:xmlns:container"><rootfiles><rootfile full-path="OEBPS/content.opf" media-type="application/oebps-package+xml"/></rootfiles></container>"#,
                ),
                (
                    "OEBPS/content.opf",
                    r#"<?xml version="1.0"?><package xmlns="http://www.idpf.org/2007/opf" xmlns:dc="http://purl.org/dc/elements/1.1/"><metadata><dc:title>The Book</dc:title><dc:creator>Sam</dc:creator><dc:date>2023-01-02</dc:date></metadata></package>"#,
                ),
            ],
        );
        let meta = DocumentContainerExtractor {}.extract(
            &epub,
            AssetKind::Document,
            &ExtractionContext::default(),
        );
        assert_eq!(meta.title.as_deref(), Some("The Book"));
        assert_eq!(meta.author.as_deref(), Some("Sam"));

        std::fs::remove_dir_all(&dir).ok();
    }
}
