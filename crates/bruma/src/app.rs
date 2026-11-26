//! The event loop: owns the overlay, the pins, the tray and the message
//! window, and carries out what a capture session asks for.

use std::collections::{HashMap, VecDeque};
use std::rc::Rc;
use std::time::{Duration, Instant};

use bruma_core::Image;
use bruma_core::doc::Annotation;
use bruma_core::redact;
use bruma_core::session::{Effect, Input, Prefs};
use bruma_core::text::Fonts;
use tray_icon::menu::MenuId;
use winit::application::ApplicationHandler;
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
use winit::window::WindowId;

use crate::config::Config;
use crate::overlay::{Overlay, Scene, Shell};
use crate::pin::{Pin, PinAction};
use crate::rewind::Rewind;
use crate::toast::{BRIEF, PinRequest, Toast, ToastAction};

/// How long the message of the very first start stays.
const WELCOME: Duration = Duration::from_secs(6);
use crate::tray::Tray;
use crate::{history, lab, platform, scroll, write_png_with};

#[derive(Debug)]
pub enum UserEvent {
    /// The hotkey, the tray icon, the tray menu or another process asked for
    /// a capture.
    Capture,
    Menu(MenuId),
}

/// A saved capture being edited again: where it lives, the picture as it was
/// before anything was drawn on it, and the drawings.
pub struct EditJob {
    pub path: std::path::PathBuf,
    pub original: Image,
    pub items: Vec<Annotation>,
}

/// Encodes frames, each shown for the given milliseconds, as a looping GIF.
fn encode_gif(frames: &[(Image, u32)]) -> Result<Vec<u8>, String> {
    let first = &frames.first().ok_or("nothing to make a clip from")?.0;
    let too_big = |_| "the area is too large for a GIF".to_string();
    let (w, h) = (
        u16::try_from(first.width).map_err(too_big)?,
        u16::try_from(first.height).map_err(too_big)?,
    );
    let mut out = Vec::new();
    {
        let mut encoder = gif::Encoder::new(&mut out, w, h, &[]).map_err(|e| e.to_string())?;
        encoder
            .set_repeat(gif::Repeat::Infinite)
            .map_err(|e| e.to_string())?;
        for (image, shown_ms) in frames {
            let mut rgba = image.rgba.clone();
            let mut frame = gif::Frame::from_rgba_speed(w, h, &mut rgba, 10);
            frame.delay = (*shown_ms / 10).clamp(2, u16::MAX as u32) as u16;
            encoder.write_frame(&frame).map_err(|e| e.to_string())?;
        }
    }
    Ok(out)
}

/// Writes a guide beside a saved capture: the picture and an empty numbered
/// list, one line per step marker, ready to be filled in.
fn write_guide(picture: &std::path::Path, steps: u32) {
    let name = picture
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let mut text = format!("# Steps\n\n![screenshot]({name})\n\n");
    for n in 1..=steps {
        text.push_str(&format!("{n}. \n"));
    }
    if let Err(e) = std::fs::write(picture.with_extension("md"), text) {
        eprintln!("bruma: could not write the guide: {e}");
    }
}

/// Runs the command from the settings on a saved file, in the background. If
/// the command prints a web address (an upload's answer), it is copied.
fn run_after(command: &str, file: &std::path::Path) {
    use std::os::windows::process::CommandExt;
    if command.trim().is_empty() {
        return;
    }
    let line = command.replace("{file}", &format!("\"{}\"", file.display()));
    let _ = std::thread::Builder::new()
        .name("bruma-after".into())
        .spawn(move || {
            // 0x08000000: no console window flashing up.
            let output = std::process::Command::new("cmd")
                .raw_arg(format!("/C {line}"))
                .creation_flags(0x0800_0000)
                .output();
            match output {
                Ok(output) => {
                    let said = String::from_utf8_lossy(&output.stdout);
                    let link = said
                        .lines()
                        .map(str::trim)
                        .find(|l| l.starts_with("http://") || l.starts_with("https://"));
                    if let Some(link) = link {
                        let _ = platform::copy_text(link);
                    }
                }
                Err(e) => eprintln!("bruma: run_after failed: {e}"),
            }
        });
}

/// What to tell the person once the overlay has closed.
enum Message {
    /// A line of text on a pill.
    Text(String),
    /// A line of text that stays long enough to be read at leisure.
    Hello(String),
    /// A capture: a preview card with this caption.
    Shot(String, Image),
}

