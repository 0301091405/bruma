//! The little message that confirms a capture: "Copied 400 × 300".
//!
//! It is a small borderless window showing a picture made by
//! `bruma_core::render::toast`: a glass pill already composed over the screen
//! it covers, so the window looks like a floating pill. It never takes focus
//! and goes away by itself.

use std::num::NonZeroU32;
use std::rc::Rc;
use std::time::{Duration, Instant};

use bruma_core::Image;
use bruma_core::tiny_skia::{IntSize, Pixmap};
use softbuffer::{Context, Surface};
use winit::dpi::{PhysicalPosition, PhysicalSize};
use winit::event::{ElementState, MouseButton, WindowEvent};
use winit::event_loop::ActiveEventLoop;
use winit::platform::windows::WindowAttributesExtWindows;
use winit::window::{Window, WindowId, WindowLevel};

use crate::overlay::blit;

/// How long a short message stays up.
pub const BRIEF: Duration = Duration::from_millis(1400);

/// A capture to pin if its preview is clicked: the image and where it goes.
pub struct PinRequest {
    pub image: Image,
    pub at: (i32, i32),
}

/// What a click on a message asks for.
pub enum ToastAction {
    None,
    Close,
    Pin(PinRequest),
}

pub struct Toast {
    window: Rc<Window>,
    surface: Surface<Rc<Window>, Rc<Window>>,
    _context: Context<Rc<Window>>,
    picture: Pixmap,
    deadline: Instant,
    /// Present on a preview card: clicking it pins the capture.
    pin: Option<PinRequest>,
}

impl Toast {
    /// Shows `image` with its top-left corner at desktop position `at`.
    pub fn show(
        event_loop: &ActiveEventLoop,
        image: Image,
        at: (i32, i32),
        lifetime: Duration,
        pin: Option<PinRequest>,
    ) -> Result<Self, String> {
        let size = PhysicalSize::new(image.width, image.height);
        let picture = IntSize::from_wh(image.width, image.height)
            .and_then(|s| Pixmap::from_vec(image.rgba, s))
            .ok_or("nothing to show")?;
        let attributes = Window::default_attributes()
            .with_title("bruma message")
            .with_decorations(false)
            .with_resizable(false)
            .with_active(false)
            .with_window_level(WindowLevel::AlwaysOnTop)
            .with_position(PhysicalPosition::new(at.0, at.1))
            .with_inner_size(size)
            .with_skip_taskbar(true);
        let window = Rc::new(
            event_loop
                .create_window(attributes)
                .map_err(|e| e.to_string())?,
        );
        let context = Context::new(window.clone()).map_err(|e| e.to_string())?;
        let surface = Surface::new(&context, window.clone()).map_err(|e| e.to_string())?;
        let mut toast = Self {
            window,
            surface,
            _context: context,
            picture,
            deadline: Instant::now() + lifetime,
            pin,
        };
        toast.draw();
        Ok(toast)
    }

    pub fn id(&self) -> WindowId {
        self.window.id()
    }

    /// When the message should disappear.
    pub fn deadline(&self) -> Instant {
        self.deadline
    }

    fn draw(&mut self) {
        let (Some(w), Some(h)) = (
            NonZeroU32::new(self.picture.width()),
            NonZeroU32::new(self.picture.height()),
        ) else {
            return;
        };
        if self.surface.resize(w, h).is_err() {
            return;
        }
        if let Ok(mut frame) = self.surface.buffer_mut() {
            blit(&self.picture, &mut frame);
            let _ = frame.present();
        }
    }

    /// A left click pins the capture a preview card shows; any other click
    /// just dismisses the message.
    pub fn on_event(&mut self, event: WindowEvent) -> ToastAction {
        match event {
            WindowEvent::RedrawRequested => self.draw(),
            WindowEvent::MouseInput {
                state: ElementState::Pressed,
                button: MouseButton::Left,
                ..
            } => {
                return match self.pin.take() {
                    Some(request) => ToastAction::Pin(request),
                    None => ToastAction::Close,
                };
            }
            WindowEvent::MouseInput {
                state: ElementState::Pressed,
                ..
            }
            | WindowEvent::CloseRequested => return ToastAction::Close,
            _ => {}
        }
        ToastAction::None
    }
}
