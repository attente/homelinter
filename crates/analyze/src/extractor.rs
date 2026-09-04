//! Plug-in seam for embedded metadata and content providers.
//!
//! The scaffold ships no extractors: scans classify, hash, and organize
//! without external tools. EXIF (kamadak-exif / exiftool), ID3, document
//! extraction, LLM analysis, and reverse geocoding implement these traits in
//! later phases; the seam keeps analysis code unchanged.

use std::path::Path;

use homelinter_model::asset_kind::AssetKind;
use homelinter_model::config::RawConfig;

/// Provider results cache handle (reserved for LLM/geocoder phases).
#[derive(Debug, Clone, Default)]
pub struct ExtractionContext {
    /// Enabled content providers configured for this scan.
    pub config: Option<RawConfig>,
}

/// A metadata extractor for embedded content, consulted in priority order
/// before filename- and provider-derived metadata.
pub trait MetadataExtractor: Send + Sync {
    fn name(&self) -> &'static str;
    fn extract_title(
        &self,
        _path: &Path,
        _kind: AssetKind,
        _ctx: &ExtractionContext,
    ) -> Option<String> {
        None
    }
    fn extract_date(
        &self,
        _path: &Path,
        _kind: AssetKind,
        _ctx: &ExtractionContext,
    ) -> Option<String> {
        None
    }
    fn extract_location(
        &self,
        _path: &Path,
        _kind: AssetKind,
        _ctx: &ExtractionContext,
    ) -> Option<String> {
        None
    }
}
