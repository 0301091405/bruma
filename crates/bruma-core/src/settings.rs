//! The settings panel: frosted glass over the misted screen, one row per
//! setting, every change kept the moment it is made.
//!
//! Pure: input in, effects out, and a function that draws it. The application
//! saves the values, applies them and answers the effects.

use std::rc::Rc;

use tiny_skia::{LineCap, LineJoin, PathBuilder, Pixmap};

use crate::backdrop::Backdrop;
use crate::draw::{fill_path, stroke_path};
use crate::geom::{Point, Rect};
use crate::logo;
use crate::render::{glass_panel, rounded_rect};
use crate::session::Key;
use crate::text::{Face, Fonts};
use crate::theme::{ACCENT, FAINT, MUTED, PAPER};

const WHITE: [u8; 3] = [255, 255, 255];
/// The track of a switch that is off.
const OFF: [u8; 3] = [0x4A, 0x4A, 0x47];

/// What the panel shows and changes.
#[derive(Clone, Debug, PartialEq)]
pub struct Values {
    /// As the settings file writes it: `PrintScreen`, `ctrl+shift+KeyS`.
    pub hotkey: String,
    /// Copy the moment an area is chosen.
    pub quick: bool,
    pub snap: bool,
    pub cursor: bool,
    /// Seconds; 0 is off.
    pub rewind: f32,
    /// Seconds of screen kept for GIFs; 0 is off.
    pub clip: f32,
    /// Seconds; 0 is a short message instead.
    pub preview: f32,
    pub autosave: bool,
    /// The save folder, as it should be shown.
    pub folder: String,
    /// Captures kept; 0 is off.
    pub history: usize,
    pub autostart: bool,
    /// Run as administrator, so the hotkey works over games that do.
    pub games: bool,
}

/// Everything on the panel that can be pointed at.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Item {
    Hotkey,
    Quick,
    Snap,
    Cursor,
    Rewind,
    Clip,
    Preview,
    Autosave,
    Folder,
    History,
    Autostart,
    Games,
    EditFile,
    Source,
    Close,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Input {
    Move(Point),
    Press(Point),
    Key(Key),
    /// A key combination pressed while listening for one, in the settings
    /// file's words.
    Hotkey(String),
}

#[derive(Clone, Debug, PartialEq)]
pub enum Effect {
    Redraw,
    /// A value changed; [`Form::values`] holds the new ones.
    Changed,
    /// Start (`true`) or stop listening for a key combination.
    Record(bool),
    PickFolder,
    EditFile,
    OpenSource,
    Close,
}

const SECTIONS: &[(&str, &[(Item, &str)])] = &[
    (
        "CAPTURE",
        &[
            (Item::Hotkey, "Hotkey"),
            (Item::Quick, "Copy as soon as an area is chosen"),
            (Item::Snap, "Snap to boxes and edges"),
            (Item::Cursor, "Include the mouse cursor"),
            (Item::Rewind, "Rewind"),
            (Item::Clip, "GIF length"),
        ],
    ),
    (
        "AFTER A CAPTURE",
        &[
            (Item::Preview, "Preview card"),
            (Item::Autosave, "Also save a file"),
            (Item::Folder, "Save to"),
            (Item::History, "History"),
        ],
    ),
    (
        "BRUMA",
        &[
            (Item::Autostart, "Start with Windows"),
            (Item::Games, "Work over games run as administrator"),
        ],
    ),
];

/// The steps a stepper moves through.
const REWIND_STEPS: &[f32] = &[0.0, 2.0, 3.0, 4.0, 5.0, 6.0, 8.0, 10.0, 15.0, 20.0, 30.0];
const PREVIEW_STEPS: &[f32] = &[0.0, 1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 8.0, 10.0, 15.0];
const HISTORY_STEPS: &[f32] = &[0.0, 50.0, 100.0, 200.0, 300.0, 500.0, 1000.0];
const CLIP_STEPS: &[f32] = &[0.0, 2.0, 3.0, 4.0, 5.0, 6.0, 8.0, 10.0];

