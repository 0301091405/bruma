//! Font loading and text drawing.
//!
//! Fonts come from the system. [`Fonts::empty`] draws nothing and measures by
//! estimate, which keeps image tests independent of the machine they run on.

use std::cell::RefCell;
use std::collections::HashMap;

use fontdue::{Font, FontSettings, Metrics};
use tiny_skia::Pixmap;

use crate::geom::Point;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Face {
    /// Proportional interface font, used for annotation text.
    Ui,
    /// Monospace, used for every number and key hint in the chrome.
    Mono,
}

type GlyphKey = (Face, char, u32);

#[derive(Default)]
pub struct Fonts {
    ui: Option<Font>,
    mono: Option<Font>,
    cache: RefCell<HashMap<GlyphKey, (Metrics, Vec<u8>)>>,
}

const UI_FAMILIES: &[&str] = &[
    "Segoe UI",
    "Inter",
    "Noto Sans",
    "DejaVu Sans",
    "Helvetica Neue",
];
const MONO_FAMILIES: &[&str] = &[
    "Cascadia Mono",
    "Consolas",
    "JetBrains Mono",
    "DejaVu Sans Mono",
    "Menlo",
];

#[cfg(windows)]
const UI_FILES: &[&str] = &["seguisb.ttf", "segoeui.ttf"];
#[cfg(windows)]
const MONO_FILES: &[&str] = &["CascadiaMono.ttf", "consola.ttf"];

impl Fonts {
    /// No fonts: text is skipped when drawing and estimated when measuring.
    pub fn empty() -> Self {
        Self::default()
    }

