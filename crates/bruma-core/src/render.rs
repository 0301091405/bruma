//! Session → pixels. Pure: the same session always renders the same frame.
//!
//! The look is one idea: what you selected is in focus, everything else is
//! mist. Panels are frosted glass over that mist.

use tiny_skia::{
    FillRule, FilterQuality, IntSize, LineCap, LineJoin, Paint, Path, PathBuilder, Pattern, Pixmap,
    PixmapPaint, SpreadMode, Transform,
};

use crate::blur;
use crate::doc::Shape;
use crate::draw::{draw_annotation, fill_path, fill_rect, stroke_path};
use crate::geom::{Point, Rect};
use crate::glyph;
use crate::image::Image;
use crate::session::Session;
use crate::strip::{StripItem, StripLayout};
use crate::text::{Face, Fonts};
use crate::theme::{self, ACCENT, FAINT, INK, PAPER};

/// Source pixels across the loupe (odd, so one pixel sits in the centre).
const LOUPE_PIXELS: i32 = 15;
const WHITE: [u8; 3] = [255, 255, 255];

/// Holds the versions of the current frame that are costly to make: the
/// dimmed screen, the mist, and the glass backdrop.
#[derive(Default)]
pub struct Renderer {
    /// Frame index and size the cached layers belong to.
    key: Option<(usize, u32, u32)>,
    /// The sharp screen, dimmed: shown before anything is selected.
    dim: Vec<u8>,
    /// The screen blurred and dimmed, at full size: shown around a selection.
    mist: Option<Vec<u8>>,
    /// The screen heavily blurred at quarter size: the backdrop of every panel.
    glass: Option<Pixmap>,
    /// The screen at quarter size, unblurred: what mist and glass are made from.
    quarter: Option<blur::Buf>,
}

/// The costly versions of one frame that the first picture needs, built
/// ahead of time. They can be made on another thread while the window opens.
pub struct Layers {
    key: (usize, u32, u32),
    dim: Vec<u8>,
    quarter: blur::Buf,
    glass: Option<Pixmap>,
}

fn build_dim(screen: &Image, into: &mut Vec<u8>) {
    let keep = (theme::DIM_KEEP * 256.0) as u32;
    if into.len() != screen.rgba.len() {
        // Zeroed memory comes from the system untouched; the bands below are
        // the first to write to it, each on its own core.
        *into = vec![0u8; screen.rgba.len()];
    }
    let row = (screen.width as usize * 4).max(4);
    crate::par::bands(into, row, |first, band| {
        let source = &screen.rgba[first * row..first * row + band.len()];
        // A pixel as one word: red and blue are dimmed together, green on
        // its own, and alpha is set full.
        for (out, px) in band.chunks_exact_mut(4).zip(source.chunks_exact(4)) {
            let v = u32::from_le_bytes([px[0], px[1], px[2], px[3]]);
            let red_blue = (((v & 0x00FF_00FF) * keep) >> 8) & 0x00FF_00FF;
            let green = (((v & 0x0000_FF00) * keep) >> 8) & 0x0000_FF00;
            out.copy_from_slice(&(red_blue | green | 0xFF00_0000).to_le_bytes());
        }
    });
}

/// The quarter-size screen. Blur hides the lost detail, and a sixteenth of
/// the pixels is a sixteenth of the work.
fn build_quarter(screen: &Image) -> blur::Buf {
    blur::downscale(
        &screen.rgba,
        screen.width as usize,
        screen.height as usize,
        4,
    )
}

pub(crate) fn build_glass(quarter: &blur::Buf, scale: f32) -> Option<Pixmap> {
    let mut small = quarter.clone();
    blur::blur(&mut small, (5.0 * scale).round().max(2.0) as usize, 3);
    IntSize::from_wh(small.w as u32, small.h as u32)
        .and_then(|size| Pixmap::from_vec(small.px, size))
}

impl Renderer {
    pub fn new() -> Self {
        Self::default()
    }

    /// Builds what the first frame of `screen` needs. Safe to call on another
    /// thread; hand the result to [`Renderer::adopt`].
    pub fn prepare(screen: &Image, frame_index: usize, scale: f32) -> Layers {
        let mut dim = Vec::new();
        build_dim(screen, &mut dim);
        let quarter = build_quarter(screen);
        let glass = build_glass(&quarter, scale);
        Layers {
            key: (frame_index, screen.width, screen.height),
            dim,
            quarter,
            glass,
        }
    }

    /// Takes layers built by [`Renderer::prepare`], so the first frame has
    /// nothing left to compute.
    pub fn adopt(&mut self, layers: Layers) {
        self.key = Some(layers.key);
        self.dim = layers.dim;
        self.quarter = Some(layers.quarter);
        self.glass = layers.glass;
        self.mist = None;
    }

    /// Drops cached layers when the session shows a different frame.
    fn sync(&mut self, s: &Session) {
        let screen = s.screen();
        let key = Some((s.frame_index(), screen.width, screen.height));
        if self.key == key {
            return;
        }
        self.key = key;
        build_dim(screen, &mut self.dim);
        self.mist = None;
        self.glass = None;
        self.quarter = None;
    }

    fn quarter(&mut self, s: &Session) -> blur::Buf {
        self.quarter
            .get_or_insert_with(|| build_quarter(s.screen()))
            .clone()
    }

    fn ensure_glass(&mut self, s: &Session) {
        if self.glass.is_some() {
            return;
        }
        let quarter = self.quarter(s);
        self.glass = build_glass(&quarter, s.scale());
    }

    fn ensure_mist(&mut self, s: &Session) {
        if self.mist.is_some() {
            return;
        }
        let mut small = self.quarter(s);
        blur::blur(&mut small, (2.0 * s.scale()).round().max(1.0) as usize, 3);
        let screen = s.screen();
        let (w, h) = (screen.width as usize, screen.height as usize);
        self.mist = Some(blur::upscale(&small, w, h, theme::SOFT_KEEP));
    }

    /// Builds the cached layers now, so the first selection does not stall.
    /// The mist is left until the focus tool asks for it.
    pub fn warm(&mut self, s: &Session) {
        self.sync(s);
        self.ensure_glass(s);
    }

