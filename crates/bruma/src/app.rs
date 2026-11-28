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
use winit::application::ApplicationHandler;
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
use winit::window::WindowId;

use crate::config::Config;
use crate::overlay::{Overlay, Scene, Shell, Snapshot};
use crate::pin::{Pin, PinAction};
use crate::rewind::Rewind;
use crate::toast::{BRIEF, PinRequest, Toast, ToastAction};

/// How long the message of the very first start stays.
const WELCOME: Duration = Duration::from_secs(6);
use crate::stage::{Stage, StageEvent};
use crate::tray::Tray;
use crate::traymenu::{MenuAction, TrayMenu};
use crate::{history, lab, platform, scroll, write_png_with};

#[derive(Debug)]
pub enum UserEvent {
    /// The hotkey, the tray icon, the tray menu or another process asked for
    /// a capture.
    Capture,
    /// A right click on the tray icon, whose place (x, y, width, height) is
    /// given.
    TrayMenu((i32, i32, i32, i32)),
    /// Another process asked for the settings.
    Settings,
    /// A GIF has been written (or not), from this many milliseconds of
    /// recording; `true` if it was only to be saved.
    GifDone(Result<std::path::PathBuf, String>, bool, u64),
    /// Another process (setup, say) asked Bruma to quit.
    Quit,
    /// The link found in the area chosen in capture `capture`, for the card.
    CardLink {
        capture: u64,
        link: bruma_core::links::Link,
    },
    /// The links found on the frozen screen of capture `capture`, in its
    /// frame `frame`.
    Links {
        capture: u64,
        frame: usize,
        links: Vec<bruma_core::links::Link>,
    },
    /// The report for a bug, ready to be copied.
    Report(String),
    /// The task that starts Bruma as administrator was made (`Ok(true)`), or
    /// not, after Windows asked the person.
    GamesTask(Result<bool, String>),
}

/// Where the source code is.
const SOURCE: &str = "https://github.com/0301091405/bruma";