struct App {
    /// `true`: live in the tray. `false`: one capture, then exit.
    background: bool,
    config: Config,
    fonts: Rc<Fonts>,
    proxy: EventLoopProxy<UserEvent>,
    overlay: Option<Overlay>,
    /// The overlay's window, kept ready between captures (background mode).
    shell: Option<Shell>,
    pins: HashMap<WindowId, Pin>,
    toast: Option<Toast>,
    tray: Option<Tray>,
    rewind: Option<Rewind>,
    /// Ink, stroke and snap as the last capture left them.
    prefs: Option<Prefs>,
    /// Set when this run is editing a saved capture instead of the screen.
    editing: Option<EditJob>,
    started: bool,
    failure: Option<String>,
}

impl App {
    fn open_overlay(&mut self, event_loop: &ActiveEventLoop) {
        if self.overlay.is_some() {
            return;
        }
        // A message from the last capture must not be part of this one. The
        // recorder reads the screen as the compositor last drew it, which may
        // still show a message closed this instant; GDI waits for the
        // compositor, so such a capture is read the slow way.
        let had_message = self.toast.take().is_some();
        // Stop recording first: the overlay must never end up in its own past.
        if let Some(rewind) = &self.rewind {
            rewind.pause();
        }
        let opened = Overlay::open(
            event_loop,
            self.fonts.clone(),
            &self.config,
            self.rewind.as_ref(),
            self.prefs,
            self.shell.take(),
            !had_message,
        );
        match opened {
            Ok(overlay) => self.overlay = Some(overlay),
            Err(e) => {
                if let Some(rewind) = &self.rewind {
                    rewind.resume();
                }
                eprintln!("bruma: {e}");
                if !self.background {
                    self.failure = Some(e);
                }
            }
        }
    }

    /// Saves a capture and returns the caption for it. With `source` the file
    /// can be edited again later; with `steps` a guide is written beside it.
    fn save(
        &self,
        image: &Image,
        source: Option<(Image, Vec<Annotation>)>,
        steps: Option<u32>,
    ) -> String {
        // An edited capture goes back where it came from.
        let path = match &self.editing {
            Some(job) => job.path.clone(),
            None => self.config.next_path(platform::local_time()),
        };
        let meta = source.and_then(|(original, items)| {
            bruma_core::export::pack_editable(&original, &items).ok()
        });
        match write_png_with(image, &path, meta.as_deref()) {
            Ok(()) => {
                println!("{}", path.display());
                if let Some(steps) = steps {
                    write_guide(&path, steps);
                }
                run_after(&self.config.run_after, &path);
                let name = path.file_name().map(|n| n.to_string_lossy().into_owned());
                format!("Saved {}", name.unwrap_or_default())
            }
            Err(e) => {
                // Never lose a capture: fall back to the clipboard.
                eprintln!("bruma: could not save ({e}); copied to the clipboard instead");
                match platform::copy_image(image) {
                    Ok(()) => "Could not save, copied instead".into(),
                    Err(e) => e,
                }
            }
        }
    }

    /// Adds a capture to history.
    fn remember(&self, image: &Image) {
        history::record(image.clone(), self.config.history, platform::local_time());
    }