    /// Draws the whole overlay. `pixmap` must be the size of the screen.
    pub fn render(&mut self, s: &Session, pixmap: &mut Pixmap) {
        let screen = s.screen();
        assert_eq!(
            (pixmap.width(), pixmap.height()),
            (screen.width, screen.height)
        );
        self.sync(s);
        self.ensure_glass(s);
        let selection = s.selection();
        // The focus tool: inside the selection only the focus areas stay
        // sharp, exactly as the exported image will look.
        let focus: Vec<Rect> = match selection {
            Some(_) => s
                .doc()
                .items()
                .iter()
                .chain(s.draft())
                .filter_map(|item| match item.shape {
                    Shape::Focus { rect } => Some(rect),
                    _ => None,
                })
                .collect(),
            None => Vec::new(),
        };
        if !focus.is_empty() {
            self.ensure_mist(s);
        }
        let Some(glass) = self.glass.as_ref() else {
            return;
        };
        let scale = s.scale();
        let line = scale.round().max(1.0);

        // Around the selection the screen stays sharp, only a little dimmed,
        // so that nothing is hidden; the selection itself is at full
        // brightness.
        pixmap.data_mut().copy_from_slice(&self.dim);
        let lit = selection.or_else(|| s.hover_target());
        if let Some(rect) = lit {
            copy_rows(pixmap, &screen.rgba, screen, rect);
        }
        // A GIF in the making plays inside the selection.
        if let (Some(sel), Some(picture)) = (selection, s.clip_frame()) {
            paste(pixmap, picture, sel.round().intersect(&screen.bounds()));
        }
        if let (Some(sel), Some(mist)) = (selection, &self.mist) {
            if !focus.is_empty() {
                copy_rows(pixmap, mist, screen, sel);
                for rect in focus {
                    copy_rows(pixmap, &screen.rgba, screen, rect.round().intersect(&sel));
                }
            }
        }

        for item in s.doc().items().iter().chain(s.draft()) {
            draw_annotation(pixmap, item, Point::default(), screen, s.fonts());
        }
        if let Some(edit) = s.text_edit() {
            draw_text_edit(pixmap, s, &edit.text, edit.at);
        }
        if let Some(item) = s.selected().and_then(|i| s.doc().items().get(i)) {
            frame(
                pixmap,
                item.bounds().inflate(3.0 * scale).round(),
                line,
                ACCENT,
                1.0,
            );
        }

        if s.selecting() {
            let p = s.pointer();
            let (px, py) = (p.x.floor(), p.y.floor());
            let (w, h) = (screen.width as f32, screen.height as f32);
            fill_rect(pixmap, Rect::new(0.0, py, w, line), WHITE, 0.4);
            fill_rect(pixmap, Rect::new(px, 0.0, line, h), WHITE, 0.4);
            if let Some(rect) = lit {
                hairline(pixmap, rect, line);
                if selection.is_some() {
                    size_label(pixmap, glass, s.fonts(), rect, screen.bounds(), None, scale);
                }
            }
            loupe(pixmap, glass, s, p);
        } else if let Some(sel) = selection {
            hairline(pixmap, sel, line);
            crop_marks(pixmap, sel, scale);
            let strip = s.strip();
            let avoid = strip.as_ref().map(|l| l.rect);
            size_label(pixmap, glass, s.fonts(), sel, screen.bounds(), avoid, scale);
            if let Some(layout) = strip {
                draw_strip(pixmap, glass, s, &layout);
                if let Some(item) = s.strip_hover() {
                    strip_tooltip(pixmap, glass, s, &layout, item);
                }
            }
        }

        let rewound = s.frame_index() + 1 != s.frame_count();
        if s.clip().is_none() && s.frame_count() > 1 && (s.selecting() || rewound) {
            timeline(pixmap, glass, s);
        }
        if let (Some(view), Some(bar)) = (s.clip(), s.clip_bar()) {
            clip_bar(pixmap, glass, s.fonts(), view, &bar, scale);
        }
        links(pixmap, glass, s, scale);
        let monitor = s.monitor_of(s.pointer());
        if let Some(text) = s.notice() {
            let rect = pill_rect(s.fonts(), text, monitor, scale);
            pill(pixmap, glass, s.fonts(), text, rect, scale);
        } else if selection.is_none() && !s.help() {
            // Nothing selected yet: say what can be done.
            let text = match (s.quick(), s.snap()) {
                (true, true) => {
                    "Drag or click a box to copy    Tab for a larger area    Shift to draw first    K for a GIF    F1 keys"
                }
                (true, false) => {
                    "Drag or click a window to copy    Shift to draw first    K for a GIF    F1 keys"
                }
                (false, true) => {
                    "Drag to select    Click a box or window    Tab for a larger area    F1 keys"
                }
                (false, false) => "Drag to select    Click a window    F1 keys",
            };
            let rect = pill_rect(s.fonts(), text, monitor, scale);
            pill(pixmap, glass, s.fonts(), text, rect, scale);
        }
        if s.help() {
            help_panel(pixmap, glass, s.fonts(), monitor, scale);
        }
    }
}

/// Every key, in two columns.
const KEYS: [&[(&str, &str)]; 2] = [
    &[
        ("drag", "select a region"),
        ("click", "select the box or window"),
        ("Tab", "larger area, Shift smaller"),
        ("F", "whole monitor"),
        ("S", "shrink to content"),
        ("G", "snap on / off"),
        ("Alt", "hold to suspend snap"),
        ("arrows", "nudge, Shift x10"),
        ("Ctrl+arrows", "resize"),
        ("wheel", "rewind, before selecting"),
        ("PgUp PgDn", "rewind"),
        ("W", "select what changed"),
        ("C", "copy colour (Shift rgb, Alt hsl)"),
        ("Z", "select the last region again"),
        ("K", "save the last seconds as a GIF"),
        ("J", "before / after picture"),
        ("F1", "this list"),
    ],
    &[
        ("V", "select and move"),
        ("A L R E", "arrow, line, box, ellipse"),
        ("D H T N", "pen, marker, text, step"),
        ("B O Q", "blur, focus, measure"),
        ("1-6", "ink"),
        ("[ ]", "stroke width"),
        ("Ctrl+Z Y", "undo, redo"),
        ("Del", "delete a drawing"),
        ("double click", "edit text / copy"),
        ("M", "mask private text"),
        ("X", "copy text"),
        ("U", "frame the capture"),
        ("Y", "scrolling capture"),
        ("Enter", "copy"),
        ("Ctrl+S", "save"),
        ("P", "pin on screen"),
        ("Esc", "back, then close"),
    ],
];

/// The list of keys, on glass in the middle of the monitor.
fn help_panel(pixmap: &mut Pixmap, glass: &Pixmap, fonts: &Fonts, monitor: Rect, scale: f32) {
    let px = (12.0 * scale).round();
    let row = (23.0 * scale).round();
    let pad = (22.0 * scale).round();
    let key_w = (112.0 * scale).round();
    let column = (310.0 * scale).round();
    let title = (34.0 * scale).round();
    let rows = KEYS.iter().map(|c| c.len()).max().unwrap_or(0) as f32;
    let (w, h) = (2.0 * pad + 2.0 * column, 2.0 * pad + title + rows * row);
    let rect = Rect::new(
        (monitor.x + (monitor.w - w) / 2.0).round(),
        (monitor.y + (monitor.h - h) / 2.0).round(),
        w,
        h,
    )
    .clamp_inside(&monitor);
    glass_panel(pixmap, glass, rect, 14.0 * scale, scale);
    // A second tint: a list needs a calmer ground than a toolbar.
    if let Some(path) = rounded_rect(rect, 14.0 * scale) {
        fill_path(pixmap, &path, INK, 0.45);
    }
    let top = rect.y + pad;
    fonts.draw(
        pixmap,
        Face::Mono,
        "Keys",
        px,
        Point::new(rect.x + pad, top),
        PAPER,
    );
    let close = "any key closes this";
    let (cw, _) = fonts.measure(Face::Mono, close, px);
    fonts.draw(
        pixmap,
        Face::Mono,
        close,
        px,
        Point::new(rect.right() - pad - cw, top),
        FAINT,
    );
    let line = scale.round().max(1.0);
    let rule = Rect::new(
        rect.x + pad,
        top + title - (10.0 * scale).round(),
        rect.w - 2.0 * pad,
        line,
    );
    fill_rect(pixmap, rule, WHITE, 0.16);
    for (c, entries) in KEYS.iter().enumerate() {
        let x = rect.x + pad + c as f32 * column;
        for (i, (key, what)) in entries.iter().enumerate() {
            let y = top + title + i as f32 * row;
            fonts.draw(pixmap, Face::Mono, key, px, Point::new(x, y), PAPER);
            fonts.draw(
                pixmap,
                Face::Mono,
                what,
                px,
                Point::new(x + key_w, y),
                FAINT,
            );
        }
    }
}

/// The key that does what a strip item does.
fn strip_key(item: StripItem) -> String {
    match item {
        StripItem::Tool(tool) => tool.key().to_ascii_uppercase().to_string(),
        StripItem::Color => "1-6".into(),
        StripItem::Undo => "Ctrl+Z".into(),
        StripItem::Redo => "Ctrl+Y".into(),
        StripItem::Copy => "Enter".into(),
        StripItem::CopyText => "X".into(),
        StripItem::Save => "Ctrl+S".into(),
        StripItem::Pin => "P".into(),
    }
}

/// A small label naming the strip item under the pointer and its key.
fn strip_tooltip(
    pixmap: &mut Pixmap,
    glass: &Pixmap,
    s: &Session,
    layout: &StripLayout,
    item: StripItem,
) {
    let Some((_, cell)) = layout.cells.iter().find(|(i, _)| *i == item) else {
        return;
    };
    let (scale, fonts) = (s.scale(), s.fonts());
    let px = (11.0 * scale).round();
    let (name, key) = (item.name(), strip_key(item));
    let (nw, th) = fonts.measure(Face::Mono, name, px);
    let (kw, _) = fonts.measure(Face::Mono, &key, px);
    let (pad_x, pad_y, gap) = (
        (9.0 * scale).round(),
        (5.0 * scale).round(),
        (10.0 * scale).round(),
    );
    let (w, h) = (pad_x + nw + gap + kw + pad_x, th + 2.0 * pad_y);
    let bounds = s.screen().bounds();
    let above = layout.rect.y - h - (6.0 * scale).round();
    let y = if above >= bounds.y {
        above
    } else {
        layout.rect.bottom() + (6.0 * scale).round()
    };
    let rect = Rect::new((cell.x + cell.w / 2.0 - w / 2.0).round(), y, w, h).clamp_inside(&bounds);
    glass_panel(pixmap, glass, rect, 7.0 * scale, scale);
    if let Some(path) = rounded_rect(rect, 7.0 * scale) {
        fill_path(pixmap, &path, INK, 0.35);
    }
    fonts.draw(
        pixmap,
        Face::Mono,
        name,
        px,
        Point::new(rect.x + pad_x, rect.y + pad_y),
        PAPER,
    );
    fonts.draw(
        pixmap,
        Face::Mono,
        &key,
        px,
        Point::new(rect.x + pad_x + nw + gap, rect.y + pad_y),
        FAINT,
    );
}

