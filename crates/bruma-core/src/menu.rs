//! The tray menu: a small pane of frosted glass instead of the system's grey
//! menu. Pure: the picture of the screen under it in, the menu's picture out.

use tiny_skia::{IntSize, PathBuilder, Pixmap};

use crate::blur;
use crate::draw::fill_path;
use crate::geom::{Point, Rect};
use crate::image::Image;
use crate::render::{glass_panel, rounded_rect};
use crate::text::{Face, Fonts};
use crate::theme::{ACCENT, FAINT, PAPER};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Entry {
    Capture,
    Later,
    Folder,
    History,
    Lab,
    Settings,
    /// Copy what `bruma doctor` says, for a bug report.
    Report,
    Quit,
}

/// The menu from top to bottom; `None` is a separator.
const ENTRIES: &[Option<(Entry, &str)>] = &[
    Some((Entry::Capture, "Capture")),
    Some((Entry::Later, "Capture in 3 seconds")),
    None,
    Some((Entry::Folder, "Screenshots folder")),
    Some((Entry::History, "History")),
    Some((Entry::Lab, "Try every feature")),
    Some((Entry::Settings, "Settings")),
    Some((Entry::Report, "Copy a report for a bug")),
    None,
    Some((Entry::Quit, "Quit Bruma")),
];

pub struct Menu {
    /// The hotkey, as people write it, shown beside "Capture".
    hotkey: Option<String>,
    /// A line at the top about something that needs attention.
    warning: Option<String>,
    scale: f32,
    hover: Option<Entry>,
}

impl Menu {
    pub fn new(hotkey: Option<String>, warning: Option<String>, scale: f32) -> Self {
        Self {
            hotkey,
            warning,
            scale,
            hover: None,
        }
    }

    fn px(&self, v: f32) -> f32 {
        (v * self.scale).round()
    }

    /// The menu's size in pixels.
    pub fn size(&self) -> (u32, u32) {
        let (w, h) = self.extent();
        (w as u32, h as u32)
    }

    fn extent(&self) -> (f32, f32) {
        let items = ENTRIES.iter().filter(|e| e.is_some()).count() as f32;
        let gaps = ENTRIES.iter().filter(|e| e.is_none()).count() as f32;
        let warning = if self.warning.is_some() {
            self.px(40.0)
        } else {
            0.0
        };
        (
            self.px(244.0),
            self.px(6.0) * 2.0 + warning + items * self.px(32.0) + gaps * self.px(9.0),
        )
    }

    fn rows(&self) -> Vec<(Entry, &'static str, Rect)> {
        let (w, _) = self.extent();
        let pad = self.px(6.0);
        let mut y = pad
            + if self.warning.is_some() {
                self.px(40.0)
            } else {
                0.0
            };
        let mut out = Vec::new();
        for entry in ENTRIES {
            match entry {
                Some((e, label)) => {
                    out.push((*e, *label, Rect::new(pad, y, w - 2.0 * pad, self.px(32.0))));
                    y += self.px(32.0);
                }
                None => y += self.px(9.0),
            }
        }
        out
    }

    /// The entry under `p`, in the menu's own pixels.
    pub fn hit(&self, p: Point) -> Option<Entry> {
        self.rows()
            .into_iter()
            .find(|(_, _, r)| r.contains(p))
            .map(|(e, _, _)| e)
    }

    /// Points at `p`; `true` if what is lit changed.
    pub fn point(&mut self, p: Option<Point>) -> bool {
        let hover = p.and_then(|p| self.hit(p));
        let changed = hover != self.hover;
        self.hover = hover;
        changed
    }

    /// Moves the light up or down with the keyboard.
    pub fn step(&mut self, down: bool) {
        let entries: Vec<Entry> = ENTRIES.iter().flatten().map(|(e, _)| *e).collect();
        let n = entries.len();
        let now = self
            .hover
            .and_then(|h| entries.iter().position(|&e| e == h));
        let next = match (now, down) {
            (None, true) => 0,
            (None, false) => n - 1,
            (Some(i), true) => (i + 1) % n,
            (Some(i), false) => (i + n - 1) % n,
        };
        self.hover = Some(entries[next]);
    }

    pub fn lit(&self) -> Option<Entry> {
        self.hover
    }