    fn apply(&mut self, event_loop: &ActiveEventLoop, effects: Vec<Effect>) {
        let Some(overlay) = &self.overlay else {
            return;
        };
        if effects.contains(&Effect::Close) {
            // Get out of the way before doing any slow work.
            overlay.hide();
        }
        let scene = overlay.context();
        let mut message: Option<Message> = None;
        // Sent just ahead of a save: what makes the file editable, and its steps.
        let mut source: Option<(Image, Vec<Annotation>)> = None;
        let mut steps: Option<u32> = None;
        let mut queue: VecDeque<Effect> = effects.into();
        while let Some(effect) = queue.pop_front() {
            match effect {
                Effect::Redraw => {
                    if let Some(overlay) = &self.overlay {
                        overlay.request_redraw();
                    }
                }
                Effect::Copy(image) => {
                    let caption = match platform::copy_image(&image) {
                        Ok(()) => format!("Copied {} \u{00D7} {}", image.width, image.height),
                        Err(e) => e,
                    };
                    if self.config.autosave {
                        self.save(&image, None, None);
                    }
                    self.remember(&image);
                    message = Some(Message::Shot(caption, image));
                }
                Effect::CopyText(text) => {
                    message = Some(Message::Text(match platform::copy_text(&text) {
                        Ok(()) => format!("{text} copied"),
                        Err(e) => e,
                    }));
                }
                Effect::Source { original, items } => source = Some((original, items)),
                Effect::Guide(count) => steps = Some(count),
                Effect::SaveClip(frames) => {
                    let path = self
                        .config
                        .next_path(platform::local_time())
                        .with_extension("gif");
                    let written = encode_gif(&frames).and_then(|bytes| {
                        if let Some(dir) = path.parent() {
                            std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
                        }
                        std::fs::write(&path, bytes).map_err(|e| format!("{}: {e}", path.display()))
                    });
                    message = Some(Message::Text(match written {
                        Ok(()) => {
                            println!("{}", path.display());
                            run_after(&self.config.run_after, &path);
                            let name = path.file_name().map(|n| n.to_string_lossy().into_owned());
                            format!("Saved clip {}", name.unwrap_or_default())
                        }
                        Err(e) => e,
                    }));
                }
                Effect::Scroll(area) => {
                    message = Some(match scroll::capture(area, scene.origin) {
                        Some(image) => {
                            let caption = match platform::copy_image(&image) {
                                Ok(()) => format!(
                                    "Copied {} \u{00D7} {}, scrolled",
                                    image.width, image.height
                                ),
                                Err(e) => e,
                            };
                            if self.config.autosave {
                                self.save(&image, None, None);
                            }
                            self.remember(&image);
                            Message::Shot(caption, image)
                        }
                        None => Message::Text("Could not capture that area".into()),
                    });
                }
                Effect::Save(image) => {
                    let caption = self.save(&image, source.take(), steps.take());
                    self.remember(&image);
                    message = Some(Message::Shot(caption, image));
                }
                Effect::Pin(image, rect) => {
                    let at = (
                        scene.origin.0 + rect.x as i32,
                        scene.origin.1 + rect.y as i32,
                    );
                    self.pin(event_loop, image, at);
                }
                Effect::ExtractText(image) => {
                    let report = match platform::ocr(&image).map(|words| redact::text(&words)) {
                        Ok(text) if text.trim().is_empty() => "No text found".into(),
                        Ok(text) => match platform::copy_text(&text) {
                            Ok(()) => format!("Text copied, {} characters", text.chars().count()),
                            Err(e) => e,
                        },
                        Err(e) => e,
                    };
                    message = Some(Message::Text(report));
                }
                Effect::FindWords(image, origin) => {
                    let words = match platform::ocr(&image) {
                        Ok(words) => words
                            .into_iter()
                            .map(|mut word| {
                                word.rect = word.rect.translate(origin.x, origin.y);
                                word
                            })
                            .collect(),
                        Err(e) => {
                            eprintln!("bruma: {e}");
                            Vec::new()
                        }
                    };
                    if let Some(overlay) = self.overlay.as_mut() {
                        queue.extend(overlay.feed(Input::Words(words)));
                    }
                }
                Effect::Close => {
                    // Remember ink, stroke and snap for the next capture.
                    if let Some(overlay) = self.overlay.take() {
                        self.prefs = Some(overlay.prefs());
                        if self.background {
                            // Keep the window; the next capture opens sooner.
                            self.shell = Some(overlay.into_shell());
                        }
                    }
                    crate::flush_trace();
                    if let Some(rewind) = &self.rewind {
                        rewind.resume();
                    }
                }
            }
        }
        if self.overlay.is_none() {
            if let Some(message) = message {
                self.show_message(event_loop, &scene, message);
            }
        }
    }

    /// The first time Bruma runs, says that it is running and which key takes
    /// a capture. A program that starts into the tray otherwise looks as if
    /// nothing had happened.
    fn welcome(&mut self, event_loop: &ActiveEventLoop) {
        let Some(marker) = history::dir().and_then(|dir| Some(dir.parent()?.join("welcomed")))
        else {
            return;
        };
        if marker.exists() {
            return;
        }
        let monitor = event_loop
            .primary_monitor()
            .or_else(|| event_loop.available_monitors().next());
        let (Ok(capture), Some(monitor)) = (platform::capture(), monitor) else {
            return;
        };
        let (at, size) = (monitor.position(), monitor.size());
        let scene = Scene {
            origin: capture.origin,
            monitor: bruma_core::Rect::new(
                (at.x - capture.origin.0) as f32,
                (at.y - capture.origin.1) as f32,
                size.width as f32,
                size.height as f32,
            ),
            scale: monitor.scale_factor() as f32,
            selection: None,
            screen: std::sync::Arc::new(capture.image),
        };
        let text = match self.tray.as_ref().and_then(|tray| tray.hotkey.as_deref()) {
            Some(key) => format!("Bruma is running. Press {key} to take a screenshot."),
            None => "Bruma is running. Its hotkey is taken; see the tray menu.".to_string(),
        };
        self.show_message(event_loop, &scene, Message::Hello(text));
        if let Some(dir) = marker.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = std::fs::write(&marker, b"");
    }