/// The card that appears after a capture: a small picture of it and what was
/// done with it, on glass, in the corner of the monitor. Like [`toast`] it is
/// returned already composed over the part of `screen` it covers.
pub fn preview_card(
    screen: &Image,
    shot: &Image,
    fonts: &Fonts,
    text: &str,
    monitor: Rect,
    scale: f32,
) -> (Image, Rect) {
    let (card, rect, _) = preview_card_with_link(screen, shot, fonts, text, monitor, scale, None);
    (card, rect)
}

/// [`preview_card`] with one more line under the caption when there is a
/// link to offer; the third value is that line, in the card's coordinates.
pub fn preview_card_with_link(
    screen: &Image,
    shot: &Image,
    fonts: &Fonts,
    text: &str,
    monitor: Rect,
    scale: f32,
    link: Option<&str>,
) -> (Image, Rect, Option<Rect>) {
    let pad = (10.0 * scale).round();
    let caption = (30.0 * scale).round();
    let px = (11.0 * scale).round();
    let (max_w, max_h) = (240.0 * scale, 150.0 * scale);
    let (sw, sh) = (shot.width.max(1) as f32, shot.height.max(1) as f32);
    let fit = (max_w / sw).min(max_h / sh).min(1.0);
    let (tw, th) = ((sw * fit).round().max(1.0), (sh * fit).round().max(1.0));
    let (text_w, text_h) = fonts.measure(Face::Mono, text, px);
    let dot = (6.0 * scale).round();
    let inner = tw
        .max(dot + (8.0 * scale).round() + text_w)
        .max(150.0 * scale)
        .round();
    let row = (26.0 * scale).round();
    let link_w = link.map_or(0.0, |l| fonts.measure(Face::Mono, l, px).0);
    let inner = inner.max(link_w).round();
    let extra = if link.is_some() { row } else { 0.0 };
    let (w, h) = (inner + 2.0 * pad, pad + th + caption + extra);
    let margin = (24.0 * scale).round();
    let rect = Rect::new(
        monitor.right() - margin - w,
        monitor.bottom() - margin - h,
        w,
        h,
    )
    .round()
    .intersect(&screen.bounds());
    let crop = screen.crop(rect);
    let (cw, ch) = (crop.width, crop.height);
    let line_rect = link.map(|_| Rect::new(0.0, pad + th + caption, cw as f32, row));
    let mut small = blur::downscale(&crop.rgba, cw as usize, ch as usize, 4);
    blur::blur(&mut small, (4.0 * scale).round().max(2.0) as usize, 3);
    let glass = IntSize::from_wh(small.w as u32, small.h as u32)
        .and_then(|size| Pixmap::from_vec(small.px, size));
    let pixmap =
        IntSize::from_wh(cw, ch).and_then(|size| Pixmap::from_vec(crop.rgba.clone(), size));
    let (Some(glass), Some(mut pixmap)) = (glass, pixmap) else {
        return (crop, rect, line_rect);
    };
    let card = Rect::new(0.0, 0.0, cw as f32, ch as f32);
    glass_panel(&mut pixmap, &glass, card, 12.0 * scale, scale);

    // The thumbnail. Shrinking by whole steps first keeps it from shimmering.
    let step = (1.0 / fit).floor().max(1.0) as usize;
    let reduced = blur::downscale(&shot.rgba, shot.width as usize, shot.height as usize, step);
    let thumb = Rect::new(((card.w - tw) / 2.0).round(), pad, tw, th);
    if let Some(source) = IntSize::from_wh(reduced.w as u32, reduced.h as u32)
        .and_then(|size| Pixmap::from_vec(reduced.px, size))
    {
        let paint = PixmapPaint {
            quality: FilterQuality::Bilinear,
            ..PixmapPaint::default()
        };
        let transform =
            Transform::from_scale(tw / source.width() as f32, th / source.height() as f32)
                .post_translate(thumb.x, thumb.y);
        pixmap.draw_pixmap(0, 0, source.as_ref(), &paint, transform, None);
    }
    let line = scale.round().max(1.0);
    frame(&mut pixmap, thumb.inflate(line), line, WHITE, 0.22);

    let cy = pad + th + caption / 2.0;
    if let Some(path) = PathBuilder::from_circle(pad + dot / 2.0, cy, dot / 2.0) {
        fill_path(&mut pixmap, &path, ACCENT, 1.0);
    }
    let at = Point::new(
        pad + dot + (8.0 * scale).round(),
        (cy - text_h / 2.0).round(),
    );
    fonts.draw(&mut pixmap, Face::Mono, text, px, at, PAPER);
    // The link, in the accent colour, on a line of its own.
    if let (Some(line), Some(label)) = (line_rect, link) {
        let (_, lh) = fonts.measure(Face::Mono, label, px);
        let at = Point::new(pad, (line.y + (line.h - lh) / 2.0 - (4.0 * scale)).round());
        fonts.draw(&mut pixmap, Face::Mono, label, px, at, ACCENT);
    }
    (Image::new(cw, ch, pixmap.take()), rect, line_rect)
}

/// Copies a finished frame into a window's buffer, which wants one
/// `0x00RRGGBB` word per pixel.
pub fn present(pixmap: &Pixmap, frame: &mut [u32]) {
    let source = pixmap.data();
    let width = (pixmap.width() as usize).max(1);
    crate::par::bands(frame, width, |first, band| {
        let bytes = &source[first * width * 4..];
        for (out, px) in band.iter_mut().zip(bytes.chunks_exact(4)) {
            let v = u32::from_le_bytes([px[0], px[1], px[2], px[3]]);
            *out = ((v & 0xFF) << 16) | (v & 0xFF00) | ((v >> 16) & 0xFF);
        }
    });
}

/// Links on the screen: a light underline under written addresses, a thin
/// outline round QR codes, and the button of the one the pointer is over.
fn links(pixmap: &mut Pixmap, glass: &Pixmap, s: &Session, scale: f32) {
    let line = (1.5 * scale).max(1.0);
    for link in s.links() {
        let r = link.rect;
        if link.qr {
            for edge in [
                Rect::new(r.x, r.y, r.w, line),
                Rect::new(r.x, r.bottom() - line, r.w, line),
                Rect::new(r.x, r.y, line, r.h),
                Rect::new(r.right() - line, r.y, line, r.h),
            ] {
                fill_rect(pixmap, edge, ACCENT, 0.55);
            }
        } else {
            fill_rect(
                pixmap,
                Rect::new(r.x, r.bottom() + line, r.w, line),
                ACCENT,
                0.55,
            );
        }
    }
    if let Some((button, link)) = s.link_button() {
        glass_panel(pixmap, glass, button, button.h / 2.0, scale);
        let label = crate::session::link_label(&link.kind);
        let px = (12.0 * scale).round();
        let (tw, th) = s.fonts().measure(Face::Mono, label, px);
        let at = Point::new(
            (button.x + (button.w - tw) / 2.0).round(),
            (button.y + (button.h - th) / 2.0).round(),
        );
        s.fonts().draw(pixmap, Face::Mono, label, px, at, PAPER);
    }
}

/// Where a message pill sits: bottom centre of `monitor`.
fn pill_rect(fonts: &Fonts, text: &str, monitor: Rect, scale: f32) -> Rect {
    let px = (12.0 * scale).round();
    let (tw, _) = fonts.measure(Face::Mono, text, px);
    let pad = (14.0 * scale).round();
    let dot = (6.0 * scale).round();
    let h = (34.0 * scale).round();
    let w = (pad + dot + (9.0 * scale).round() + tw + pad).round();
    Rect::new(
        (monitor.x + (monitor.w - w) / 2.0).round(),
        (monitor.bottom() - h - (56.0 * scale).round()).max(monitor.y),
        w,
        h,
    )
}

