//! Pinned screenshots: small always-on-top windows that float over your work.
//!
//! Drag to move, wheel to resize, `Ctrl+C` to copy, `Esc`, right click or
//! double click to close.

use std::num::NonZeroU32;
use std::rc::Rc;
use std::time::{Duration, Instant};

use bruma_core::Image;
use bruma_core::theme::MUTED;
use bruma_core::tiny_skia::{
    Color, FilterQuality, IntSize, Paint, Pixmap, PixmapPaint, Rect as SkRect, Transform,
};
use softbuffer::{Context, Surface};
use winit::dpi::{PhysicalPosition, PhysicalSize};
use winit::event::{ElementState, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::ActiveEventLoop;
use winit::keyboard::{Key, ModifiersState, NamedKey};
use winit::platform::modifier_supplement::KeyEventExtModifierSupplement;
use winit::platform::windows::WindowAttributesExtWindows;
use winit::window::{Window, WindowId, WindowLevel};

use crate::overlay::blit;
use crate::platform;

const DOUBLE_CLICK: Duration = Duration::from_millis(400);
const ZOOM_MIN: f32 = 0.2;
const ZOOM_MAX: f32 = 4.0;

pub struct Pin {
    window: Rc<Window>,
    surface: Surface<Rc<Window>, Rc<Window>>,
    _context: Context<Rc<Window>>,
    image: Image,
    /// The image as a pixmap, for scaled drawing.
    source: Pixmap,
    zoom: f32,
    last_press: Option<Instant>,
    modifiers: ModifiersState,
}

/// What a pin wants after an event.
#[derive(PartialEq)]
pub enum PinAction {
    None,
    Close,
}

impl Pin {
    /// Opens a pin showing `image` with its top-left at desktop position `at`.
    pub fn open(
        event_loop: &ActiveEventLoop,
        image: Image,
        at: (i32, i32),
    ) -> Result<Self, String> {
        let source = IntSize::from_wh(image.width, image.height)
            .and_then(|size| Pixmap::from_vec(image.rgba.clone(), size))
            .ok_or("nothing to pin")?;
        let attributes = Window::default_attributes()
            .with_title("bruma pin")
            .with_decorations(false)
            .with_resizable(false)
            .with_window_level(WindowLevel::AlwaysOnTop)
            .with_position(PhysicalPosition::new(at.0, at.1))
            .with_inner_size(PhysicalSize::new(image.width, image.height))
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
        let surface = Surface::new(&context, window.clone()).map_err(|e| e.to_string())?;
        let mut pin = Self {
            window,
            surface,
            _context: context,
            image,
            source,
            zoom: 1.0,
            last_press: None,
            modifiers: ModifiersState::empty(),
        };
        pin.draw();
        Ok(pin)
    }

    pub fn id(&self) -> WindowId {
        self.window.id()
    }

    fn draw(&mut self) {
        let size = self.window.inner_size();
        let (Some(w), Some(h)) = (NonZeroU32::new(size.width), NonZeroU32::new(size.height)) else {
            return;
        };
        if self.surface.resize(w, h).is_err() {
            return;
        }
        let Some(mut pixmap) = Pixmap::new(size.width, size.height) else {
            return;
        };
        let sx = size.width as f32 / self.source.width() as f32;
        let sy = size.height as f32 / self.source.height() as f32;
        let paint = PixmapPaint {
            quality: if sx == 1.0 && sy == 1.0 {
                FilterQuality::Nearest
            } else {
                FilterQuality::Bilinear
            },
            ..PixmapPaint::default()
        };
        pixmap.draw_pixmap(
            0,
            0,
            self.source.as_ref(),
            &paint,
            Transform::from_scale(sx, sy),
            None,
        );

        // A one-pixel frame so the pin reads as an object on any background.
        let mut edge = Paint::default();
        edge.set_color(Color::from_rgba8(MUTED[0], MUTED[1], MUTED[2], 255));
        edge.anti_alias = false;
        let (fw, fh) = (size.width as f32, size.height as f32);
        for (x, y, w, h) in [
            (0.0, 0.0, fw, 1.0),
            (0.0, fh - 1.0, fw, 1.0),
            (0.0, 0.0, 1.0, fh),
            (fw - 1.0, 0.0, 1.0, fh),
        ] {
            if let Some(rect) = SkRect::from_xywh(x, y, w, h) {
                pixmap.fill_rect(rect, &edge, Transform::identity(), None);
            }
        }

        if let Ok(mut frame) = self.surface.buffer_mut() {
            blit(&pixmap, &mut frame);
            let _ = frame.present();
        }
    }

    fn set_zoom(&mut self, zoom: f32) {
        self.zoom = zoom.clamp(ZOOM_MIN, ZOOM_MAX);
        let w = (self.image.width as f32 * self.zoom).round().max(16.0) as u32;
        let h = (self.image.height as f32 * self.zoom).round().max(16.0) as u32;
        let _ = self.window.request_inner_size(PhysicalSize::new(w, h));
        self.window.request_redraw();
    }

    pub fn on_event(&mut self, event: WindowEvent) -> PinAction {
        match event {
            WindowEvent::RedrawRequested | WindowEvent::Resized(_) => self.draw(),
            WindowEvent::CloseRequested => return PinAction::Close,
            WindowEvent::ModifiersChanged(m) => self.modifiers = m.state(),
            WindowEvent::MouseInput {
                state: ElementState::Pressed,
                button,
                ..
            } => match button {
                MouseButton::Left => {
                    let now = Instant::now();
                    if self
                        .last_press
                        .is_some_and(|t| now.duration_since(t) < DOUBLE_CLICK)
                    {
                        return PinAction::Close;
                    }
                    self.last_press = Some(now);
                    let _ = self.window.drag_window();
                }
                MouseButton::Right => return PinAction::Close,
                _ => {}
            },
            WindowEvent::MouseWheel { delta, .. } => {
                let steps = match delta {
                    MouseScrollDelta::LineDelta(_, y) => y,
                    MouseScrollDelta::PixelDelta(p) => (p.y as f32).signum(),
                };
                self.set_zoom(self.zoom * (1.0 + 0.1 * steps));
            }
            WindowEvent::KeyboardInput { event, .. } if event.state == ElementState::Pressed => {
                match event.key_without_modifiers() {
                    Key::Named(NamedKey::Escape) => return PinAction::Close,
                    Key::Character(c)
                        if c.eq_ignore_ascii_case("c") && self.modifiers.control_key() =>
                    {
                        if let Err(e) = platform::copy_image(&self.image) {
                            eprintln!("bruma: {e}");
                        }
                    }
                    _ => {}
                }
            }
            _ => {}
        }
        PinAction::None
    }
}
