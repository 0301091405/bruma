//! Tray icon and the global hotkey. A left click on the icon takes a
//! capture; a right click asks for Bruma's own menu.

use bruma_core::theme::{ACCENT, INK, PAPER};
use bruma_core::tiny_skia::{Color, FillRule, Paint, PathBuilder, Pixmap, Rect, Transform};
use global_hotkey::hotkey::HotKey;
use global_hotkey::{GlobalHotKeyEvent, GlobalHotKeyManager, HotKeyState};
use winit::event_loop::EventLoopProxy;

use crate::app::UserEvent;
use crate::platform::{self, NotifyIcon, TrayClick};

pub struct Tray {
    _icon: NotifyIcon,
    manager: Option<GlobalHotKeyManager>,
    /// The hotkey registered now, and its name in the settings file's words.
    registered: Option<(HotKey, String)>,
    /// Why the configured hotkey is not the one in use.
    pub problem: Option<String>,
    /// Whether the hotkey has been let go of by [`Tray::pause`].
    paused: bool,
}

/// The bruma mark: a viewfinder. Four crop marks and one accent pixel, on ink.
pub fn mark(size: u32) -> Vec<u8> {
    let Some(mut pixmap) = Pixmap::new(size, size) else {
        return vec![0; (size * size * 4) as usize];
    };
    let s = size as f32;
    let paint = |c: [u8; 3]| {
        let mut p = Paint::default();
        p.set_color(Color::from_rgba8(c[0], c[1], c[2], 255));
        p.anti_alias = true;
        p
    };
    let mut plate = PathBuilder::new();
    let r = s * 0.19;
    plate.move_to(r, 0.0);
    plate.line_to(s - r, 0.0);
    plate.quad_to(s, 0.0, s, r);
    plate.line_to(s, s - r);
    plate.quad_to(s, s, s - r, s);
    plate.line_to(r, s);
    plate.quad_to(0.0, s, 0.0, s - r);
    plate.line_to(0.0, r);
    plate.quad_to(0.0, 0.0, r, 0.0);
    plate.close();
    if let Some(path) = plate.finish() {
        pixmap.fill_path(
            &path,
            &paint(INK),
            FillRule::Winding,
            Transform::identity(),
            None,
        );
    }

    let t = (s / 10.0).round().max(1.0);
    let inset = (s * 0.19).round();
    let len = (s * 0.25).round();
    let mut marks = paint(PAPER);
    marks.anti_alias = false;
    for (x, sx) in [(inset, 1.0), (s - inset, -1.0)] {
        for (y, sy) in [(inset, 1.0), (s - inset, -1.0)] {
            let hx = if sx > 0.0 { x } else { x - len };
            let vy = if sy > 0.0 { y } else { y - len };
            let hy = if sy > 0.0 { y } else { y - t };
            let vx = if sx > 0.0 { x } else { x - t };
            for rect in [
                Rect::from_xywh(hx, hy, len, t),
                Rect::from_xywh(vx, vy, t, len),
            ]
            .into_iter()
            .flatten()
            {
                pixmap.fill_rect(rect, &marks, Transform::identity(), None);
            }
        }
    }
    // Even on an even icon (and odd on an odd one), so it sits in the middle.
    let mut dot = (s * 0.22).round().max(2.0);
    if (size as f32 - dot) % 2.0 != 0.0 {
        dot += 1.0;
    }
    let mut accent = paint(ACCENT);
    accent.anti_alias = false;
    if let Some(rect) = Rect::from_xywh(
        ((s - dot) / 2.0).round(),
        ((s - dot) / 2.0).round(),
        dot,
        dot,
    ) {
        pixmap.fill_rect(rect, &accent, Transform::identity(), None);
    }

    // tiny-skia stores premultiplied alpha; icons want straight alpha.
    let mut rgba = pixmap.take();
    for px in rgba.chunks_exact_mut(4) {
        let a = px[3] as u32;
        if a > 0 && a < 255 {
            for c in &mut px[..3] {
                *c = ((*c as u32 * 255 + a / 2) / a).min(255) as u8;
            }
        }
    }
    rgba
}

impl Tray {
    /// Creates the tray icon and registers `hotkey`, or a fallback if another
    /// program has it.
    pub fn new(proxy: EventLoopProxy<UserEvent>, hotkey: &str) -> Result<Self, String> {
        let registration = register(hotkey);
        if let Some(problem) = &registration.problem {
            eprintln!("bruma: {problem}");
        }
        let size = platform::small_icon_size();
        let click_proxy = proxy.clone();
        let icon = NotifyIcon::new(&mark(size), size, "Bruma", move |click| {
            let event = match click {
                TrayClick::Left => UserEvent::Capture,
                TrayClick::Right((x, y)) => UserEvent::TrayMenu((x, y, 1, 1)),
            };
            let _ = click_proxy.send_event(event);
        })?;
        GlobalHotKeyEvent::set_event_handler(Some(move |event: GlobalHotKeyEvent| {
            if event.state() == HotKeyState::Pressed {
                let _ = proxy.send_event(UserEvent::Capture);
            }
        }));

        Ok(Self {
            _icon: icon,
            manager: registration.manager,
            registered: registration.active,
            problem: registration.problem,
            paused: false,
        })
    }

    /// The hotkey that takes a capture, as people write it.
    pub fn hotkey(&self) -> Option<String> {
        self.registered.as_ref().map(|(_, name)| pretty(name))
    }