/// A saved capture being edited again: where it lives, the picture as it was
/// before anything was drawn on it, and the drawings.
pub struct EditJob {
    pub path: std::path::PathBuf,
    pub original: Image,
    pub items: Vec<Annotation>,
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
    /// The intro or the settings, over a whole monitor.
    stage: Option<Stage>,
    /// The capture just copied, while its preview card is up: a click on the
    /// card opens it again to draw on.
    again: Option<Snapshot>,
    /// GIFs being written in the background.
    gifs: usize,
    /// Counts the overlays opened, so that a late answer about an earlier
    /// one is told apart.
    capture: u64,
    /// The last finished search for links, and the overlay it was for.
    scan: Option<(u64, usize, Vec<bruma_core::links::Link>)>,
    /// The link the next preview card offers.
    card_link: Option<bruma_core::links::Link>,
    /// What the preview card showed, to show it again with a link found
    /// after it came up.
    card_redo: Option<(Scene, String, Image)>,
    /// The full-screen game the overlay opened over, to give it the
    /// keyboard back when the overlay closes.
    back_to: Option<isize>,
    /// A game run as administrator kept the keyboard (Bruma is not), so only
    /// the whole screen could be copied: say how to choose an area next time.
    games_hint: bool,
    menu: Option<TrayMenu>,
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
        // Nothing of Bruma's own may end up in the capture.
        if self.overlay.is_some() || self.stage.is_some() || self.menu.is_some() {
            return;
        }
        // A message from the last capture must not be part of this one. The
        // recorder reads the screen as the compositor last drew it, which may
        // still show a message closed this instant; GDI waits for the
        // compositor, so such a capture is read the slow way.
        let had_message = self.toast.take().is_some();
        self.again = None;
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
            Ok(overlay) => {
                self.overlay = Some(overlay);
                self.scan_for_links();
            }
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
        let mut snapshot = overlay.snapshot();
        let mut message: Option<Message> = None;
        // Whether the card may offer a link: not for a scrolled capture,
        // whose picture is not the frozen screen's.
        let mut offer_link = false;
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
                    offer_link = true;
                    let caption = match platform::copy_image(&image) {
                        Ok(()) => format!("Copied {} \u{00D7} {}", image.width, image.height),
                        Err(e) => e,
                    };
                    if self.config.autosave {
                        self.save(&image, None, None);
                    }
                    self.remember(&image);
                    self.again = snapshot.take();
                    message = Some(Message::Shot(caption, image));
                }
                Effect::CopyText(text) => {
                    let copied = platform::copy_text(&text);
                    crate::note(|| format!("copy text: {copied:?}"));
                    message = Some(Message::Text(match copied {
                        Ok(()) => format!("{text} copied"),
                        Err(e) => e,
                    }));
                }
                Effect::Source { original, items } => source = Some((original, items)),
                Effect::Guide(count) => steps = Some(count),
                Effect::StartClip(area) => {
                    let whole = bruma_core::clip::Area::new(
                        area.x as usize,
                        area.y as usize,
                        area.w as usize,
                        area.h as usize,
                    );
                    let off = self.config.clip <= 0.0;
                    let recorded = self
                        .rewind
                        .as_ref()
                        .and_then(|r| r.clip(whole, scene.origin));
                    if let Some(overlay) = self.overlay.as_mut() {
                        match recorded.or_else(|| overlay.still_clip(area)) {
                            Some(clip) => overlay.start_clip(clip),
                            None if off => overlay.say("GIFs are off; turn them on in Settings"),
                            None => overlay.say("Nothing recorded for a GIF yet"),
                        }
                    }
                }
                Effect::MakeGif {
                    area: _,
                    from,
                    to,
                    items,
                    save,
                } => {
                    if let Some(clip) = self.overlay.as_mut().and_then(Overlay::take_clip) {
                        let path = self
                            .config
                            .next_path(platform::local_time())
                            .with_extension("gif");
                        let proxy = self.proxy.clone();
                        let scale = scene.scale;
                        self.gifs += 1;
                        let spawned =
                            std::thread::Builder::new()
                                .name("bruma-gif".into())
                                .spawn(move || {
                                    // Cheap rather than quick: the band
                                    // threads go ahead of a game.
                                    bruma_core::par::keep_jobs_on_this_thread();
                                    let fonts = Fonts::system();
                                    let written = clip
                                        .gif(from, to, |picture| {
                                            if items.is_empty() {
                                                return picture;
                                            }
                                            let whole = picture.bounds();
                                            bruma_core::export::flatten_scaled(
                                                &picture, &items, whole, &fonts, scale,
                                            )
                                        })
                                        .and_then(|bytes| {
                                            if let Some(dir) = path.parent() {
                                                std::fs::create_dir_all(dir).map_err(|e| {
                                                    format!("{}: {e}", dir.display())
                                                })?;
                                            }
                                            std::fs::write(&path, bytes)
                                                .map_err(|e| format!("{}: {e}", path.display()))?;
                                            Ok(path)
                                        });
                                    let _ = proxy.send_event(UserEvent::GifDone(
                                        written,
                                        save,
                                        to - from,
                                    ));
                                });
                        if spawned.is_err() {
                            self.gifs -= 1;
                        }
                        message = Some(Message::Text("Making the GIF".into()));
                    }
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
                    offer_link = true;
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
                    self.drop_overlay();
                    // Back to the game the overlay opened over.
                    if let Some(game) = self.back_to.take() {
                        platform::give_back(game);
                    }
                }
            }
        }
        if self.overlay.is_none() {
            self.card_link = None;
            let offer = card_offers_link(offer_link, self.editing.is_some());
            if let (true, Some(Message::Shot(caption, image))) = (offer, &message) {
                self.prepare_card_link(&scene, caption, image);
            }
            if let Some(message) = message {
                self.show_message(event_loop, &scene, message);
            }
        }
    }

    /// Finds the link the card for `scene`'s chosen area offers: from the
    /// search of the whole screen if it is done, otherwise by looking at
    /// the area alone, away from this thread; the card is then shown again
    /// with it when it comes.
    fn prepare_card_link(&mut self, scene: &Scene, caption: &str, image: &Image) {
        self.card_link = None;
        self.card_redo = None;
        let Some(area) = scene.selection.filter(|_| self.config.links) else {
            return;
        };
        if let Some(links) = links_for(&self.scan, self.capture, scene.frame) {
            self.card_link = bruma_core::links::for_card(links, area).cloned();
            return;
        }
        self.card_redo = Some((scene.clone(), caption.to_string(), image.clone()));
        let crop = scene.screen.crop(area);
        let capture = self.capture;
        let proxy = self.proxy.clone();
        let _ = std::thread::Builder::new()
            .name("bruma-card-links".into())
            .spawn(move || {
                platform::run_this_thread_in_the_background();
                bruma_core::par::keep_jobs_on_this_thread();
                let words = platform::ocr(&crop).unwrap_or_default();
                let mut links = bruma_core::links::in_words(&words);
                links.extend(bruma_core::links::in_qr_codes(&crop));
                for link in &mut links {
                    link.rect = link.rect.translate(area.x, area.y);
                }
                if let Some(link) = bruma_core::links::for_card(&links, area).cloned() {
                    let _ = proxy.send_event(UserEvent::CardLink { capture, link });
                }
            });
    }

    /// The first time Bruma runs: the intro, and after it a message saying
    /// that Bruma is running and which key takes a capture. A program that
    /// starts into the tray otherwise looks as if nothing had happened.
    fn welcome(&mut self, event_loop: &ActiveEventLoop) {
        let Some(marker) = history::dir().and_then(|dir| Some(dir.parent()?.join("welcomed")))
        else {
            return;
        };
        if marker.exists() {
            return;
        }
        // Written first, so that something going wrong cannot repeat it.
        if let Some(dir) = marker.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = std::fs::write(&marker, b"");
        let monitor = event_loop
            .primary_monitor()
            .or_else(|| event_loop.available_monitors().next());
        let opened = monitor
            .ok_or_else(|| "no monitor".to_string())
            .and_then(|m| Stage::intro(event_loop, m));
        match opened {
            Ok(stage) => {
                self.pause_recording();
                self.stage = Some(stage);
            }
            Err(e) => {
                eprintln!("bruma: {e}");
                self.hello(event_loop);
            }
        }
    }

    /// The message that follows the intro.
    /// The screen now, on the monitor the pointer is on, for a message that
    /// does not come from an overlay.
    fn scene_now(&self, event_loop: &ActiveEventLoop) -> Option<Scene> {
        let monitor = crate::stage::pointer_monitor(event_loop)?;
        let capture = platform::capture().ok()?;
        let (at, size) = (monitor.position(), monitor.size());
        Some(Scene {
            frame: 0,
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
        })
    }

    /// Starts looking for links and QR codes on the overlay's frozen screen,
    /// away from the overlay's thread and behind other work: the overlay
    /// never waits for it. Off in the settings, nothing is looked at.
    fn scan_for_links(&mut self) {
        self.capture += 1;
        self.scan = None;
        let Some(overlay) = self.overlay.as_ref().filter(|_| self.config.links) else {
            return;
        };
        let (image, frame, monitors) = overlay.scan_input();
        let capture = self.capture;
        let proxy = self.proxy.clone();
        let _ = std::thread::Builder::new()
            .name("bruma-links".into())
            .spawn(move || {
                platform::run_this_thread_in_the_background();
                bruma_core::par::keep_jobs_on_this_thread();
                let started = Instant::now();
                // A monitor at a time: a whole desktop of several may be
                // wider than text recognition accepts.
                let links = bruma_core::links::in_parts(&image, &monitors, |part| {
                    platform::ocr(part).unwrap_or_default()
                });
                crate::note(|| {
                    format!(
                        "links: {} found in {:.0} ms",
                        links.len(),
                        started.elapsed().as_secs_f64() * 1000.0
                    )
                });
                let _ = proxy.send_event(UserEvent::Links {
                    capture,
                    frame,
                    links,
                });
            });
    }

    /// Closes the overlay, keeping what it learnt for the next capture.
    fn drop_overlay(&mut self) {
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

    /// With a game or a film filling the screen, copies that monitor at once
    /// and shows the card instead of opening the overlay over it. The game
    /// keeps the keyboard (Windows does not let Bruma take it from a game
    /// run as administrator), nothing has to be clicked away, and a click
    /// on the card opens the capture to draw on. `false` when the screen
    /// could not be read.
    fn capture_over_a_game(&mut self, event_loop: &ActiveEventLoop) -> bool {
        let started = Instant::now();
        // A capture of its own: a search for links made for an earlier one
        // says nothing about it.
        self.capture += 1;
        self.scan = None;
        let (screen, origin) = match self.rewind.as_ref().and_then(Rewind::shot) {
            Some(shot) => (shot.image, shot.origin),
            None => match platform::capture() {
                Ok(capture) => (std::sync::Arc::new(capture.image), capture.origin),
                Err(e) => {
                    eprintln!("bruma: {e}");
                    return false;
                }
            },
        };
        let local = |m: &winit::monitor::MonitorHandle| {
            let (at, size) = (m.position(), m.size());
            bruma_core::Rect::new(
                (at.x - origin.0) as f32,
                (at.y - origin.1) as f32,
                size.width as f32,
                size.height as f32,
            )
            .intersect(&screen.bounds())
        };
        // The monitor the game is on: the one under the middle of its window.
        let (x, y) = platform::window_centre(platform::foreground_window())
            .or_else(platform::cursor_position)
            .unwrap_or((0, 0));
        let Some(monitor) = event_loop
            .available_monitors()
            .find(|m| {
                let (p, s) = (m.position(), m.size());
                x >= p.x && y >= p.y && x < p.x + s.width as i32 && y < p.y + s.height as i32
            })
            .or_else(|| event_loop.primary_monitor())
        else {
            return false;
        };
        let area = local(&monitor);
        if area.is_empty() {
            return false;
        }
        let image = screen.crop(area);
        crate::trace("capture over a game", started);
        let caption = match platform::copy_image(&image) {
            Ok(()) => {
                // A game may not let the card show over it: this says the
                // capture was taken.
                platform::play_shutter();
                format!("Copied {} \u{00D7} {}", image.width, image.height)
            }
            Err(e) => e,
        };
        if self.config.autosave {
            self.save(&image, None, None);
        }
        self.remember(&image);
        let scale = monitor.scale_factor() as f32;
        self.again = Some(Snapshot {
            screen: screen.clone(),
            origin,
            windows: platform::windows_and_panes()
                .0
                .into_iter()
                .map(|r| r.translate(-origin.0 as f32, -origin.1 as f32))
                .collect(),
            monitors: event_loop.available_monitors().map(|m| local(&m)).collect(),
            selection: area,
            scale,
        });
        let scene = Scene {
            frame: 0,
            screen,
            origin,
            monitor: area,
            scale,
            selection: Some(area),
        };
        self.prepare_card_link(&scene, &caption, &image);
        self.show_message(event_loop, &scene, Message::Shot(caption, image));
        true
    }

    fn hello(&mut self, event_loop: &ActiveEventLoop) {
        let Some(scene) = self.scene_now(event_loop) else {
            return;
        };
        let text = match self.tray.as_ref().and_then(Tray::hotkey) {
            Some(key) => format!("Bruma is running. Press {key} to take a screenshot."),
            None => "Bruma is running. Its hotkey is taken; see Settings.".to_string(),
        };
        self.show_message(event_loop, &scene, Message::Hello(text));
    }

    fn pause_recording(&self) {
        if let Some(rewind) = &self.rewind {
            rewind.pause();
        }
    }

    fn resume_recording(&self) {
        if let Some(rewind) = &self.rewind {
            rewind.resume();
        }
    }

    /// What the settings panel shows, from the settings and the system.
    fn settings_values(&self) -> bruma_core::settings::Values {
        bruma_core::settings::Values {
            hotkey: self.config.hotkey.clone(),
            quick: self.config.quick,
            snap: self.prefs.map_or(self.config.snap, |p| p.snap),
            cursor: self.config.cursor,
            rewind: self.config.rewind,
            clip: self.config.clip,
            preview: self.config.preview,
            autosave: self.config.autosave,
            folder: self.config.save_dir().display().to_string(),
            history: self.config.history,
            autostart: platform::autostart_enabled(),
            // On only when it is so: as administrator, or with the task there.
            links: self.config.links,
            games: self.config.games
                && (platform::running_as_administrator()
                    || std::env::current_exe().is_ok_and(|exe| platform::games_task_starts(&exe))),
        }
    }

    fn open_settings(&mut self, event_loop: &ActiveEventLoop) {
        if self.overlay.is_some() || self.stage.is_some() {
            return;
        }
        self.menu = None;
        let Some(monitor) = crate::stage::pointer_monitor(event_loop) else {
            return;
        };
        let values = self.settings_values();
        match Stage::settings(event_loop, monitor, values, self.fonts.clone()) {
            Ok(stage) => {
                self.pause_recording();
                self.stage = Some(stage);
            }
            Err(e) => eprintln!("bruma: {e}"),
        }
    }

    fn close_stage(&mut self) {
        if self.stage.take().is_some() {
            // A hotkey being recorded gives its key back.
            if let Some(tray) = &mut self.tray {
                tray.resume();
            }
            self.resume_recording();
        }
    }

    /// Answers what the settings panel asks for.
    fn on_settings(
        &mut self,
        event_loop: &ActiveEventLoop,
        effects: Vec<bruma_core::settings::Effect>,
    ) {
        use bruma_core::settings::Effect;
        for effect in effects {
            match effect {
                Effect::Redraw => {}
                Effect::Record(true) => {
                    if let Some(tray) = &mut self.tray {
                        tray.pause();
                    }
                }
                Effect::Record(false) => {
                    if let Some(tray) = &mut self.tray {
                        tray.resume();
                    }
                }
                Effect::Changed => self.apply_settings(),
                Effect::PickFolder => {
                    let owner = self.stage.as_ref().and_then(Stage::handle);
                    if let Some(folder) = platform::pick_folder(owner, &self.config.save_dir()) {
                        self.config.save_dir = folder.display().to_string();
                        let saved = self.config.save().err();
                        if let Some(form) = self.stage.as_mut().and_then(Stage::form) {
                            form.values.folder = self.config.save_dir.clone();
                            form.set_message(saved);
                        }
                    }
                    if let Some(stage) = &self.stage {
                        stage.redraw();
                    }
                }
                Effect::EditFile => {
                    self.close_stage();
                    if let Err(e) = Config::open_in_editor() {
                        eprintln!("bruma: {e}");
                    }
                }
                Effect::OpenSource => {
                    self.close_stage();
                    let _ = std::process::Command::new("explorer").arg(SOURCE).spawn();
                }
                Effect::Close => self.close_stage(),
            }
        }
        let _ = event_loop;
    }

    /// Takes what the settings panel shows, applies what changed, and
    /// saves it. What cannot be applied goes back, with a word on why.
    fn apply_settings(&mut self) {
        let Some(values) = self
            .stage
            .as_mut()
            .and_then(Stage::form)
            .map(|f| f.values.clone())
        else {
            return;
        };
        let mut problem = None;
        let mut values = values;

        if values.hotkey != self.config.hotkey {
            let result = match crate::stage::unsuitable(&values.hotkey) {
                Some(why) => Err(why),
                None => match &mut self.tray {
                    Some(tray) => tray.set_hotkey(&values.hotkey),
                    None => Err("there is no tray icon".to_string()),
                },
            };
            match result {
                Ok(()) => self.config.hotkey = values.hotkey.clone(),
                Err(why) => {
                    problem = Some(why);
                    values.hotkey = self.config.hotkey.clone();
                }
            }
        }
        if values.autostart != platform::autostart_enabled() {
            if let Err(why) = platform::set_autostart(values.autostart) {
                problem = Some(why);
                values.autostart = platform::autostart_enabled();
            }
        }
        if values.cursor != self.config.cursor {
            platform::set_capture_cursor(values.cursor);
        }
        if values.games != self.config.games {
            let (games, said) = self.switch_games(values.games);
            values.games = games;
            if said.is_some() {
                problem = said;
            }
        }
        let rewind_changed = values.rewind != self.config.rewind || values.clip != self.config.clip;
        self.config.quick = values.quick;
        self.config.clip = values.clip;
        self.config.snap = values.snap;
        self.config.links = values.links;
        if let Some(prefs) = &mut self.prefs {
            prefs.snap = values.snap;
        }
        self.config.cursor = values.cursor;
        self.config.rewind = values.rewind;
        self.config.preview = values.preview;
        self.config.autosave = values.autosave;
        self.config.history = values.history;
        if rewind_changed {
            // A new recorder for the new length; paused, since the settings
            // are on the screen.
            self.rewind = None;
            self.rewind = Rewind::start(
                self.config.rewind,
                self.config.rewind_fullscreen,
                self.config.fast_capture,
                self.config.clip,
            );
            self.pause_recording();
        }
        if let Err(why) = self.config.save() {
            problem = Some(why);
        }
        if let Some(form) = self.stage.as_mut().and_then(Stage::form) {
            form.values = values;
            form.set_message(problem);
        }
        if let Some(stage) = &self.stage {
            stage.redraw();
        }
    }

    /// Turns running as administrator on or off, as far as can be done at
    /// once. Returns what the switch should show and what to tell.
    ///
    /// On, Windows first asks the person; a Bruma run as administrator then
    /// makes the task that starts it that way, and this one makes way for
    /// it (see [`App::games_task_made`]). Off, Bruma starts without the task
    /// from then on; the one running keeps its rights until it quits.
    fn switch_games(&mut self, on: bool) -> (bool, Option<String>) {
        if on {
            if platform::running_as_administrator() {
                let made = std::env::current_exe()
                    .map_err(|e| e.to_string())
                    .and_then(|exe| platform::create_games_task(&exe));
                return match made {
                    Ok(()) => {
                        self.config.games = true;
                        (true, None)
                    }
                    Err(why) => (false, Some(why)),
                };
            }
            // The task is there already (made by setup): no need to ask.
            if std::env::current_exe().is_ok_and(|exe| platform::games_task_starts(&exe)) {
                let _ = self.proxy.send_event(UserEvent::GamesTask(Ok(true)));
                return (true, None);
            }
            let proxy = self.proxy.clone();
            let asked = std::thread::Builder::new()
                .name("bruma-games".into())
                .spawn(move || {
                    // Who it is for: Windows may ask for another account.
                    let me = std::env::var("USERNAME").unwrap_or_default();
                    let made = platform::run_as_administrator(&format!("games on \"{me}\""));
                    let _ = proxy.send_event(UserEvent::GamesTask(made));
                });
            return match asked {
                Ok(_) => (true, Some("Windows asks for permission once".into())),
                Err(e) => (false, Some(e.to_string())),
            };
        }
        self.config.games = false;
        if platform::running_as_administrator() {
            platform::delete_games_task();
            return (
                false,
                Some("Without administrator rights from the next start".into()),
            );
        }
        (false, None)
    }

    /// After Windows asked: with the task made, Bruma starts again through
    /// it, as administrator, and this one quits.
    fn games_task_made(&mut self, event_loop: &ActiveEventLoop, made: Result<bool, String>) {
        let problem = match made {
            Ok(true) => {
                self.config.games = true;
                match self.config.save() {
                    Ok(_) if platform::start_games_task() => {
                        event_loop.exit();
                        return;
                    }
                    Ok(_) => "The task was made but did not start; restart Bruma".to_string(),
                    Err(why) => why,
                }
            }
            Ok(false) => "Windows could not make the task".to_string(),
            Err(why) => why,
        };
        if let Some(form) = self.stage.as_mut().and_then(Stage::form) {
            form.values.games = self.config.games;
            form.set_message(Some(problem));
        }
        if let Some(stage) = &self.stage {
            stage.redraw();
        }
    }

    fn open_menu(&mut self, event_loop: &ActiveEventLoop, icon: (i32, i32, i32, i32)) {
        if self.overlay.is_some() || self.stage.is_some() {
            return;
        }
        self.menu = None;
        let (hotkey, warning) = match &self.tray {
            Some(tray) => (
                tray.hotkey(),
                tray.problem.as_ref().map(|_| {
                    format!(
                        "{} is taken; change it in Settings",
                        crate::tray::pretty(&self.config.hotkey)
                    )
                }),
            ),
            None => (None, None),
        };
        match TrayMenu::open(event_loop, self.fonts.clone(), icon, hotkey, warning) {
            Ok(menu) => self.menu = Some(menu),
            Err(e) => eprintln!("bruma: {e}"),
        }
    }

    fn on_menu(&mut self, event_loop: &ActiveEventLoop, entry: bruma_core::menu::Entry) {
        use bruma_core::menu::Entry;
        self.menu = None;
        let later = |proxy: &EventLoopProxy<UserEvent>, wait: Duration| {
            let proxy = proxy.clone();
            std::thread::spawn(move || {
                std::thread::sleep(wait);
                let _ = proxy.send_event(UserEvent::Capture);
            });
        };
        match entry {
            // A moment for the menu to leave the screen before it is read.
            Entry::Capture => later(&self.proxy, Duration::from_millis(120)),
            Entry::Later => later(&self.proxy, Duration::from_secs(3)),
            Entry::Folder => {
                let dir = self.config.save_dir();
                let _ = std::fs::create_dir_all(&dir);
                if let Err(e) = std::process::Command::new("explorer").arg(&dir).spawn() {
                    eprintln!("bruma: {e}");
                }
            }
            Entry::History => {
                if let Err(e) = history::open() {
                    eprintln!("bruma: {e}");
                }
            }
            Entry::Lab => {
                if let Err(e) = lab::open() {
                    eprintln!("bruma: {e}");
                }
            }
            Entry::Settings => self.open_settings(event_loop),
            Entry::Report => {
                // Looking at the monitors takes a moment; not on this thread.
                let proxy = self.proxy.clone();
                let _ = std::thread::Builder::new()
                    .name("bruma-report".into())
                    .spawn(move || {
                        let _ = proxy.send_event(UserEvent::Report(crate::doctor::report()));
                    });
            }
            Entry::Quit => event_loop.exit(),
        }
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
        let mut link = None;
        let (picture, rect, lifetime, pin) = match message {
            // A capture gets a card with a thumbnail; clicking it pins the
            // capture back where it was taken.
            Message::Shot(caption, image) if !preview.is_zero() => {
                let caption = if self.again.is_some() {
                    format!("{caption}  \u{00B7}  click to edit")
                } else {
                    caption
                };
                let offered = self.card_link.take();
                let line = offered.as_ref().map(bruma_core::links::card_line);
                let (picture, rect, line_rect) = bruma_core::render::preview_card_with_link(
                    &scene.screen,
                    &image,
                    &self.fonts,
                    &caption,
                    scene.monitor,
                    scene.scale,
                    line.as_deref(),
                );
                link = line_rect.zip(offered).map(|(r, l)| (r, l.url, l.kind));
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
        let edit = pin.is_some() && self.again.is_some();
        match Toast::show(
            event_loop,
            picture,
            desktop(rect.x, rect.y),
            lifetime,
            pin,
            edit,
            link,
        ) {
            Ok(toast) => {
                event_loop.set_control_flow(ControlFlow::WaitUntil(toast.deadline()));
                self.toast = Some(toast);
            }
            Err(e) => eprintln!("bruma: {e}"),
        }
    }

    /// Opens the capture just copied again, to draw on it.
    fn edit_again(&mut self, event_loop: &ActiveEventLoop) {
        let Some(snapshot) = self.again.take() else {
            return;
        };
        if self.overlay.is_some() || self.stage.is_some() {
            return;
        }
        self.menu = None;
        self.pause_recording();
        let opened = Overlay::reopen(
            event_loop,
            self.fonts.clone(),
            &self.config,
            self.prefs,
            self.shell.take(),
            snapshot,
        );
        match opened {
            Ok(overlay) => {
                self.overlay = Some(overlay);
                self.scan_for_links();
            }
            Err(e) => {
                eprintln!("bruma: {e}");
                self.resume_recording();
            }
        }
    }

    fn exit_when_idle(&self, event_loop: &ActiveEventLoop) {
        if !self.background
            && self.gifs == 0
            && self.stage.is_none()
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
                self.config.clip,
            );
            // Have the overlay's window ready before the first hotkey.
            let (origin, (width, height)) = platform::virtual_screen();
            let size = winit::dpi::PhysicalSize::new(width, height);
            self.shell = Shell::create(event_loop, origin, size).ok();
            let proxy = self.proxy.clone();
            platform::listen_for_capture_requests(move || {
                let _ = proxy.send_event(UserEvent::Capture);
            });
            let proxy = self.proxy.clone();
            platform::listen_for_settings_requests(move || {
                let _ = proxy.send_event(UserEvent::Settings);
            });
            let proxy = self.proxy.clone();
            platform::listen_for_quit_requests(move || {
                let _ = proxy.send_event(UserEvent::Quit);
            });
            match Tray::new(self.proxy.clone(), &self.config.hotkey) {
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
            UserEvent::Capture => {
                crate::note(|| {
                    format!(
                        "capture asked: {:?}, overlay open {}, in front: {}",
                        platform::attention(),
                        self.overlay.is_some(),
                        platform::describe_window(platform::foreground_window())
                    )
                });
                self.menu = None;
                // Already open (perhaps behind a window that pushed in
                // front): bring it back rather than doing nothing.
                if let Some(overlay) = &self.overlay {
                    overlay.raise();
                } else if self.stage.is_some()
                    || platform::attention() != platform::Attention::FullScreen
                {
                    self.open_overlay(event_loop);
                    if std::mem::take(&mut self.games_hint) {
                        if let Some(overlay) = self.overlay.as_mut() {
                            overlay.say(
                                "That game runs as administrator: turn on \"Work over games \
                                 run as administrator\" in Settings to choose an area over it",
                            );
                        }
                    }
                } else {
                    // A game filling the screen: the overlay opens over it
                    // once it has the keyboard (the game steps back, as for
                    // Alt+Tab), and the game gets it back afterwards. When
                    // Windows keeps the keyboard with the game, nothing can
                    // show over it: the screen is copied as it is.
                    let game = platform::foreground_window();
                    self.open_overlay(event_loop);
                    match &self.overlay {
                        Some(overlay) if overlay.has_keyboard() => self.back_to = Some(game),
                        Some(_) => {
                            self.drop_overlay();
                            self.capture_over_a_game(event_loop);
                            self.games_hint = platform::window_is_elevated(game)
                                && !platform::running_as_administrator();
                        }
                        None => {
                            self.capture_over_a_game(event_loop);
                        }
                    }
                }
                crate::note(|| {
                    format!(
                        "after opening, in front: {}",
                        platform::describe_window(platform::foreground_window())
                    )
                });
                crate::flush_trace();
            }
            UserEvent::TrayMenu(icon) => self.open_menu(event_loop, icon),
            UserEvent::Settings => self.open_settings(event_loop),
            UserEvent::Quit => event_loop.exit(),
            UserEvent::CardLink { capture, link } => {
                // Still the card it was looked for, and still on screen.
                if capture == self.capture && self.toast.is_some() {
                    if let Some((scene, caption, image)) = self.card_redo.take() {
                        self.card_link = Some(link);
                        self.show_message(event_loop, &scene, Message::Shot(caption, image));
                    }
                }
            }
            UserEvent::Links {
                capture,
                frame,
                links,
            } => {
                // An answer about an overlay that has closed since is no use.
                if capture != self.capture {
                    return;
                }
                self.scan = Some((capture, frame, links.clone()));
                let effects = match self.overlay.as_mut() {
                    Some(overlay) => overlay.feed(Input::Links { frame, links }),
                    None => Vec::new(),
                };
                self.apply(event_loop, effects);
            }
            UserEvent::Report(text) => {
                let said = match platform::copy_text(&text) {
                    Ok(()) => "A report is copied: paste it into your bug report. \
                               It leaves out your name and folders"
                        .to_string(),
                    Err(e) => e,
                };
                if let Some(scene) = self.scene_now(event_loop) {
                    self.show_message(event_loop, &scene, Message::Text(said));
                }
            }
            UserEvent::GamesTask(made) => self.games_task_made(event_loop, made),
            UserEvent::GifDone(written, save, ms) => {
                self.gifs = self.gifs.saturating_sub(1);
                let seconds = ms as f32 / 1000.0;
                let text = match written {
                    Ok(path) => {
                        println!("{}", path.display());
                        run_after(&self.config.run_after, &path);
                        if save {
                            format!("Saved a GIF, {seconds:.1} s")
                        } else {
                            match platform::copy_files(&[path]) {
                                Ok(()) => format!("Copied a GIF, {seconds:.1} s"),
                                Err(e) => e,
                            }
                        }
                    }
                    Err(e) => e,
                };
                if self.overlay.is_none() {
                    if let Some(scene) = self.scene_now(event_loop) {
                        self.show_message(event_loop, &scene, Message::Text(text));
                    }
                }
                self.exit_when_idle(event_loop);
            }
        }
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, id: WindowId, event: WindowEvent) {
        if let Some(stage) = self.stage.as_mut().filter(|s| s.id() == id) {
            match stage.on_event(event) {
                StageEvent::None => {}
                StageEvent::Done => {
                    self.close_stage();
                    self.hello(event_loop);
                }
                StageEvent::Settings(effects) => self.on_settings(event_loop, effects),
            }
        } else if let Some(menu) = self.menu.as_mut().filter(|m| m.id() == id) {
            match menu.on_event(event) {
                MenuAction::None => {}
                MenuAction::Close => self.menu = None,
                MenuAction::Pick(entry) => self.on_menu(event_loop, entry),
            }
        } else if let Some(overlay) = self.overlay.as_mut().filter(|o| o.id() == id) {
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
                    self.again = None;
                    self.pin(event_loop, request.image, request.at);
                }
                ToastAction::Edit => {
                    self.toast = None;
                    self.edit_again(event_loop);
                }
                ToastAction::CopyLink(url, kind) => {
                    self.toast = None;
                    let said = match platform::copy_text(&url) {
                        Ok(()) => match kind {
                            bruma_core::links::Kind::Web => "Link copied".to_string(),
                            bruma_core::links::Kind::Text => "Text copied".to_string(),
                        },
                        Err(e) => e,
                    };
                    if let Some(scene) = self.scene_now(event_loop) {
                        self.show_message(event_loop, &scene, Message::Text(said));
                    }
                }
            }
        }
        self.exit_when_idle(event_loop);
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        if self
            .toast
            .as_ref()
            .is_some_and(|t| Instant::now() >= t.deadline())
        {
            self.toast = None;
            // The card is gone; so is the way back to the capture.
            self.again = None;
            self.exit_when_idle(event_loop);
        }
        // A menu closes when something else comes to the front, even if the
        // system never told it that it lost the keyboard.
        if self.menu.as_ref().is_some_and(TrayMenu::lost) {
            self.menu = None;
        }
        let wake = [
            self.overlay.as_mut().and_then(Overlay::tick),
            // Over a game, looking again in a moment whether Windows has put
            // the desktop back to its own size.
            self.back_to.and(self.overlay.as_ref()).map(|overlay| {
                overlay.fit_desktop();
                Instant::now() + Duration::from_millis(250)
            }),
            self.toast.as_ref().map(Toast::deadline),
            self.stage.as_mut().and_then(Stage::tick),
            self.menu
                .as_ref()
                .map(|_| Instant::now() + Duration::from_millis(150)),
        ]
        .into_iter()
        .flatten()
        .min();
        event_loop.set_control_flow(match wake {
            Some(at) => ControlFlow::WaitUntil(at),
            None => ControlFlow::Wait,
        });
    }
}

