//! A window over one whole monitor that shows the screen in mist and
//! something on it: the first-run intro, or the settings.

use std::num::NonZeroU32;
use std::rc::Rc;
use std::time::{Duration, Instant};

use bruma_core::backdrop::Backdrop;
use bruma_core::intro;
use bruma_core::session::Key;
use bruma_core::settings::{Effect, Form, Input, Values};
use bruma_core::text::Fonts;
use bruma_core::tiny_skia::Pixmap;
use bruma_core::{Point, Rect};
use softbuffer::{Context, Surface};
use winit::dpi::{PhysicalPosition, PhysicalSize};
use winit::event::{ElementState, KeyEvent, MouseButton, WindowEvent};
use winit::event_loop::ActiveEventLoop;
use winit::keyboard::{Key as WinitKey, KeyCode, ModifiersState, NamedKey, PhysicalKey};
use winit::monitor::MonitorHandle;
use winit::platform::windows::WindowAttributesExtWindows;
use winit::window::{Window, WindowId, WindowLevel};

use crate::overlay::{blit, window_handle};
use crate::platform;

pub enum Scene {
    Intro { started: Instant },
    Settings(Form),
}

/// What the stage tells the application.
pub enum StageEvent {
    None,
    /// The intro has played to the end.
    Done,
    Settings(Vec<Effect>),
}

/// Time between two frames of the intro.
const FRAME: Duration = Duration::from_millis(16);

pub struct Stage {
    window: Rc<Window>,
    surface: Surface<Rc<Window>, Rc<Window>>,
    _context: Context<Rc<Window>>,
    pixmap: Pixmap,
    backdrop: Backdrop,
    scale: f32,
    pub scene: Scene,
    mods: ModifiersState,
    cursor: Point,
    next_frame: Instant,
}

/// The monitor the pointer is on, or the main one.
pub fn pointer_monitor(event_loop: &ActiveEventLoop) -> Option<MonitorHandle> {
    let (x, y) = platform::cursor_position()?;
    event_loop
        .available_monitors()
        .find(|m| {
            let (p, s) = (m.position(), m.size());
            x >= p.x && y >= p.y && x < p.x + s.width as i32 && y < p.y + s.height as i32
        })
        .or_else(|| event_loop.primary_monitor())
}

impl Stage {
    pub fn intro(event_loop: &ActiveEventLoop, monitor: MonitorHandle) -> Result<Self, String> {
        Self::open(event_loop, monitor, |_, _| Scene::Intro {
            started: Instant::now(),
        })
    }

    pub fn settings(
        event_loop: &ActiveEventLoop,
        monitor: MonitorHandle,
        values: Values,
        fonts: Rc<Fonts>,
    ) -> Result<Self, String> {
        Self::open(event_loop, monitor, |area, scale| {
            Scene::Settings(Form::new(
                values,
                fonts,
                area,
                scale,
                env!("CARGO_PKG_VERSION"),
            ))
        })
    }

    fn open(
        event_loop: &ActiveEventLoop,
        monitor: MonitorHandle,
        scene: impl FnOnce(Rect, f32) -> Scene,
    ) -> Result<Self, String> {
        let (at, size) = (monitor.position(), monitor.size());
        let scale = monitor.scale_factor() as f32;
        let capture = platform::capture()?;
        let screen = capture.image.crop(Rect::new(
            (at.x - capture.origin.0) as f32,
            (at.y - capture.origin.1) as f32,
            size.width as f32,
            size.height as f32,
        ));
        let (w, h) = (screen.width, screen.height);
        let backdrop = Backdrop::new(&screen, scale).ok_or("there is no screen to show")?;
        let attributes = Window::default_attributes()
            .with_title("bruma")
            .with_decorations(false)
            .with_resizable(false)
            .with_visible(false)
            .with_window_level(WindowLevel::AlwaysOnTop)
            .with_position(PhysicalPosition::new(at.x, at.y))
            .with_inner_size(PhysicalSize::new(w, h))
            .with_skip_taskbar(true)
            // Owned by a window never shown: never on the taskbar, so
            // showing it does not wait for the taskbar (seconds, on a busy
            // machine).
            .with_owner_window(platform::owner_window());
        let window = Rc::new(
            event_loop
                .create_window(attributes)
                .map_err(|e| e.to_string())?,
        );
        let context = Context::new(window.clone()).map_err(|e| e.to_string())?;
        let mut surface = Surface::new(&context, window.clone()).map_err(|e| e.to_string())?;
        let (nw, nh) = (
            NonZeroU32::new(w).ok_or("empty screen")?,
            NonZeroU32::new(h).ok_or("empty screen")?,
        );
        surface.resize(nw, nh).map_err(|e| e.to_string())?;
        let pixmap = Pixmap::new(w, h).ok_or("could not allocate the frame")?;
        let mut stage = Self {
            window,
            surface,
            _context: context,
            pixmap,
            backdrop,
            scale,
            scene: scene(Rect::new(0.0, 0.0, w as f32, h as f32), scale),
            mods: ModifiersState::default(),
            cursor: Point::new(-1.0, -1.0),
            next_frame: Instant::now(),
        };
        // The first picture goes up before the window takes the keyboard,
        // so that nothing flashes.
        stage.render();
        if let Some(handle) = window_handle(&stage.window) {
            platform::show_without_activating(handle);
        }
        stage.present();
        stage.window.set_visible(true);
        stage.window.focus_window();
        Ok(stage)
    }