    fn pin(&mut self, event_loop: &ActiveEventLoop, image: Image, at: (i32, i32)) {
        match Pin::open(event_loop, image, at) {
            Ok(pin) => {
                self.pins.insert(pin.id(), pin);
            }
            Err(e) => eprintln!("bruma: {e}"),
        }
    }

    fn show_message(&mut self, event_loop: &ActiveEventLoop, scene: &Scene, message: Message) {
        let desktop = |x: f32, y: f32| (scene.origin.0 + x as i32, scene.origin.1 + y as i32);
        let preview = Duration::from_secs_f32(self.config.preview.clamp(0.0, 60.0));
        let hello = matches!(message, Message::Hello(_));
        let (picture, rect, lifetime, pin) = match message {
            // A capture gets a card with a thumbnail; clicking it pins the
            // capture back where it was taken.
            Message::Shot(caption, image) if !preview.is_zero() => {
                let (picture, rect) = bruma_core::render::preview_card(
                    &scene.screen,
                    &image,
                    &self.fonts,
                    &caption,
                    scene.monitor,
                    scene.scale,
                );
                let at = match scene.selection {
                    Some(sel) => desktop(sel.x, sel.y),
                    None => desktop(scene.monitor.x, scene.monitor.y),
                };
                (picture, rect, preview, Some(PinRequest { image, at }))
            }
            Message::Shot(text, _) | Message::Text(text) | Message::Hello(text) => {
                let lifetime = if hello { WELCOME } else { BRIEF };
                let (picture, rect) = bruma_core::render::toast(
                    &scene.screen,
                    &self.fonts,
                    &text,
                    scene.monitor,
                    scene.scale,
                );
                (picture, rect, lifetime, None)
            }
        };
        if picture.width == 0 || picture.height == 0 {
            return;
        }
        match Toast::show(event_loop, picture, desktop(rect.x, rect.y), lifetime, pin) {
            Ok(toast) => {
                event_loop.set_control_flow(ControlFlow::WaitUntil(toast.deadline()));
                self.toast = Some(toast);
            }
            Err(e) => eprintln!("bruma: {e}"),
        }
    }

    fn exit_when_idle(&self, event_loop: &ActiveEventLoop) {
        if !self.background
            && self.overlay.is_none()
            && self.pins.is_empty()
            && self.toast.is_none()
        {
            event_loop.exit();
        }
    }
}

