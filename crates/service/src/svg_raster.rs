//! Bounded, in-process SVG rasterization for vision description requests.
//!
//! SVG input is parsed without a resource directory, so rendering cannot load
//! external files. Raster dimensions are capped independently of configuration
//! to keep a vector file from requesting an unbounded pixel buffer.

use std::io::Read;
use std::path::Path;
use std::sync::{Arc, OnceLock};

use resvg::tiny_skia::{Pixmap, Transform};
use resvg::usvg::{ImageHrefResolver, Options, Tree};

const MAX_SVG_BYTES: u64 = 20 * 1024 * 1024;
const MAX_RASTER_BYTES: usize = 20 * 1024 * 1024;
const HARD_MAX_DIMENSION: u64 = 4096;
static FONT_DB: OnceLock<Arc<resvg::usvg::fontdb::Database>> = OnceLock::new();

/// Rasterize an SVG to PNG for a vision request. Returns `None` when the file
/// is too large, invalid, unsupported, or cannot be rasterized; callers then
/// retain extracted SVG text and use a text-only provider request. Rendering
/// is capped at `max_dimension`, or at the hard cap when the configured value
/// is zero.
pub fn rasterize_svg(path: &Path, max_dimension: u64) -> Option<Vec<u8>> {
    let file = std::fs::File::open(path).ok()?;
    if file.metadata().ok()?.len() > MAX_SVG_BYTES {
        return None;
    }
    let mut bytes = Vec::new();
    let mut bounded = file.take(MAX_SVG_BYTES + 1);
    bounded.read_to_end(&mut bytes).ok()?;
    if bytes.len() as u64 > MAX_SVG_BYTES {
        return None;
    }
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        rasterize_svg_bytes(&bytes, max_dimension)
    }))
    .ok()
    .flatten()
}

fn rasterize_svg_bytes(bytes: &[u8], max_dimension: u64) -> Option<Vec<u8>> {
    let cap = match max_dimension {
        0 => HARD_MAX_DIMENSION,
        value => value.min(HARD_MAX_DIMENSION),
    } as u32;
    let fontdb = FONT_DB
        .get_or_init(|| {
            let mut db = resvg::usvg::fontdb::Database::new();
            db.load_system_fonts();
            Arc::new(db)
        })
        .clone();
    // Deny filesystem references rather than relying on `resources_dir`:
    // usvg's default resolver can read local paths even with no resource dir.
    // Embedded data URLs remain available, and the SVG's directory is never
    // exposed to the parser. System fonts are available for SVG text.
    let options = Options {
        resources_dir: None,
        image_href_resolver: ImageHrefResolver {
            resolve_data: ImageHrefResolver::default_data_resolver(),
            resolve_string: Box::new(|_, _| None),
        },
        fontdb,
        ..Options::default()
    };
    let tree = Tree::from_data(bytes, &options).ok()?;
    let size = tree.size();
    let width = size.width();
    let height = size.height();
    if !width.is_finite() || !height.is_finite() || width <= 0.0 || height <= 0.0 {
        return None;
    }

    // Render the longest side exactly at the selected cap. This both improves
    // small vector icons for vision models and prevents extreme SVG dimensions
    // from allocating an oversized pixmap.
    let scale = cap as f32 / width.max(height);
    let pixel_width = (width * scale).ceil().clamp(1.0, cap as f32) as u32;
    let pixel_height = (height * scale).ceil().clamp(1.0, cap as f32) as u32;
    let mut pixmap = Pixmap::new(pixel_width, pixel_height)?;
    resvg::render(
        &tree,
        Transform::from_scale(scale, scale),
        &mut pixmap.as_mut(),
    );
    let png = pixmap.encode_png().ok()?;
    (png.len() <= MAX_RASTER_BYTES).then_some(png)
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::GenericImageView;

    #[test]
    fn rasterizes_svg_with_capped_dimensions() {
        let svg = br#"<svg xmlns="http://www.w3.org/2000/svg" width="8000" height="4000"><rect width="8000" height="4000" fill="red"/></svg>"#;
        let png = rasterize_svg_bytes(svg, 512).unwrap();
        let image = image::load_from_memory(&png).unwrap();
        assert_eq!(image.dimensions(), (512, 256));
    }

    #[test]
    fn invalid_svg_degrades_without_panicking() {
        assert!(rasterize_svg_bytes(b"not an svg", 1024).is_none());
    }
}