    /// Lets go of the hotkey for a moment, so that pressing it reaches a
    /// window instead (the settings, listening for a new one).
    pub fn pause(&mut self) {
        if self.paused {
            return;
        }
        if let (Some(manager), Some((key, _))) = (&self.manager, &self.registered) {
            let _ = manager.unregister(*key);
        }
        self.paused = true;
    }

    /// Takes the hotkey back after [`Tray::pause`].
    pub fn resume(&mut self) {
        if !self.paused {
            return;
        }
        if let (Some(manager), Some((key, _))) = (&self.manager, &self.registered) {
            let _ = manager.register(*key);
        }
        self.paused = false;
    }

    /// Changes the hotkey to `wanted`. When that is not possible the old one
    /// stays, and the reason is returned.
    pub fn set_hotkey(&mut self, wanted: &str) -> Result<(), String> {
        let parsed: HotKey = wanted
            .parse()
            .map_err(|_| format!("{} cannot be a hotkey", pretty(wanted)))?;
        let manager = self.manager.as_ref().ok_or("hotkeys are not available")?;
        if let Some((old, _)) = &self.registered {
            if !self.paused {
                let _ = manager.unregister(*old);
            }
        }
        self.paused = false;
        match manager.register(parsed) {
            Ok(()) => {
                self.registered = Some((parsed, wanted.to_string()));
                self.problem = None;
                Ok(())
            }
            Err(_) => {
                if let Some((old, _)) = &self.registered {
                    let _ = manager.register(*old);
                }
                Err(format!("{} is taken by another app", pretty(wanted)))
            }
        }
    }
}

/// Tried in order when the configured hotkey belongs to another program.
const FALLBACK_HOTKEYS: &[&str] = &["ctrl+PrintScreen", "ctrl+alt+KeyB"];

struct Registration {
    manager: Option<GlobalHotKeyManager>,
    /// The hotkey that actually works, if any, and its name.
    active: Option<(HotKey, String)>,
    /// Why the configured hotkey is not the active one.
    problem: Option<String>,
}

fn register(wanted: &str) -> Registration {
    let manager = match GlobalHotKeyManager::new() {
        Ok(manager) => manager,
        Err(e) => {
            return Registration {
                manager: None,
                active: None,
                problem: Some(e.to_string()),
            };
        }
    };
    let attempt = |name: &str| -> Result<HotKey, String> {
        let parsed: HotKey = name
            .parse()
            .map_err(|_| format!("hotkey '{name}' is not a key combination Bruma understands"))?;
        manager
            .register(parsed)
            .map(|()| parsed)
            .map_err(|_| format!("{} is taken by another app", pretty(name)))
    };
    let problem = match attempt(wanted) {
        Ok(key) => {
            return Registration {
                manager: Some(manager),
                active: Some((key, wanted.to_string())),
                problem: None,
            };
        }
        Err(problem) => problem,
    };
    let active = FALLBACK_HOTKEYS.iter().find_map(|name| {
        if name.eq_ignore_ascii_case(wanted) {
            return None;
        }
        attempt(name).ok().map(|key| (key, name.to_string()))
    });
    let problem = match &active {
        Some((_, name)) => format!("{problem}; using {} instead", pretty(name)),
        None => format!("{problem}; choose another in Settings"),
    };
    Registration {
        manager: Some(manager),
        active,
        problem: Some(problem),
    }
}

/// `ctrl+alt+KeyB` → `Ctrl+Alt+B`, for showing to people.
pub fn pretty(hotkey: &str) -> String {
    bruma_core::settings::pretty(hotkey)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_dot_is_in_the_middle_at_every_size() {
        for size in [16u32, 20, 24, 32] {
            let rgba = mark(size);
            let accent = |x: u32, y: u32| {
                let i = ((y * size + x) * 4) as usize;
                rgba[i..i + 3] == ACCENT[..]
            };
            let row: Vec<u32> = (0..size).filter(|&x| accent(x, size / 2)).collect();
            let (first, last) = (row[0], *row.last().unwrap());
            assert_eq!(first, size - 1 - last, "{size}: {first}..{last}");
        }
    }

    #[test]
    fn mark_has_transparent_corners_and_an_accent_centre() {
        let rgba = mark(32);
        assert_eq!(rgba.len(), 32 * 32 * 4);
        assert_eq!(rgba[3], 0, "corner is transparent");
        let centre = (16 * 32 + 16) * 4;
        assert_eq!(
            &rgba[centre..centre + 4],
            &[ACCENT[0], ACCENT[1], ACCENT[2], 255]
        );
    }

    #[test]
    fn hotkeys_are_shown_in_plain_words() {
        assert_eq!(pretty("ctrl+alt+KeyB"), "Ctrl+Alt+B");
        assert_eq!(pretty("PrintScreen"), "PrintScreen");
        assert_eq!(pretty("shift+Digit4"), "Shift+4");
        assert_eq!(pretty("ctrl+F12"), "Ctrl+F12");
    }

    #[test]
    fn fallbacks_parse() {
        for name in FALLBACK_HOTKEYS {
            assert!(name.parse::<HotKey>().is_ok(), "{name}");
        }
    }

    #[test]
    fn default_hotkey_parses() {
        assert!("PrintScreen".parse::<HotKey>().is_ok());
        assert!("ctrl+shift+KeyS".parse::<HotKey>().is_ok());
        assert!("not a key".parse::<HotKey>().is_err());
    }
}