    /// The menu drawn over `under`, the picture of the screen it covers
    /// (the menu's size).
    pub fn draw(&self, under: &Image, fonts: &Fonts) -> Pixmap {
        let (w, h) = (under.width, under.height);
        let mut pixmap = IntSize::from_wh(w.max(1), h.max(1))
            .and_then(|size| Pixmap::from_vec(under.rgba.clone(), size))
            .unwrap_or_else(|| Pixmap::new(1, 1).expect("pixmap"));
        let mut small = blur::downscale(&under.rgba, w as usize, h as usize, 4);
        blur::blur(&mut small, (4.0 * self.scale).round().max(2.0) as usize, 3);
        let Some(glass) = IntSize::from_wh(small.w as u32, small.h as u32)
            .and_then(|s| Pixmap::from_vec(small.px, s))
        else {
            return pixmap;
        };
        let whole = Rect::new(0.0, 0.0, w as f32, h as f32);
        glass_panel(&mut pixmap, &glass, whole, self.px(12.0), self.scale);

        let pad = self.px(6.0);
        if let Some(warning) = &self.warning {
            let at = Point::new(pad + self.px(14.0), pad + self.px(8.0));
            fonts.draw(&mut pixmap, Face::Ui, warning, self.px(11.5), at, FAINT);
            let line = Rect::new(
                pad + self.px(8.0),
                pad + self.px(36.0),
                w as f32 - 2.0 * pad - self.px(16.0),
                self.scale.round().max(1.0),
            );
            crate::draw::fill_rect(&mut pixmap, line, PAPER, 0.12);
        }
        let label_px = self.px(13.0);
        for (entry, label, rect) in self.rows() {
            let lit = self.hover == Some(entry);
            if lit {
                if let Some(path) = rounded_rect(rect, self.px(7.0)) {
                    fill_path(&mut pixmap, &path, [255, 255, 255], 0.09);
                }
                let r = self.px(3.0);
                if let Some(dot) =
                    PathBuilder::from_circle(rect.x + self.px(14.0), rect.y + rect.h / 2.0, r)
                {
                    fill_path(&mut pixmap, &dot, ACCENT, 1.0);
                }
            }
            let th = fonts.measure(Face::Ui, label, label_px).1;
            let y = (rect.y + (rect.h - th) / 2.0).round();
            fonts.draw(
                &mut pixmap,
                Face::Ui,
                label,
                label_px,
                Point::new(rect.x + self.px(26.0), y),
                PAPER,
            );
            if entry == Entry::Capture {
                if let Some(key) = &self.hotkey {
                    let kpx = self.px(11.0);
                    let (kw, kh) = fonts.measure(Face::Mono, key, kpx);
                    let at = Point::new(
                        rect.right() - self.px(12.0) - kw,
                        (rect.y + (rect.h - kh) / 2.0).round(),
                    );
                    fonts.draw(&mut pixmap, Face::Mono, key, kpx, at, FAINT);
                }
            }
        }
        // Separators.
        let mut y = pad
            + if self.warning.is_some() {
                self.px(40.0)
            } else {
                0.0
            };
        for entry in ENTRIES {
            match entry {
                Some(_) => y += self.px(32.0),
                None => {
                    let line = Rect::new(
                        pad + self.px(12.0),
                        y + self.px(4.0),
                        w as f32 - 2.0 * pad - self.px(24.0),
                        self.scale.round().max(1.0),
                    );
                    crate::draw::fill_rect(&mut pixmap, line, PAPER, 0.12);
                    y += self.px(9.0);
                }
            }
        }
        pixmap
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_entry_can_be_pointed_at() {
        let menu = Menu::new(Some("PrintScreen".into()), None, 1.0);
        let (w, h) = menu.size();
        let mut seen = Vec::new();
        for y in 0..h {
            if let Some(e) = menu.hit(Point::new(w as f32 / 2.0, y as f32)) {
                if seen.last() != Some(&e) {
                    seen.push(e);
                }
            }
        }
        let all: Vec<Entry> = ENTRIES.iter().flatten().map(|(e, _)| *e).collect();
        assert_eq!(seen, all);
    }

    #[test]
    fn a_warning_makes_room_at_the_top() {
        let plain = Menu::new(None, None, 1.0);
        let warned = Menu::new(None, Some("PrintScreen is taken".into()), 1.0);
        assert!(warned.size().1 > plain.size().1);
        // The first entry moves down with it.
        let top = |m: &Menu| m.rows()[0].2.y;
        assert!(top(&warned) > top(&plain));
    }

    #[test]
    fn the_keyboard_walks_the_entries_round() {
        let mut menu = Menu::new(None, None, 1.0);
        menu.step(true);
        assert_eq!(menu.lit(), Some(Entry::Capture));
        menu.step(false);
        assert_eq!(menu.lit(), Some(Entry::Quit));
    }

    #[test]
    fn pointing_lights_one_entry_and_reports_changes() {
        let mut menu = Menu::new(None, None, 1.0);
        let p = menu.rows()[0].2.center();
        assert!(menu.point(Some(p)));
        assert!(!menu.point(Some(p)));
        assert_eq!(menu.lit(), Some(Entry::Capture));
        assert!(menu.point(None));
        assert_eq!(menu.lit(), None);
    }

    #[test]
    fn it_draws_at_its_own_size() {
        let menu = Menu::new(Some("PrintScreen".into()), None, 1.5);
        let (w, h) = menu.size();
        let under = Image::filled(w, h, [200, 200, 200, 255]);
        let pixmap = menu.draw(&under, &Fonts::empty());
        assert_eq!((pixmap.width(), pixmap.height()), (w, h));
        // The middle is tinted glass, darker than the screen under it.
        let i = (((h / 2) * w + w / 2) * 4) as usize;
        assert!(pixmap.data()[i] < 150);
    }
}
