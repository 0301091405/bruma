//! The menu a right click on the tray icon opens: a small glass window just
//! above the icon, drawn by `bruma_core::menu`.

use std::num::NonZeroU32;
use std::rc::Rc;
use std::time::{Duration, Instant};

use bruma_core::menu::{Entry, Menu};
use bruma_core::text::Fonts;
use bruma_core::{Image, Point, Rect};
use softbuffer::{Context, Surface};
use winit::dpi::{PhysicalPosition, PhysicalSize};
use winit::event::{ElementState, MouseButton, WindowEvent};
use winit::event_loop::ActiveEventLoop;
use winit::keyboard::{Key as WinitKey, NamedKey};
use winit::platform::windows::WindowAttributesExtWindows;
use winit::window::{Window, WindowId, WindowLevel};

use crate::overlay::{blit, window_handle};
use crate::platform;

pub enum MenuAction {
    None,
    Close,
    Pick(Entry),
}

pub struct TrayMenu {
    window: Rc<Window>,
    surface: Surface<Rc<Window>, Rc<Window>>,
    _context: Context<Rc<Window>>,
    menu: Menu,
    under: Image,
    fonts: Rc<Fonts>,
    cursor: Option<Point>,
    opened: Instant,
}

/// Where a `size` menu goes for an icon at `icon` (x, y, width, height) on a
/// screen whose usable part is `work`: next to the icon, inside `work`, with
/// a small margin.
fn place(
    icon: (i32, i32, i32, i32),
    size: (i32, i32),
    work: (i32, i32, i32, i32),
    margin: i32,
) -> (i32, i32) {
    let (ax, ay) = (icon.0 + icon.2 / 2, icon.1 + icon.3 / 2);
    let (wx, wy, ww, wh) = work;
    let (mw, mh) = size;
    let clamp = |v: i32, lo: i32, hi: i32| v.max(lo).min(hi.max(lo));
    let x = clamp(ax - mw / 2, wx + margin, wx + ww - mw - margin);
    let y = if ay >= wy + wh {
        // The taskbar is at the bottom: open upwards.
        wy + wh - mh - margin
    } else if ay < wy {
        wy + margin
    } else {
        clamp(ay - mh / 2, wy + margin, wy + wh - mh - margin)
    };
    (x, y)
}

impl TrayMenu {
    pub fn open(
        event_loop: &ActiveEventLoop,
        fonts: Rc<Fonts>,
        icon: (i32, i32, i32, i32),
        hotkey: Option<String>,
        warning: Option<String>,
    ) -> Result<Self, String> {
        // An icon without a size: put the menu by the pointer instead.
        let icon = if icon.2 <= 0 || icon.3 <= 0 {
            let (x, y) = platform::cursor_position().unwrap_or((icon.0, icon.1));
            (x, y, 1, 1)
        } else {
            icon
        };
        let anchor = (icon.0 + icon.2 / 2, icon.1 + icon.3 / 2);
        let scale = event_loop
            .available_monitors()
            .find(|m| {
                let (p, s) = (m.position(), m.size());
                anchor.0 >= p.x
                    && anchor.1 >= p.y
                    && anchor.0 < p.x + s.width as i32
                    && anchor.1 < p.y + s.height as i32
            })
            .or_else(|| event_loop.primary_monitor())
            .map_or(1.0, |m| m.scale_factor() as f32);
        let menu = Menu::new(hotkey, warning, scale);
        let (mw, mh) = menu.size();
        let work = platform::work_area(anchor).ok_or("no monitor for the menu")?;
        let margin = (8.0 * scale).round() as i32;
        let (x, y) = place(icon, (mw as i32, mh as i32), work, margin);

        let capture = platform::capture()?;
        let under = capture.image.crop(Rect::new(
            (x - capture.origin.0) as f32,
            (y - capture.origin.1) as f32,
            mw as f32,
            mh as f32,
        ));
        let attributes = Window::default_attributes()
            .with_title("bruma menu")
            .with_decorations(false)
            .with_resizable(false)
            .with_visible(false)
            .with_window_level(WindowLevel::AlwaysOnTop)
            .with_position(PhysicalPosition::new(x, y))
            .with_inner_size(PhysicalSize::new(under.width, under.height))
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
        let (w, h) = (
            NonZeroU32::new(under.width).ok_or("empty menu")?,
            NonZeroU32::new(under.height).ok_or("empty menu")?,
        );
        surface.resize(w, h).map_err(|e| e.to_string())?;
        let mut tray_menu = Self {
            window,
            surface,
            _context: context,
            menu,
            under,
            fonts,
            cursor: None,
            opened: Instant::now(),
        };
        if let Some(handle) = window_handle(&tray_menu.window) {
            platform::show_without_activating(handle);
        }
        tray_menu.draw();
        tray_menu.window.set_visible(true);
        tray_menu.window.focus_window();
        Ok(tray_menu)
    }