/// Runs until quit (background) or until the capture and its pins are closed.
pub fn run(background: bool, config: Config, editing: Option<EditJob>) -> Result<(), String> {
    // The hotkey arrives on this thread, and the picture is taken and shown
    // from it: on a machine kept busy by a game it must still go first.
    // Helpers it waits for go at the same priority.
    platform::run_this_thread_first();
    bruma_core::par::on_new_worker(platform::put_first);
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
        stage: None,
        menu: None,
        again: None,
        gifs: 0,
        capture: 0,
        scan: None,
        card_link: None,
        card_redo: None,
        back_to: None,
        games_hint: false,
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

/// The links a finished search found, if it was made for this capture and
/// the frame the card's picture comes from (the person may have rewound).
fn links_for(
    scan: &Option<(u64, usize, Vec<bruma_core::links::Link>)>,
    capture: u64,
    frame: usize,
) -> Option<&[bruma_core::links::Link]> {
    scan.as_ref()
        .filter(|(c, f, _)| *c == capture && *f == frame)
        .map(|(_, _, links)| links.as_slice())
}

/// Whether a preview card may offer a link: after copying or saving an area
/// chosen on the frozen screen, not in the file editor.
fn card_offers_link(chosen_area: bool, editing: bool) -> bool {
    chosen_area && !editing
}

#[cfg(test)]
mod card_tests {
    use super::*;
    use bruma_core::links::{Kind, Link};

    #[test]
    fn a_search_serves_only_its_capture_and_frame() {
        let link = Link {
            url: "https://example.com".into(),
            rect: bruma_core::Rect::new(0.0, 0.0, 10.0, 10.0),
            kind: Kind::Web,
            qr: false,
        };
        let scan = Some((7, 3, vec![link]));
        assert_eq!(links_for(&scan, 7, 3).map(<[Link]>::len), Some(1));
        assert_eq!(links_for(&scan, 7, 2), None, "another frame");
        assert_eq!(links_for(&scan, 8, 3), None, "another capture");
        assert_eq!(links_for(&None, 7, 3), None);
    }

    #[test]
    fn the_file_editor_and_scrolled_captures_offer_no_link() {
        assert!(card_offers_link(true, false));
        assert!(!card_offers_link(true, true), "file editor");
        assert!(
            !card_offers_link(false, false),
            "not an area of the frozen screen"
        );
    }
}
