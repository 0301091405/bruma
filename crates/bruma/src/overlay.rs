//! The capture overlay: one borderless window over the whole virtual desktop.
//!
//! All behaviour lives in `bruma_core::session::Session`; this file only
//! translates window events into session input and presents frames.

use std::num::NonZeroU32;
use std::rc::Rc;
use std::time::{Duration, Instant};

use bruma_core::clip::{Clip, Cursor};
use bruma_core::doc::Annotation;
use bruma_core::render::Renderer;
use bruma_core::session::{Effect, Frame, Input, Key, Mods, Options, Prefs, Session};
use bruma_core::text::Fonts;
use bruma_core::tiny_skia::Pixmap;
use bruma_core::{Image, Point, Rect};
use softbuffer::{Context, Surface};
use winit::dpi::{PhysicalPosition, PhysicalSize};
use winit::event::{ElementState, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::ActiveEventLoop;
use winit::keyboard::{Key as WinitKey, ModifiersState, NamedKey};
use winit::platform::modifier_supplement::KeyEventExtModifierSupplement;
use winit::platform::windows::WindowAttributesExtWindows;
use winit::window::{CursorIcon, Window, WindowId, WindowLevel};

use crate::config::Config;
use crate::rewind::Rewind;
use crate::{note, platform, trace};

pub struct Overlay {
    window: Rc<Window>,
    surface: Surface<Rc<Window>, Rc<Window>>,
    _context: Context<Rc<Window>>,
    session: Session,
    renderer: Renderer,
    pixmap: Pixmap,
    size: PhysicalSize<u32>,
    /// Desktop coordinates of the overlay's top-left pixel.
    origin: (i32, i32),
    mods: Mods,
    cursor: Point,
    /// When and where the left button last went down, to spot a double click.
    last_press: Option<(Instant, Point)>,
    /// The recording of the selection while a GIF is being made, and where
    /// playing it has got to.
    clip: Option<(Clip, Cursor)>,
    /// The moment of the recording on screen.
    shown_at: Option<u64>,
    /// When the GIF's playback last moved on.
    ticked: Instant,
}

/// A capture just copied, kept a little while so that it can be opened
/// again to draw on.
pub struct Snapshot {
    pub screen: std::sync::Arc<Image>,
    pub origin: (i32, i32),
    pub windows: Vec<Rect>,
    pub monitors: Vec<Rect>,
    pub selection: Rect,
    pub scale: f32,
}

/// Time between two pictures of a GIF playing.
const CLIP_FRAME: Duration = Duration::from_millis(33);

/// The screen an overlay was showing, for messages that outlive it.
pub struct Scene {
    pub screen: std::sync::Arc<bruma_core::Image>,
    /// Desktop coordinates of the screen image's top-left pixel.
    pub origin: (i32, i32),
    /// The monitor the pointer was on, in image coordinates.
    pub monitor: Rect,
    pub scale: f32,
    /// What was selected, in image coordinates.
    pub selection: Option<Rect>,
}

/// The overlay's window and its buffers, without a capture in them. The
/// background process keeps one ready, hidden, so that a capture does not have
/// to wait for a window to be made.
pub struct Shell {
    window: Rc<Window>,
    surface: Surface<Rc<Window>, Rc<Window>>,
    context: Context<Rc<Window>>,
    pixmap: Pixmap,
    size: PhysicalSize<u32>,
    origin: (i32, i32),
}

impl Shell {
    /// Makes a hidden, borderless, always-on-top window at `origin`.
    pub fn create(
        event_loop: &ActiveEventLoop,
        origin: (i32, i32),
        size: PhysicalSize<u32>,
    ) -> Result<Self, String> {
        let attributes = Window::default_attributes()
            .with_title("bruma")
            .with_decorations(false)
            .with_resizable(false)
            .with_visible(false)
            .with_window_level(WindowLevel::AlwaysOnTop)
            .with_position(PhysicalPosition::new(origin.0, origin.1))
            .with_inner_size(size)
            .with_skip_taskbar(true);
        let window = Rc::new(
            event_loop
                .create_window(attributes)
                .map_err(|e| e.to_string())?,
        );
        let context = Context::new(window.clone()).map_err(|e| e.to_string())?;
        let mut surface = Surface::new(&context, window.clone()).map_err(|e| e.to_string())?;
        let (w, h) = (
            NonZeroU32::new(size.width).ok_or("empty screen")?,
            NonZeroU32::new(size.height).ok_or("empty screen")?,
        );
        surface.resize(w, h).map_err(|e| e.to_string())?;
        let pixmap = Pixmap::new(size.width, size.height).ok_or("could not allocate the frame")?;
        Ok(Self {
            window,
            surface,
            context,
            pixmap,
            size,
            origin,
        })
    }

    /// Whether this shell covers exactly the given area of the desktop.
    fn fits(&self, origin: (i32, i32), size: PhysicalSize<u32>) -> bool {
        self.origin == origin && self.size == size
    }
}

/// The Win32 handle of a window.
pub fn window_handle(window: &Window) -> Option<isize> {
    use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
    match window.window_handle().ok()?.as_raw() {
        RawWindowHandle::Win32(handle) => Some(handle.hwnd.get()),
        _ => None,
    }
}

/// Copies a finished frame into a window's buffer.
pub use bruma_core::render::present as blit;

impl Overlay {
    /// Freezes the screen and shows it. With `rewind`, the last few seconds
    /// come along as earlier frames, and with `from_recorder` the present
    /// moment is asked of the recorder too.
    pub fn open(
        event_loop: &ActiveEventLoop,
        fonts: Rc<Fonts>,
        config: &Config,
        rewind: Option<&Rewind>,
        prefs: Option<Prefs>,
        shell: Option<Shell>,
        from_recorder: bool,
    ) -> Result<Self, String> {
        let started = Instant::now();
        // The recorder, when it runs, has the screen sooner than a capture
        // made from scratch.
        let shot = rewind.filter(|_| from_recorder).and_then(Rewind::shot);
        let (image, origin) = match shot {
            Some(shot) => (shot.image, shot.origin),
            None => {
                let capture = platform::capture()?;
                (std::sync::Arc::new(capture.image), capture.origin)
            }
        };
        trace("capture", started);
        let step = Instant::now();
        let mut frames = rewind
            .map(|r| r.frames_before(&image, origin, started))
            .unwrap_or_default();

        let local = |r: Rect| r.translate(-origin.0 as f32, -origin.1 as f32);
        let (windows, panes) = platform::windows_and_panes();
        let windows = windows.into_iter().map(local).collect();
        let panes: Vec<(Rect, Rect)> = panes
            .into_iter()
            .map(|(owner, pane)| (local(owner), local(pane)))
            .collect();
        let monitors = event_loop
            .available_monitors()
            .map(|m| {
                let (p, s) = (m.position(), m.size());
                local(Rect::new(
                    p.x as f32,
                    p.y as f32,
                    s.width as f32,
                    s.height as f32,
                ))
            })
            .collect();
        let scale = event_loop
            .primary_monitor()
            .or_else(|| event_loop.available_monitors().next())
            .map(|m| m.scale_factor() as f32)
            .unwrap_or(1.0);

        trace("  history, windows, monitors", step);
        let size = PhysicalSize::new(image.width, image.height);
        note(|| {
            let ages: Vec<u32> = frames.iter().map(|f| f.age_ms).collect();
            format!(
                "history: {} earlier frames, ages in ms {ages:?}",
                frames.len()
            )
        });
        frames.push(Frame { image, age_ms: 0 });
        // Diagnostic: `BRUMA_DUMP=<folder>` writes every frame of the recording.
        if let Some(dir) = std::env::var_os("BRUMA_DUMP") {
            for (i, frame) in frames.iter().enumerate() {
                let name = format!("frame-{i}-{}ms.png", frame.age_ms);
                let _ = crate::write_png(&frame.image, &std::path::Path::new(&dir).join(name));
            }
        }
        // What was set during the last capture wins over the settings file.
        let mut session = Session::new(
            frames,
            fonts,
            windows,
            monitors,
            Self::options(config, prefs, scale, config.quick),
        );
        session.set_panes(panes);
        // Build the dimmed screen and the glass on another core while the
        // window is being made.
        let (newest, index) = (session.present(), session.frame_index());
        let layers = std::thread::Builder::new()
            .name("bruma-layers".into())
            .spawn(move || Renderer::prepare(&newest, index, scale))
            .ok();
        Self::show(event_loop, session, origin, size, started, layers, shell)
    }

    fn options(config: &Config, prefs: Option<Prefs>, scale: f32, quick: bool) -> Options {
        Options {
            scale,
            color: prefs.map_or(config.color_index(), |p| p.color),
            stroke: prefs.map_or(config.stroke, |p| p.stroke),
            snap: prefs.map_or(config.snap, |p| p.snap),
            framed: prefs.is_some_and(|p| p.framed),
            last: prefs.and_then(|p| p.last),
            quick,
        }
    }

    /// Opens a capture just copied again, the same picture and area, to draw
    /// on it.
    pub fn reopen(
        event_loop: &ActiveEventLoop,
        fonts: Rc<Fonts>,
        config: &Config,
        prefs: Option<Prefs>,
        shell: Option<Shell>,
        snapshot: Snapshot,
    ) -> Result<Self, String> {
        let started = Instant::now();
        let size = PhysicalSize::new(snapshot.screen.width, snapshot.screen.height);
        let scale = snapshot.scale;
        let mut session = Session::new(
            vec![Frame {
                image: snapshot.screen,
                age_ms: 0,
            }],
            fonts,
            snapshot.windows,
            snapshot.monitors,
            Self::options(config, prefs, scale, false),
        );
        session.select(snapshot.selection);
        let (newest, index) = (session.present(), session.frame_index());
        let layers = std::thread::Builder::new()
            .name("bruma-layers".into())
            .spawn(move || Renderer::prepare(&newest, index, scale))
            .ok();
        Self::show(
            event_loop,
            session,
            snapshot.origin,
            size,
            started,
            layers,
            shell,
        )
    }

    /// What it takes to open this capture again, if an area is chosen.
    pub fn snapshot(&self) -> Option<Snapshot> {
        Some(Snapshot {
            screen: self.session.current(),
            origin: self.origin,
            windows: self.session.windows().to_vec(),
            monitors: self.session.monitors().to_vec(),
            selection: self.session.selection()?,
            scale: self.session.scale(),
        })
    }

    /// Starts making a GIF of the selection from `clip`.
    pub fn start_clip(&mut self, clip: Clip) {
        self.session.enter_clip(clip.length(), clip.rate());
        self.clip = Some((clip, Cursor::default()));
        self.shown_at = None;
        self.ticked = Instant::now();
        self.sync_clip();
    }

    /// A clip of `area` from the few pictures a second the rewind keeps, for
    /// when nothing finer was recorded.
    pub fn still_clip(&self, area: Rect) -> Option<Clip> {
        let frames = self.session.frames();
        if frames.len() < 2 {
            return None;
        }
        let oldest = frames[0].age_ms as u64;
        let crops: Vec<(Image, u64)> = frames
            .iter()
            .map(|f| {
                (
                    f.image.crop(area),
                    oldest - f.age_ms.min(oldest as u32) as u64,
                )
            })
            .collect();
        let pictures: Vec<(&Image, u64)> = crops.iter().map(|(i, t)| (i, *t)).collect();
        Clip::from_pictures(&pictures, oldest.max(1))
    }

    /// The recording of the GIF being made, to write it.
    pub fn take_clip(&mut self) -> Option<Clip> {
        self.clip.take().map(|(clip, _)| clip)
    }

    /// A short message on the overlay.
    pub fn say(&mut self, text: &str) {
        self.session.say(text);
        self.window.request_redraw();
    }

    /// Moves the GIF being made on; says when to come back.
    pub fn tick(&mut self) -> Option<Instant> {
        self.session.clip()?;
        let now = Instant::now();
        let ms = now.duration_since(self.ticked).as_millis().min(1000) as u32;
        if ms >= CLIP_FRAME.as_millis() as u32 {
            self.ticked = now;
            self.session.handle(Input::Tick(ms));
            self.sync_clip();
        }
        Some(self.ticked + CLIP_FRAME)
    }

    /// Puts the picture of the moment the session wants to show on screen.
    fn sync_clip(&mut self) {
        let Some(t) = self.session.clip_time() else {
            return;
        };
        if self.shown_at == Some(t) {
            return;
        }
        let Some((clip, cursor)) = self.clip.as_mut() else {
            return;
        };
        clip.seek(cursor, t);
        let picture = clip.picture(cursor);
        self.session.show_clip_frame(std::sync::Arc::new(picture));
        self.shown_at = Some(t);
        self.window.request_redraw();
    }

    /// Opens an earlier capture for editing: the picture sits in the middle of
    /// the primary monitor on a dark ground, selected, with its drawings.
    pub fn open_editor(
        event_loop: &ActiveEventLoop,
        fonts: Rc<Fonts>,
        config: &Config,
        original: &Image,
        items: &[Annotation],
    ) -> Result<Self, String> {
        let started = Instant::now();
        let monitor = event_loop
            .primary_monitor()
            .or_else(|| event_loop.available_monitors().next())
            .ok_or("no monitor to edit on")?;
        let (at, size, scale) = (
            monitor.position(),
            monitor.size(),
            monitor.scale_factor() as f32,
        );
        if original.width > size.width || original.height > size.height {
            return Err(format!(
                "the picture is {} x {}, larger than the screen; it cannot be edited here yet",
                original.width, original.height
            ));
        }
        let mut canvas = Image::filled(size.width, size.height, [0x17, 0x19, 0x1C, 255]);
        let (left, top) = (
            (size.width - original.width) / 2,
            (size.height - original.height) / 2,
        );
        let row = original.width as usize * 4;
        for y in 0..original.height as usize {
            let to = ((y + top as usize) * size.width as usize + left as usize) * 4;
            canvas.rgba[to..to + row].copy_from_slice(&original.rgba[y * row..(y + 1) * row]);
        }
        let area = Rect::new(
            left as f32,
            top as f32,
            original.width as f32,
            original.height as f32,
        );
        let items = items
            .iter()
            .cloned()
            .map(|mut item| {
                item.translate(area.x, area.y);
                item
            })
            .collect();
        let whole = Rect::new(0.0, 0.0, size.width as f32, size.height as f32);
        let options = Self::options(config, None, scale, false);
        let mut session = Session::new(
            vec![Frame::now(canvas)],
            fonts,
            Vec::new(),
            vec![whole],
            options,
        );
        session.load(area, items);
        Self::show(event_loop, session, (at.x, at.y), size, started, None, None)
    }

    /// Puts a session on screen in a borderless window at `origin`.
    fn show(
        event_loop: &ActiveEventLoop,
        mut session: Session,
        origin: (i32, i32),
        size: PhysicalSize<u32>,
        started: Instant,
        layers: Option<std::thread::JoinHandle<bruma_core::render::Layers>>,
        shell: Option<Shell>,
    ) -> Result<Self, String> {
        let step = Instant::now();
        // Use the window kept ready if it still covers the desktop.
        let shell = match shell {
            Some(shell) if shell.fits(origin, size) => shell,
            _ => Shell::create(event_loop, origin, size)?,
        };
        trace("  window, surface, buffers", step);

        let cursor = platform::cursor_position()
            .map(|(x, y)| Point::new((x - origin.0) as f32, (y - origin.1) as f32))
            .unwrap_or_else(|| session.pointer());
        session.place_pointer(cursor);

        let mut overlay = Self {
            window: shell.window,
            surface: shell.surface,
            _context: shell.context,
            session,
            renderer: Renderer::new(),
            pixmap: shell.pixmap,
            size,
            origin,
            mods: Mods::default(),
            cursor,
            last_press: None,
            clip: None,
            shown_at: None,
            ticked: Instant::now(),
        };
        // Draw the first picture while the window is still hidden, so that
        // once it shows there is only the copy to the screen left to do.
        let step = Instant::now();
        if let Some(layers) = layers.and_then(|handle| handle.join().ok()) {
            overlay.renderer.adopt(layers);
        }
        trace("  waiting for layers", step);
        let step = Instant::now();
        overlay
            .renderer
            .render(&overlay.session, &mut overlay.pixmap);
        trace("  render", step);
        // Put the window on screen without making it the active one, which
        // is the quick part, and copy the picture into it; taking the
        // keyboard from the window in front can wait those few milliseconds.
        let step = Instant::now();
        overlay.window.set_cursor(CursorIcon::Crosshair);
        if let Some(handle) = window_handle(&overlay.window) {
            platform::show_without_activating(handle);
        }
        trace("  show", step);
        overlay.present();
        trace("hotkey to first frame", started);
        let step = Instant::now();
        overlay.window.set_visible(true);
        overlay.window.focus_window();
        trace("  focus", step);
        // Now that there is a picture, find out what is under the pointer.
        let step = Instant::now();
        overlay.session.handle(Input::PointerMove(cursor));
        overlay.window.request_redraw();
        trace("first look under the pointer", step);
        // With the first frame on screen, prepare the mist so the first
        // selection does not stall.
        let warming = Instant::now();
        overlay.renderer.warm(&overlay.session);
        trace("mist", warming);
        Ok(overlay)
    }

    /// Hides the window and hands it back, ready for the next capture.
    pub fn into_shell(self) -> Shell {
        self.window.set_visible(false);
        Shell {
            window: self.window,
            surface: self.surface,
            context: self._context,
            pixmap: self.pixmap,
            size: self.size,
            origin: self.origin,
        }
    }

    pub fn id(&self) -> WindowId {
        self.window.id()
    }

    /// What a message shown after this overlay closes needs to know.
    pub fn context(&self) -> Scene {
        Scene {
            screen: self.session.present(),
            origin: self.origin,
            monitor: self.session.monitor_of(self.session.pointer()),
            scale: self.session.scale(),
            selection: self.session.selection(),
        }
    }

    /// Ink, stroke and snap as they were left, for the next capture.
    pub fn prefs(&self) -> Prefs {
        self.session.prefs()
    }

    /// Hands the session something that did not come from the window, such as
    /// recognised words.
    pub fn feed(&mut self, input: Input) -> Vec<Effect> {
        let effects = self.session.handle(input);
        self.sync_clip();
        effects
    }

    pub fn hide(&self) {
        self.window.set_visible(false);
    }

    pub fn request_redraw(&self) {
        self.window.request_redraw();
    }

    fn draw(&mut self) {
        let started = Instant::now();
        self.renderer.render(&self.session, &mut self.pixmap);
        trace("  render", started);
        self.present();
        trace("frame", started);
    }

    /// Copies the frame that was rendered last to the screen.
    fn present(&mut self) {
        let step = Instant::now();
        if let Ok(mut frame) = self.surface.buffer_mut() {
            blit(&self.pixmap, &mut frame);
            let _ = frame.present();
        }
        trace("  present", step);
    }

    /// Feeds one window event to the session and returns what it asks for.
    pub fn on_event(&mut self, event: WindowEvent) -> Vec<Effect> {
        let before = self.session.frame_index();
        let effects = self.translate(event);
        self.sync_clip();
        let after = self.session.frame_index();
        if after != before {
            note(|| {
                format!(
                    "rewind: frame {} of {}, {} ms ago",
                    after + 1,
                    self.session.frame_count(),
                    self.session.frame_age_ms()
                )
            });
        }
        effects
    }

    fn translate(&mut self, event: WindowEvent) -> Vec<Effect> {
        match event {
            WindowEvent::RedrawRequested => {
                self.draw();
                Vec::new()
            }
            WindowEvent::CloseRequested => vec![Effect::Close],
            WindowEvent::ScaleFactorChanged {
                mut inner_size_writer,
                ..
            } => {
                // The overlay is sized in physical pixels; never let a DPI
                // change resize it.
                let _ = inner_size_writer.request_inner_size(self.size);
                Vec::new()
            }
            WindowEvent::CursorMoved { position, .. } => {
                self.cursor = Point::new(position.x as f32, position.y as f32);
                self.session.handle(Input::PointerMove(self.cursor))
            }
            WindowEvent::MouseInput { state, button, .. } => {
                // winit's button events carry no position, and the last move
                // event may be stale; ask the message itself.
                let (x, y) = platform::message_position();
                self.cursor = Point::new((x - self.origin.0) as f32, (y - self.origin.1) as f32);
                match (button, state) {
                    (MouseButton::Left, ElementState::Pressed) => {
                        let now = Instant::now();
                        let double = self.last_press.is_some_and(|(at, place)| {
                            now.duration_since(at) < Duration::from_millis(350)
                                && place.distance(self.cursor) < 4.0
                        });
                        self.last_press = if double {
                            None
                        } else {
                            Some((now, self.cursor))
                        };
                        // A double click edits the text under it or copies the
                        // selection; where it means neither, it is a click.
                        let handled = if double {
                            self.session.handle(Input::DoubleClick(self.cursor))
                        } else {
                            Vec::new()
                        };
                        if handled.is_empty() {
                            self.session.handle(Input::PointerDown(self.cursor))
                        } else {
                            handled
                        }
                    }
                    (MouseButton::Left, ElementState::Released) => {
                        self.session.handle(Input::PointerUp(self.cursor))
                    }
                    (MouseButton::Right, ElementState::Pressed) => {
                        self.session.handle(Input::SecondaryDown)
                    }
                    _ => Vec::new(),
                }
            }
            WindowEvent::MouseWheel { delta, .. } => {
                let steps = match delta {
                    MouseScrollDelta::LineDelta(_, y) => y,
                    MouseScrollDelta::PixelDelta(p) => p.y as f32,
                };
                self.session.handle(Input::Wheel(steps))
            }
            WindowEvent::ModifiersChanged(modifiers) => {
                self.mods = mods(modifiers.state());
                self.session.handle(Input::Mods(self.mods))
            }
            WindowEvent::KeyboardInput { event, .. } if event.state == ElementState::Pressed => {
                let mut effects = Vec::new();
                if let Some(key) = key(&event.key_without_modifiers()) {
                    effects.extend(self.session.handle(Input::Key(key, self.mods)));
                }
                if let Some(text) = &event.text {
                    if !self.mods.ctrl && !effects.contains(&Effect::Close) {
                        effects.extend(self.session.handle(Input::Text(text.to_string())));
                    }
                }
                effects
            }
            _ => Vec::new(),
        }
    }
}

fn mods(state: ModifiersState) -> Mods {
    Mods {
        shift: state.shift_key(),
        ctrl: state.control_key(),
        alt: state.alt_key(),
    }
}

fn key(key: &WinitKey) -> Option<Key> {
    Some(match key {
        WinitKey::Named(NamedKey::Escape) => Key::Escape,
        WinitKey::Named(NamedKey::Enter) => Key::Enter,
        WinitKey::Named(NamedKey::Delete) => Key::Delete,
        WinitKey::Named(NamedKey::Backspace) => Key::Backspace,
        WinitKey::Named(NamedKey::ArrowLeft) => Key::Left,
        WinitKey::Named(NamedKey::ArrowRight) => Key::Right,
        WinitKey::Named(NamedKey::ArrowUp) => Key::Up,
        WinitKey::Named(NamedKey::ArrowDown) => Key::Down,
        WinitKey::Named(NamedKey::F1) => Key::Help,
        WinitKey::Named(NamedKey::PageUp) => Key::PageUp,
        WinitKey::Named(NamedKey::PageDown) => Key::PageDown,
        WinitKey::Named(NamedKey::Tab) => Key::Tab,
        WinitKey::Character(s) => Key::Char(s.chars().next()?.to_lowercase().next()?),
        _ => return None,
    })
}