    pub fn id(&self) -> WindowId {
        self.window.id()
    }

    /// Whether something else has been brought to the front since the menu
    /// opened, which closes a menu.
    pub fn lost(&self) -> bool {
        self.opened.elapsed() > Duration::from_millis(400)
            && window_handle(&self.window) != Some(platform::foreground_window())
    }

    fn draw(&mut self) {
        let picture = self.menu.draw(&self.under, &self.fonts);
        if let Ok(mut frame) = self.surface.buffer_mut() {
            blit(&picture, &mut frame);
            let _ = frame.present();
        }
    }

    pub fn on_event(&mut self, event: WindowEvent) -> MenuAction {
        match event {
            WindowEvent::RedrawRequested => self.draw(),
            WindowEvent::CursorMoved { position, .. } => {
                let p = Point::new(position.x as f32, position.y as f32);
                self.cursor = Some(p);
                if self.menu.point(Some(p)) {
                    self.window.request_redraw();
                }
            }
            WindowEvent::CursorLeft { .. } => {
                self.cursor = None;
                if self.menu.point(None) {
                    self.window.request_redraw();
                }
            }
            WindowEvent::MouseInput {
                state: ElementState::Released,
                button: MouseButton::Left,
                ..
            } => {
                if let Some(entry) = self.cursor.and_then(|p| self.menu.hit(p)) {
                    return MenuAction::Pick(entry);
                }
            }
            WindowEvent::KeyboardInput { event, .. } if event.state == ElementState::Pressed => {
                match event.logical_key {
                    WinitKey::Named(NamedKey::Escape) => return MenuAction::Close,
                    WinitKey::Named(NamedKey::ArrowDown) | WinitKey::Named(NamedKey::Tab) => {
                        self.menu.step(true);
                        self.window.request_redraw();
                    }
                    WinitKey::Named(NamedKey::ArrowUp) => {
                        self.menu.step(false);
                        self.window.request_redraw();
                    }
                    WinitKey::Named(NamedKey::Enter) | WinitKey::Named(NamedKey::Space) => {
                        if let Some(entry) = self.menu.lit() {
                            return MenuAction::Pick(entry);
                        }
                    }
                    _ => {}
                }
            }
            WindowEvent::Focused(false) | WindowEvent::CloseRequested => return MenuAction::Close,
            _ => {}
        }
        MenuAction::None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const WORK: (i32, i32, i32, i32) = (0, 0, 1920, 1040);

    #[test]
    fn above_a_taskbar_at_the_bottom() {
        let (x, y) = place((1800, 1045, 24, 30), (240, 300), WORK, 8);
        assert_eq!(y, 1040 - 300 - 8);
        assert!(x + 240 <= 1920 - 8, "it stays on the screen");
        assert!(x <= 1812 && 1812 <= x + 240, "it is above the icon");
    }

    #[test]
    fn below_a_taskbar_at_the_top_and_beside_one_at_the_side() {
        let top = (0, 40, 1920, 1040);
        assert_eq!(place((900, 5, 24, 30), (240, 300), top, 8).1, 48);
        let (x, y) = place((5, 600, 30, 24), (240, 300), (40, 0, 1880, 1080), 8);
        assert_eq!(x, 48);
        assert_eq!(y, 612 - 150);
    }
}