/// The next value up or down from `now`, staying on the steps.
fn step(steps: &[f32], now: f32, up: bool) -> f32 {
    if up {
        steps
            .iter()
            .copied()
            .find(|&s| s > now + 1e-3)
            .unwrap_or(steps[steps.len() - 1])
    } else {
        steps
            .iter()
            .rev()
            .copied()
            .find(|&s| s < now - 1e-3)
            .unwrap_or(steps[0])
    }
}

fn seconds(v: f32) -> String {
    if v <= 0.0 {
        "Off".into()
    } else if v.fract().abs() < 1e-3 {
        format!("{} s", v as u32)
    } else {
        format!("{v:.1} s")
    }
}

/// `ctrl+alt+KeyB` → `Ctrl+Alt+B`, for showing to people.
pub fn pretty(hotkey: &str) -> String {
    hotkey
        .split('+')
        .map(|part| {
            let part = part.trim();
            let lower = part.to_ascii_lowercase();
            match lower.as_str() {
                "ctrl" | "control" => "Ctrl".to_string(),
                "alt" | "option" => "Alt".to_string(),
                "shift" => "Shift".to_string(),
                "super" | "cmd" | "command" | "win" => "Win".to_string(),
                _ => match (lower.strip_prefix("key"), lower.strip_prefix("digit")) {
                    (Some(letter), _) if letter.len() == 1 => letter.to_ascii_uppercase(),
                    (_, Some(digit)) if digit.len() == 1 => digit.to_string(),
                    _ => part.to_string(),
                },
            }
        })
        .collect::<Vec<_>>()
        .join("+")
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Key,
    Toggle,
    Stepper,
    Folder,
}

fn kind(item: Item) -> Kind {
    match item {
        Item::Hotkey => Kind::Key,
        Item::Rewind | Item::Clip | Item::Preview | Item::History => Kind::Stepper,
        Item::Folder => Kind::Folder,
        _ => Kind::Toggle,
    }
}

/// Which part of a row a point is on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Part {
    Body,
    Minus,
    Plus,
}

struct Row {
    item: Item,
    label: &'static str,
    rect: Rect,
    control: Rect,
    minus: Rect,
    plus: Rect,
}