/// A short message on a glass pill: an accent dot, then the text.
fn pill(pixmap: &mut Pixmap, glass: &Pixmap, fonts: &Fonts, text: &str, rect: Rect, scale: f32) {
    glass_panel(pixmap, glass, rect, rect.h / 2.0, scale);
    let px = (12.0 * scale).round();
    let pad = (14.0 * scale).round();
    let dot = (6.0 * scale).round();
    let cy = rect.y + rect.h / 2.0;
    if let Some(path) = PathBuilder::from_circle(rect.x + pad + dot / 2.0, cy, dot / 2.0) {
        fill_path(pixmap, &path, ACCENT, 1.0);
    }
    let (_, th) = fonts.measure(Face::Mono, text, px);
    let at = Point::new(
        rect.x + pad + dot + (9.0 * scale).round(),
        (cy - th / 2.0).round(),
    );
    fonts.draw(pixmap, Face::Mono, text, px, at, PAPER);
}

/// A message that floats over the live screen after the overlay has closed:
/// "Copied 400 × 300". Returns the picture of the pill, already composed over
/// the part of `screen` it covers, and where on the screen it belongs.
pub fn toast(
    screen: &Image,
    fonts: &Fonts,
    text: &str,
    monitor: Rect,
    scale: f32,
) -> (Image, Rect) {
    let rect = pill_rect(fonts, text, monitor, scale).intersect(&screen.bounds());
    let crop = screen.crop(rect);
    let (w, h) = (crop.width, crop.height);
    let local = Rect::new(0.0, 0.0, w as f32, h as f32);
    let mut small = blur::downscale(&crop.rgba, w as usize, h as usize, 4);
    blur::blur(&mut small, (4.0 * scale).round().max(2.0) as usize, 3);
    let glass = IntSize::from_wh(small.w as u32, small.h as u32)
        .and_then(|size| Pixmap::from_vec(small.px, size));
    let pixmap = IntSize::from_wh(w, h).and_then(|size| Pixmap::from_vec(crop.rgba.clone(), size));
    match (glass, pixmap) {
        (Some(glass), Some(mut pixmap)) => {
            pill(&mut pixmap, &glass, fonts, text, local, scale);
            (Image::new(w, h, pixmap.take()), rect)
        }
        _ => (crop, rect),
    }
}

/// Puts `picture` into the frame with its top-left corner at the corner of
/// `area`, cut to `area`.
fn paste(pixmap: &mut Pixmap, picture: &Image, area: Rect) {
    let (fw, fh) = (pixmap.width() as i64, pixmap.height() as i64);
    let (x0, y0) = (area.x as i64, area.y as i64);
    let w = (area.w as i64)
        .min(picture.width as i64)
        .min(fw - x0)
        .max(0) as usize;
    let h = (area.h as i64)
        .min(picture.height as i64)
        .min(fh - y0)
        .max(0) as usize;
    if w == 0 || x0 < 0 || y0 < 0 {
        return;
    }
    let stride = fw as usize * 4;
    let data = pixmap.data_mut();
    for y in 0..h {
        let from = y * picture.width as usize * 4;
        let to = (y0 as usize + y) * stride + x0 as usize * 4;
        data[to..to + w * 4].copy_from_slice(&picture.rgba[from..from + w * 4]);
    }
}

/// The bar under a GIF in the making: play and pause, the recording laid out
/// along a track, the part kept between two handles, and how long it is.
fn clip_bar(
    pixmap: &mut Pixmap,
    glass: &Pixmap,
    fonts: &Fonts,
    view: &crate::session::ClipView,
    bar: &crate::session::ClipBar,
    scale: f32,
) {
    let px = |v: f32| (v * scale).round();
    glass_panel(pixmap, glass, bar.rect, bar.rect.h / 2.0, scale);

    // Play is a triangle; pause, two bars.
    let c = bar.play.center();
    let r = px(6.0);
    if view.playing {
        for dx in [-px(3.5), px(3.5)] {
            fill_rect(
                pixmap,
                Rect::new((c.x + dx - px(1.5)).round(), c.y - r, px(3.0), 2.0 * r),
                PAPER,
                1.0,
            );
        }
    } else {
        let mut pb = PathBuilder::new();
        pb.move_to(c.x - r * 0.7, c.y - r);
        pb.line_to(c.x + r, c.y);
        pb.line_to(c.x - r * 0.7, c.y + r);
        pb.close();
        if let Some(path) = pb.finish() {
            fill_path(pixmap, &path, PAPER, 1.0);
        }
    }

    let track = bar.track;
    let mid = track.y + track.h / 2.0;
    let t = px(2.0).max(2.0);
    // The whole recording, faint; the part kept, bright.
    fill_rect(
        pixmap,
        Rect::new(track.x, (mid - t / 2.0).round(), track.w, t),
        PAPER,
        0.25,
    );
    let (a, b) = (bar.x_of(view.from), bar.x_of(view.to));
    fill_rect(
        pixmap,
        Rect::new(a, (mid - t / 2.0).round(), (b - a).max(1.0), t),
        ACCENT,
        1.0,
    );
    // The handles.
    for x in [a, b] {
        let handle = Rect::new((x - px(3.0)).round(), track.y, px(6.0), track.h);
        if let Some(path) = rounded_rect(handle, px(3.0)) {
            fill_path(pixmap, &path, PAPER, 1.0);
        }
    }
    // The moment shown.
    let at = bar.x_of(view.at);
    fill_rect(
        pixmap,
        Rect::new(
            at.round(),
            track.y + px(3.0),
            px(1.5).max(1.0),
            track.h - px(6.0),
        ),
        PAPER,
        0.85,
    );

    let seconds = (view.to - view.from) as f32 / 1000.0;
    let text = if view.rate > 0 {
        format!("{seconds:.1} s  {} fps", view.rate)
    } else {
        format!("{seconds:.1} s")
    };
    let size = px(12.0);
    let (tw, th) = fonts.measure(Face::Mono, &text, size);
    let at = Point::new(
        (bar.rect.right() - px(18.0) - tw).round(),
        (bar.rect.y + (bar.rect.h - th) / 2.0).round(),
    );
    fonts.draw(pixmap, Face::Mono, &text, size, at, PAPER);
}

/// Copies the pixels under `rect` from `source`, a buffer the size of the
/// screen, into the frame.
fn copy_rows(pixmap: &mut Pixmap, source: &[u8], screen: &Image, rect: Rect) {
    let (x0, y0, x1, y1) = screen.pixel_span(rect);
    if x1 <= x0 {
        return;
    }
    let stride = screen.width as usize * 4;
    let data = pixmap.data_mut();
    for y in y0..y1 {
        let a = y as usize * stride + x0 as usize * 4;
        let b = y as usize * stride + x1 as usize * 4;
        data[a..b].copy_from_slice(&source[a..b]);
    }
}

/// A rectangular outline of thickness `t` drawn just inside `rect`.
fn frame(pixmap: &mut Pixmap, rect: Rect, t: f32, color: [u8; 3], alpha: f32) {
    if rect.w <= 0.0 || rect.h <= 0.0 {
        return;
    }
    fill_rect(pixmap, Rect::new(rect.x, rect.y, rect.w, t), color, alpha);
    fill_rect(
        pixmap,
        Rect::new(rect.x, rect.bottom() - t, rect.w, t),
        color,
        alpha,
    );
    fill_rect(
        pixmap,
        Rect::new(rect.x, rect.y + t, t, rect.h - 2.0 * t),
        color,
        alpha,
    );
    fill_rect(
        pixmap,
        Rect::new(rect.right() - t, rect.y + t, t, rect.h - 2.0 * t),
        color,
        alpha,
    );
}

/// The selection outline: a light line hugging the selection from outside and
/// a faint dark one outside that, so it reads on any background.
fn hairline(pixmap: &mut Pixmap, rect: Rect, t: f32) {
    frame(pixmap, rect.inflate(2.0 * t), t, INK, 0.45);
    frame(pixmap, rect.inflate(t), t, WHITE, 0.95);
}

/// Short strokes off each corner, like the crop marks on a print sheet.
fn crop_marks(pixmap: &mut Pixmap, rect: Rect, scale: f32) {
    let t = (1.5 * scale).round().max(1.0);
    let gap = (5.0 * scale).round();
    let len = (9.0 * scale).round();
    for (cx, sx) in [(rect.x, -1.0), (rect.right(), 1.0)] {
        for (cy, sy) in [(rect.y, -1.0), (rect.bottom(), 1.0)] {
            // The mark sits on the outline's axis, outside the selection.
            let ax = if sx < 0.0 { cx - t } else { cx };
            let ay = if sy < 0.0 { cy - t } else { cy };
            let hx = if sx < 0.0 { cx - gap - len } else { cx + gap };
            let vy = if sy < 0.0 { cy - gap - len } else { cy + gap };
            fill_rect(pixmap, Rect::new(hx, ay, len, t), WHITE, 0.95);
            fill_rect(pixmap, Rect::new(ax, vy, t, len), WHITE, 0.95);
        }
    }
}