impl ApplicationHandler<UserEvent> for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.started {
            return;
        }
        self.started = true;
        if self.background {
            self.rewind = Rewind::start(
                self.config.rewind,
                self.config.rewind_fullscreen,
                self.config.fast_capture,
            );
            // Have the overlay's window ready before the first hotkey.
            let (origin, (width, height)) = platform::virtual_screen();
            let size = winit::dpi::PhysicalSize::new(width, height);
            self.shell = Shell::create(event_loop, origin, size).ok();
            let proxy = self.proxy.clone();
            platform::listen_for_capture_requests(move || {
                let _ = proxy.send_event(UserEvent::Capture);
            });
            let autostart = platform::autostart_enabled();
            match Tray::new(self.proxy.clone(), &self.config.hotkey, autostart) {
                Ok(tray) => {
                    self.tray = Some(tray);
                    self.welcome(event_loop);
                }
                Err(e) => {
                    self.failure = Some(format!("could not create the tray icon: {e}"));
                    event_loop.exit();
                }
            }
        } else {
            match &self.editing {
                Some(job) => {
                    let opened = Overlay::open_editor(
                        event_loop,
                        self.fonts.clone(),
                        &self.config,
                        &job.original,
                        &job.items,
                    );
                    match opened {
                        Ok(overlay) => self.overlay = Some(overlay),
                        Err(e) => self.failure = Some(e),
                    }
                }
                None => self.open_overlay(event_loop),
            }
            self.exit_when_idle(event_loop);
        }
    }

    fn user_event(&mut self, event_loop: &ActiveEventLoop, event: UserEvent) {
        match event {
            UserEvent::Capture => self.open_overlay(event_loop),
            UserEvent::Menu(id) => {
                let Some(tray) = &self.tray else { return };
                if id == tray.capture {
                    self.open_overlay(event_loop);
                } else if id == tray.folder {
                    let dir = self.config.save_dir();
                    let _ = std::fs::create_dir_all(&dir);
                    if let Err(e) = std::process::Command::new("explorer").arg(&dir).spawn() {
                        eprintln!("bruma: {e}");
                    }
                } else if id == tray.lab {
                    if let Err(e) = lab::open() {
                        eprintln!("bruma: {e}");
                    }
                } else if id == tray.later {
                    // Time to open a menu or hover something first.
                    let proxy = self.proxy.clone();
                    std::thread::spawn(move || {
                        std::thread::sleep(Duration::from_secs(3));
                        let _ = proxy.send_event(UserEvent::Capture);
                    });
                } else if id == tray.history {
                    if let Err(e) = history::open() {
                        eprintln!("bruma: {e}");
                    }
                } else if id == tray.settings {
                    if let Err(e) = Config::open_in_editor() {
                        eprintln!("bruma: {e}");
                    }
                } else if id == *tray.autostart.id() {
                    // The menu has already flipped the tick; make it true.
                    let wanted = tray.autostart.is_checked();
                    if let Err(e) = platform::set_autostart(wanted) {
                        eprintln!("bruma: {e}");
                        tray.autostart.set_checked(platform::autostart_enabled());
                    }
                } else if id == tray.quit {
                    event_loop.exit();
                }
            }
        }
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, id: WindowId, event: WindowEvent) {
        if let Some(overlay) = self.overlay.as_mut().filter(|o| o.id() == id) {
            let effects = overlay.on_event(event);
            self.apply(event_loop, effects);
        } else if let Some(pin) = self.pins.get_mut(&id) {
            if pin.on_event(event) == PinAction::Close {
                self.pins.remove(&id);
            }
        } else if let Some(toast) = self.toast.as_mut().filter(|t| t.id() == id) {
            match toast.on_event(event) {
                ToastAction::None => {}
                ToastAction::Close => self.toast = None,
                ToastAction::Pin(request) => {
                    self.toast = None;
                    self.pin(event_loop, request.image, request.at);
                }
            }
        }
        self.exit_when_idle(event_loop);
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        match &self.toast {
            Some(toast) if Instant::now() >= toast.deadline() => {
                self.toast = None;
                event_loop.set_control_flow(ControlFlow::Wait);
                self.exit_when_idle(event_loop);
            }
            Some(toast) => event_loop.set_control_flow(ControlFlow::WaitUntil(toast.deadline())),
            None => event_loop.set_control_flow(ControlFlow::Wait),
        }
    }
}

/// Runs until quit (background) or until the capture and its pins are closed.
pub fn run(background: bool, config: Config, editing: Option<EditJob>) -> Result<(), String> {
    let event_loop = EventLoop::<UserEvent>::with_user_event()
        .build()
        .map_err(|e| e.to_string())?;
    event_loop.set_control_flow(ControlFlow::Wait);
    let mut app = App {
        background,
        config,
        fonts: Rc::new(Fonts::system()),
        proxy: event_loop.create_proxy(),
        overlay: None,
        shell: None,
        pins: HashMap::new(),
        toast: None,
        tray: None,
        rewind: None,
        prefs: None,
        editing,
        started: false,
        failure: None,
    };
    event_loop.run_app(&mut app).map_err(|e| e.to_string())?;
    match app.failure {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_clip_is_a_looping_gif_with_every_frame_and_its_timing() {
        let frames: Vec<(Image, u32)> = [(200u8, 500u32), (120, 1000), (40, 1500)]
            .into_iter()
            .map(|(v, ms)| (Image::filled(32, 20, [v, v, v, 255]), ms))
            .collect();
        let bytes = encode_gif(&frames).unwrap();
        assert_eq!(&bytes[..6], b"GIF89a");
        let mut decoder = gif::DecodeOptions::new().read_info(&bytes[..]).unwrap();
        assert_eq!((decoder.width(), decoder.height()), (32, 20));
        let mut delays = Vec::new();
        while let Some(frame) = decoder.read_next_frame().unwrap() {
            delays.push(frame.delay);
        }
        assert_eq!(delays, [50, 100, 150], "hundredths of a second");
    }

    #[test]
    fn a_clip_needs_frames_and_a_size_a_gif_can_hold() {
        assert!(encode_gif(&[]).is_err());
        let huge = Image::filled(70_000, 1, [0, 0, 0, 255]);
        assert!(encode_gif(&[(huge, 100)]).is_err());
    }

    #[test]
    fn a_guide_lists_one_line_per_step_beside_the_picture() {
        let dir = std::env::temp_dir().join(format!("bruma-guide-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let picture = dir.join("shot.png");
        write_guide(&picture, 3);
        let text = std::fs::read_to_string(dir.join("shot.md")).unwrap();
        assert!(text.contains("![screenshot](shot.png)"));
        assert_eq!(text.lines().filter(|l| l.ends_with(". ")).count(), 3);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