struct Layout {
    panel: Rect,
    title_at: Point,
    logo_at: Point,
    close: Rect,
    sections: Vec<(Point, &'static str)>,
    rows: Vec<Row>,
    links: Vec<(Item, &'static str, Rect)>,
    message_at: Point,
    version_right: Point,
}

pub struct Form {
    pub values: Values,
    fonts: Rc<Fonts>,
    area: Rect,
    scale: f32,
    version: String,
    hover: Option<(Item, Part)>,
    focus: Option<Item>,
    recording: bool,
    message: Option<String>,
}

impl Form {
    /// A panel in the middle of `area` (the monitor, in the pixmap's
    /// coordinates), drawn at `scale`.
    pub fn new(values: Values, fonts: Rc<Fonts>, area: Rect, scale: f32, version: &str) -> Self {
        Self {
            values,
            fonts,
            area,
            scale,
            version: version.to_string(),
            hover: None,
            focus: None,
            recording: false,
            message: None,
        }
    }

    pub fn recording(&self) -> bool {
        self.recording
    }

    /// A line under the settings, for what went wrong. `None` clears it.
    pub fn set_message(&mut self, message: Option<String>) {
        self.message = message;
    }

    fn px(&self, v: f32) -> f32 {
        (v * self.scale).round()
    }

    fn layout(&self) -> Layout {
        let px = |v: f32| self.px(v);
        let width = px(500.0);
        let pad = px(28.0);
        let row_h = px(39.0);
        let section_h = px(30.0);
        let rows: usize = SECTIONS.iter().map(|(_, r)| r.len()).sum();
        let height = px(24.0)
            + px(48.0)
            + SECTIONS.len() as f32 * section_h
            + rows as f32 * row_h
            + px(16.0)
            + px(60.0);
        let panel = Rect::new(
            (self.area.x + (self.area.w - width) / 2.0).round(),
            (self.area.y + (self.area.h - height) / 2.0)
                .round()
                .max(self.area.y),
            width,
            height,
        );
        let left = panel.x + pad;
        let right = panel.right() - pad;
        let mut y = panel.y + px(24.0);
        let logo_at = Point::new(left + px(11.0), y + px(16.0));
        let title_at = Point::new(left + px(32.0), y + px(4.0));
        let close = Rect::new(right - px(28.0), y + px(2.0), px(28.0), px(28.0));
        y += px(48.0);

        let mut sections = Vec::new();
        let mut out = Vec::new();
        for (title, items) in SECTIONS {
            sections.push((Point::new(left, y + px(12.0)), *title));
            y += section_h;
            for (item, label) in items.iter() {
                let rect = Rect::new(left, y, right - left, row_h);
                let cy = y + row_h / 2.0;
                let (control, minus, plus) = match kind(*item) {
                    Kind::Toggle => {
                        let (w, h) = (px(36.0), px(20.0));
                        (
                            Rect::new(right - w, cy - h / 2.0, w, h),
                            Rect::default(),
                            Rect::default(),
                        )
                    }
                    Kind::Stepper => {
                        let b = px(26.0);
                        let value = px(62.0);
                        let plus = Rect::new(right - b, cy - b / 2.0, b, b);
                        let minus = Rect::new(plus.x - value - b, cy - b / 2.0, b, b);
                        (
                            Rect::new(minus.x, minus.y, plus.right() - minus.x, b),
                            minus,
                            plus,
                        )
                    }
                    Kind::Key => {
                        let text = if self.recording {
                            "Press keys".to_string()
                        } else {
                            pretty(&self.values.hotkey)
                        };
                        let tw = self.fonts.measure(Face::Mono, &text, px(12.0)).0;
                        let w = (tw + px(28.0)).max(px(110.0));
                        let h = px(28.0);
                        (
                            Rect::new(right - w, cy - h / 2.0, w, h),
                            Rect::default(),
                            Rect::default(),
                        )
                    }
                    Kind::Folder => {
                        let tw = self.fonts.measure(Face::Ui, "Change", px(13.0)).0;
                        let w = tw + px(24.0);
                        let h = px(28.0);
                        (
                            Rect::new(right - w, cy - h / 2.0, w, h),
                            Rect::default(),
                            Rect::default(),
                        )
                    }
                };
                out.push(Row {
                    item: *item,
                    label,
                    rect,
                    control,
                    minus,
                    plus,
                });
                y += row_h;
            }
        }

        y += px(16.0);
        let message_at = Point::new(left, y);
        let link_y = y + px(28.0);
        let mut links = Vec::new();
        let mut x = left;
        for (item, text) in [
            (Item::EditFile, "Edit the settings file"),
            (Item::Source, "Source code"),
        ] {
            let (w, h) = self.fonts.measure(Face::Ui, text, px(12.0));
            links.push((item, text, Rect::new(x, link_y, w, h)));
            x += w + px(22.0);
        }
        Layout {
            panel,
            title_at,
            logo_at,
            close,
            sections,
            rows: out,
            links,
            message_at,
            version_right: Point::new(right, link_y),
        }
    }

    fn hit(&self, p: Point) -> Option<(Item, Part)> {
        let layout = self.layout();
        if layout.close.inflate(self.px(4.0)).contains(p) {
            return Some((Item::Close, Part::Body));
        }
        for (item, _, rect) in &layout.links {
            if rect.inflate(self.px(4.0)).contains(p) {
                return Some((*item, Part::Body));
            }
        }
        for row in &layout.rows {
            if !row.rect.contains(p) {
                continue;
            }
            let part = if row.minus.inflate(self.px(3.0)).contains(p) {
                Part::Minus
            } else if row.plus.inflate(self.px(3.0)).contains(p) {
                Part::Plus
            } else {
                Part::Body
            };
            return Some((row.item, part));
        }
        None
    }

    /// The items keyboard focus moves through, in order.
    fn order() -> Vec<Item> {
        let mut items: Vec<Item> = SECTIONS
            .iter()
            .flat_map(|(_, rows)| rows.iter().map(|(item, _)| *item))
            .collect();
        items.extend([Item::EditFile, Item::Source]);
        items
    }

    fn adjust(&mut self, item: Item, up: bool) -> Vec<Effect> {
        let v = &mut self.values;
        let before = v.clone();
        match item {
            Item::Rewind => v.rewind = step(REWIND_STEPS, v.rewind, up),
            Item::Clip => v.clip = step(CLIP_STEPS, v.clip, up),
            Item::Quick => v.quick = up,
            Item::Preview => v.preview = step(PREVIEW_STEPS, v.preview, up),
            Item::History => v.history = step(HISTORY_STEPS, v.history as f32, up) as usize,
            Item::Snap => v.snap = up,
            Item::Cursor => v.cursor = up,
            Item::Autosave => v.autosave = up,
            Item::Autostart => v.autostart = up,
            Item::Games => v.games = up,
            _ => {}
        }
        if *v == before {
            vec![]
        } else {
            vec![Effect::Changed, Effect::Redraw]
        }
    }

    fn activate(&mut self, item: Item, part: Part) -> Vec<Effect> {
        match (kind(item), item) {
            (_, Item::Close) => vec![Effect::Close],
            (_, Item::EditFile) => vec![Effect::EditFile],
            (_, Item::Source) => vec![Effect::OpenSource],
            (Kind::Toggle, _) => {
                let on = match item {
                    Item::Quick => self.values.quick,
                    Item::Snap => self.values.snap,
                    Item::Cursor => self.values.cursor,
                    Item::Autosave => self.values.autosave,
                    Item::Autostart => self.values.autostart,
                    Item::Games => self.values.games,
                    _ => false,
                };
                self.adjust(item, !on)
            }
            (Kind::Stepper, _) => match part {
                Part::Minus => self.adjust(item, false),
                Part::Plus => self.adjust(item, true),
                Part::Body => vec![],
            },
            (Kind::Key, _) => {
                self.recording = true;
                self.message = None;
                vec![Effect::Record(true), Effect::Redraw]
            }
            (Kind::Folder, _) => vec![Effect::PickFolder],
        }
    }

    pub fn handle(&mut self, input: Input) -> Vec<Effect> {
        match input {
            Input::Move(p) => {
                let hover = self.hit(p);
                if hover == self.hover {
                    return vec![];
                }
                self.hover = hover;
                vec![Effect::Redraw]
            }
            Input::Press(p) => {
                let mut out = vec![];
                if self.recording {
                    self.recording = false;
                    out.extend([Effect::Record(false), Effect::Redraw]);
                }
                if !self.layout().panel.contains(p) {
                    out.push(Effect::Close);
                    return out;
                }
                if let Some((item, part)) = self.hit(p) {
                    self.focus = None;
                    // A click on the key that was listening just stops it.
                    if !(item == Item::Hotkey && !out.is_empty()) {
                        out.extend(self.activate(item, part));
                    }
                }
                out
            }
            Input::Hotkey(name) => {
                if !self.recording {
                    return vec![];
                }
                self.recording = false;
                let mut out = vec![Effect::Record(false), Effect::Redraw];
                if name != self.values.hotkey {
                    self.values.hotkey = name;
                    out.insert(1, Effect::Changed);
                }
                out
            }
            Input::Key(key) => {
                if self.recording {
                    if key == Key::Escape {
                        self.recording = false;
                        return vec![Effect::Record(false), Effect::Redraw];
                    }
                    return vec![];
                }
                let order = Self::order();
                let now = self.focus.and_then(|f| order.iter().position(|&i| i == f));
                match key {
                    Key::Escape => vec![Effect::Close],
                    Key::Down | Key::Up => {
                        let n = order.len();
                        let next = match (now, key) {
                            (None, Key::Down) => 0,
                            (None, _) => n - 1,
                            (Some(i), Key::Down) => (i + 1) % n,
                            (Some(i), _) => (i + n - 1) % n,
                        };
                        self.focus = Some(order[next]);
                        vec![Effect::Redraw]
                    }
                    Key::Enter | Key::Char(' ') => match self.focus {
                        Some(item) if kind(item) == Kind::Stepper => self.adjust(item, true),
                        Some(item) => self.activate(item, Part::Body),
                        None => vec![],
                    },
                    Key::Left | Key::Right => match self.focus {
                        Some(item) => self.adjust(item, key == Key::Right),
                        None => vec![],
                    },
                    _ => vec![],
                }
            }
        }
    }

    fn lit(&self, item: Item, part: Part) -> bool {
        self.hover == Some((item, part)) || (part == Part::Body && self.focus == Some(item))
    }

    /// Draws the misted screen and the panel on it. `pixmap` and `backdrop`
    /// are the size of the screen.
    pub fn draw(&self, pixmap: &mut Pixmap, backdrop: &Backdrop) {
        backdrop.paint(pixmap, 1.0);
        let layout = self.layout();
        let px = |v: f32| self.px(v);
        let fonts = &self.fonts;
        glass_panel(pixmap, backdrop.glass(), layout.panel, px(16.0), self.scale);

        logo::draw(pixmap, layout.logo_at, px(22.0));
        fonts.draw(
            pixmap,
            Face::Ui,
            "Settings",
            px(17.0),
            layout.title_at,
            PAPER,
        );
        let close_lit = self.lit(Item::Close, Part::Body);
        if close_lit {
            if let Some(path) = rounded_rect(layout.close, layout.close.w / 2.0) {
                fill_path(pixmap, &path, WHITE, 0.10);
            }
        }
        cross(
            pixmap,
            layout.close.center(),
            px(5.0),
            if close_lit { PAPER } else { FAINT },
            self.scale,
        );

        for (at, title) in &layout.sections {
            fonts.draw(pixmap, Face::Mono, title, px(10.5), *at, MUTED);
        }

        for row in &layout.rows {
            if self.lit(row.item, Part::Body)
                || self.lit(row.item, Part::Minus)
                || self.lit(row.item, Part::Plus)
            {
                let band = Rect::new(
                    row.rect.x - px(12.0),
                    row.rect.y + px(2.0),
                    row.rect.w + px(24.0),
                    row.rect.h - px(4.0),
                );
                if let Some(path) = rounded_rect(band, px(8.0)) {
                    fill_path(pixmap, &path, WHITE, 0.05);
                }
            }
            let label_px = px(14.0);
            let lh = fonts.measure(Face::Ui, row.label, label_px).1;
            let cy = row.rect.y + row.rect.h / 2.0;
            fonts.draw(
                pixmap,
                Face::Ui,
                row.label,
                label_px,
                Point::new(row.rect.x, (cy - lh / 2.0).round()),
                PAPER,
            );
            match kind(row.item) {
                Kind::Toggle => {
                    let on = match row.item {
                        Item::Quick => self.values.quick,
                        Item::Snap => self.values.snap,
                        Item::Cursor => self.values.cursor,
                        Item::Autosave => self.values.autosave,
                        Item::Autostart => self.values.autostart,
                        Item::Games => self.values.games,
                        _ => false,
                    };
                    switch(pixmap, row.control, on, self.scale);
                }
                Kind::Stepper => {
                    let (text, off) = match row.item {
                        Item::Rewind => (seconds(self.values.rewind), self.values.rewind <= 0.0),
                        Item::Clip => (seconds(self.values.clip), self.values.clip <= 0.0),
                        Item::Preview => (seconds(self.values.preview), self.values.preview <= 0.0),
                        _ if self.values.history == 0 => ("Off".to_string(), true),
                        _ => (self.values.history.to_string(), false),
                    };
                    for (rect, part, plus) in [
                        (row.minus, Part::Minus, false),
                        (row.plus, Part::Plus, true),
                    ] {
                        if self.hover == Some((row.item, part)) {
                            if let Some(path) = rounded_rect(rect, px(7.0)) {
                                fill_path(pixmap, &path, WHITE, 0.12);
                            }
                        }
                        sign(pixmap, rect.center(), px(4.5), plus, self.scale);
                    }
                    let vpx = px(13.0);
                    let (tw, th) = fonts.measure(Face::Mono, &text, vpx);
                    let mid = (row.minus.right() + row.plus.x) / 2.0;
                    fonts.draw(
                        pixmap,
                        Face::Mono,
                        &text,
                        vpx,
                        Point::new((mid - tw / 2.0).round(), (cy - th / 2.0).round()),
                        if off { FAINT } else { PAPER },
                    );
                }
                Kind::Key => {
                    let (text, color, edge) = if self.recording {
                        ("Press keys".to_string(), ACCENT, ACCENT)
                    } else {
                        (pretty(&self.values.hotkey), PAPER, FAINT)
                    };
                    cap(pixmap, row.control, edge, self.scale);
                    centre_text(
                        fonts,
                        pixmap,
                        Face::Mono,
                        &text,
                        px(12.0),
                        row.control,
                        color,
                    );
                }
                Kind::Folder => {
                    cap(pixmap, row.control, FAINT, self.scale);
                    centre_text(
                        fonts,
                        pixmap,
                        Face::Ui,
                        "Change",
                        px(13.0),
                        row.control,
                        PAPER,
                    );
                    let label_w = fonts.measure(Face::Ui, row.label, label_px).0;
                    let room = row.control.x - px(14.0) - (row.rect.x + label_w + px(18.0));
                    let path_px = px(12.0);
                    let shown = fit_from_left(fonts, &self.values.folder, path_px, room);
                    let (tw, th) = fonts.measure(Face::Mono, &shown, path_px);
                    fonts.draw(
                        pixmap,
                        Face::Mono,
                        &shown,
                        path_px,
                        Point::new(
                            (row.control.x - px(14.0) - tw).round(),
                            (cy - th / 2.0).round(),
                        ),
                        FAINT,
                    );
                }
            }
        }

        if let Some(message) = &self.message {
            fonts.draw(
                pixmap,
                Face::Ui,
                message,
                px(12.5),
                layout.message_at,
                ACCENT,
            );
        }
        for (item, text, rect) in &layout.links {
            let color = if self.lit(*item, Part::Body) {
                PAPER
            } else {
                FAINT
            };
            fonts.draw(
                pixmap,
                Face::Ui,
                text,
                px(12.0),
                Point::new(rect.x, rect.y),
                color,
            );
            if self.lit(*item, Part::Body) {
                let line = Rect::new(
                    rect.x,
                    rect.bottom() - px(1.0),
                    rect.w,
                    self.scale.round().max(1.0),
                );
                crate::draw::fill_rect(pixmap, line, PAPER, 0.6);
            }
        }
        let version = format!("Bruma {}", self.version);
        let vpx = px(11.0);
        let tw = fonts.measure(Face::Mono, &version, vpx).0;
        fonts.draw(
            pixmap,
            Face::Mono,
            &version,
            vpx,
            Point::new(
                layout.version_right.x - tw,
                layout.version_right.y + px(1.0),
            ),
            MUTED,
        );
    }
}

fn centre_text(
    fonts: &Fonts,
    pixmap: &mut Pixmap,
    face: Face,
    text: &str,
    px: f32,
    rect: Rect,
    color: [u8; 3],
) {
    let (tw, th) = fonts.measure(face, text, px);
    let at = Point::new(
        (rect.x + (rect.w - tw) / 2.0).round(),
        (rect.y + (rect.h - th) / 2.0).round(),
    );
    fonts.draw(pixmap, face, text, px, at, color);
}

/// `text`, cut from the left with an ellipsis until it fits `room` pixels.
fn fit_from_left(fonts: &Fonts, text: &str, px: f32, room: f32) -> String {
    if fonts.measure(Face::Mono, text, px).0 <= room {
        return text.to_string();
    }
    let chars: Vec<char> = text.chars().collect();
    for start in 1..chars.len() {
        let shown: String = std::iter::once('…')
            .chain(chars[start..].iter().copied())
            .collect();
        if fonts.measure(Face::Mono, &shown, px).0 <= room {
            return shown;
        }
    }
    "…".to_string()
}

/// An on/off switch.
fn switch(pixmap: &mut Pixmap, rect: Rect, on: bool, scale: f32) {
    if let Some(path) = rounded_rect(rect, rect.h / 2.0) {
        fill_path(pixmap, &path, if on { ACCENT } else { OFF }, 1.0);
    }
    let r = rect.h / 2.0 - (3.0 * scale).round().max(2.0);
    let cx = if on {
        rect.right() - rect.h / 2.0
    } else {
        rect.x + rect.h / 2.0
    };
    if let Some(path) = PathBuilder::from_circle(cx, rect.y + rect.h / 2.0, r) {
        fill_path(pixmap, &path, PAPER, 1.0);
    }
}

/// A key cap or button: a faint fill and an outline.
fn cap(pixmap: &mut Pixmap, rect: Rect, edge: [u8; 3], scale: f32) {
    let t = scale.round().max(1.0);
    if let Some(path) = rounded_rect(rect, (7.0 * scale).round()) {
        fill_path(pixmap, &path, WHITE, 0.05);
    }
    if let Some(path) = rounded_rect(rect.inflate(-t / 2.0), (7.0 * scale).round() - t / 2.0) {
        stroke_path(pixmap, &path, edge, 0.55, t, LineCap::Butt, LineJoin::Round);
    }
}

/// A minus or plus sign.
fn sign(pixmap: &mut Pixmap, c: Point, half: f32, plus: bool, scale: f32) {
    let mut pb = PathBuilder::new();
    pb.move_to(c.x - half, c.y);
    pb.line_to(c.x + half, c.y);
    if plus {
        pb.move_to(c.x, c.y - half);
        pb.line_to(c.x, c.y + half);
    }
    if let Some(path) = pb.finish() {
        stroke_path(
            pixmap,
            &path,
            PAPER,
            0.9,
            (1.5 * scale).max(1.0),
            LineCap::Round,
            LineJoin::Round,
        );
    }
}

/// The close button's cross.
fn cross(pixmap: &mut Pixmap, c: Point, half: f32, color: [u8; 3], scale: f32) {
    let mut pb = PathBuilder::new();
    pb.move_to(c.x - half, c.y - half);
    pb.line_to(c.x + half, c.y + half);
    pb.move_to(c.x + half, c.y - half);
    pb.line_to(c.x - half, c.y + half);
    if let Some(path) = pb.finish() {
        stroke_path(
            pixmap,
            &path,
            color,
            1.0,
            (1.5 * scale).max(1.0),
            LineCap::Round,
            LineJoin::Round,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn values() -> Values {
        Values {
            hotkey: "PrintScreen".into(),
            quick: true,
            snap: true,
            cursor: false,
            rewind: 4.0,
            clip: 10.0,
            preview: 4.0,
            autosave: false,
            folder: "C:\\Pictures\\bruma".into(),
            history: 200,
            autostart: true,
            games: false,
        }
    }

    fn form() -> Form {
        Form::new(
            values(),
            Rc::new(Fonts::empty()),
            Rect::new(0.0, 0.0, 1280.0, 800.0),
            1.0,
            "1.0.0",
        )
    }

    /// The middle of a row's control, or of the row itself.
    fn spot(form: &Form, item: Item, part: Part) -> Point {
        let layout = form.layout();
        if item == Item::Close {
            return layout.close.center();
        }
        if let Some((_, _, rect)) = layout.links.iter().find(|(i, _, _)| *i == item) {
            return rect.center();
        }
        let row = layout.rows.iter().find(|r| r.item == item).unwrap();
        match part {
            Part::Minus => row.minus.center(),
            Part::Plus => row.plus.center(),
            Part::Body => row.control.center(),
        }
    }

    fn click(form: &mut Form, item: Item, part: Part) -> Vec<Effect> {
        let p = spot(form, item, part);
        form.handle(Input::Press(p))
    }

    #[test]
    fn a_switch_flips_and_says_so() {
        let mut f = form();
        assert_eq!(
            click(&mut f, Item::Snap, Part::Body),
            [Effect::Changed, Effect::Redraw]
        );
        assert!(!f.values.snap);
        click(&mut f, Item::Snap, Part::Body);
        assert!(f.values.snap);
    }

    #[test]
    fn steppers_move_along_their_steps_and_stop_at_the_ends() {
        let mut f = form();
        click(&mut f, Item::Rewind, Part::Plus);
        assert_eq!(f.values.rewind, 5.0);
        for _ in 0..20 {
            click(&mut f, Item::Rewind, Part::Minus);
        }
        assert_eq!(f.values.rewind, 0.0, "the bottom step is off");
        assert!(
            click(&mut f, Item::Rewind, Part::Minus).is_empty(),
            "nothing changes below off"
        );
        click(&mut f, Item::History, Part::Plus);
        assert_eq!(f.values.history, 300);
        // The GIF length stops at ten seconds.
        click(&mut f, Item::Clip, Part::Plus);
        assert_eq!(f.values.clip, 10.0);
        click(&mut f, Item::Clip, Part::Minus);
        assert_eq!(f.values.clip, 8.0);
        // A value between steps goes to the next one.
        f.values.preview = 3.5;
        click(&mut f, Item::Preview, Part::Plus);
        assert_eq!(f.values.preview, 4.0);
    }

    #[test]
    fn the_hotkey_is_recorded_from_the_next_combination() {
        let mut f = form();
        assert_eq!(
            click(&mut f, Item::Hotkey, Part::Body),
            [Effect::Record(true), Effect::Redraw]
        );
        assert!(f.recording());
        let effects = f.handle(Input::Hotkey("ctrl+shift+KeyS".into()));
        assert_eq!(
            effects,
            [Effect::Record(false), Effect::Changed, Effect::Redraw]
        );
        assert_eq!(f.values.hotkey, "ctrl+shift+KeyS");
        // Not listening: a combination means nothing.
        assert!(f.handle(Input::Hotkey("KeyA".into())).is_empty());
    }

    #[test]
    fn escape_stops_listening_before_it_closes() {
        let mut f = form();
        click(&mut f, Item::Hotkey, Part::Body);
        assert_eq!(
            f.handle(Input::Key(Key::Escape)),
            [Effect::Record(false), Effect::Redraw]
        );
        assert_eq!(f.values.hotkey, "PrintScreen");
        assert_eq!(f.handle(Input::Key(Key::Escape)), [Effect::Close]);
    }

    #[test]
    fn a_click_outside_the_panel_closes_it() {
        let mut f = form();
        assert_eq!(
            f.handle(Input::Press(Point::new(5.0, 5.0))),
            [Effect::Close]
        );
        assert_eq!(click(&mut f, Item::Close, Part::Body), [Effect::Close]);
    }

    #[test]
    fn links_and_the_folder_ask_the_application() {
        let mut f = form();
        assert_eq!(
            click(&mut f, Item::Folder, Part::Body),
            [Effect::PickFolder]
        );
        assert_eq!(
            click(&mut f, Item::EditFile, Part::Body),
            [Effect::EditFile]
        );
        assert_eq!(
            click(&mut f, Item::Source, Part::Body),
            [Effect::OpenSource]
        );
    }

    #[test]
    fn the_keyboard_reaches_every_setting() {
        let mut f = form();
        f.handle(Input::Key(Key::Down));
        f.handle(Input::Key(Key::Down));
        f.handle(Input::Key(Key::Down));
        assert_eq!(f.focus, Some(Item::Snap));
        assert_eq!(
            f.handle(Input::Key(Key::Char(' '))),
            [Effect::Changed, Effect::Redraw]
        );
        assert!(!f.values.snap);
        f.handle(Input::Key(Key::Down));
        f.handle(Input::Key(Key::Down));
        assert_eq!(f.focus, Some(Item::Rewind));
        f.handle(Input::Key(Key::Left));
        assert_eq!(f.values.rewind, 3.0);
        // Up from the top wraps to the last link.
        let mut g = form();
        g.handle(Input::Key(Key::Up));
        assert_eq!(g.focus, Some(Item::Source));
    }

    #[test]
    fn hotkeys_read_the_way_people_write_them() {
        assert_eq!(pretty("ctrl+alt+KeyB"), "Ctrl+Alt+B");
        assert_eq!(pretty("shift+Digit3"), "Shift+3");
        assert_eq!(pretty("PrintScreen"), "PrintScreen");
        assert_eq!(seconds(0.0), "Off");
        assert_eq!(seconds(4.0), "4 s");
    }

    #[test]
    fn it_draws_a_panel_on_the_mist() {
        use crate::image::Image;
        let screen = Image::filled(1280, 800, [90, 100, 110, 255]);
        let backdrop = Backdrop::new(&screen, 1.0).unwrap();
        let mut pixmap = Pixmap::new(1280, 800).unwrap();
        let f = form();
        f.draw(&mut pixmap, &backdrop);
        let layout = f.layout();
        // The panel fits the screen, and the switch that is on is accent.
        assert!(layout.panel.y >= 0.0 && layout.panel.bottom() <= 800.0);
        let row = layout.rows.iter().find(|r| r.item == Item::Snap).unwrap();
        let (x, y) = (row.control.x as u32 + 6, row.control.center().y as u32);
        let i = ((y * 1280 + x) * 4) as usize;
        assert_eq!(pixmap.data()[i..i + 3], ACCENT);
    }
}