pub(crate) fn rounded_rect(rect: Rect, radius: f32) -> Option<Path> {
    let r = radius.min(rect.w / 2.0).min(rect.h / 2.0).max(0.0);
    let (x0, y0, x1, y1) = (rect.x, rect.y, rect.right(), rect.bottom());
    // Control-point offset that makes a cubic corner a near-perfect arc.
    let k = r * 0.4477;
    let mut pb = PathBuilder::new();
    pb.move_to(x0 + r, y0);
    pb.line_to(x1 - r, y0);
    pb.cubic_to(x1 - k, y0, x1, y0 + k, x1, y0 + r);
    pb.line_to(x1, y1 - r);
    pb.cubic_to(x1, y1 - k, x1 - k, y1, x1 - r, y1);
    pb.line_to(x0 + r, y1);
    pb.cubic_to(x0 + k, y1, x0, y1 - k, x0, y1 - r);
    pb.line_to(x0, y0 + r);
    pb.cubic_to(x0, y0 + k, x0 + k, y0, x0 + r, y0);
    pb.close();
    pb.finish()
}

/// A paint that shows the blurred screen, lined up with the frame.
fn glass_paint<'a>(pixmap: &Pixmap, glass: &'a Pixmap) -> Paint<'a> {
    let sx = pixmap.width() as f32 / glass.width() as f32;
    let sy = pixmap.height() as f32 / glass.height() as f32;
    Paint {
        shader: Pattern::new(
            glass.as_ref(),
            SpreadMode::Pad,
            FilterQuality::Bilinear,
            1.0,
            Transform::from_scale(sx, sy),
        ),
        anti_alias: true,
        ..Paint::default()
    }
}

/// A frosted-glass shape: the blurred screen behind it, a dark tint, and a
/// thin rim of light.
fn glass_shape(pixmap: &mut Pixmap, glass: &Pixmap, path: &Path, rim: Option<&Path>, scale: f32) {
    let backdrop = glass_paint(pixmap, glass);
    pixmap.fill_path(
        path,
        &backdrop,
        FillRule::Winding,
        Transform::identity(),
        None,
    );
    fill_path(pixmap, path, INK, theme::GLASS_TINT);
    if let Some(rim) = rim {
        let t = scale.round().max(1.0);
        stroke_path(pixmap, rim, WHITE, 0.16, t, LineCap::Butt, LineJoin::Round);
    }
}

pub(crate) fn glass_panel(
    pixmap: &mut Pixmap,
    glass: &Pixmap,
    rect: Rect,
    radius: f32,
    scale: f32,
) {
    let t = scale.round().max(1.0);
    let Some(path) = rounded_rect(rect, radius) else {
        return;
    };
    let rim = rounded_rect(rect.inflate(-t / 2.0), radius - t / 2.0);
    glass_shape(pixmap, glass, &path, rim.as_ref(), scale);
}

/// The `1280 × 720` readout above the selection's top-left corner.
fn size_label(
    pixmap: &mut Pixmap,
    glass: &Pixmap,
    fonts: &Fonts,
    sel: Rect,
    bounds: Rect,
    avoid: Option<Rect>,
    scale: f32,
) {
    let px = (11.0 * scale).round();
    let text = format!("{} \u{00D7} {}", sel.w as i64, sel.h as i64);
    let (tw, th) = fonts.measure(Face::Mono, &text, px);
    let (pad_x, pad_y) = ((8.0 * scale).round(), (4.0 * scale).round());
    let gap = (8.0 * scale).round();
    let (w, h) = (tw + 2.0 * pad_x, th + 2.0 * pad_y);
    let mut rect = Rect::new(sel.x, sel.y - gap - h, w, h);
    let blocked = |r: &Rect| avoid.is_some_and(|a| !a.intersect(r).is_empty());
    if rect.y < bounds.y || blocked(&rect) {
        rect = Rect::new(sel.x + gap, sel.y + gap, w, h);
    }
    let rect = rect.clamp_inside(&bounds).round();
    glass_panel(pixmap, glass, rect, 7.0 * scale, scale);
    fonts.draw(
        pixmap,
        Face::Mono,
        &text,
        px,
        Point::new(rect.x + pad_x, rect.y + pad_y),
        PAPER,
    );
}

/// The round magnifier that follows the pointer while selecting.
fn loupe(pixmap: &mut Pixmap, glass: &Pixmap, s: &Session, p: Point) {
    let scale = s.scale();
    let screen = s.screen();
    let bounds = screen.bounds();
    let zoom = (8.0 * scale).round();
    let side = LOUPE_PIXELS as f32 * zoom;
    let t = scale.round().max(1.0);
    let px = (11.0 * scale).round();
    let pad = (9.0 * scale).round();
    let line_h = s.fonts().line_height(Face::Mono, px);
    let info_h = 2.0 * line_h + 2.0 * (6.0 * scale).round();
    let gap = (6.0 * scale).round();
    let total_h = side + gap + info_h;
    let offset = (18.0 * scale).round();

    let mut x = p.x + offset;
    let mut y = p.y + offset;
    if x + side + 2.0 * t > bounds.right() {
        x = p.x - offset - side;
    }
    if y + total_h + 2.0 * t > bounds.bottom() {
        y = p.y - offset - total_h;
    }
    let (x, y) = (
        x.max(bounds.x + 2.0 * t).round(),
        y.max(bounds.y + 2.0 * t).round(),
    );

    // The magnified pixels, as a tiny image scaled up without smoothing.
    let (cx, cy) = (p.x.floor() as i32, p.y.floor() as i32);
    let half = LOUPE_PIXELS / 2;
    let n = LOUPE_PIXELS as u32;
    let mut cells = Vec::with_capacity((n * n * 4) as usize);
    for row in 0..LOUPE_PIXELS {
        for col in 0..LOUPE_PIXELS {
            let c = screen.pixel(cx + col - half, cy + row - half);
            cells.extend_from_slice(&[c[0], c[1], c[2], 255]);
        }
    }
    let radius = side / 2.0;
    let centre = Point::new(x + radius, y + radius);
    if let (Some(tile), Some(lens)) = (
        IntSize::from_wh(n, n).and_then(|size| Pixmap::from_vec(cells, size)),
        PathBuilder::from_circle(centre.x, centre.y, radius),
    ) {
        let paint = Paint {
            shader: Pattern::new(
                tile.as_ref(),
                SpreadMode::Pad,
                FilterQuality::Nearest,
                1.0,
                Transform::from_scale(zoom, zoom).post_translate(x, y),
            ),
            anti_alias: true,
            ..Paint::default()
        };
        pixmap.fill_path(
            &lens,
            &paint,
            FillRule::Winding,
            Transform::identity(),
            None,
        );
    }
    if let Some(ring) = PathBuilder::from_circle(centre.x, centre.y, radius + t) {
        stroke_path(
            pixmap,
            &ring,
            INK,
            0.4,
            3.0 * t,
            LineCap::Butt,
            LineJoin::Round,
        );
    }
    if let Some(ring) = PathBuilder::from_circle(centre.x, centre.y, radius) {
        stroke_path(
            pixmap,
            &ring,
            WHITE,
            0.95,
            1.5 * t,
            LineCap::Butt,
            LineJoin::Round,
        );
    }
    let cell = Rect::new(x + half as f32 * zoom, y + half as f32 * zoom, zoom, zoom);
    let mark = (1.5 * scale).round().max(1.0);
    frame(pixmap, cell.inflate(mark), mark, ACCENT, 1.0);

    // Colour and position, on glass.
    let colour = screen.pixel(cx, cy);
    let hex = theme::hex(colour);
    let coords = format!("{cx}, {cy}");
    let swatch = (10.0 * scale).round();
    let text_w = s
        .fonts()
        .measure(Face::Mono, &hex, px)
        .0
        .max(s.fonts().measure(Face::Mono, &coords, px).0);
    let info_w = (pad + swatch + (7.0 * scale).round() + text_w + pad).max(side * 0.8);
    let info = Rect::new(
        (centre.x - info_w / 2.0).round(),
        y + side + gap,
        info_w,
        info_h,
    );
    let info = info.clamp_inside(&bounds);
    glass_panel(pixmap, glass, info, 9.0 * scale, scale);
    let text_y = info.y + (6.0 * scale).round();
    let dot = Point::new(info.x + pad + swatch / 2.0, text_y + line_h / 2.0);
    if let Some(path) = PathBuilder::from_circle(dot.x, dot.y, swatch / 2.0 + t) {
        fill_path(pixmap, &path, WHITE, 0.85);
    }
    if let Some(path) = PathBuilder::from_circle(dot.x, dot.y, swatch / 2.0) {
        fill_path(pixmap, &path, [colour[0], colour[1], colour[2]], 1.0);
    }
    let text_x = info.x + pad + swatch + (7.0 * scale).round();
    s.fonts().draw(
        pixmap,
        Face::Mono,
        &hex,
        px,
        Point::new(text_x, text_y),
        PAPER,
    );
    s.fonts().draw(
        pixmap,
        Face::Mono,
        &coords,
        px,
        Point::new(text_x, text_y + line_h),
        FAINT,
    );
}

