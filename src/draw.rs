//! Shared by the pictures (`.iceberg`, charts): bundled fonts and SVG → PNG with resvg.

use std::sync::{Arc, OnceLock};

use anyhow::{Context, Result};
use resvg::{tiny_skia, usvg};

/// Bundled fonts (assets/fonts, OFL) first; system fonts cover what they lack, such as CJK.
pub const FALLBACK: &str = "DejaVu Sans, Noto Sans CJK JP, WenQuanYi Zen Hei, sans-serif";
pub const DISPLAY: &str = "Bricolage Grotesque";
pub const SANS: &str = "Inter";
pub const MONO: &str = "JetBrains Mono";
const BUNDLED: [&[u8]; 3] = [
    include_bytes!("../assets/fonts/BricolageGrotesque-ExtraBold.ttf"),
    include_bytes!("../assets/fonts/Inter-SemiBold.ttf"),
    include_bytes!("../assets/fonts/JetBrainsMono-Medium.ttf"),
];

/// Loaded once. The Docker image installs DejaVu and a CJK font; glyphs no font has render as nothing.
static FONTS: OnceLock<Arc<usvg::fontdb::Database>> = OnceLock::new();

pub fn pixmap(svg: &str) -> Result<tiny_skia::Pixmap> {
    let fontdb = FONTS
        .get_or_init(|| {
            let mut db = usvg::fontdb::Database::new();
            for font in BUNDLED {
                db.load_font_data(font.to_vec());
            }
            db.load_system_fonts();
            Arc::new(db)
        })
        .clone();
    let opt = usvg::Options { font_family: SANS.into(), fontdb, ..Default::default() };
    let tree = usvg::Tree::from_str(svg, &opt)?;
    let size = tree.size().to_int_size();
    let mut pixmap = tiny_skia::Pixmap::new(size.width(), size.height()).context("empty image")?;
    resvg::render(&tree, tiny_skia::Transform::default(), &mut pixmap.as_mut());
    Ok(pixmap)
}

pub fn png(svg: &str) -> Result<Vec<u8>> {
    Ok(pixmap(svg)?.encode_png()?)
}

pub fn clip(s: &str, max: usize) -> String {
    if s.chars().count() <= max { s.to_owned() } else { format!("{}…", s.chars().take(max - 1).collect::<String>()) }
}

/// Escapes text for the SVG and drops control characters, which XML forbids (broken tags sometimes carry them).
pub fn xml(s: &str) -> String {
    s.chars().filter(|c| !c.is_control()).collect::<String>().replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}