    pub fn id(&self) -> WindowId {
        self.window.id()
    }

    /// The window's Win32 handle, to own dialogs opened over it.
    pub fn handle(&self) -> Option<isize> {
        window_handle(&self.window)
    }

    pub fn form(&mut self) -> Option<&mut Form> {
        match &mut self.scene {
            Scene::Settings(form) => Some(form),
            Scene::Intro { .. } => None,
        }
    }

    pub fn redraw(&self) {
        self.window.request_redraw();
    }

    /// While the intro plays: asks for the next frame when it is due and
    /// says when to look again. `None` when nothing moves.
    pub fn tick(&mut self) -> Option<Instant> {
        if !matches!(self.scene, Scene::Intro { .. }) {
            return None;
        }
        let now = Instant::now();
        if now >= self.next_frame {
            self.window.request_redraw();
            self.next_frame = now + FRAME;
        }
        Some(self.next_frame)
    }

    fn render(&mut self) {
        match &self.scene {
            Scene::Intro { started } => {
                let t = started.elapsed().as_secs_f32();
                intro::draw(&mut self.pixmap, &self.backdrop, self.scale, t);
            }
            Scene::Settings(form) => form.draw(&mut self.pixmap, &self.backdrop),
        }
    }

    fn present(&mut self) {
        if let Ok(mut frame) = self.surface.buffer_mut() {
            blit(&self.pixmap, &mut frame);
            let _ = frame.present();
        }
    }

    /// A key or a click during the intro goes straight to its end.
    fn skip(&mut self) {
        if let Scene::Intro { started } = &mut self.scene {
            let skipped = Duration::from_secs_f32(intro::OUTRO);
            if started.elapsed() < skipped {
                *started = Instant::now() - skipped;
            }
        }
    }

    fn tell_form(&mut self, input: Input) -> StageEvent {
        let Scene::Settings(form) = &mut self.scene else {
            return StageEvent::None;
        };
        let effects = form.handle(input);
        if effects.contains(&Effect::Redraw) {
            self.window.request_redraw();
        }
        if effects.is_empty() {
            StageEvent::None
        } else {
            StageEvent::Settings(effects)
        }
    }

    pub fn on_event(&mut self, event: WindowEvent) -> StageEvent {
        let intro = matches!(self.scene, Scene::Intro { .. });
        match event {
            WindowEvent::RedrawRequested => {
                if let Scene::Intro { started } = &self.scene {
                    if started.elapsed().as_secs_f32() >= intro::LENGTH {
                        return StageEvent::Done;
                    }
                }
                self.render();
                self.present();
                StageEvent::None
            }
            WindowEvent::CloseRequested if intro => StageEvent::Done,
            WindowEvent::CloseRequested => StageEvent::Settings(vec![Effect::Close]),
            WindowEvent::ModifiersChanged(m) => {
                self.mods = m.state();
                StageEvent::None
            }
            WindowEvent::CursorMoved { position, .. } => {
                self.cursor = Point::new(position.x as f32, position.y as f32);
                self.tell_form(Input::Move(self.cursor))
            }
            WindowEvent::MouseInput {
                state: ElementState::Pressed,
                button,
                ..
            } => {
                if intro {
                    self.skip();
                    return StageEvent::None;
                }
                match button {
                    MouseButton::Left => self.tell_form(Input::Press(self.cursor)),
                    MouseButton::Right => self.tell_form(Input::Key(Key::Escape)),
                    _ => StageEvent::None,
                }
            }
            WindowEvent::KeyboardInput { event, .. } => {
                if intro {
                    if event.state == ElementState::Pressed {
                        self.skip();
                    }
                    return StageEvent::None;
                }
                let listening = matches!(&self.scene, Scene::Settings(f) if f.recording());
                match if listening {
                    combination(&event, self.mods)
                } else {
                    settings_key(&event, self.mods)
                } {
                    Some(input) => self.tell_form(input),
                    None => StageEvent::None,
                }
            }
            _ => StageEvent::None,
        }
    }
}