/// The rewind readout: one dot per recorded moment and how far back we are.
fn timeline(pixmap: &mut Pixmap, glass: &Pixmap, s: &Session) {
    let scale = s.scale();
    let monitor = s.monitor_of(s.pointer());
    let px = (11.0 * scale).round();
    let label = match s.frame_age_ms() {
        0 => "now".to_string(),
        ms => format!("-{:.1} s", ms as f32 / 1000.0),
    };
    let (tw, th) = s.fonts().measure(Face::Mono, &label, px);
    // Every label is laid out as wide as the widest, so the pill never jumps.
    let label_w = tw.max(s.fonts().measure(Face::Mono, "-00.0 s", px).0);
    let count = s.frame_count().min(24);
    let first = s.frame_count() - count;
    let dot = (6.0 * scale).round();
    let step = (11.0 * scale).round();
    let pad = (12.0 * scale).round();
    let dots_w = step * (count - 1) as f32 + dot;
    let h = (28.0 * scale).round();
    let w = pad + dots_w + pad + label_w + pad;
    let rect = Rect::new(
        (monitor.x + (monitor.w - w) / 2.0).round(),
        monitor.y + (16.0 * scale).round(),
        w,
        h,
    );
    glass_panel(pixmap, glass, rect, h / 2.0, scale);
    let cy = rect.y + h / 2.0;
    for i in 0..count {
        let current = first + i == s.frame_index();
        let cx = rect.x + pad + dot / 2.0 + step * i as f32;
        let r = if current {
            dot / 2.0 + scale
        } else {
            dot / 2.0
        };
        if let Some(path) = PathBuilder::from_circle(cx, cy, r) {
            let (colour, alpha) = if current { (ACCENT, 1.0) } else { (WHITE, 0.4) };
            fill_path(pixmap, &path, colour, alpha);
        }
    }
    let text_at = Point::new(rect.x + pad + dots_w + pad, (cy - th / 2.0).round());
    s.fonts()
        .draw(pixmap, Face::Mono, &label, px, text_at, PAPER);
}

fn draw_strip(pixmap: &mut Pixmap, glass: &Pixmap, s: &Session, layout: &StripLayout) {
    let scale = s.scale();
    let fonts = s.fonts();
    glass_panel(pixmap, glass, layout.rect, 11.0 * scale, scale);
    let t = scale.round().max(1.0);
    for x in &layout.separators {
        let inset = (9.0 * scale).round();
        let sep = Rect::new(*x, layout.rect.y + inset, t, layout.rect.h - 2.0 * inset);
        fill_rect(pixmap, sep, WHITE, 0.16);
    }

    let glyph_size = (16.0 * scale).round();
    let glyph_top = (8.0 * scale).round();
    let hint_px = (11.0 * scale).round();
    let hint_top = (27.0 * scale).round();
    for (item, cell) in &layout.cells {
        let active = matches!(item, StripItem::Tool(tool) if *tool == s.tool());
        if active {
            let inset_x = (2.0 * scale).round();
            let inset_y = (4.0 * scale).round();
            let bg = Rect::new(
                cell.x + inset_x,
                cell.y + inset_y,
                cell.w - 2.0 * inset_x,
                cell.h - 2.0 * inset_y,
            );
            if let Some(path) = rounded_rect(bg, 7.0 * scale) {
                fill_path(pixmap, &path, PAPER, 0.96);
            }
        }
        // Undo and redo fade when there is nothing to undo or redo.
        let usable = match item {
            StripItem::Undo => s.doc().can_undo(),
            StripItem::Redo => s.doc().can_redo(),
            _ => true,
        };
        let (fg, dim) = match (active, usable) {
            (true, _) => (INK, INK),
            (false, true) => (PAPER, FAINT),
            (false, false) => ([0x74, 0x72, 0x6C], [0x5E, 0x5C, 0x57]),
        };
        let origin = Point::new(
            (cell.x + (cell.w - glyph_size) / 2.0).round(),
            cell.y + glyph_top,
        );

        if *item == StripItem::Color {
            let c = Point::new(origin.x + glyph_size / 2.0, origin.y + glyph_size / 2.0);
            if let Some(ring) = PathBuilder::from_circle(c.x, c.y, 7.0 * scale) {
                fill_path(pixmap, &ring, WHITE, 0.85);
            }
            if let Some(dot) = PathBuilder::from_circle(c.x, c.y, 7.0 * scale - 1.5 * t) {
                fill_path(pixmap, &dot, theme::ink(s.color()), 1.0);
            }
        } else {
            glyph::draw(pixmap, *item, origin, glyph_size, fg);
        }

        let hint = match item {
            StripItem::Tool(tool) => tool.key().to_ascii_uppercase().to_string(),
            StripItem::Color => (s.color() + 1).to_string(),
            StripItem::Undo => "^Z".to_string(),
            StripItem::Redo => "^Y".to_string(),
            StripItem::Copy => String::new(),
            StripItem::CopyText => "X".to_string(),
            StripItem::Save => "^S".to_string(),
            StripItem::Pin => "P".to_string(),
        };
        if hint.is_empty() {
            let size = (11.0 * scale).round();
            let at = Point::new(
                (cell.x + (cell.w - size) / 2.0).round(),
                cell.y + hint_top + scale.round(),
            );
            glyph::draw_enter(pixmap, at, size, dim);
        } else {
            let (w, _) = fonts.measure(Face::Mono, &hint, hint_px);
            let at = Point::new((cell.x + (cell.w - w) / 2.0).round(), cell.y + hint_top);
            fonts.draw(pixmap, Face::Mono, &hint, hint_px, at, dim);
        }
    }
}

