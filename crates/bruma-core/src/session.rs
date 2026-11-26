//! One capture, from frozen screen to output.
//!
//! [`Session`] is a pure state machine: feed it [`Input`], apply the
//! [`Effect`]s it returns. It never touches a window or the OS.

use std::rc::Rc;
use std::sync::Arc;

use crate::diff;
use crate::doc::{Annotation, Doc, Shape};
use crate::export::flatten_scaled;
use crate::geom::{Point, Rect, constrain_45, constrain_square};
use crate::image::Image;
use crate::redact::{self, Word};
use crate::snap;
use crate::strip::{self, StripItem, StripLayout};
use crate::text::{Face, Fonts};
use crate::theme;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Tool {
    Select,
    Arrow,
    Line,
    Rect,
    Ellipse,
    Pen,
    Marker,
    Text,
    Step,
    Blur,
    Focus,
    Measure,
}

impl Tool {
    pub const ALL: [Tool; 12] = [
        Tool::Select,
        Tool::Arrow,
        Tool::Line,
        Tool::Rect,
        Tool::Ellipse,
        Tool::Pen,
        Tool::Marker,
        Tool::Text,
        Tool::Step,
        Tool::Blur,
        Tool::Focus,
        Tool::Measure,
    ];

    /// The single key that activates this tool.
    pub fn key(self) -> char {
        match self {
            Tool::Select => 'v',
            Tool::Arrow => 'a',
            Tool::Line => 'l',
            Tool::Rect => 'r',
            Tool::Ellipse => 'e',
            Tool::Pen => 'd',
            Tool::Marker => 'h',
            Tool::Text => 't',
            Tool::Step => 'n',
            Tool::Blur => 'b',
            Tool::Focus => 'o',
            Tool::Measure => 'q',
        }
    }