    /// Loads the interface and monospace fonts from the system.
    pub fn system() -> Self {
        #[cfg(windows)]
        let (mut ui, mut mono) = {
            let dir = std::env::var_os("WINDIR")
                .map(std::path::PathBuf::from)
                .unwrap_or_else(|| "C:\\Windows".into())
                .join("Fonts");
            (
                load_first_file(&dir, UI_FILES),
                load_first_file(&dir, MONO_FILES),
            )
        };
        #[cfg(not(windows))]
        let (mut ui, mut mono): (Option<Font>, Option<Font>) = (None, None);
        if ui.is_none() || mono.is_none() {
            let mut db = fontdb::Database::new();
            db.load_system_fonts();
            if ui.is_none() {
                ui = query(&db, UI_FAMILIES, fontdb::Family::SansSerif);
            }
            if mono.is_none() {
                mono = query(&db, MONO_FAMILIES, fontdb::Family::Monospace);
            }
        }
        Self {
            ui,
            mono,
            cache: RefCell::default(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.ui.is_none() && self.mono.is_none()
    }

    fn font(&self, face: Face) -> Option<&Font> {
        match face {
            Face::Ui => self.ui.as_ref(),
            Face::Mono => self.mono.as_ref(),
        }
    }

    pub fn line_height(&self, face: Face, px: f32) -> f32 {
        self.font(face)
            .and_then(|f| f.horizontal_line_metrics(px))
            .map(|m| m.new_line_size.ceil())
            .unwrap_or((px * 1.3).ceil())
    }

    fn ascent(&self, face: Face, px: f32) -> f32 {
        self.font(face)
            .and_then(|f| f.horizontal_line_metrics(px))
            .map(|m| m.ascent)
            .unwrap_or(px)
    }

    fn line_width(&self, face: Face, line: &str, px: f32) -> f32 {
        match self.font(face) {
            Some(font) => {
                let mut width = 0.0;
                let mut prev = None;
                for c in line.chars() {
                    if let Some(p) = prev {
                        width += font.horizontal_kern(p, c, px).unwrap_or(0.0);
                    }
                    width += font.metrics(c, px).advance_width;
                    prev = Some(c);
                }
                width
            }
            None => line.chars().count() as f32 * px * 0.6,
        }
    }

    /// Width and height of `text`, which may span several lines.
    pub fn measure(&self, face: Face, text: &str, px: f32) -> (f32, f32) {
        let mut width = 0.0f32;
        let mut lines = 0;
        for line in text.split('\n') {
            width = width.max(self.line_width(face, line, px));
            lines += 1;
        }
        (width.ceil(), lines as f32 * self.line_height(face, px))
    }

    /// Draws `text` with its top-left corner at `origin`.
    pub fn draw(
        &self,
        pixmap: &mut Pixmap,
        face: Face,
        text: &str,
        px: f32,
        origin: Point,
        color: [u8; 3],
    ) {
        let Some(font) = self.font(face) else {
            return;
        };
        let line_height = self.line_height(face, px);
        let ascent = self.ascent(face, px);
        let size_key = (px * 4.0).round() as u32;
        let mut cache = self.cache.borrow_mut();
        for (row, line) in text.split('\n').enumerate() {
            let baseline = (origin.y + ascent + row as f32 * line_height).round();
            let mut pen = origin.x;
            let mut prev = None;
            for c in line.chars() {
                if let Some(p) = prev {
                    pen += font.horizontal_kern(p, c, px).unwrap_or(0.0);
                }
                let (metrics, coverage) = cache
                    .entry((face, c, size_key))
                    .or_insert_with(|| font.rasterize(c, px));
                let gx = pen.round() as i32 + metrics.xmin;
                let gy = baseline as i32 - metrics.ymin - metrics.height as i32;
                blend_coverage(
                    pixmap,
                    gx,
                    gy,
                    metrics.width,
                    metrics.height,
                    coverage,
                    color,
                );
                pen += metrics.advance_width;
                prev = Some(c);
            }
        }
    }
}

fn load_font(data: Vec<u8>, index: u32) -> Option<Font> {
    let settings = FontSettings {
        collection_index: index,
        ..FontSettings::default()
    };
    Font::from_bytes(data, settings).ok()
}

#[cfg(windows)]
fn load_first_file(dir: &std::path::Path, names: &[&str]) -> Option<Font> {
    names.iter().find_map(|name| {
        std::fs::read(dir.join(name))
            .ok()
            .and_then(|d| load_font(d, 0))
    })
}

fn query(db: &fontdb::Database, names: &[&str], generic: fontdb::Family<'static>) -> Option<Font> {
    let mut families: Vec<fontdb::Family<'_>> =
        names.iter().map(|n| fontdb::Family::Name(n)).collect();
    families.push(generic);
    let id = db.query(&fontdb::Query {
        families: &families,
        ..fontdb::Query::default()
    })?;
    db.with_face_data(id, |data, index| load_font(data.to_vec(), index))?
}

/// Composites an 8-bit coverage mask in a solid colour onto a premultiplied pixmap.
fn blend_coverage(
    pixmap: &mut Pixmap,
    x0: i32,
    y0: i32,
    width: usize,
    height: usize,
    coverage: &[u8],
    color: [u8; 3],
) {
    let (pw, ph) = (pixmap.width() as i32, pixmap.height() as i32);
    let data = pixmap.data_mut();
    for row in 0..height {
        let y = y0 + row as i32;
        if y < 0 || y >= ph {
            continue;
        }
        for col in 0..width {
            let x = x0 + col as i32;
            if x < 0 || x >= pw {
                continue;
            }
            let a = coverage[row * width + col] as u32;
            if a == 0 {
                continue;
            }
            let i = (y as usize * pw as usize + x as usize) * 4;
            let inv = 255 - a;
            for ch in 0..3 {
                data[i + ch] =
                    ((color[ch] as u32 * a + data[i + ch] as u32 * inv + 127) / 255) as u8;
            }
            data[i + 3] = (a + (data[i + 3] as u32 * inv + 127) / 255).min(255) as u8;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_fonts_estimate_size() {
        let fonts = Fonts::empty();
        let (w, h) = fonts.measure(Face::Mono, "1280", 10.0);
        assert_eq!(w, 24.0);
        assert_eq!(h, 13.0);
    }

    #[test]
    fn measure_counts_lines() {
        let fonts = Fonts::empty();
        let (w, h) = fonts.measure(Face::Ui, "ab\nabcd\n", 10.0);
        assert_eq!(w, 24.0);
        assert_eq!(h, 39.0);
    }

    #[test]
    fn empty_fonts_draw_nothing() {
        let fonts = Fonts::empty();
        let mut pixmap = Pixmap::new(20, 20).unwrap();
        fonts.draw(
            &mut pixmap,
            Face::Ui,
            "hello",
            12.0,
            Point::new(0.0, 0.0),
            [255, 255, 255],
        );
        assert!(pixmap.data().iter().all(|&b| b == 0));
    }

    #[test]
    fn coverage_blend_clips_at_edges() {
        let mut pixmap = Pixmap::new(4, 4).unwrap();
        let coverage = vec![255u8; 9];
        blend_coverage(&mut pixmap, 2, 2, 3, 3, &coverage, [255, 0, 0]);
        blend_coverage(&mut pixmap, -2, -2, 3, 3, &coverage, [0, 255, 0]);
        assert_eq!(&pixmap.data()[0..4], &[0, 255, 0, 255]);
        let last = (3 * 4 + 3) * 4;
        assert_eq!(&pixmap.data()[last..last + 4], &[255, 0, 0, 255]);
    }
}