/// A key for the settings while they are not listening for a hotkey.
fn settings_key(event: &KeyEvent, mods: ModifiersState) -> Option<Input> {
    if event.state != ElementState::Pressed {
        return None;
    }
    let key = match &event.logical_key {
        WinitKey::Named(NamedKey::Escape) => Key::Escape,
        WinitKey::Named(NamedKey::Enter) => Key::Enter,
        WinitKey::Named(NamedKey::Space) => Key::Char(' '),
        WinitKey::Named(NamedKey::ArrowUp) => Key::Up,
        WinitKey::Named(NamedKey::ArrowDown) => Key::Down,
        WinitKey::Named(NamedKey::ArrowLeft) => Key::Left,
        WinitKey::Named(NamedKey::ArrowRight) => Key::Right,
        WinitKey::Named(NamedKey::Tab) if mods.shift_key() => Key::Up,
        WinitKey::Named(NamedKey::Tab) => Key::Down,
        _ => return None,
    };
    Some(Input::Key(key))
}

/// While listening: the combination pressed, in the settings file's words.
/// Esc stops listening; modifier keys alone are not a combination.
fn combination(event: &KeyEvent, mods: ModifiersState) -> Option<Input> {
    let PhysicalKey::Code(code) = event.physical_key else {
        return None;
    };
    let modifier = matches!(
        code,
        KeyCode::ControlLeft
            | KeyCode::ControlRight
            | KeyCode::ShiftLeft
            | KeyCode::ShiftRight
            | KeyCode::AltLeft
            | KeyCode::AltRight
            | KeyCode::SuperLeft
            | KeyCode::SuperRight
    );
    if modifier {
        return None;
    }
    // Windows often reports only PrintScreen going up.
    let counts = match event.state {
        ElementState::Pressed => true,
        ElementState::Released => code == KeyCode::PrintScreen,
    };
    if !counts {
        return None;
    }
    if code == KeyCode::Escape && mods.is_empty() {
        return Some(Input::Key(Key::Escape));
    }
    Some(Input::Hotkey(hotkey_name(code, mods)))
}

/// `ctrl+shift+KeyS`, the way the settings file and the hotkey parser write
/// a combination.
fn hotkey_name(code: KeyCode, mods: ModifiersState) -> String {
    let mut parts = Vec::new();
    if mods.control_key() {
        parts.push("ctrl".to_string());
    }
    if mods.alt_key() {
        parts.push("alt".to_string());
    }
    if mods.shift_key() {
        parts.push("shift".to_string());
    }
    if mods.super_key() {
        parts.push("super".to_string());
    }
    parts.push(format!("{code:?}"));
    parts.join("+")
}

/// Why `name` should not be a hotkey, if it should not: a plain letter or
/// number would be taken away from typing.
pub fn unsuitable(name: &str) -> Option<String> {
    let parts: Vec<&str> = name.split('+').collect();
    let key = parts.last().copied().unwrap_or("");
    let has_modifier = parts
        .iter()
        .any(|p| matches!(p.to_ascii_lowercase().as_str(), "ctrl" | "alt" | "super"));
    let typing = key.starts_with("Key")
        || key.starts_with("Digit")
        || matches!(
            key,
            "Space"
                | "Enter"
                | "Tab"
                | "Backspace"
                | "Minus"
                | "Equal"
                | "Comma"
                | "Period"
                | "Slash"
        );
    (typing && !has_modifier).then(|| {
        format!(
            "{} alone would stop it from typing; add Ctrl or Alt",
            bruma_core::settings::pretty(name)
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn combinations_are_written_the_way_the_hotkey_parser_reads_them() {
        let both = ModifiersState::CONTROL | ModifiersState::SHIFT;
        let name = hotkey_name(KeyCode::KeyS, both);
        assert_eq!(name, "ctrl+shift+KeyS");
        for name in [
            name,
            hotkey_name(KeyCode::PrintScreen, ModifiersState::empty()),
            hotkey_name(KeyCode::F9, ModifiersState::ALT),
            hotkey_name(KeyCode::Digit3, ModifiersState::CONTROL),
        ] {
            assert!(
                name.parse::<global_hotkey::hotkey::HotKey>().is_ok(),
                "{name}"
            );
        }
    }

    #[test]
    fn a_typing_key_needs_ctrl_or_alt() {
        assert!(unsuitable("KeyS").is_some());
        assert!(unsuitable("shift+Digit4").is_some());
        assert!(unsuitable("ctrl+KeyS").is_none());
        assert!(unsuitable("PrintScreen").is_none());
        assert!(unsuitable("F9").is_none());
    }
}