/// Text being typed, with a caret after the last character.
fn draw_text_edit(pixmap: &mut Pixmap, s: &Session, text: &str, at: Point) {
    let fonts = s.fonts();
    let size = s.text_size();
    fonts.draw(pixmap, Face::Ui, text, size, at, theme::ink(s.color()));
    let line_h = fonts.line_height(Face::Ui, size);
    let last = text.rsplit('\n').next().unwrap_or("");
    let (w, _) = fonts.measure(Face::Ui, last, size);
    let rows = text.split('\n').count().max(1) as f32;
    let t = (1.5 * s.scale()).round().max(1.0);
    let caret = Rect::new(
        (at.x + w).round() + t,
        (at.y + (rows - 1.0) * line_h).round(),
        t,
        line_h,
    );
    fill_rect(pixmap, caret, ACCENT, 1.0);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::{Frame, Input, Key, Mods, Options};
    use std::rc::Rc;
    use std::sync::Arc;

    const SIZE: (u32, u32) = (640, 480);

    /// Grey screen with one lighter window.
    fn screen() -> Image {
        let mut screen = Image::filled(SIZE.0, SIZE.1, [200, 200, 200, 255]);
        screen.fill_rect(Rect::new(100.0, 100.0, 200.0, 150.0), [240, 240, 240, 255]);
        screen
    }

    fn session_with(frames: Vec<Frame>, scale: f32) -> Session {
        Session::new(
            frames,
            Rc::new(Fonts::empty()),
            vec![Rect::new(100.0, 100.0, 200.0, 150.0)],
            vec![Rect::new(0.0, 0.0, 640.0, 480.0)],
            Options {
                scale,
                ..Options::default()
            },
        )
    }

    fn session(scale: f32) -> Session {
        session_with(vec![Frame::now(screen())], scale)
    }

    fn render(s: &Session) -> Image {
        let mut pixmap = Pixmap::new(SIZE.0, SIZE.1).unwrap();
        Renderer::new().render(s, &mut pixmap);
        Image::new(SIZE.0, SIZE.1, pixmap.take())
    }

    #[test]
    fn a_card_with_a_link_is_taller_and_says_so() {
        let screen = Image::filled(1280, 800, [90, 90, 90, 255]);
        let shot = Image::filled(400, 300, [200, 200, 200, 255]);
        let monitor = Rect::new(0.0, 0.0, 1280.0, 800.0);
        let fonts = Fonts::empty();
        let (plain, _, none) =
            preview_card_with_link(&screen, &shot, &fonts, "Copied", monitor, 1.0, None);
        let (card, _, line) = preview_card_with_link(
            &screen,
            &shot,
            &fonts,
            "Copied",
            monitor,
            1.0,
            Some("Copy link  github.com/0301091405/bruma"),
        );
        assert!(
            card.height > plain.height,
            "{} vs {}",
            card.height,
            plain.height
        );
        assert_eq!(none, None);
        let line = line.expect("a link line");
        assert!(
            line.bottom() <= card.height as f32 && line.right() <= card.width as f32,
            "{line:?}"
        );
        assert!(line.y >= plain.height as f32 - 1.0, "{line:?}");
    }

    #[test]
    fn links_get_an_underline_and_the_hovered_one_a_button() {
        let link = crate::links::Link {
            url: "https://example.com".into(),
            rect: Rect::new(320.0, 300.0, 150.0, 16.0),
            kind: crate::links::Kind::Web,
            qr: false,
        };
        let mut s = session(1.0);
        let before = render(&s);
        let frame = s.frame_index();
        s.handle(Input::Links {
            frame,
            links: vec![link],
        });
        let underlined = render(&s);
        let at = |img: &Image, x: u32, y: u32| {
            let i = ((y * img.width + x) * 4) as usize;
            img.rgba[i..i + 4].to_vec()
        };
        assert_ne!(
            at(&before, 400, 317),
            at(&underlined, 400, 317),
            "an underline"
        );
        s.handle(Input::PointerMove(Point::new(400.0, 308.0)));
        let (button, _) = s.link_button().expect("a button");
        let hovered = render(&s);
        let (bx, by) = ((button.x + 3.0) as u32, (button.y + button.h / 2.0) as u32);
        assert_ne!(at(&underlined, bx, by), at(&hovered, bx, by), "a button");
    }

    fn select(s: &mut Session) {
        s.handle(Input::PointerDown(Point::new(200.0, 120.0)));
        s.handle(Input::PointerMove(Point::new(500.0, 320.0)));
        s.handle(Input::PointerUp(Point::new(500.0, 320.0)));
    }

    fn scaled(value: u32, keep: f32) -> u8 {
        ((value * (keep * 256.0) as u32) >> 8) as u8
    }

    #[test]
    fn idle_screen_is_dimmed_but_sharp() {
        let mut s = session(1.0);
        s.handle(Input::PointerMove(Point::new(600.0, 30.0)));
        let frame = render(&s);
        let grey = scaled(200, theme::DIM_KEEP);
        assert_eq!(frame.pixel(20, 400), [grey, grey, grey, 255]);
        // The window edge at x = 100 is still a hard step.
        let light = scaled(240, theme::DIM_KEEP);
        assert_eq!(frame.pixel(99, 200)[0], grey);
        assert_eq!(frame.pixel(100, 200)[0], light);
    }

    #[test]
    fn hovered_window_is_in_focus() {
        let mut s = session(1.0);
        s.handle(Input::PointerMove(Point::new(150.0, 240.0)));
        let frame = render(&s);
        assert_eq!(frame.pixel(280, 110), [240, 240, 240, 255]);
        assert_eq!(frame.pixel(20, 400)[0], scaled(200, theme::DIM_KEEP));
    }

    #[test]
    fn selection_is_sharp_and_bright() {
        let mut s = session(1.0);
        select(&mut s);
        let frame = render(&s);
        assert_eq!(frame.pixel(400, 300), [200, 200, 200, 255]);
        assert_eq!(frame.pixel(250, 200), [240, 240, 240, 255]);
        assert!(
            frame.pixel(199, 200)[0] > 235,
            "light hairline left of the selection"
        );
    }

    #[test]
    fn outside_the_selection_stays_sharp_and_is_only_dimmed() {
        let mut s = session(1.0);
        select(&mut s);
        let frame = render(&s);
        assert_eq!(frame.pixel(20, 440)[0], scaled(200, theme::DIM_KEEP));
        // The window's hard edge at x = 100 is still hard: no blur.
        assert_eq!(frame.pixel(99, 160)[0], scaled(200, theme::DIM_KEEP));
        assert_eq!(frame.pixel(101, 160)[0], scaled(240, theme::DIM_KEEP));
    }

    #[test]
    fn a_gif_in_the_making_plays_in_the_selection_with_its_bar() {
        let mut s = session(1.0);
        select(&mut s);
        s.handle(Input::Key(Key::Char('k'), Mods::default()));
        s.enter_clip(3000, 30);
        let sel = s.selection().unwrap();
        let picture = Image::filled(sel.w as u32, sel.h as u32, [10, 200, 30, 255]);
        s.show_clip_frame(std::sync::Arc::new(picture));
        let frame = render(&s);
        let inside = frame.pixel((sel.x + 20.0) as i32, (sel.y + 20.0) as i32);
        assert_eq!(inside, [10, 200, 30, 255]);
        let bar = s.clip_bar().unwrap();
        let (a, mid) = (bar.x_of(1500), bar.track.center().y);
        assert_eq!(
            frame.pixel(a as i32, mid as i32)[..3],
            ACCENT,
            "the part kept is lit"
        );
    }

    #[test]
    fn panels_are_glass_not_flat_ink() {
        let mut s = session(1.0);
        select(&mut s);
        let frame = render(&s);
        let strip = s.strip().unwrap().rect;
        let inside = frame.pixel((strip.x + 2.0) as i32, (strip.y + strip.h / 2.0) as i32);
        assert!(
            inside[0] > INK[0] + 30,
            "the screen shows through the panel: {inside:?}"
        );
        assert!(inside[0] < 200, "but tinted dark: {inside:?}");
    }

    #[test]
    fn focus_tool_previews_the_mist_inside_the_selection() {
        let mut s = session(1.0);
        select(&mut s);
        s.handle(Input::Key(Key::Char('o'), Mods::default()));
        s.handle(Input::PointerDown(Point::new(350.0, 200.0)));
        s.handle(Input::PointerMove(Point::new(450.0, 300.0)));
        s.handle(Input::PointerUp(Point::new(450.0, 300.0)));
        let frame = render(&s);
        assert_eq!(
            frame.pixel(400, 250),
            [200, 200, 200, 255],
            "the focus area is sharp and bright"
        );
        assert_eq!(
            frame.pixel(480, 150)[0],
            scaled(200, theme::SOFT_KEEP),
            "the rest of the selection is mist"
        );
    }

    #[test]
    fn a_notice_is_drawn_and_goes_with_the_next_action() {
        let mut s = session(1.0);
        s.handle(Input::PointerMove(Point::new(600.0, 30.0)));
        let plain = render(&s);
        s.handle(Input::Key(Key::Char('w'), Mods::default()));
        assert_eq!(s.notice(), Some("nothing changed"));
        assert_ne!(render(&s), plain, "the notice changes the picture");
        s.handle(Input::Key(Key::Up, Mods::default()));
        assert_eq!(render(&s), plain);
    }

    #[test]
    fn toast_is_a_pill_over_the_screen_it_covers() {
        let screen = screen();
        let monitor = Rect::new(0.0, 0.0, 640.0, 480.0);
        let (image, rect) = toast(&screen, &Fonts::empty(), "Copied 400 x 300", monitor, 1.0);
        assert_eq!((image.width as f32, image.height as f32), (rect.w, rect.h));
        assert!(rect.bottom() <= 480.0 && rect.x > 0.0 && rect.right() < 640.0);
        assert!(
            (rect.x + rect.w / 2.0 - 320.0).abs() <= 1.0,
            "centred on the monitor"
        );
        assert_eq!(
            image.pixel(0, 0),
            screen.pixel(rect.x as i32, rect.y as i32),
            "corners show the screen"
        );
        let middle = image.pixel(image.width as i32 / 2, image.height as i32 / 2);
        assert!(middle[0] < 150, "the middle is tinted glass: {middle:?}");
    }

    #[test]
    fn toast_survives_a_tiny_or_offscreen_monitor() {
        let screen = screen();
        toast(
            &screen,
            &Fonts::empty(),
            "a very long message that cannot possibly fit",
            Rect::new(0.0, 0.0, 40.0, 20.0),
            2.0,
        );
        toast(
            &screen,
            &Fonts::empty(),
            "x",
            Rect::new(5000.0, 5000.0, 100.0, 100.0),
            1.0,
        );
    }

    #[test]
    fn help_changes_the_picture_until_a_key_closes_it() {
        let mut s = session(1.0);
        select(&mut s);
        let plain = render(&s);
        s.handle(Input::Key(Key::Help, Mods::default()));
        let with_help = render(&s);
        assert_ne!(with_help, plain);
        assert!(
            with_help.pixel(320, 240)[0] < 120,
            "a dark panel covers the middle of the monitor"
        );
        s.handle(Input::Key(Key::Up, Mods::default()));
        assert_eq!(render(&s), plain);
    }

    #[test]
    fn strip_tooltip_appears_over_the_hovered_item() {
        let mut s = session(1.0);
        select(&mut s);
        let plain = render(&s);
        let cell = s.strip().unwrap().cells[2].1;
        s.handle(Input::PointerMove(cell.center()));
        assert_ne!(render(&s), plain);
    }

    #[test]
    fn preview_card_shows_the_capture_small_in_the_corner() {
        let screen = screen();
        let shot = Image::filled(800, 400, [220, 30, 30, 255]);
        let monitor = Rect::new(0.0, 0.0, 640.0, 480.0);
        let (card, rect) = preview_card(
            &screen,
            &shot,
            &Fonts::empty(),
            "Copied 800 x 400",
            monitor,
            1.0,
        );
        assert_eq!((card.width as f32, card.height as f32), (rect.w, rect.h));
        assert_eq!(
            (rect.right(), rect.bottom()),
            (616.0, 456.0),
            "24 px in from the corner"
        );
        // 800 x 400 fits into 240 x 150 as 240 x 120, centred with a 10 px margin.
        assert_eq!(
            card.pixel(card.width as i32 / 2, 70),
            [220, 30, 30, 255],
            "the thumbnail"
        );
        assert_eq!(
            card.pixel(0, 0),
            screen.pixel(rect.x as i32, rect.y as i32),
            "the corner shows the screen"
        );
    }

    #[test]
    fn preview_card_survives_odd_captures_and_monitors() {
        let screen = screen();
        let monitor = Rect::new(0.0, 0.0, 640.0, 480.0);
        for (w, h) in [(1, 1), (4000, 3), (3, 4000), (5000, 5000)] {
            let shot = Image::filled(w, h, [9, 9, 9, 255]);
            let (card, rect) = preview_card(&screen, &shot, &Fonts::empty(), "x", monitor, 2.0);
            assert_eq!((card.width as f32, card.height as f32), (rect.w, rect.h));
        }
        let tiny = Rect::new(0.0, 0.0, 30.0, 20.0);
        preview_card(
            &screen,
            &Image::filled(10, 10, [0, 0, 0, 255]),
            &Fonts::empty(),
            "x",
            tiny,
            1.0,
        );
        let away = Rect::new(9000.0, 9000.0, 100.0, 100.0);
        preview_card(
            &screen,
            &Image::filled(10, 10, [0, 0, 0, 255]),
            &Fonts::empty(),
            "x",
            away,
            1.0,
        );
    }

    #[test]
    fn present_packs_rgb_into_one_word_per_pixel() {
        let mut pixmap = Pixmap::new(3, 2).unwrap();
        pixmap.data_mut()[..8].copy_from_slice(&[0x11, 0x22, 0x33, 0xFF, 0xAA, 0xBB, 0xCC, 0xFF]);
        let mut frame = vec![0u32; 6];
        present(&pixmap, &mut frame);
        assert_eq!(&frame[..3], [0x0011_2233, 0x00AA_BBCC, 0]);
        // The multi-core path gives the same answer as the rule.
        let mut big = Pixmap::new(640, 480).unwrap();
        for (i, b) in big.data_mut().iter_mut().enumerate() {
            *b = (i * 13 % 256) as u8;
        }
        let mut frame = vec![0u32; 640 * 480];
        present(&big, &mut frame);
        for (out, px) in frame.iter().zip(big.data().chunks_exact(4)) {
            assert_eq!(
                *out,
                (px[0] as u32) << 16 | (px[1] as u32) << 8 | px[2] as u32
            );
        }
    }

    #[test]
    fn layers_prepared_ahead_give_the_same_picture() {
        let mut s = session(1.0);
        s.handle(Input::PointerMove(Point::new(150.0, 240.0)));
        let mut renderer = Renderer::new();
        renderer.adopt(Renderer::prepare(s.screen(), s.frame_index(), s.scale()));
        let mut pixmap = Pixmap::new(SIZE.0, SIZE.1).unwrap();
        renderer.render(&s, &mut pixmap);
        assert_eq!(Image::new(SIZE.0, SIZE.1, pixmap.take()), render(&s));
    }

    #[test]
    fn rendering_is_deterministic() {
        let mut s = session(1.0);
        select(&mut s);
        assert_eq!(render(&s), render(&s));
    }

    #[test]
    fn a_reused_renderer_follows_the_frame_in_view() {
        let old = Image::filled(SIZE.0, SIZE.1, [40, 40, 40, 255]);
        let frames = vec![
            Frame {
                image: Arc::new(old),
                age_ms: 1000,
            },
            Frame::now(screen()),
        ];
        let mut s = session_with(frames, 1.0);
        s.handle(Input::PointerMove(Point::new(600.0, 430.0)));
        let mut renderer = Renderer::new();
        let mut pixmap = Pixmap::new(SIZE.0, SIZE.1).unwrap();
        renderer.render(&s, &mut pixmap);
        assert_eq!(pixmap.data()[0], scaled(200, theme::DIM_KEEP));
        s.handle(Input::Key(Key::Char(','), Mods::default()));
        renderer.render(&s, &mut pixmap);
        assert_eq!(pixmap.data()[0], scaled(40, theme::DIM_KEEP));
    }

    #[test]
    fn warm_builds_the_same_layers_as_rendering() {
        let mut s = session(1.0);
        select(&mut s);
        let mut warmed = Renderer::new();
        warmed.warm(&s);
        let mut pixmap = Pixmap::new(SIZE.0, SIZE.1).unwrap();
        warmed.render(&s, &mut pixmap);
        assert_eq!(Image::new(SIZE.0, SIZE.1, pixmap.take()), render(&s));
    }

    #[test]
    fn every_phase_renders_at_both_scales() {
        for scale in [1.0, 2.0] {
            let old = Frame {
                image: Arc::new(screen()),
                age_ms: 1500,
            };
            let mut s = session_with(vec![old, Frame::now(screen())], scale);
            render(&s);
            s.handle(Input::Key(Key::Char(','), Mods::default()));
            render(&s);
            s.handle(Input::PointerDown(Point::new(630.0, 470.0)));
            s.handle(Input::PointerMove(Point::new(5.0, 5.0)));
            render(&s);
            s.handle(Input::PointerUp(Point::new(5.0, 5.0)));
            for tool in ['a', 'l', 'r', 'e', 'd', 'h', 'b'] {
                s.handle(Input::Key(Key::Char(tool), Mods::default()));
                s.handle(Input::PointerDown(Point::new(50.0, 60.0)));
                s.handle(Input::PointerMove(Point::new(300.0, 200.0)));
                render(&s);
                s.handle(Input::PointerUp(Point::new(320.0, 240.0)));
            }
            s.handle(Input::Key(Key::Char('n'), Mods::default()));
            s.handle(Input::PointerDown(Point::new(400.0, 100.0)));
            s.handle(Input::PointerUp(Point::new(400.0, 100.0)));
            s.handle(Input::Key(Key::Char('t'), Mods::default()));
            s.handle(Input::PointerDown(Point::new(400.0, 300.0)));
            s.handle(Input::PointerUp(Point::new(400.0, 300.0)));
            s.handle(Input::Text("bruma".into()));
            render(&s);
            assert_eq!(s.doc().items().len(), 8);
        }
    }
}