    pub fn from_key(c: char) -> Option<Tool> {
        Tool::ALL.into_iter().find(|t| t.key() == c)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Key {
    Escape,
    Enter,
    Delete,
    Backspace,
    Left,
    Right,
    Up,
    Down,
    PageUp,
    PageDown,
    /// Before choosing: the next larger area around the pointer (with Shift,
    /// the next smaller).
    Tab,
    /// F1: show or hide the list of keys.
    Help,
    /// A character key, lower-cased, independent of modifiers.
    Char(char),
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Mods {
    pub shift: bool,
    pub ctrl: bool,
    pub alt: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Input {
    PointerMove(Point),
    PointerDown(Point),
    PointerUp(Point),
    /// Right mouse button; behaves like `Esc`.
    SecondaryDown,
    /// Wheel steps; positive thickens the stroke.
    Wheel(f32),
    Key(Key, Mods),
    /// Characters typed, for the text tool.
    Text(String),
    Mods(Mods),
    /// Words recognised in the image sent with [`Effect::FindWords`], in
    /// screen coordinates.
    Words(Vec<Word>),
    /// A double click: edits the text under it, or copies the selection.
    /// Answers with nothing when it means nothing here.
    DoubleClick(Point),
    /// Time has passed, in milliseconds: a GIF being made plays on.
    Tick(u32),
}

#[derive(Clone, Debug, PartialEq)]
pub enum Effect {
    Redraw,
    Copy(Image),
    Save(Image),
    /// The image and where on the screen it was taken from.
    Pin(Image, Rect),
    CopyText(String),
    /// Recognise the text in this image and copy it.
    ExtractText(Image),
    /// Recognise the words in this image, whose top-left corner is at the
    /// given screen position, and answer with [`Input::Words`].
    FindWords(Image, Point),
    /// Make a GIF of this area of the screen: the application fetches the
    /// recording and answers with [`Session::enter_clip`], or with
    /// [`Session::say`] when there is none.
    StartClip(Rect),
    /// Write the GIF: the part of the recording from `from` to `to`
    /// (milliseconds into it) of `area`, with `items` drawn on every frame
    /// (in the area's own coordinates). Copied, or with `save` only saved.
    MakeGif {
        area: Rect,
        from: u64,
        to: u64,
        items: Vec<Annotation>,
        save: bool,
    },
    /// The capture about to be saved has this many numbered steps.
    Guide(u32),
    /// The capture about to be saved, before anything was drawn on it, and
    /// the drawings: what it takes to edit it again.
    Source {
        original: Image,
        items: Vec<Annotation>,
    },
    /// Scroll this area of the screen and capture all of it.
    Scroll(Rect),
    Close,
}

/// The screen at one moment.
#[derive(Clone, Debug)]
pub struct Frame {
    pub image: Arc<Image>,
    /// How long before the capture this was on screen; 0 for "now".
    pub age_ms: u32,
}

impl Frame {
    /// The present moment.
    pub fn now(image: Image) -> Self {
        Self {
            image: Arc::new(image),
            age_ms: 0,
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Options {
    /// UI scale factor (1.0 = 96 DPI).
    pub scale: f32,
    /// Initial ink, an index into the palette.
    pub color: u8,
    /// Initial stroke width in logical pixels.
    pub stroke: f32,
    /// Whether selections snap to what is on screen.
    pub snap: bool,
    /// Whether captures are set in a frame.
    pub framed: bool,
    /// The region selected last time, for selecting it again.
    pub last: Option<Rect>,
    /// Copy the moment an area is chosen, without stopping to draw.
    pub quick: bool,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            scale: 1.0,
            color: 0,
            stroke: 3.0,
            snap: true,
            framed: false,
            last: None,
            quick: false,
        }
    }
}

/// What was asked for before an area was chosen, to happen once it is.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Pending {
    None,
    /// Stay open to draw, with this tool.
    Edit(Option<Tool>),
    /// Make a GIF of it.
    Clip,
}

/// A GIF in the making: the recording of the selection, playing, and the
/// part of it that will be kept.
#[derive(Clone, Debug, PartialEq)]
pub struct ClipView {
    /// How long the recording is, in milliseconds.
    pub length: u64,
    /// The part kept, in milliseconds into the recording.
    pub from: u64,
    pub to: u64,
    /// The moment shown.
    pub at: u64,
    pub playing: bool,
    /// Pictures a second, at most.
    pub rate: u32,
}

/// The shortest GIF that can be cut, in milliseconds.
const SHORTEST_CLIP: u64 = 200;

#[derive(Clone, Copy, Debug, PartialEq)]
enum ClipGrab {
    From,
    To,
    At,
}

/// Where the bar of a GIF in the making is, and its parts.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ClipBar {
    pub rect: Rect,
    /// The play and pause button.
    pub play: Rect,
    /// The line the recording is laid out along.
    pub track: Rect,
    length: u64,
}

impl ClipBar {
    /// Where on the track a moment is.
    pub fn x_of(&self, t: u64) -> f32 {
        let k = if self.length == 0 {
            0.0
        } else {
            t as f32 / self.length as f32
        };
        self.track.x + self.track.w * k.clamp(0.0, 1.0)
    }

    /// The moment at a place on the track.
    pub fn t_of(&self, x: f32) -> u64 {
        let k = ((x - self.track.x) / self.track.w.max(1.0)).clamp(0.0, 1.0);
        (k * self.length as f32).round() as u64
    }
}

/// What a person set during a capture and would expect to find again.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Prefs {
    pub color: u8,
    pub stroke: f32,
    pub snap: bool,
    pub framed: bool,
    pub last: Option<Rect>,
}

/// What the pointer is over, remembered so it is not worked out on every
/// mouse move.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Hover {
    Unknown,
    /// Background of this colour: nothing to select.
    Miss([u8; 4]),
    /// A flat area of this colour.
    Flat(Rect, [u8; 4]),
    /// A box around busy content.
    Busy(Rect),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Handle {
    N,
    S,
    E,
    W,
    NE,
    NW,
    SE,
    SW,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Drag {
    None,
    /// Rubber-banding a new selection; `active` once the pointer has moved far enough.
    Select {
        anchor: Point,
        active: bool,
    },
    MoveSelection {
        grab: Point,
        orig: Rect,
    },
    Resize {
        handle: Handle,
        grab: Point,
        orig: Rect,
    },
    Draw {
        anchor: Point,
    },
    MoveItem {
        index: usize,
        last: Point,
    },
}

/// Text being typed with the text tool.
#[derive(Clone, Debug, PartialEq)]
pub struct TextEdit {
    pub at: Point,
    pub text: String,
}

enum Output {
    Copy,
    Save,
    Pin,
}

/// A drag shorter than this many pixels is a click.
const CLICK_SLOP: f32 = 4.0;

pub struct Session {
    frames: Vec<Frame>,
    frame: usize,
    fonts: Rc<Fonts>,
    windows: Vec<Rect>,
    monitors: Vec<Rect>,
    scale: f32,
    selection: Option<Rect>,
    tool: Tool,
    color: u8,
    stroke: f32,
    doc: Doc,
    drag: Drag,
    draft: Option<Annotation>,
    text: Option<TextEdit>,
    selected: Option<usize>,
    pointer: Point,
    mods: Mods,
    /// A short message for the person, shown until their next action.
    notice: Option<String>,
    snap: bool,
    hover: Hover,
    /// The list of keys is showing.
    help: bool,
    /// The strip item under the pointer, for its tooltip.
    strip_hover: Option<StripItem>,
    framed: bool,
    last: Option<Rect>,
    /// Copy as soon as an area is chosen; off once anything else is asked for.
    quick: bool,
    pending: Pending,
    clip: Option<ClipView>,
    clip_frame: Option<Arc<Image>>,
    clip_grab: Option<ClipGrab>,
    /// Parts of windows that the system knows about (a browser's page, an
    /// editor's text, a panel), each with the window it belongs to.
    panes: Vec<(Rect, Rect)>,
    /// How many steps larger than the smallest area under the pointer the
    /// one offered is, and the smallest area it is counted from.
    level: usize,
    level_base: Option<Rect>,
}

/// Whether two areas are the same to the eye: every edge within a few pixels.
fn alike(a: &Rect, b: &Rect) -> bool {
    (a.x - b.x).abs() <= 6.0
        && (a.y - b.y).abs() <= 6.0
        && (a.right() - b.right()).abs() <= 6.0
        && (a.bottom() - b.bottom()).abs() <= 6.0
}

impl Session {
    /// `frames` are the screen at successive moments, oldest first; the last
    /// one is "now" and must exist. `windows` are top-level window rectangles,
    /// front-most first; `monitors` are monitor rectangles. Both are in
    /// screen-image coordinates.
    pub fn new(
        mut frames: Vec<Frame>,
        fonts: Rc<Fonts>,
        windows: Vec<Rect>,
        monitors: Vec<Rect>,
        options: Options,
    ) -> Self {
        let newest = frames.last().expect("a session needs at least one frame");
        let (width, height) = (newest.image.width, newest.image.height);
        // A resolution change makes older frames useless.
        frames.retain(|f| f.image.width == width && f.image.height == height);
        let bounds = Rect::new(0.0, 0.0, width as f32, height as f32);
        let clip = |rects: Vec<Rect>| -> Vec<Rect> {
            rects
                .into_iter()
                .map(|r| r.round().intersect(&bounds))
                .filter(|r| !r.is_empty())
                .collect()
        };
        Self {
            windows: clip(windows),
            monitors: clip(monitors),
            frame: frames.len() - 1,
            frames,
            fonts,
            scale: options.scale.max(0.5),
            selection: None,
            tool: Tool::Select,
            color: options.color % theme::PALETTE.len() as u8,
            stroke: options.stroke.clamp(theme::STROKE_MIN, theme::STROKE_MAX),
            doc: Doc::new(),
            drag: Drag::None,
            draft: None,
            text: None,
            selected: None,
            pointer: Point::new(bounds.w / 2.0, bounds.h / 2.0),
            mods: Mods::default(),
            notice: None,
            snap: options.snap,
            hover: Hover::Unknown,
            help: false,
            strip_hover: None,
            framed: options.framed,
            last: options.last,
            quick: options.quick,
            pending: Pending::None,
            clip: None,
            clip_frame: None,
            clip_grab: None,
            panes: Vec::new(),
            level: 0,
            level_base: None,
        }
    }

    /// The parts of windows the system knows about, each with its window, in
    /// screen-image coordinates.
    pub fn set_panes(&mut self, panes: Vec<(Rect, Rect)>) {
        let bounds = self.screen().bounds();
        self.panes = panes
            .into_iter()
            .map(|(owner, pane)| {
                (
                    owner.round().intersect(&bounds),
                    pane.round().intersect(&bounds),
                )
            })
            .filter(|(owner, pane)| !owner.is_empty() && pane.w >= 40.0 && pane.h >= 40.0)
            .collect();
    }

    /// The top-level window under the pointer, front-most first.
    fn window_under(&self, p: Point) -> Option<Rect> {
        self.windows.iter().copied().find(|w| w.contains(p))
    }

    /// Everything a click could select while nothing is selected yet, from
    /// the smallest to the window: the box under the pointer, the parts of
    /// the window around it, the window.
    fn targets(&self) -> Vec<Rect> {
        if self.selection.is_some() {
            return Vec::new();
        }
        let p = self.pointer;
        let window = self.window_under(p);
        let mut found: Vec<Rect> = Vec::new();
        if let Hover::Flat(rect, _) | Hover::Busy(rect) = self.hover {
            found.push(rect);
        }
        if let Some(w) = window {
            found.extend(
                self.panes
                    .iter()
                    .filter(|(owner, pane)| alike(owner, &w) && pane.contains(p))
                    .map(|(_, pane)| *pane),
            );
        }
        found.sort_by(|a, b| (a.w * a.h).total_cmp(&(b.w * b.h)));
        let mut out: Vec<Rect> = Vec::new();
        for rect in found {
            // Something with the edges of the window is the window, which
            // comes last; the page of a full-screen browser is not.
            let whole = window.is_some_and(|w| alike(&rect, &w));
            if whole || out.iter().any(|r| alike(r, &rect)) {
                continue;
            }
            out.push(rect);
        }
        out.extend(window);
        out
    }

    /// Forgets the chosen size when the smallest area under the pointer is
    /// another one.
    fn settle_level(&mut self) {
        let base = self.targets().first().copied();
        if base != self.level_base {
            self.level_base = base;
            self.level = 0;
        }
    }

    // ---- read access for the renderer -------------------------------------

    /// The frame being looked at: "now", or an earlier moment after a rewind.
    pub fn screen(&self) -> &Image {
        &self.frames[self.frame].image
    }
    pub fn frame_index(&self) -> usize {
        self.frame
    }
    pub fn frame_count(&self) -> usize {
        self.frames.len()
    }
    /// How long before the capture the current frame was on screen.
    pub fn frame_age_ms(&self) -> u32 {
        self.frames[self.frame].age_ms
    }
    pub fn fonts(&self) -> &Fonts {
        &self.fonts
    }
    pub fn scale(&self) -> f32 {
        self.scale
    }
    pub fn selection(&self) -> Option<Rect> {
        self.selection
    }
    pub fn tool(&self) -> Tool {
        self.tool
    }
    pub fn color(&self) -> u8 {
        self.color
    }
    /// Stroke width in logical pixels.
    pub fn stroke(&self) -> f32 {
        self.stroke
    }
    pub fn doc(&self) -> &Doc {
        &self.doc
    }
    pub fn draft(&self) -> Option<&Annotation> {
        self.draft.as_ref()
    }
    pub fn text_edit(&self) -> Option<&TextEdit> {
        self.text.as_ref()
    }
    pub fn selected(&self) -> Option<usize> {
        self.selected
    }
    pub fn notice(&self) -> Option<&str> {
        self.notice.as_deref()
    }
    /// The frame being looked at, to keep.
    pub fn current(&self) -> Arc<Image> {
        self.frames[self.frame].image.clone()
    }
    /// Every frame, oldest first.
    pub fn frames(&self) -> &[Frame] {
        &self.frames
    }
    pub fn windows(&self) -> &[Rect] {
        &self.windows
    }
    pub fn monitors(&self) -> &[Rect] {
        &self.monitors
    }
    /// Whether choosing an area copies it straight away.
    pub fn quick(&self) -> bool {
        self.quick
    }
    /// The GIF in the making, if one is.
    pub fn clip(&self) -> Option<&ClipView> {
        self.clip.as_ref()
    }
    /// The picture of the selection at the moment the GIF shows.
    pub fn clip_frame(&self) -> Option<&Image> {
        self.clip_frame.as_deref()
    }
    /// The bar under a GIF in the making: where to play, pause and trim it.
    pub fn clip_bar(&self) -> Option<ClipBar> {
        let view = self.clip.as_ref()?;
        let sel = self.selection?;
        let monitor = self.monitor_of(sel.center());
        let px = |v: f32| (v * self.scale).round();
        let w = px(540.0).min(monitor.w - px(32.0));
        let h = px(44.0);
        let rect = Rect::new(
            (monitor.x + (monitor.w - w) / 2.0).round(),
            monitor.bottom() - h - px(24.0),
            w,
            h,
        );
        let play = Rect::new(rect.x + px(8.0), rect.y + px(6.0), px(32.0), px(32.0));
        let track = Rect::new(
            play.right() + px(14.0),
            rect.y + px(12.0),
            rect.right() - px(118.0) - (play.right() + px(14.0)),
            px(20.0),
        );
        Some(ClipBar {
            rect,
            play,
            track,
            length: view.length,
        })
    }

    /// The newest frame: the screen as it is now.
    pub fn present(&self) -> Arc<Image> {
        self.frames[self.frames.len() - 1].image.clone()
    }
    pub fn pointer(&self) -> Point {
        self.pointer
    }

    /// True while there is no settled selection: the crosshair and loupe show.
    pub fn selecting(&self) -> bool {
        self.selection.is_none() || matches!(self.drag, Drag::Select { active: true, .. })
    }

    /// What a click would select while nothing is selected yet: the box,
    /// button, picture or circle under the pointer, or failing that the window.
    pub fn hover_target(&self) -> Option<Rect> {
        let targets = self.targets();
        targets
            .get(self.level.min(targets.len().saturating_sub(1)))
            .copied()
    }

    /// A short message for the person, shown until their next action.
    pub fn say(&mut self, text: &str) {
        self.notice = Some(text.to_string());
    }

    /// Chooses `area`, as if it had been dragged, and stays open to draw: a
    /// capture opened again to be edited.
    pub fn select(&mut self, area: Rect) {
        let area = area.round().intersect(&self.screen().bounds());
        if !area.is_empty() {
            self.selection = Some(area);
            self.drag = Drag::None;
            self.quick = false;
            self.pending = Pending::None;
        }
    }

    /// Starts making a GIF of the selection from a recording `length`
    /// milliseconds long with at most `rate` pictures a second.
    pub fn enter_clip(&mut self, length: u64, rate: u32) {
        self.quick = false;
        self.clip = Some(ClipView {
            length,
            from: 0,
            to: length,
            at: 0,
            playing: true,
            rate,
        });
        self.clip_frame = None;
        self.clip_grab = None;
    }

    /// The moment of the recording to show now, while a GIF is being made.
    pub fn clip_time(&self) -> Option<u64> {
        self.clip.as_ref().map(|c| c.at)
    }

    /// The picture of the selection at [`Session::clip_time`].
    pub fn show_clip_frame(&mut self, picture: Arc<Image>) {
        if self.clip.is_some() {
            self.clip_frame = Some(picture);
        }
    }

    /// Says where the pointer is without working out what is under it yet.
    /// Opening must not wait for that; the first real move does it.
    pub fn place_pointer(&mut self, p: Point) {
        self.pointer = p;
    }

    /// Whether selections snap to what is on screen.
    pub fn snap(&self) -> bool {
        self.snap
    }
    /// Whether the list of keys is showing.
    pub fn help(&self) -> bool {
        self.help
    }
    pub fn strip_hover(&self) -> Option<StripItem> {
        self.strip_hover
    }
    pub fn prefs(&self) -> Prefs {
        Prefs {
            color: self.color,
            stroke: self.stroke,
            snap: self.snap,
            framed: self.framed,
            last: self.selection.or(self.last),
        }
    }

    /// Whether captures are set in a frame.
    pub fn framed(&self) -> bool {
        self.framed
    }

    /// Starts from a capture made earlier: `selection` is where the picture
    /// lies on the screen image and `items` are its drawings.
    pub fn load(&mut self, selection: Rect, items: Vec<Annotation>) {
        self.selection = Some(selection);
        self.doc = Doc::new();
        self.doc.edit(|v| v.extend(items));
        self.hover = Hover::Unknown;
    }

    /// Works out what the pointer is over, reusing the last answer while the
    /// pointer stays on the same thing.
    fn update_hover(&mut self) {
        if !self.snap || self.selection.is_some() {
            self.hover = Hover::Unknown;
            return;
        }
        let p = self.pointer;
        let colour = self.screen().pixel(p.x as i32, p.y as i32);
        let same = |c: [u8; 4]| (0..3).all(|i| (c[i] as i32 - colour[i] as i32).abs() <= 10);
        let keep = match self.hover {
            Hover::Miss(c) => same(c),
            Hover::Flat(rect, c) => rect.contains(p) && same(c),
            Hover::Busy(_) | Hover::Unknown => false,
        };
        if keep {
            return;
        }
        let window = self.window_under(p);
        self.hover = match snap::flat_region(self.screen(), p, window) {
            Some(rect) => Hover::Flat(rect, colour),
            None => match self.hover {
                // Still inside the same picture: no need to box it again.
                Hover::Busy(rect) if rect.contains(p) => Hover::Busy(rect),
                _ => match snap::busy_box(self.screen(), p) {
                    Some(rect) => Hover::Busy(rect),
                    None => Hover::Miss(colour),
                },
            },
        };
    }

    /// Snapping is on, and `Alt` is not held to suspend it.
    fn snapping(&self) -> bool {
        self.snap && !self.mods.alt
    }

    /// The strip layout, when it is visible.
    pub fn strip(&self) -> Option<StripLayout> {
        if self.selecting() {
            return None;
        }
        let sel = self.selection?;
        Some(strip::layout(
            sel,
            self.monitor_of(sel.center()),
            self.scale,
        ))
    }

    /// Font size used by the text tool, in physical pixels.
    pub fn text_size(&self) -> f32 {
        ((self.stroke * 6.0).max(14.0) * self.scale).round()
    }

    /// The monitor under `p`, or the whole screen when `p` is on none.
    pub fn monitor_of(&self, p: Point) -> Rect {
        self.monitors
            .iter()
            .copied()
            .find(|m| m.contains(p))
            .unwrap_or_else(|| self.screen().bounds())
    }

    fn physical_stroke(&self) -> f32 {
        self.stroke * self.scale
    }

    fn hit_tolerance(&self) -> f32 {
        6.0 * self.scale
    }

    // ---- input ------------------------------------------------------------

    pub fn handle(&mut self, input: Input) -> Vec<Effect> {
        let mut fx = Vec::new();
        // A notice lasts until the next deliberate action.
        if matches!(
            input,
            Input::PointerDown(_) | Input::Key(..) | Input::Wheel(_) | Input::SecondaryDown
        ) {
            self.notice = None;
        }
        // While the list of keys is up, the next click or key only closes it.
        if self.help
            && matches!(
                input,
                Input::PointerDown(_) | Input::Key(..) | Input::SecondaryDown
            )
        {
            self.help = false;
            return vec![Effect::Redraw];
        }
        match input {
            Input::Mods(mods) => {
                self.mods = mods;
                if self.drag != Drag::None {
                    self.pointer_move(self.pointer, &mut fx);
                }
            }
            Input::PointerMove(p) => self.pointer_move(p, &mut fx),
            Input::PointerDown(p) => self.pointer_down(p, &mut fx),
            Input::PointerUp(p) => self.pointer_up(p, &mut fx),
            Input::SecondaryDown => self.escape(&mut fx),
            Input::Wheel(steps) => {
                if steps != 0.0 {
                    if self.selecting() && self.drag == Drag::None {
                        // Before anything is selected the wheel moves through
                        // time: up is earlier.
                        self.rewind(if steps > 0.0 { -1 } else { 1 });
                    } else {
                        self.set_stroke(self.stroke + steps.signum());
                    }
                    fx.push(Effect::Redraw);
                }
            }
            Input::Key(key, mods) => {
                self.mods = mods;
                self.key(key, mods, &mut fx);
            }
            Input::DoubleClick(p) => {
                if self.text.is_some() {
                    return fx;
                }
                let hit = self.doc.hit(p, self.hit_tolerance());
                let text = hit.and_then(|i| match &self.doc.items()[i].shape {
                    Shape::Text { at, text, .. } => {
                        Some((i, *at, text.clone(), self.doc.items()[i].color))
                    }
                    _ => None,
                });
                if let Some((index, at, text, color)) = text {
                    // Take the text back out of the picture and into the editor.
                    self.doc.edit(|v| {
                        v.remove(index);
                    });
                    self.selected = None;
                    self.drag = Drag::None;
                    self.tool = Tool::Text;
                    self.color = color;
                    self.text = Some(TextEdit { at, text });
                    fx.push(Effect::Redraw);
                } else if self.tool == Tool::Select
                    && !self.selecting()
                    && self.selection.is_some_and(|s| s.contains(p))
                    && !self.strip().is_some_and(|l| l.rect.contains(p))
                {
                    self.drag = Drag::None;
                    self.finish(Output::Copy, &mut fx);
                }
            }
            Input::Words(words) => {
                let found = redact::sensitive(&words);
                if found.is_empty() {
                    self.notice = Some("nothing to mask".into());
                } else {
                    let pad = 2.0 * self.scale;
                    let count = found.len();
                    let masks: Vec<Annotation> = found
                        .into_iter()
                        .map(|rect| {
                            self.annotation(Shape::Blur {
                                rect: rect.inflate(pad),
                            })
                        })
                        .collect();
                    self.doc.edit(|v| v.extend(masks));
                    self.notice = Some(format!("{count} masked"));
                }
                fx.push(Effect::Redraw);
            }
            Input::Text(typed) => {
                if let Some(edit) = &mut self.text {
                    edit.text.extend(typed.chars().filter(|c| !c.is_control()));
                    fx.push(Effect::Redraw);
                }
            }
            Input::Tick(ms) => {
                if let Some(view) = &mut self.clip {
                    if view.playing && self.clip_grab.is_none() {
                        let span = view.to.saturating_sub(view.from).max(1);
                        view.at =
                            view.from + (view.at.saturating_sub(view.from) + ms as u64) % span;
                        fx.push(Effect::Redraw);
                    }
                }
            }
        }
        fx
    }

    /// An area has just been chosen: copy it, or do what was asked before.
    fn chosen(&mut self, fx: &mut Vec<Effect>) {
        let Some(sel) = self.selection.filter(|s| !s.is_empty()) else {
            return;
        };
        match std::mem::replace(&mut self.pending, Pending::None) {
            Pending::Clip => {
                self.quick = false;
                self.notice = None;
                fx.push(Effect::StartClip(
                    sel.round().intersect(&self.screen().bounds()),
                ));
            }
            Pending::Edit(tool) => {
                self.quick = false;
                self.notice = None;
                if let Some(tool) = tool {
                    self.set_tool(tool);
                }
            }
            Pending::None if self.quick && !self.mods.shift => self.finish(Output::Copy, fx),
            Pending::None => self.quick = false,
        }
    }

    /// A press on the bar of a GIF in the making. `true` if it was on it.
    fn clip_press(&mut self, p: Point) -> bool {
        let (Some(bar), Some(view)) = (self.clip_bar(), self.clip.as_mut()) else {
            return false;
        };
        if !bar.rect.contains(p) {
            return false;
        }
        if bar.play.inflate(4.0 * self.scale).contains(p) {
            view.playing = !view.playing;
            return true;
        }
        let near = 10.0 * self.scale;
        let (from_x, to_x) = (bar.x_of(view.from), bar.x_of(view.to));
        self.clip_grab = Some(if (p.x - to_x).abs() <= near {
            ClipGrab::To
        } else if (p.x - from_x).abs() <= near {
            ClipGrab::From
        } else {
            ClipGrab::At
        });
        self.clip_drag(p);
        true
    }

    /// Moves what is held on the bar to the pointer.
    fn clip_drag(&mut self, p: Point) {
        let (Some(bar), Some(grab)) = (self.clip_bar(), self.clip_grab) else {
            return;
        };
        let Some(view) = self.clip.as_mut() else {
            return;
        };
        let t = bar.t_of(p.x);
        let shortest = SHORTEST_CLIP.min(view.length);
        match grab {
            ClipGrab::From => {
                view.from = t.min(view.to.saturating_sub(shortest));
                view.at = view.from;
            }
            ClipGrab::To => {
                view.to = t.max(view.from + shortest).min(view.length);
                view.at = view.to;
            }
            ClipGrab::At => {
                view.at = t.clamp(view.from, view.to);
                view.playing = false;
            }
        }
    }

    /// The GIF as it stands, written.
    fn make_gif(&mut self, save: bool, fx: &mut Vec<Effect>) {
        self.commit_text();
        let (Some(view), Some(sel)) = (self.clip.as_ref(), self.selection) else {
            return;
        };
        let area = sel.round().intersect(&self.screen().bounds());
        if area.is_empty() {
            return;
        }
        let items = self
            .doc
            .items()
            .iter()
            .cloned()
            .map(|mut item| {
                item.translate(-area.x, -area.y);
                item
            })
            .collect();
        fx.push(Effect::MakeGif {
            area,
            from: view.from,
            to: view.to,
            items,
            save,
        });
        fx.push(Effect::Close);
    }

    fn pointer_down(&mut self, p: Point, fx: &mut Vec<Effect>) {
        self.pointer = p;
        fx.push(Effect::Redraw);
        self.commit_text();
        if self.clip_press(p) {
            return;
        }

        let Some(sel) = self.selection else {
            self.drag = Drag::Select {
                anchor: p,
                active: false,
            };
            return;
        };
        if let Some(layout) = self.strip() {
            if let Some(item) = layout.hit(p) {
                self.strip_click(item, fx);
                return;
            }
            if layout.rect.contains(p) {
                return;
            }
        }
        match self.tool {
            // While a GIF is being made the area stays where it is: only the
            // drawings move.
            Tool::Select if self.clip.is_some() => {
                if let Some(index) = self.doc.hit(p, self.hit_tolerance()) {
                    self.selected = Some(index);
                    self.doc.begin_gesture();
                    self.drag = Drag::MoveItem { index, last: p };
                } else {
                    self.selected = None;
                }
            }
            Tool::Select => {
                if let Some(handle) = self.handle_at(p, sel) {
                    self.selected = None;
                    self.drag = Drag::Resize {
                        handle,
                        grab: p,
                        orig: sel,
                    };
                } else if let Some(index) = self.doc.hit(p, self.hit_tolerance()) {
                    self.selected = Some(index);
                    self.doc.begin_gesture();
                    self.drag = Drag::MoveItem { index, last: p };
                } else if sel.contains(p) {
                    self.selected = None;
                    self.drag = Drag::MoveSelection { grab: p, orig: sel };
                } else {
                    self.selected = None;
                    self.drag = Drag::Select {
                        anchor: p,
                        active: false,
                    };
                }
            }
            Tool::Text => {
                self.text = Some(TextEdit {
                    at: p,
                    text: String::new(),
                });
            }
            Tool::Step => {
                let n = self.doc.next_step();
                let item = self.annotation(Shape::Step { at: p, n });
                self.doc.edit(|v| v.push(item));
            }
            tool => {
                let shape = match tool {
                    Tool::Arrow => Shape::Arrow { from: p, to: p },
                    Tool::Line => Shape::Line { from: p, to: p },
                    Tool::Rect => Shape::Rect {
                        rect: Rect::from_points(p, p),
                    },
                    Tool::Ellipse => Shape::Ellipse {
                        rect: Rect::from_points(p, p),
                    },
                    Tool::Blur => Shape::Blur {
                        rect: Rect::from_points(p, p),
                    },
                    Tool::Focus => Shape::Focus {
                        rect: Rect::from_points(p, p),
                    },
                    Tool::Measure => Shape::Measure { from: p, to: p },
                    Tool::Pen => Shape::Pen { points: vec![p] },
                    _ => Shape::Marker { points: vec![p] },
                };
                self.draft = Some(self.annotation(shape));
                self.drag = Drag::Draw { anchor: p };
            }
        }
    }

    fn pointer_move(&mut self, p: Point, fx: &mut Vec<Effect>) {
        self.pointer = p;
        if self.clip_grab.is_some() {
            self.clip_drag(p);
            fx.push(Effect::Redraw);
            return;
        }
        let bounds = self.screen().bounds();
        match self.drag {
            Drag::None => {
                if self.selecting() {
                    self.update_hover();
                    self.settle_level();
                    fx.push(Effect::Redraw);
                } else {
                    // Over the strip: remember which item, for its tooltip.
                    let over = self.strip().and_then(|layout| layout.hit(p));
                    if over != self.strip_hover {
                        self.strip_hover = over;
                        fx.push(Effect::Redraw);
                    }
                }
                return;
            }
            Drag::Select { anchor, active } => {
                if !active {
                    self.update_hover();
                }
                if active || anchor.distance(p) >= CLICK_SLOP {
                    self.drag = Drag::Select {
                        anchor,
                        active: true,
                    };
                    let (mut from, mut to) = (anchor, p);
                    if self.snapping() {
                        // Both corners look for a real edge nearby.
                        let (screen, reach) = (self.screen(), 8.0 * self.scale);
                        to.x = snap::snap_x(screen, p.x, anchor.y, p.y, reach);
                        to.y = snap::snap_y(screen, p.y, anchor.x, to.x, reach);
                        from.x = snap::snap_x(screen, anchor.x, anchor.y, to.y, reach);
                        from.y = snap::snap_y(screen, anchor.y, from.x, to.x, reach);
                    }
                    let rect = Rect::from_points(from, to).round().intersect(&bounds);
                    self.selection = Some(rect);
                }
            }
            Drag::MoveSelection { grab, orig } => {
                let moved = orig.translate(p.x - grab.x, p.y - grab.y).round();
                self.selection = Some(moved.clamp_inside(&bounds));
            }
            Drag::Resize { handle, grab, orig } => {
                let (dx, dy) = (p.x - grab.x, p.y - grab.y);
                let (mut l, mut t, mut r, mut b) = (orig.x, orig.y, orig.right(), orig.bottom());
                if matches!(handle, Handle::W | Handle::NW | Handle::SW) {
                    l += dx;
                }
                if matches!(handle, Handle::E | Handle::NE | Handle::SE) {
                    r += dx;
                }
                if matches!(handle, Handle::N | Handle::NW | Handle::NE) {
                    t += dy;
                }
                if matches!(handle, Handle::S | Handle::SW | Handle::SE) {
                    b += dy;
                }
                if self.snapping() {
                    // The edges being dragged look for a real edge nearby.
                    let (screen, reach) = (self.screen(), 8.0 * self.scale);
                    if matches!(handle, Handle::W | Handle::NW | Handle::SW) {
                        l = snap::snap_x(screen, l, t, b, reach);
                    }
                    if matches!(handle, Handle::E | Handle::NE | Handle::SE) {
                        r = snap::snap_x(screen, r, t, b, reach);
                    }
                    if matches!(handle, Handle::N | Handle::NW | Handle::NE) {
                        t = snap::snap_y(screen, t, l, r, reach);
                    }
                    if matches!(handle, Handle::S | Handle::SW | Handle::SE) {
                        b = snap::snap_y(screen, b, l, r, reach);
                    }
                }
                let rect = Rect::from_points(Point::new(l, t), Point::new(r, b))
                    .round()
                    .intersect(&bounds);
                if rect.w >= 1.0 && rect.h >= 1.0 {
                    self.selection = Some(rect);
                }
            }
            Drag::Draw { anchor } => {
                let shift = self.mods.shift;
                if let Some(draft) = &mut self.draft {
                    match &mut draft.shape {
                        Shape::Arrow { to, .. }
                        | Shape::Line { to, .. }
                        | Shape::Measure { to, .. } => {
                            *to = if shift { constrain_45(anchor, p) } else { p };
                        }
                        Shape::Rect { rect }
                        | Shape::Ellipse { rect }
                        | Shape::Blur { rect }
                        | Shape::Focus { rect } => {
                            let corner = if shift {
                                constrain_square(anchor, p)
                            } else {
                                p
                            };
                            *rect = Rect::from_points(anchor, corner);
                        }
                        Shape::Pen { points } | Shape::Marker { points } => {
                            if points.last().is_none_or(|last| last.distance(p) >= 1.5) {
                                points.push(p);
                            }
                        }
                        Shape::Text { .. } | Shape::Step { .. } => {}
                    }
                }
            }
            Drag::MoveItem { index, last } => {
                if let Some(item) = self.doc.items_mut().get_mut(index) {
                    item.translate(p.x - last.x, p.y - last.y);
                }
                self.drag = Drag::MoveItem { index, last: p };
            }
        }
        fx.push(Effect::Redraw);
    }

    fn pointer_up(&mut self, p: Point, fx: &mut Vec<Effect>) {
        if let Some(grab) = self.clip_grab.take() {
            if let Some(view) = &mut self.clip {
                if grab != ClipGrab::At {
                    // Trimmed: play the part that is left from its start.
                    view.at = view.from;
                    view.playing = true;
                }
            }
            fx.push(Effect::Redraw);
            return;
        }
        if self.drag == Drag::None {
            return;
        }
        self.pointer_move(p, fx);
        match std::mem::replace(&mut self.drag, Drag::None) {
            Drag::Select { active: false, .. } => {
                if self.selection.is_none() {
                    self.selection = self.hover_target();
                    self.chosen(fx);
                }
            }
            Drag::Select { active: true, .. } => {
                if self.selection.is_some_and(|s| s.w < 1.0 || s.h < 1.0) {
                    self.selection = None;
                } else {
                    self.chosen(fx);
                }
            }
            Drag::Draw { anchor } => {
                if let Some(draft) = self.draft.take() {
                    let keep = match &draft.shape {
                        Shape::Pen { points } | Shape::Marker { points } => points.len() >= 2,
                        _ => anchor.distance(p) >= CLICK_SLOP,
                    };
                    if keep {
                        self.doc.edit(|v| v.push(draft));
                    }
                }
            }
            Drag::MoveItem { .. } => self.doc.end_gesture(),
            Drag::MoveSelection { .. } | Drag::Resize { .. } | Drag::None => {}
        }
        if !fx.contains(&Effect::Redraw) {
            fx.push(Effect::Redraw);
        }
    }

    fn key(&mut self, key: Key, mods: Mods, fx: &mut Vec<Effect>) {
        if self.text.is_some() {
            match key {
                Key::Escape => self.commit_text(),
                Key::Enter if mods.shift => {
                    if let Some(edit) = &mut self.text {
                        edit.text.push('\n');
                    }
                }
                Key::Enter => self.commit_text(),
                Key::Backspace => {
                    if let Some(edit) = &mut self.text {
                        edit.text.pop();
                    }
                }
                _ => return,
            }
            fx.push(Effect::Redraw);
            return;
        }

        // A GIF in the making: the area stays, and space plays or pauses.
        if self.clip.is_some() {
            match key {
                Key::Char(' ') => {
                    if let Some(view) = &mut self.clip {
                        view.playing = !view.playing;
                    }
                    fx.push(Effect::Redraw);
                    return;
                }
                Key::Char(
                    'f' | 's' | 'z' | 'w' | 'k' | 'j' | 'p' | 'y' | 'x' | 'm' | 'u' | ',' | '.',
                ) if !mods.ctrl => {
                    return;
                }
                Key::Left | Key::Right | Key::Up | Key::Down if self.selected.is_none() => return,
                Key::PageUp | Key::PageDown => return,
                _ => {}
            }
        }
        match key {
            Key::Tab if self.selecting() && self.drag == Drag::None => {
                self.settle_level();
                let count = self.targets().len();
                self.level = if mods.shift {
                    self.level.saturating_sub(1)
                } else {
                    (self.level + 1).min(count.saturating_sub(1))
                };
            }
            Key::Tab => return,
            Key::Escape => return self.escape(fx),
            Key::Enter => return self.finish(Output::Copy, fx),
            Key::Delete | Key::Backspace => {
                if let Some(index) = self.selected.take() {
                    self.doc.edit(|v| {
                        if index < v.len() {
                            v.remove(index);
                        }
                    });
                }
            }
            Key::Help => self.help = true,
            Key::PageUp => {
                self.rewind(-1);
            }
            Key::PageDown => {
                self.rewind(1);
            }
            Key::Left => self.nudge(-1.0, 0.0, mods),
            Key::Right => self.nudge(1.0, 0.0, mods),
            Key::Up => self.nudge(0.0, -1.0, mods),
            Key::Down => self.nudge(0.0, 1.0, mods),
            Key::Char(c) if mods.ctrl => match c {
                'c' => return self.finish(Output::Copy, fx),
                's' => return self.finish(Output::Save, fx),
                'z' if mods.shift => self.redo(),
                'z' => self.undo(),
                'y' => self.redo(),
                _ => return,
            },
            Key::Char(c) => match c {
                'p' => return self.finish(Output::Pin, fx),
                'f' => {
                    let fresh = self.selection.is_none();
                    self.selection = Some(self.monitor_of(self.pointer));
                    self.drag = Drag::None;
                    if fresh {
                        self.chosen(fx);
                        if fx.contains(&Effect::Close) {
                            return;
                        }
                    }
                }
                's' => {
                    if let Some(sel) = self.selection {
                        self.selection = Some(self.screen().tighten(sel));
                    }
                }
                'c' if self.selection.is_none() => {
                    let px = self
                        .screen()
                        .pixel(self.pointer.x as i32, self.pointer.y as i32);
                    // Hex by default; Shift gives rgb(), Alt gives hsl().
                    let code = match (mods.shift, mods.alt) {
                        (true, _) => theme::rgb(px),
                        (_, true) => theme::hsl(px),
                        _ => theme::hex(px),
                    };
                    fx.push(Effect::CopyText(code));
                    fx.push(Effect::Close);
                    return;
                }
                '1'..='6' => {
                    let index = c as u8 - b'1';
                    self.color = index;
                    if let Some(i) = self.selected {
                        self.doc.edit(|v| {
                            if let Some(item) = v.get_mut(i) {
                                item.color = index;
                            }
                        });
                    }
                }
                ',' => {
                    self.rewind(-1);
                }
                '.' => {
                    self.rewind(1);
                }
                'w' => {
                    let fresh = self.selection.is_none();
                    if !self.select_change() {
                        self.notice = Some("nothing changed".into());
                    } else if fresh {
                        self.chosen(fx);
                        if fx.contains(&Effect::Close) {
                            return;
                        }
                    }
                }
                'x' | 'm' => {
                    let bounds = self.screen().bounds();
                    let Some(area) = self
                        .selection
                        .map(|s| s.round().intersect(&bounds))
                        .filter(|s| !s.is_empty())
                    else {
                        return;
                    };
                    let image = self.screen().crop(area);
                    if c == 'x' {
                        fx.push(Effect::ExtractText(image));
                        fx.push(Effect::Close);
                    } else {
                        self.notice = Some("reading".into());
                        fx.push(Effect::Redraw);
                        fx.push(Effect::FindWords(image, Point::new(area.x, area.y)));
                    }
                    return;
                }
                '?' => self.help = true,
                'u' => {
                    self.framed = !self.framed;
                    self.notice = Some(if self.framed { "frame on" } else { "frame off" }.into());
                }
                'z' if self.selection.is_none() => {
                    let bounds = self.screen().bounds();
                    match self
                        .last
                        .map(|r| r.intersect(&bounds))
                        .filter(|r| !r.is_empty())
                    {
                        Some(rect) => {
                            self.selection = Some(rect);
                            self.drag = Drag::None;
                            self.chosen(fx);
                            if fx.contains(&Effect::Close) {
                                return;
                            }
                        }
                        None => self.notice = Some("no earlier region".into()),
                    }
                }
                'k' => match self.selection.filter(|s| !s.is_empty()) {
                    Some(sel) => {
                        self.commit_text();
                        fx.push(Effect::StartClip(
                            sel.round().intersect(&self.screen().bounds()),
                        ));
                    }
                    None => {
                        self.pending = Pending::Clip;
                        self.notice = Some("Choose the area for the GIF".into());
                    }
                },
                'j' => return self.before_after(fx),
                'y' => {
                    let Some(sel) = self.selection.filter(|s| !s.is_empty()) else {
                        return;
                    };
                    fx.push(Effect::Scroll(sel));
                    fx.push(Effect::Close);
                    return;
                }
                'g' => {
                    self.snap = !self.snap;
                    self.update_hover();
                    self.notice = Some(if self.snap { "snap on" } else { "snap off" }.into());
                }
                '[' => self.set_stroke(self.stroke - 1.0),
                ']' => self.set_stroke(self.stroke + 1.0),
                _ => match Tool::from_key(c) {
                    Some(tool) if self.selection.is_some() => self.set_tool(tool),
                    // A tool picked before choosing an area: stay open to
                    // draw once it is chosen.
                    Some(tool) if self.quick => {
                        self.pending = Pending::Edit(Some(tool));
                        self.notice = Some("Choose the area to draw on".to_string());
                    }
                    _ => return,
                },
            },
        }
        fx.push(Effect::Redraw);
    }

    /// One step back: finish text, cancel a drag, drop the item selection,
    /// return to the select tool, and finally close.
    fn escape(&mut self, fx: &mut Vec<Effect>) {
        if self.text.is_some() {
            self.commit_text();
        } else if self.drag != Drag::None {
            if matches!(self.drag, Drag::MoveItem { .. }) {
                self.doc.end_gesture();
            }
            if matches!(self.drag, Drag::Select { active: true, .. }) {
                self.selection = None;
            }
            self.drag = Drag::None;
            self.draft = None;
        } else if self.selected.is_some() {
            self.selected = None;
        } else if self.tool != Tool::Select {
            self.tool = Tool::Select;
        } else if self.clip.is_some() {
            self.clip = None;
            self.clip_frame = None;
            self.clip_grab = None;
        } else if self.pending != Pending::None {
            self.pending = Pending::None;
            self.notice = None;
        } else {
            fx.push(Effect::Close);
            return;
        }
        fx.push(Effect::Redraw);
    }

    fn finish(&mut self, output: Output, fx: &mut Vec<Effect>) {
        if self.clip.is_some() {
            match output {
                Output::Copy => self.make_gif(false, fx),
                Output::Save => self.make_gif(true, fx),
                Output::Pin => {}
            }
            return;
        }
        self.commit_text();
        let Some(sel) = self.selection.filter(|s| !s.is_empty()) else {
            return;
        };
        let image = flatten_scaled(
            self.screen(),
            self.doc.items(),
            sel,
            &self.fonts,
            self.scale,
        );
        if image.width == 0 || image.height == 0 {
            return;
        }
        if matches!(output, Output::Save) {
            // A saved file can be opened and edited again, and a capture
            // with numbered steps gets a guide to go with it.
            let area = sel.round().intersect(&self.screen().bounds());
            let items = self
                .doc
                .items()
                .iter()
                .cloned()
                .map(|mut item| {
                    item.translate(-area.x, -area.y);
                    item
                })
                .collect();
            fx.push(Effect::Source {
                original: self.screen().crop(area),
                items,
            });
            let steps = self
                .doc
                .items()
                .iter()
                .filter(|a| matches!(a.shape, Shape::Step { .. }))
                .count();
            if steps >= 2 {
                fx.push(Effect::Guide(steps as u32));
            }
        }
        let image = if self.framed {
            crate::export::frame(&image, self.scale)
        } else {
            image
        };
        fx.push(match output {
            Output::Copy => Effect::Copy(image),
            Output::Save => Effect::Save(image),
            Output::Pin => Effect::Pin(image, sel),
        });
        fx.push(Effect::Close);
    }

    fn strip_click(&mut self, item: StripItem, fx: &mut Vec<Effect>) {
        match item {
            StripItem::Tool(tool) => self.set_tool(tool),
            StripItem::Color => self.color = (self.color + 1) % theme::PALETTE.len() as u8,
            StripItem::Undo => self.undo(),
            StripItem::Redo => self.redo(),
            StripItem::Copy => self.finish(Output::Copy, fx),
            StripItem::Save => self.finish(Output::Save, fx),
            StripItem::Pin => self.finish(Output::Pin, fx),
        }
    }

    /// Two moments side by side with the change outlined, copied like a capture.
    fn before_after(&mut self, fx: &mut Vec<Effect>) {
        let last = self.frames.len() - 1;
        if last == 0 {
            self.notice = Some("nothing recorded to compare with".into());
            fx.push(Effect::Redraw);
            return;
        }
        let other = if self.frame == last { 0 } else { last };
        let (older, newer) = (self.frame.min(other), self.frame.max(other));
        let (before, after) = (&self.frames[older], &self.frames[newer]);
        let marks = diff::changed_regions(&after.image, &before.image);
        // Without a selection, show the changes with some room around them.
        let area = match (self.selection.filter(|s| !s.is_empty()), marks.first()) {
            (Some(sel), _) => sel,
            (None, Some(first)) => marks
                .iter()
                .fold(*first, |all, r| all.union(r))
                .inflate(40.0 * self.scale),
            (None, None) => self.monitor_of(self.pointer),
        }
        .round()
        .intersect(&self.screen().bounds());
        let label = |ms: u32| match ms {
            0 => "now".to_string(),
            ms => format!("{:.1} s ago", ms as f32 / 1000.0),
        };
        let captions = (
            format!("before  {}", label(before.age_ms)),
            format!("after  {}", label(after.age_ms)),
        );
        let image = crate::compare::side_by_side(
            &before.image,
            &after.image,
            area,
            &marks,
            (&captions.0, &captions.1),
            &self.fonts,
            self.scale,
        );
        if image.width == 0 {
            return;
        }
        fx.push(Effect::Copy(image));
        fx.push(Effect::Close);
    }

    /// Steps through the captured frames; negative is back in time.
    fn rewind(&mut self, steps: i32) -> bool {
        let last = self.frames.len() as i32 - 1;
        let next = (self.frame as i32 + steps).clamp(0, last) as usize;
        let moved = next != self.frame;
        self.frame = next;
        if moved {
            // Another moment, another picture: what is under the pointer changed.
            self.hover = Hover::Unknown;
            self.update_hover();
        }
        moved
    }

    /// Selects what changed on screen: the difference between the frame in
    /// view and the other end of the recording, nearest the pointer.
    fn select_change(&mut self) -> bool {
        let last = self.frames.len() - 1;
        if last == 0 {
            return false;
        }
        let reference = if self.frame == last { 0 } else { last };
        let regions = diff::changed_regions(self.screen(), &self.frames[reference].image);
        let p = self.pointer;
        let distance = |r: &Rect| {
            let dx = (r.x - p.x).max(p.x - r.right()).max(0.0);
            let dy = (r.y - p.y).max(p.y - r.bottom()).max(0.0);
            dx.hypot(dy)
        };
        let Some(best) = regions.into_iter().min_by(|a, b| {
            distance(a)
                .total_cmp(&distance(b))
                .then((b.w * b.h).total_cmp(&(a.w * a.h)))
        }) else {
            return false;
        };
        self.selection = Some(best);
        self.selected = None;
        self.drag = Drag::None;
        true
    }

    fn set_tool(&mut self, tool: Tool) {
        self.tool = tool;
        self.selected = None;
    }

    fn set_stroke(&mut self, stroke: f32) {
        self.stroke = stroke.clamp(theme::STROKE_MIN, theme::STROKE_MAX);
    }

    fn undo(&mut self) {
        self.selected = None;
        self.doc.undo();
    }

    fn redo(&mut self) {
        self.selected = None;
        self.doc.redo();
    }

    fn annotation(&self, shape: Shape) -> Annotation {
        Annotation {
            shape,
            color: self.color,
            stroke: self.physical_stroke(),
        }
    }

    fn commit_text(&mut self) {
        let Some(edit) = self.text.take() else {
            return;
        };
        if edit.text.trim().is_empty() {
            return;
        }
        let size = self.text_size();
        let extent = self.fonts.measure(Face::Ui, &edit.text, size);
        let item = self.annotation(Shape::Text {
            at: edit.at,
            text: edit.text,
            size,
            extent,
        });
        self.doc.edit(|v| v.push(item));
    }

    fn nudge(&mut self, dx: f32, dy: f32, mods: Mods) {
        let step = if mods.shift { 10.0 } else { 1.0 };
        let (dx, dy) = (dx * step, dy * step);
        if let Some(index) = self.selected {
            self.doc.edit(|v| {
                if let Some(item) = v.get_mut(index) {
                    item.translate(dx, dy);
                }
            });
            return;
        }
        let Some(sel) = self.selection else {
            // Nothing to nudge yet: left and right move through time instead.
            if dx != 0.0 {
                self.rewind(dx.signum() as i32);
            }
            return;
        };
        let bounds = self.screen().bounds();
        self.selection = Some(if mods.ctrl {
            let grown = Rect::new(sel.x, sel.y, (sel.w + dx).max(1.0), (sel.h + dy).max(1.0));
            grown.intersect(&bounds)
        } else {
            sel.translate(dx, dy).clamp_inside(&bounds)
        });
    }

    fn handle_at(&self, p: Point, sel: Rect) -> Option<Handle> {
        let tol = self.hit_tolerance();
        let near_x = |x: f32| (p.x - x).abs() <= tol;
        let near_y = |y: f32| (p.y - y).abs() <= tol;
        let (l, r, t, b) = (
            near_x(sel.x),
            near_x(sel.right()),
            near_y(sel.y),
            near_y(sel.bottom()),
        );
        let in_x = p.x >= sel.x - tol && p.x <= sel.right() + tol;
        let in_y = p.y >= sel.y - tol && p.y <= sel.bottom() + tol;
        match (l, r, t, b) {
            (true, _, true, _) => Some(Handle::NW),
            (_, true, true, _) => Some(Handle::NE),
            (true, _, _, true) => Some(Handle::SW),
            (_, true, _, true) => Some(Handle::SE),
            (true, ..) if in_y => Some(Handle::W),
            (_, true, ..) if in_y => Some(Handle::E),
            (_, _, true, _) if in_x => Some(Handle::N),
            (_, _, _, true) if in_x => Some(Handle::S),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NONE: Mods = Mods {
        shift: false,
        ctrl: false,
        alt: false,
    };
    const CTRL: Mods = Mods {
        shift: false,
        ctrl: true,
        alt: false,
    };
    const SHIFT: Mods = Mods {
        shift: true,
        ctrl: false,
        alt: false,
    };

    fn pt(x: f32, y: f32) -> Point {
        Point::new(x, y)
    }

    /// An 800×600 white screen with one window and one monitor.
    fn session() -> Session {
        let screen = Image::filled(800, 600, [255, 255, 255, 255]);
        Session::new(
            vec![Frame::now(screen)],
            Rc::new(Fonts::empty()),
            vec![Rect::new(100.0, 100.0, 300.0, 200.0)],
            vec![Rect::new(0.0, 0.0, 800.0, 600.0)],
            Options::default(),
        )
    }

    fn drag(s: &mut Session, from: Point, to: Point) -> Vec<Effect> {
        let mut fx = s.handle(Input::PointerDown(from));
        fx.extend(s.handle(Input::PointerMove(to)));
        fx.extend(s.handle(Input::PointerUp(to)));
        fx
    }

    fn key(s: &mut Session, c: char) -> Vec<Effect> {
        s.handle(Input::Key(Key::Char(c), NONE))
    }

    /// A session with a 400×300 selection at (200, 150).
    fn selected() -> Session {
        let mut s = session();
        drag(&mut s, pt(200.0, 150.0), pt(600.0, 450.0));
        s
    }

    #[test]
    fn drag_creates_selection() {
        let s = selected();
        assert_eq!(s.selection(), Some(Rect::new(200.0, 150.0, 400.0, 300.0)));
        assert!(!s.selecting());
        assert!(s.strip().is_some());
    }

    #[test]
    fn tiny_drag_outside_windows_selects_nothing() {
        let mut s = session();
        drag(&mut s, pt(600.0, 500.0), pt(602.0, 501.0));
        assert_eq!(s.selection(), None);
    }

    #[test]
    fn click_selects_hovered_window() {
        let mut s = session();
        s.handle(Input::PointerMove(pt(150.0, 150.0)));
        assert_eq!(
            s.hover_target(),
            Some(Rect::new(100.0, 100.0, 300.0, 200.0))
        );
        drag(&mut s, pt(150.0, 150.0), pt(150.0, 150.0));
        assert_eq!(s.selection(), Some(Rect::new(100.0, 100.0, 300.0, 200.0)));
    }

    #[test]
    fn selection_is_clipped_to_screen() {
        let mut s = session();
        drag(&mut s, pt(700.0, 500.0), pt(900.0, 700.0));
        assert_eq!(s.selection(), Some(Rect::new(700.0, 500.0, 100.0, 100.0)));
    }

    #[test]
    fn f_selects_monitor_under_pointer() {
        let mut s = session();
        key(&mut s, 'f');
        assert_eq!(s.selection(), Some(Rect::new(0.0, 0.0, 800.0, 600.0)));
    }

    #[test]
    fn arrows_nudge_and_clamp() {
        let mut s = selected();
        s.handle(Input::Key(Key::Left, NONE));
        assert_eq!(s.selection().unwrap().x, 199.0);
        s.handle(Input::Key(Key::Down, SHIFT));
        assert_eq!(s.selection().unwrap().y, 160.0);
        for _ in 0..100 {
            s.handle(Input::Key(Key::Right, SHIFT));
        }
        assert_eq!(
            s.selection().unwrap(),
            Rect::new(400.0, 160.0, 400.0, 300.0)
        );
    }

    #[test]
    fn ctrl_arrows_resize() {
        let mut s = selected();
        s.handle(Input::Key(Key::Right, CTRL));
        s.handle(Input::Key(Key::Up, CTRL));
        assert_eq!(s.selection(), Some(Rect::new(200.0, 150.0, 401.0, 299.0)));
    }

    #[test]
    fn dragging_inside_moves_selection() {
        let mut s = selected();
        drag(&mut s, pt(400.0, 300.0), pt(350.0, 280.0));
        assert_eq!(s.selection(), Some(Rect::new(150.0, 130.0, 400.0, 300.0)));
    }

    #[test]
    fn dragging_a_corner_resizes() {
        let mut s = selected();
        drag(&mut s, pt(600.0, 450.0), pt(650.0, 500.0));
        assert_eq!(s.selection(), Some(Rect::new(200.0, 150.0, 450.0, 350.0)));
    }

    #[test]
    fn tool_keys_switch_only_with_a_selection() {
        let mut s = session();
        key(&mut s, 'a');
        assert_eq!(s.tool(), Tool::Select);
        let mut s = selected();
        key(&mut s, 'a');
        assert_eq!(s.tool(), Tool::Arrow);
    }

    #[test]
    fn drawing_adds_one_annotation_and_undo_removes_it() {
        let mut s = selected();
        key(&mut s, 'a');
        drag(&mut s, pt(250.0, 200.0), pt(350.0, 260.0));
        assert_eq!(s.doc().items().len(), 1);
        assert!(matches!(s.doc().items()[0].shape, Shape::Arrow { .. }));
        s.handle(Input::Key(Key::Char('z'), CTRL));
        assert!(s.doc().items().is_empty());
        s.handle(Input::Key(Key::Char('y'), CTRL));
        assert_eq!(s.doc().items().len(), 1);
    }

    #[test]
    fn click_with_a_draw_tool_creates_nothing() {
        let mut s = selected();
        for tool in ['a', 'l', 'r', 'e', 'd', 'h', 'b'] {
            key(&mut s, tool);
            drag(&mut s, pt(300.0, 300.0), pt(301.0, 301.0));
        }
        assert!(s.doc().items().is_empty());
        assert!(s.draft().is_none());
    }

    #[test]
    fn shift_constrains_shapes() {
        let mut s = selected();
        key(&mut s, 'r');
        s.handle(Input::Mods(SHIFT));
        drag(&mut s, pt(300.0, 200.0), pt(400.0, 240.0));
        let Shape::Rect { rect } = s.doc().items()[0].shape else {
            panic!()
        };
        assert_eq!((rect.w, rect.h), (100.0, 100.0));
    }

    #[test]
    fn enter_copies_then_closes() {
        let mut s = selected();
        let fx = s.handle(Input::Key(Key::Enter, NONE));
        assert!(
            matches!(&fx[..], [Effect::Copy(img), Effect::Close] if (img.width, img.height) == (400, 300))
        );
    }

    #[test]
    fn save_and_pin_outputs() {
        let mut s = selected();
        let fx = s.handle(Input::Key(Key::Char('s'), CTRL));
        assert!(matches!(
            &fx[..],
            [Effect::Source { .. }, Effect::Save(_), Effect::Close]
        ));
        let fx = key(&mut s, 'p');
        assert!(
            matches!(&fx[..], [Effect::Pin(_, r), Effect::Close] if *r == Rect::new(200.0, 150.0, 400.0, 300.0))
        );
    }

    #[test]
    fn outputs_need_a_selection() {
        let mut s = session();
        assert!(s.handle(Input::Key(Key::Enter, NONE)).is_empty());
        assert!(key(&mut s, 'p').is_empty());
    }

    #[test]
    fn escape_steps_back_in_order() {
        let mut s = selected();
        key(&mut s, 'l');
        drag(&mut s, pt(250.0, 200.0), pt(350.0, 200.0));
        key(&mut s, 't');
        s.handle(Input::PointerDown(pt(300.0, 300.0)));
        s.handle(Input::PointerUp(pt(300.0, 300.0)));
        s.handle(Input::Text("hi".into()));
        // 1: finishes the text
        assert_eq!(
            s.handle(Input::Key(Key::Escape, NONE)),
            vec![Effect::Redraw]
        );
        assert!(s.text_edit().is_none());
        assert_eq!(s.doc().items().len(), 2);
        // 2: back to the select tool
        s.handle(Input::Key(Key::Escape, NONE));
        assert_eq!(s.tool(), Tool::Select);
        // select the line, then 3: drop the item selection
        drag(&mut s, pt(300.0, 200.0), pt(300.0, 200.0));
        assert_eq!(s.selected(), Some(0));
        s.handle(Input::Key(Key::Escape, NONE));
        assert_eq!(s.selected(), None);
        // 4: close
        assert_eq!(s.handle(Input::Key(Key::Escape, NONE)), vec![Effect::Close]);
    }

    #[test]
    fn right_click_acts_like_escape() {
        let mut s = session();
        assert_eq!(s.handle(Input::SecondaryDown), vec![Effect::Close]);
    }

    #[test]
    fn typing_in_text_tool_does_not_switch_tools() {
        let mut s = selected();
        key(&mut s, 't');
        s.handle(Input::PointerDown(pt(300.0, 300.0)));
        s.handle(Input::PointerUp(pt(300.0, 300.0)));
        key(&mut s, 'a');
        s.handle(Input::Text("a".into()));
        key(&mut s, 'p');
        s.handle(Input::Text("p".into()));
        assert_eq!(s.tool(), Tool::Text);
        assert_eq!(s.text_edit().unwrap().text, "ap");
        s.handle(Input::Key(Key::Backspace, NONE));
        s.handle(Input::Key(Key::Backspace, NONE));
        s.handle(Input::Key(Key::Backspace, NONE));
        assert_eq!(s.text_edit().unwrap().text, "");
    }

    #[test]
    fn empty_text_commits_nothing() {
        let mut s = selected();
        key(&mut s, 't');
        drag(&mut s, pt(300.0, 300.0), pt(300.0, 300.0));
        s.handle(Input::Text("   ".into()));
        s.handle(Input::Key(Key::Enter, NONE));
        assert!(s.doc().items().is_empty());
        assert!(s.text_edit().is_none());
    }

    #[test]
    fn shift_enter_adds_a_line_and_enter_commits() {
        let mut s = selected();
        key(&mut s, 't');
        drag(&mut s, pt(300.0, 300.0), pt(300.0, 300.0));
        s.handle(Input::Text("a".into()));
        s.handle(Input::Key(Key::Enter, SHIFT));
        s.handle(Input::Text("b".into()));
        assert!(
            s.handle(Input::Key(Key::Enter, NONE))
                .iter()
                .all(|e| *e == Effect::Redraw)
        );
        let Shape::Text { text, .. } = &s.doc().items()[0].shape else {
            panic!()
        };
        assert_eq!(text, "a\nb");
    }

    #[test]
    fn steps_count_up() {
        let mut s = selected();
        key(&mut s, 'n');
        for x in [250.0, 300.0, 350.0] {
            drag(&mut s, pt(x, 200.0), pt(x, 200.0));
        }
        let numbers: Vec<u32> = s
            .doc()
            .items()
            .iter()
            .map(|a| match a.shape {
                Shape::Step { n, .. } => n,
                _ => 0,
            })
            .collect();
        assert_eq!(numbers, [1, 2, 3]);
    }

    #[test]
    fn c_without_selection_copies_pixel_colour() {
        let mut s = session();
        s.handle(Input::PointerMove(pt(10.0, 10.0)));
        assert_eq!(
            key(&mut s, 'c'),
            vec![Effect::CopyText("#FFFFFF".into()), Effect::Close]
        );
    }

    #[test]
    fn s_tightens_selection_to_content() {
        let mut screen = Image::filled(800, 600, [255, 255, 255, 255]);
        screen.fill_rect(Rect::new(300.0, 250.0, 50.0, 40.0), [0, 0, 0, 255]);
        let mut s = Session::new(
            vec![Frame::now(screen)],
            Rc::new(Fonts::empty()),
            vec![],
            vec![],
            Options::default(),
        );
        drag(&mut s, pt(200.0, 150.0), pt(600.0, 450.0));
        key(&mut s, 's');
        assert_eq!(s.selection(), Some(Rect::new(300.0, 250.0, 50.0, 40.0)));
    }

    #[test]
    fn moving_an_item_is_one_undo_step() {
        let mut s = selected();
        key(&mut s, 'l');
        drag(&mut s, pt(250.0, 200.0), pt(350.0, 200.0));
        key(&mut s, 'v');
        s.handle(Input::PointerDown(pt(300.0, 200.0)));
        s.handle(Input::PointerMove(pt(300.0, 220.0)));
        s.handle(Input::PointerMove(pt(300.0, 240.0)));
        s.handle(Input::PointerUp(pt(300.0, 240.0)));
        let Shape::Line { from, .. } = s.doc().items()[0].shape else {
            panic!()
        };
        assert_eq!(from, pt(250.0, 240.0));
        s.handle(Input::Key(Key::Char('z'), CTRL));
        let Shape::Line { from, .. } = s.doc().items()[0].shape else {
            panic!()
        };
        assert_eq!(from, pt(250.0, 200.0));
    }

    #[test]
    fn delete_removes_selected_item_and_is_a_noop_otherwise() {
        let mut s = selected();
        s.handle(Input::Key(Key::Delete, NONE));
        assert!(
            s.selection().is_some(),
            "delete never removes the selection"
        );
        key(&mut s, 'l');
        drag(&mut s, pt(250.0, 200.0), pt(350.0, 200.0));
        key(&mut s, 'v');
        drag(&mut s, pt(300.0, 200.0), pt(300.0, 200.0));
        s.handle(Input::Key(Key::Delete, NONE));
        assert!(s.doc().items().is_empty());
    }

    #[test]
    fn colour_and_stroke_keys() {
        let mut s = selected();
        key(&mut s, '4');
        assert_eq!(s.color(), 3);
        key(&mut s, ']');
        assert_eq!(s.stroke(), 4.0);
        for _ in 0..40 {
            s.handle(Input::Wheel(-1.0));
        }
        assert_eq!(s.stroke(), theme::STROKE_MIN);
    }

    #[test]
    fn strip_click_switches_tool_without_drawing() {
        let mut s = selected();
        let layout = s.strip().unwrap();
        let (_, cell) = layout
            .cells
            .iter()
            .find(|(i, _)| *i == StripItem::Tool(Tool::Rect))
            .unwrap();
        drag(&mut s, cell.center(), cell.center());
        assert_eq!(s.tool(), Tool::Rect);
        assert!(s.doc().items().is_empty());
    }

    #[test]
    fn undo_with_empty_history_is_harmless() {
        let mut s = selected();
        s.handle(Input::Key(Key::Char('z'), CTRL));
        s.handle(Input::Key(Key::Char('y'), CTRL));
        assert!(s.doc().items().is_empty());
        assert!(s.selection().is_some());
    }
    /// Three moments: blank, a box appears at (300, 200), the box is gone again.
    fn recorded() -> Session {
        let blank = Image::filled(800, 600, [255, 255, 255, 255]);
        let mut boxed = blank.clone();
        boxed.fill_rect(Rect::new(300.0, 200.0, 120.0, 60.0), [20, 20, 20, 255]);
        let frame = |image: &Image, age_ms| Frame {
            image: Arc::new(image.clone()),
            age_ms,
        };
        Session::new(
            vec![frame(&blank, 2000), frame(&boxed, 1000), frame(&blank, 0)],
            Rc::new(Fonts::empty()),
            vec![],
            vec![],
            Options::default(),
        )
    }

    #[test]
    fn a_session_opens_on_the_present() {
        let s = recorded();
        assert_eq!(
            (s.frame_index(), s.frame_count(), s.frame_age_ms()),
            (2, 3, 0)
        );
    }

    #[test]
    fn comma_and_dot_move_through_time_and_stop_at_the_ends() {
        let mut s = recorded();
        key(&mut s, ',');
        assert_eq!((s.frame_index(), s.frame_age_ms()), (1, 1000));
        assert_eq!(s.screen().pixel(310, 210), [20, 20, 20, 255]);
        key(&mut s, ',');
        key(&mut s, ',');
        assert_eq!(s.frame_index(), 0);
        for _ in 0..5 {
            key(&mut s, '.');
        }
        assert_eq!(s.frame_index(), 2);
    }

    #[test]
    fn arrows_and_page_keys_rewind_on_any_keyboard_layout() {
        let mut s = recorded();
        s.handle(Input::Key(Key::Left, NONE));
        assert_eq!(
            s.frame_index(),
            1,
            "left goes back before anything is selected"
        );
        s.handle(Input::Key(Key::Right, NONE));
        assert_eq!(s.frame_index(), 2);
        s.handle(Input::Key(Key::Up, NONE));
        assert_eq!(s.frame_index(), 2, "up and down do not rewind");
        drag(&mut s, pt(100.0, 100.0), pt(300.0, 300.0));
        s.handle(Input::Key(Key::Left, NONE));
        assert_eq!(s.frame_index(), 2, "with a selection, arrows nudge it");
        assert_eq!(s.selection().unwrap().x, 99.0);
        s.handle(Input::Key(Key::PageUp, NONE));
        assert_eq!(s.frame_index(), 1, "page keys rewind at any time");
        s.handle(Input::Key(Key::PageDown, NONE));
        assert_eq!(s.frame_index(), 2);
    }

    #[test]
    fn wheel_rewinds_before_selecting_and_sets_stroke_after() {
        let mut s = recorded();
        s.handle(Input::Wheel(1.0));
        assert_eq!(s.frame_index(), 1, "wheel up goes back in time");
        s.handle(Input::Wheel(-1.0));
        assert_eq!(s.frame_index(), 2);
        drag(&mut s, pt(100.0, 100.0), pt(300.0, 300.0));
        s.handle(Input::Wheel(1.0));
        assert_eq!(s.frame_index(), 2);
        assert_eq!(s.stroke(), 4.0);
    }

    #[test]
    fn output_comes_from_the_frame_in_view() {
        let mut s = recorded();
        key(&mut s, ',');
        drag(&mut s, pt(300.0, 200.0), pt(420.0, 260.0));
        let fx = s.handle(Input::Key(Key::Enter, NONE));
        let [Effect::Copy(image), Effect::Close] = &fx[..] else {
            panic!("{fx:?}")
        };
        assert_eq!(
            image.pixel(10, 10),
            [20, 20, 20, 255],
            "the box from one second ago"
        );
    }

    #[test]
    fn w_selects_what_vanished_when_rewound() {
        let mut s = recorded();
        key(&mut s, ',');
        key(&mut s, 'w');
        assert_eq!(s.selection(), Some(Rect::new(300.0, 200.0, 120.0, 60.0)));
        assert!(!s.selecting());
    }

    #[test]
    fn w_selects_what_appeared_and_prefers_the_change_near_the_pointer() {
        let blank = Image::filled(800, 600, [255, 255, 255, 255]);
        let mut now = blank.clone();
        now.fill_rect(Rect::new(50.0, 50.0, 200.0, 150.0), [0, 0, 0, 255]);
        now.fill_rect(Rect::new(600.0, 400.0, 80.0, 40.0), [0, 0, 0, 255]);
        let mut s = Session::new(
            vec![
                Frame {
                    image: Arc::new(blank),
                    age_ms: 1500,
                },
                Frame::now(now),
            ],
            Rc::new(Fonts::empty()),
            vec![],
            vec![],
            Options::default(),
        );
        s.handle(Input::PointerMove(pt(620.0, 380.0)));
        key(&mut s, 'w');
        assert_eq!(s.selection(), Some(Rect::new(600.0, 400.0, 80.0, 40.0)));
    }

    #[test]
    fn w_does_nothing_without_a_recording_or_a_change() {
        let mut s = session();
        key(&mut s, 'w');
        assert_eq!(s.selection(), None);
        assert_eq!(s.notice(), Some("nothing changed"));
        s.handle(Input::PointerMove(pt(5.0, 5.0)));
        assert!(s.notice().is_some(), "moving the mouse keeps the notice");
        s.handle(Input::Key(Key::Up, NONE));
        assert_eq!(s.notice(), None, "the next action clears it");
        let blank = Image::filled(100, 100, [9, 9, 9, 255]);
        let mut s = Session::new(
            vec![
                Frame {
                    image: Arc::new(blank.clone()),
                    age_ms: 900,
                },
                Frame::now(blank),
            ],
            Rc::new(Fonts::empty()),
            vec![],
            vec![],
            Options::default(),
        );
        key(&mut s, 'w');
        assert_eq!(s.selection(), None);
    }

    #[test]
    fn frames_of_another_resolution_are_dropped() {
        let old = Frame {
            image: Arc::new(Image::filled(10, 10, [0, 0, 0, 255])),
            age_ms: 500,
        };
        let s = Session::new(
            vec![old, Frame::now(Image::filled(20, 20, [0, 0, 0, 255]))],
            Rc::new(Fonts::empty()),
            vec![],
            vec![],
            Options::default(),
        );
        assert_eq!(s.frame_count(), 1);
    }
    fn word(text: &str, x: f32, y: f32, line: u32) -> Word {
        Word {
            text: text.into(),
            rect: Rect::new(x, y, text.len() as f32 * 8.0, 14.0),
            line,
        }
    }

    #[test]
    fn x_asks_for_the_text_of_the_selection_and_closes() {
        let mut s = selected();
        let fx = key(&mut s, 'x');
        assert!(
            matches!(&fx[..], [Effect::ExtractText(img), Effect::Close] if (img.width, img.height) == (400, 300))
        );
        assert!(key(&mut session(), 'x').is_empty(), "needs a selection");
    }

    #[test]
    fn m_asks_for_words_then_masks_the_sensitive_ones_as_one_undo_step() {
        let mut s = selected();
        let fx = key(&mut s, 'm');
        let Some(Effect::FindWords(image, origin)) = fx.last() else {
            panic!("{fx:?}")
        };
        assert_eq!(
            (image.width, image.height, *origin),
            (400, 300, pt(200.0, 150.0))
        );
        let fx = s.handle(Input::Words(vec![
            word("Mail:", 220.0, 200.0, 0),
            word("jane@example.com", 270.0, 200.0, 0),
            word("call", 220.0, 240.0, 1),
            word("+90-532-123-4567", 260.0, 240.0, 1),
        ]));
        assert_eq!(fx, vec![Effect::Redraw]);
        assert_eq!(s.doc().items().len(), 2);
        assert!(
            s.doc()
                .items()
                .iter()
                .all(|a| matches!(a.shape, Shape::Blur { .. }))
        );
        assert_eq!(s.notice(), Some("2 masked"));
        s.handle(Input::Key(Key::Char('z'), CTRL));
        assert!(s.doc().items().is_empty(), "one undo removes every mask");
    }

    #[test]
    fn m_with_nothing_sensitive_says_so_and_changes_nothing() {
        let mut s = selected();
        key(&mut s, 'm');
        s.handle(Input::Words(vec![word("hello", 220.0, 200.0, 0)]));
        assert!(s.doc().items().is_empty());
        assert_eq!(s.notice(), Some("nothing to mask"));
        s.handle(Input::Words(vec![]));
        assert_eq!(s.notice(), Some("nothing to mask"));
    }

    #[test]
    fn focus_tool_adds_a_focus_area_and_the_output_is_misted_around_it() {
        let mut screen = Image::filled(800, 600, [255, 255, 255, 255]);
        for x in (0..800).step_by(2) {
            screen.fill_rect(Rect::new(x as f32, 0.0, 1.0, 600.0), [0, 0, 0, 255]);
        }
        let mut s = Session::new(
            vec![Frame::now(screen)],
            Rc::new(Fonts::empty()),
            vec![],
            vec![],
            Options::default(),
        );
        drag(&mut s, pt(200.0, 150.0), pt(600.0, 450.0));
        key(&mut s, 'o');
        assert_eq!(s.tool(), Tool::Focus);
        drag(&mut s, pt(300.0, 200.0), pt(400.0, 300.0));
        assert!(matches!(s.doc().items()[0].shape, Shape::Focus { .. }));
        let fx = s.handle(Input::Key(Key::Enter, NONE));
        let [Effect::Copy(image), Effect::Close] = &fx[..] else {
            panic!("{fx:?}")
        };
        assert_eq!(
            image.pixel(150, 100),
            [0, 0, 0, 255],
            "inside the focus area stays sharp"
        );
        assert_ne!(
            image.pixel(20, 20),
            image.pixel(21, 20).map(|v| 255 - v),
            "outside the stripes are blurred away"
        );
        assert_eq!(image.pixel(20, 20), image.pixel(21, 20));
    }
    /// A white screen with a dark box at (300, 200) inside a large window.
    fn boxed() -> Session {
        let mut screen = Image::filled(800, 600, [255, 255, 255, 255]);
        screen.fill_rect(Rect::new(300.0, 200.0, 120.0, 60.0), [20, 20, 20, 255]);
        Session::new(
            vec![Frame::now(screen)],
            Rc::new(Fonts::empty()),
            vec![Rect::new(50.0, 50.0, 700.0, 500.0)],
            vec![Rect::new(0.0, 0.0, 800.0, 600.0)],
            Options::default(),
        )
    }

    const BOX: Rect = Rect::new(300.0, 200.0, 120.0, 60.0);

    #[test]
    fn pointing_at_a_box_targets_the_box_and_a_click_selects_it() {
        let mut s = boxed();
        s.handle(Input::PointerMove(pt(340.0, 230.0)));
        assert_eq!(s.hover_target(), Some(BOX));
        drag(&mut s, pt(340.0, 230.0), pt(340.0, 230.0));
        assert_eq!(s.selection(), Some(BOX));
    }

    #[test]
    fn pointing_at_plain_background_targets_the_window() {
        let mut s = boxed();
        s.handle(Input::PointerMove(pt(100.0, 100.0)));
        assert_eq!(s.hover_target(), Some(Rect::new(50.0, 50.0, 700.0, 500.0)));
        s.handle(Input::PointerMove(pt(20.0, 20.0)));
        assert_eq!(
            s.hover_target(),
            None,
            "outside every window, on background"
        );
    }

    #[test]
    fn g_switches_snapping_off_and_on() {
        let mut s = boxed();
        assert!(s.snap());
        key(&mut s, 'g');
        assert!(!s.snap());
        assert_eq!(s.notice(), Some("snap off"));
        s.handle(Input::PointerMove(pt(340.0, 230.0)));
        assert_eq!(
            s.hover_target(),
            Some(Rect::new(50.0, 50.0, 700.0, 500.0)),
            "boxes are ignored"
        );
        key(&mut s, 'g');
        assert_eq!(s.notice(), Some("snap on"));
        assert_eq!(s.hover_target(), Some(BOX));
    }

    #[test]
    fn a_rough_drag_around_a_box_snaps_to_it_exactly() {
        let mut s = boxed();
        drag(&mut s, pt(295.0, 196.0), pt(425.0, 264.0));
        assert_eq!(s.selection(), Some(BOX));
    }

    #[test]
    fn alt_or_snap_off_keeps_the_drag_exactly_where_it_was_drawn() {
        let rough = Rect::new(295.0, 196.0, 130.0, 68.0);
        let mut s = boxed();
        s.handle(Input::Mods(Mods { alt: true, ..NONE }));
        drag(&mut s, pt(295.0, 196.0), pt(425.0, 264.0));
        assert_eq!(s.selection(), Some(rough));

        let mut s = boxed();
        key(&mut s, 'g');
        drag(&mut s, pt(295.0, 196.0), pt(425.0, 264.0));
        assert_eq!(s.selection(), Some(rough));
    }

    #[test]
    fn resizing_snaps_the_edge_being_dragged() {
        let mut s = boxed();
        drag(&mut s, pt(250.0, 190.0), pt(500.0, 270.0));
        assert_eq!(s.selection(), Some(Rect::new(250.0, 190.0, 250.0, 80.0)));
        // Drag the right edge to just short of the box's right side (x = 420).
        drag(&mut s, pt(500.0, 230.0), pt(417.0, 230.0));
        assert_eq!(s.selection(), Some(Rect::new(250.0, 190.0, 170.0, 80.0)));
        // And the bottom edge to near the box's bottom (y = 260).
        drag(&mut s, pt(330.0, 270.0), pt(330.0, 263.0));
        assert_eq!(s.selection(), Some(Rect::new(250.0, 190.0, 170.0, 70.0)));
    }

    #[test]
    fn a_short_edge_does_not_pull_a_long_selection_side() {
        let mut s = boxed();
        drag(&mut s, pt(250.0, 100.0), pt(500.0, 500.0));
        // The box's 60 px side is only a fraction of this 400 px selection side.
        drag(&mut s, pt(500.0, 300.0), pt(417.0, 300.0));
        assert_eq!(s.selection().unwrap().right(), 417.0);
    }

    #[test]
    fn f1_shows_the_keys_and_the_next_key_only_closes_them() {
        let mut s = selected();
        s.handle(Input::Key(Key::Help, NONE));
        assert!(s.help());
        let fx = key(&mut s, 'a');
        assert_eq!(fx, vec![Effect::Redraw]);
        assert!(!s.help());
        assert_eq!(
            s.tool(),
            Tool::Select,
            "the key that closed the list did nothing else"
        );
        key(&mut s, '?');
        assert!(s.help());
        s.handle(Input::PointerDown(pt(400.0, 300.0)));
        assert!(!s.help());
    }

    #[test]
    fn strip_has_undo_and_redo_for_the_mouse() {
        let mut s = selected();
        key(&mut s, 'l');
        drag(&mut s, pt(250.0, 200.0), pt(350.0, 200.0));
        let cell = |s: &Session, item| {
            let layout = s.strip().unwrap();
            layout
                .cells
                .iter()
                .find(|(i, _)| *i == item)
                .unwrap()
                .1
                .center()
        };
        let undo = cell(&s, StripItem::Undo);
        drag(&mut s, undo, undo);
        assert!(s.doc().items().is_empty());
        let redo = cell(&s, StripItem::Redo);
        drag(&mut s, redo, redo);
        assert_eq!(s.doc().items().len(), 1);
    }

    #[test]
    fn hovering_the_strip_reports_the_item_for_its_tooltip() {
        let mut s = selected();
        let layout = s.strip().unwrap();
        let (item, cell) = layout.cells[1];
        assert_eq!(
            s.handle(Input::PointerMove(cell.center())),
            vec![Effect::Redraw]
        );
        assert_eq!(s.strip_hover(), Some(item));
        assert!(
            s.handle(Input::PointerMove(cell.center())).is_empty(),
            "no redraw while it stays"
        );
        s.handle(Input::PointerMove(pt(10.0, 10.0)));
        assert_eq!(s.strip_hover(), None);
    }

    #[test]
    fn prefs_report_what_was_set() {
        let mut s = selected();
        key(&mut s, '3');
        key(&mut s, ']');
        key(&mut s, 'g');
        assert_eq!(
            s.prefs(),
            Prefs {
                color: 2,
                stroke: 4.0,
                snap: false,
                framed: false,
                last: Some(Rect::new(200.0, 150.0, 400.0, 300.0)),
            }
        );
    }

    #[test]
    fn measure_tool_draws_a_ruler() {
        let mut s = selected();
        key(&mut s, 'q');
        assert_eq!(s.tool(), Tool::Measure);
        drag(&mut s, pt(250.0, 200.0), pt(370.0, 200.0));
        assert_eq!(
            s.doc().items()[0].shape,
            Shape::Measure {
                from: pt(250.0, 200.0),
                to: pt(370.0, 200.0)
            }
        );
    }

    #[test]
    fn double_click_on_text_puts_it_back_in_the_editor() {
        let mut s = selected();
        key(&mut s, 't');
        drag(&mut s, pt(300.0, 300.0), pt(300.0, 300.0));
        s.handle(Input::Text("first".into()));
        s.handle(Input::Key(Key::Enter, NONE));
        key(&mut s, 'v');
        let Shape::Text { at, extent, .. } = s.doc().items()[0].shape.clone() else {
            panic!()
        };
        let inside = pt(at.x + extent.0 / 2.0, at.y + extent.1 / 2.0);
        assert_eq!(s.handle(Input::DoubleClick(inside)), vec![Effect::Redraw]);
        assert!(s.doc().items().is_empty());
        assert_eq!(s.text_edit().unwrap().text, "first");
        assert_eq!(s.tool(), Tool::Text);
        s.handle(Input::Text(" second".into()));
        s.handle(Input::Key(Key::Enter, NONE));
        let Shape::Text { text, .. } = &s.doc().items()[0].shape else {
            panic!()
        };
        assert_eq!(text, "first second");
    }

    #[test]
    fn double_click_inside_the_selection_copies_and_elsewhere_does_nothing() {
        let mut s = selected();
        let fx = s.handle(Input::DoubleClick(pt(400.0, 300.0)));
        assert!(matches!(&fx[..], [Effect::Copy(_), Effect::Close]));
        let mut s = selected();
        assert!(
            s.handle(Input::DoubleClick(pt(50.0, 50.0))).is_empty(),
            "outside the selection"
        );
        key(&mut s, 'a');
        assert!(
            s.handle(Input::DoubleClick(pt(400.0, 300.0))).is_empty(),
            "not while a drawing tool is active"
        );
        assert!(
            session()
                .handle(Input::DoubleClick(pt(400.0, 300.0)))
                .is_empty(),
            "nothing selected"
        );
    }

    #[test]
    fn c_copies_the_colour_as_hex_rgb_or_hsl() {
        let code = |mods: Mods| match &session().handle(Input::Key(Key::Char('c'), mods))[..] {
            [Effect::CopyText(text), Effect::Close] => text.clone(),
            other => panic!("{other:?}"),
        };
        assert_eq!(code(NONE), "#FFFFFF");
        assert_eq!(code(SHIFT), "rgb(255, 255, 255)");
        assert_eq!(code(Mods { alt: true, ..NONE }), "hsl(0, 0%, 100%)");
    }

    #[test]
    fn z_selects_the_region_used_last_time() {
        let last = Rect::new(120.0, 80.0, 300.0, 200.0);
        let options = Options {
            last: Some(last),
            ..Options::default()
        };
        let screen = Image::filled(800, 600, [255, 255, 255, 255]);
        let mut s = Session::new(
            vec![Frame::now(screen)],
            Rc::new(Fonts::empty()),
            vec![],
            vec![],
            options,
        );
        key(&mut s, 'z');
        assert_eq!(s.selection(), Some(last));
        assert_eq!(s.prefs().last, Some(last));
        let mut fresh = session();
        key(&mut fresh, 'z');
        assert_eq!(fresh.selection(), None);
        assert_eq!(fresh.notice(), Some("no earlier region"));
    }

    #[test]
    fn u_sets_the_capture_in_a_frame() {
        let mut s = selected();
        key(&mut s, 'u');
        assert!(s.framed());
        assert_eq!(s.notice(), Some("frame on"));
        let fx = s.handle(Input::Key(Key::Enter, NONE));
        let [Effect::Copy(image), Effect::Close] = &fx[..] else {
            panic!("{fx:?}")
        };
        assert_eq!((image.width, image.height), (400 + 56, 300 + 56));
        assert!(s.prefs().framed);
    }

    #[test]
    fn k_asks_for_a_gif_of_the_selection_and_plays_it_once_given() {
        let mut s = selected();
        let fx = key(&mut s, 'k');
        assert_eq!(
            fx,
            [
                Effect::StartClip(Rect::new(200.0, 150.0, 400.0, 300.0)),
                Effect::Redraw
            ]
        );
        s.enter_clip(4000, 30);
        assert_eq!(s.clip_time(), Some(0));
        s.handle(Input::Tick(1500));
        assert_eq!(s.clip_time(), Some(1500));
        s.handle(Input::Tick(3000));
        assert_eq!(s.clip_time(), Some(500), "it loops");
        // Space pauses.
        key(&mut s, ' ');
        s.handle(Input::Tick(100));
        assert_eq!(s.clip_time(), Some(500));
    }

    #[test]
    fn the_bar_trims_the_gif_and_enter_writes_it_with_the_drawings() {
        let mut s = selected();
        key(&mut s, 'k');
        s.enter_clip(10_000, 30);
        let bar = s.clip_bar().unwrap();
        let y = bar.track.center().y;
        // Pull the start to 2 s and the end to 6 s.
        s.handle(Input::PointerDown(pt(bar.x_of(0), y)));
        s.handle(Input::PointerMove(pt(bar.x_of(2000), y)));
        s.handle(Input::PointerUp(pt(bar.x_of(2000), y)));
        s.handle(Input::PointerDown(pt(bar.x_of(10_000), y)));
        s.handle(Input::PointerMove(pt(bar.x_of(6000), y)));
        s.handle(Input::PointerUp(pt(bar.x_of(6000), y)));
        let view = s.clip().unwrap().clone();
        assert!((1950..=2050).contains(&view.from), "{view:?}");
        assert!((5950..=6050).contains(&view.to), "{view:?}");
        assert_eq!(view.at, view.from, "it plays the part left from its start");
        // An arrow, drawn over the GIF.
        key(&mut s, 'a');
        drag(&mut s, pt(250.0, 200.0), pt(350.0, 260.0));
        let fx = s.handle(Input::Key(Key::Enter, NONE));
        let [
            Effect::MakeGif {
                area,
                from,
                to,
                items,
                save: false,
            },
            Effect::Close,
        ] = &fx[..]
        else {
            panic!("{fx:?}")
        };
        assert_eq!(*area, Rect::new(200.0, 150.0, 400.0, 300.0));
        assert_eq!((*from, *to), (view.from, view.to));
        assert_eq!(items.len(), 1);
        assert!(items[0].bounds().x < 100.0, "in the area's own coordinates");
    }

    #[test]
    fn escape_leaves_the_gif_for_the_still_picture() {
        let mut s = selected();
        key(&mut s, 'k');
        s.enter_clip(3000, 30);
        s.handle(Input::Key(Key::Escape, NONE));
        assert!(s.clip().is_none());
        assert!(s.selection().is_some());
    }

    #[test]
    fn while_a_gif_is_made_the_area_stays_put() {
        let mut s = selected();
        key(&mut s, 'k');
        s.enter_clip(3000, 30);
        drag(&mut s, pt(300.0, 300.0), pt(500.0, 500.0));
        key(&mut s, 'f');
        assert_eq!(s.selection(), Some(Rect::new(200.0, 150.0, 400.0, 300.0)));
    }

    #[test]
    fn tab_steps_out_from_the_box_to_the_parts_of_the_window_and_the_window() {
        // A window with a box in it and a part the system knows about (the
        // page) around it.
        let mut screen = Image::filled(800, 600, [255, 255, 255, 255]);
        screen.fill_rect(Rect::new(100.0, 100.0, 600.0, 450.0), [40, 40, 40, 255]);
        screen.fill_rect(Rect::new(200.0, 250.0, 120.0, 60.0), [90, 160, 230, 255]);
        let window = Rect::new(80.0, 60.0, 640.0, 520.0);
        let page = Rect::new(100.0, 100.0, 600.0, 470.0);
        let mut s = Session::new(
            vec![Frame::now(screen)],
            Rc::new(Fonts::empty()),
            vec![window],
            vec![Rect::new(0.0, 0.0, 800.0, 600.0)],
            Options::default(),
        );
        s.set_panes(vec![(window, page)]);
        s.handle(Input::PointerMove(pt(250.0, 280.0)));
        let button = Rect::new(200.0, 250.0, 120.0, 60.0);
        assert_eq!(s.hover_target(), Some(button));
        s.handle(Input::Key(Key::Tab, NONE));
        assert_eq!(s.hover_target(), Some(page));
        s.handle(Input::Key(Key::Tab, NONE));
        assert_eq!(s.hover_target(), Some(window));
        s.handle(Input::Key(Key::Tab, NONE));
        assert_eq!(s.hover_target(), Some(window), "it stops at the window");
        s.handle(Input::Key(Key::Tab, SHIFT));
        assert_eq!(s.hover_target(), Some(page));
        // Moving to another box starts again from the smallest.
        s.handle(Input::PointerMove(pt(600.0, 520.0)));
        s.handle(Input::PointerMove(pt(260.0, 270.0)));
        assert_eq!(s.hover_target(), Some(button));
        // A click selects what is offered.
        s.handle(Input::Key(Key::Tab, NONE));
        drag(&mut s, pt(260.0, 270.0), pt(260.0, 270.0));
        assert_eq!(s.selection(), Some(page));
    }

    #[test]
    fn the_page_of_a_full_screen_browser_is_offered_before_the_window() {
        // A maximised browser: the page is most of the window but not all of
        // it, and a layer the size of the whole window is not a part.
        let screen = Image::filled(800, 600, [32, 32, 32, 255]);
        let window = Rect::new(0.0, 0.0, 800.0, 560.0);
        let page = Rect::new(20.0, 40.0, 780.0, 520.0);
        let mut s = Session::new(
            vec![Frame::now(screen)],
            Rc::new(Fonts::empty()),
            vec![window],
            vec![Rect::new(0.0, 0.0, 800.0, 600.0)],
            Options::default(),
        );
        s.set_panes(vec![(window, page), (window, window)]);
        s.handle(Input::PointerMove(pt(400.0, 300.0)));
        assert_eq!(s.hover_target(), Some(page));
        s.handle(Input::Key(Key::Tab, NONE));
        assert_eq!(s.hover_target(), Some(window));
    }

    #[test]
    fn parts_of_other_windows_are_not_offered() {
        let screen = Image::filled(800, 600, [255, 255, 255, 255]);
        let front = Rect::new(0.0, 0.0, 400.0, 300.0);
        let back = Rect::new(0.0, 0.0, 800.0, 600.0);
        let mut s = Session::new(
            vec![Frame::now(screen)],
            Rc::new(Fonts::empty()),
            vec![front, back],
            vec![back],
            Options::default(),
        );
        s.set_panes(vec![(back, Rect::new(50.0, 50.0, 200.0, 150.0))]);
        s.handle(Input::PointerMove(pt(100.0, 100.0)));
        s.handle(Input::Key(Key::Tab, NONE));
        assert_eq!(s.hover_target(), Some(front));
    }

    fn quick() -> Session {
        let screen = Image::filled(800, 600, [255, 255, 255, 255]);
        Session::new(
            vec![Frame::now(screen)],
            Rc::new(Fonts::empty()),
            vec![Rect::new(100.0, 100.0, 300.0, 200.0)],
            vec![Rect::new(0.0, 0.0, 800.0, 600.0)],
            Options {
                quick: true,
                ..Options::default()
            },
        )
    }

    #[test]
    fn quickly_an_area_is_copied_the_moment_it_is_chosen() {
        let mut s = quick();
        let fx = drag(&mut s, pt(200.0, 150.0), pt(600.0, 450.0));
        assert!(
            matches!(&fx[..], [.., Effect::Copy(img), Effect::Close, _] | [.., Effect::Copy(img), Effect::Close] if img.width == 400),
            "{fx:?}"
        );
        // A click on a window copies the window.
        let mut s = quick();
        s.handle(Input::PointerMove(pt(150.0, 150.0)));
        let fx = drag(&mut s, pt(150.0, 150.0), pt(150.0, 150.0));
        assert!(fx.contains(&Effect::Close), "{fx:?}");
        // So does the whole monitor.
        let mut s = quick();
        assert!(key(&mut s, 'f').contains(&Effect::Close));
    }

    #[test]
    fn quickly_shift_or_a_tool_first_keeps_it_open_to_draw() {
        let mut s = quick();
        s.handle(Input::Mods(SHIFT));
        let fx = drag(&mut s, pt(200.0, 150.0), pt(600.0, 450.0));
        assert!(!fx.contains(&Effect::Close));
        assert!(s.selection().is_some());

        let mut s = quick();
        key(&mut s, 'a');
        assert!(s.notice().is_some(), "it says what to do next");
        let fx = drag(&mut s, pt(200.0, 150.0), pt(600.0, 450.0));
        assert!(!fx.contains(&Effect::Close));
        assert_eq!(s.tool(), Tool::Arrow);
        // Once open, choosing again does not copy.
        assert!(!s.quick());
    }

    #[test]
    fn quickly_k_first_makes_a_gif_of_the_area_chosen() {
        let mut s = quick();
        key(&mut s, 'k');
        let fx = drag(&mut s, pt(200.0, 150.0), pt(600.0, 450.0));
        assert!(
            fx.contains(&Effect::StartClip(Rect::new(200.0, 150.0, 400.0, 300.0))),
            "{fx:?}"
        );
        assert!(!fx.contains(&Effect::Close));
        // Escape before choosing forgets the request.
        let mut s = quick();
        key(&mut s, 'k');
        s.handle(Input::Key(Key::Escape, NONE));
        let fx = drag(&mut s, pt(200.0, 150.0), pt(600.0, 450.0));
        assert!(fx.contains(&Effect::Close), "copied as usual");
    }

    #[test]
    fn j_copies_before_and_after_side_by_side() {
        let mut s = recorded();
        key(&mut s, ',');
        let fx = key(&mut s, 'j');
        let [Effect::Copy(image), Effect::Close] = &fx[..] else {
            panic!("{fx:?}")
        };
        // The change is 120 x 60; with 40 px of room on every side, twice, plus the gap and the caption bar.
        assert_eq!((image.width, image.height), (2 * 200 + 12, 140 + 30));
        assert_eq!(
            key(&mut session(), 'j'),
            vec![Effect::Redraw],
            "nothing recorded"
        );
    }

    #[test]
    fn y_asks_for_a_scrolling_capture_of_the_selection() {
        let mut s = selected();
        assert_eq!(
            key(&mut s, 'y'),
            vec![
                Effect::Scroll(Rect::new(200.0, 150.0, 400.0, 300.0)),
                Effect::Close
            ]
        );
        assert!(key(&mut session(), 'y').is_empty());
    }

    #[test]
    fn saving_sends_what_is_needed_to_edit_again_and_a_guide_for_steps() {
        let mut s = selected();
        key(&mut s, 'n');
        drag(&mut s, pt(250.0, 200.0), pt(250.0, 200.0));
        drag(&mut s, pt(300.0, 200.0), pt(300.0, 200.0));
        let fx = s.handle(Input::Key(Key::Char('s'), CTRL));
        let [
            Effect::Source { original, items },
            Effect::Guide(2),
            Effect::Save(_),
            Effect::Close,
        ] = &fx[..]
        else {
            panic!("{fx:?}")
        };
        assert_eq!((original.width, original.height), (400, 300));
        assert_eq!(
            items[0].shape,
            Shape::Step {
                at: pt(50.0, 50.0),
                n: 1
            },
            "relative to the capture"
        );
        let mut plain = selected();
        let fx = plain.handle(Input::Key(Key::Char('s'), CTRL));
        assert!(
            matches!(
                &fx[..],
                [Effect::Source { .. }, Effect::Save(_), Effect::Close]
            ),
            "no guide without steps"
        );
        assert!(matches!(
            &plain.handle(Input::Key(Key::Enter, NONE))[..],
            [Effect::Copy(_), Effect::Close]
        ));
    }

    #[test]
    fn load_starts_from_an_earlier_capture() {
        let mut s = session();
        let item = Annotation {
            shape: Shape::Step {
                at: pt(10.0, 10.0),
                n: 1,
            },
            color: 0,
            stroke: 3.0,
        };
        s.load(Rect::new(100.0, 100.0, 200.0, 100.0), vec![item.clone()]);
        assert_eq!(s.selection(), Some(Rect::new(100.0, 100.0, 200.0, 100.0)));
        assert_eq!(s.doc().items(), [item]);
        assert!(!s.selecting());
    }
}
