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
        let mut registration = register(hotkey);
        let size = platform::small_icon_size();
        let click_proxy = proxy.clone();
        let icon = NotifyIcon::new(&mark(size), size, "Bruma", move |click| {
            let event = match click {
                TrayClick::Left => UserEvent::Capture,
                TrayClick::Right((x, y)) => UserEvent::TrayMenu((x, y, 1, 1)),
            };
            let _ = click_proxy.send_event(event);
        })?;
        // The keyboard hook, for games that hold back registered hotkeys;
        // the registration still keeps the key from other programs.
        let watcher = std::sync::Mutex::new(proxy.clone());
        let watching = platform::watch_for_hotkey(move || {
            if let Ok(proxy) = watcher.lock() {
                let _ = proxy.send_event(UserEvent::Capture);
            }
        });
        // Windows 11 gives PrintScreen to its Snipping Tool unless that was
        // turned off: registering it then fails, or the tool opens as well.
        // The keyboard hook sees the key before Windows acts on it, so with
        // the hook Bruma keeps PrintScreen, and no other key is put in its
        // place.
        let snipping =
            platform::windows_takes_printscreen() && virtual_key(hotkey) == Some((0x2C, 0));
        platform::keep_keyboard_hook(snipping);
        if watching && snipping && registration.problem.is_some() {
            if let (Some(manager), Some((fallback, _))) =
                (&registration.manager, &registration.active)
            {
                let _ = manager.unregister(*fallback);
            }
            if let Ok(key) = hotkey.parse::<HotKey>() {
                registration.active = Some((key, hotkey.to_string()));
                registration.problem = None;
            }
        }
        if let Some(problem) = &registration.problem {
            eprintln!("bruma: {problem}");
        }
        if watching {
            platform::set_watched_hotkey(
                registration
                    .active
                    .as_ref()
                    .and_then(|(_, name)| virtual_key(name)),
            );
        }
        // The hotkey goes straight to the event loop. Should that not be
        // possible, the library's own way still works.
        let direct = proxy.clone();
        if !platform::answer_hotkeys_directly(move || {
            let _ = direct.send_event(UserEvent::Capture);
        }) {
            GlobalHotKeyEvent::set_event_handler(Some(move |event: GlobalHotKeyEvent| {
                if event.state() == HotKeyState::Pressed {
                    let _ = proxy.send_event(UserEvent::Capture);
                }
            }));
        }

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
        platform::set_watched_hotkey(None);
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
        platform::set_watched_hotkey(self.registered.as_ref().and_then(|(_, n)| virtual_key(n)));
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
                platform::set_watched_hotkey(virtual_key(wanted));
                Ok(())
            }
            Err(_) => {
                if let Some((old, _)) = &self.registered {
                    let _ = manager.register(*old);
                }
                platform::set_watched_hotkey(
                    self.registered.as_ref().and_then(|(_, n)| virtual_key(n)),
                );
                Err(format!("{} is taken by another app", pretty(wanted)))
            }
        }
    }
}

/// The virtual-key code and modifier bits of a hotkey written the way the
/// settings write it ("ctrl+shift+KeyS", "PrintScreen"), for the keyboard
/// hook. `None` for a key it does not know; the registered hotkey then
/// works alone.
fn virtual_key(name: &str) -> Option<(u16, u32)> {
    let mut mods = 0;
    let mut key = None;
    for part in name.split('+').map(str::trim) {
        match part.to_ascii_lowercase().as_str() {
            "ctrl" | "control" => mods |= platform::MOD_CTRL,
            "shift" => mods |= platform::MOD_SHIFT,
            "alt" | "option" => mods |= platform::MOD_ALT,
            "super" | "win" | "meta" | "cmd" | "command" => mods |= platform::MOD_WIN,
            _ => key = Some(part),
        }
    }
    let key = key?;
    let lower = key.to_ascii_lowercase();
    // "KeyS" and "Digit4": the virtual-key code is the character's own.
    let single = |rest: &str| {
        let mut chars = rest.chars();
        let c = chars.next()?.to_ascii_uppercase();
        chars.next().is_none().then_some(c)
    };
    let vk = if let Some(rest) = key.strip_prefix("Key") {
        single(rest).filter(char::is_ascii_uppercase)? as u16
    } else if let Some(rest) = key.strip_prefix("Digit") {
        single(rest).filter(char::is_ascii_digit)? as u16
    } else if let Some(n) = key
        .strip_prefix('F')
        .and_then(|n| n.parse::<u16>().ok())
        .filter(|n| (1..=24).contains(n))
    {
        0x6F + n
    } else if let Some(n) = key
        .strip_prefix("Numpad")
        .and_then(|n| n.parse::<u16>().ok())
        .filter(|n| *n <= 9)
    {
        0x60 + n
    } else {
        match lower.as_str() {
            "printscreen" => 0x2C,
            "pause" => 0x13,
            "scrolllock" => 0x91,
            "insert" => 0x2D,
            "delete" => 0x2E,
            "home" => 0x24,
            "end" => 0x23,
            "pageup" => 0x21,
            "pagedown" => 0x22,
            "arrowleft" => 0x25,
            "arrowup" => 0x26,
            "arrowright" => 0x27,
            "arrowdown" => 0x28,
            "space" => 0x20,
            "backquote" => 0xC0,
            "minus" => 0xBD,
            "equal" => 0xBB,
            "bracketleft" => 0xDB,
            "bracketright" => 0xDD,
            "backslash" => 0xDC,
            "semicolon" => 0xBA,
            "quote" => 0xDE,
            "comma" => 0xBC,
            "period" => 0xBE,
            "slash" => 0xBF,
            "numpadmultiply" => 0x6A,
            "numpadadd" => 0x6B,
            "numpadsubtract" => 0x6D,
            "numpaddecimal" => 0x6E,
            "numpaddivide" => 0x6F,
            _ => return None,
        }
    };
    Some((vk, mods))
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
    fn hotkeys_become_the_keys_the_keyboard_hook_watches_for() {
        use platform::{MOD_ALT, MOD_CTRL, MOD_SHIFT, MOD_WIN};
        assert_eq!(virtual_key("PrintScreen"), Some((0x2C, 0)));
        assert_eq!(virtual_key("ctrl+PrintScreen"), Some((0x2C, MOD_CTRL)));
        assert_eq!(
            virtual_key("ctrl+alt+KeyB"),
            Some((0x42, MOD_CTRL | MOD_ALT))
        );
        assert_eq!(
            virtual_key("ctrl+shift+KeyS"),
            Some((0x53, MOD_CTRL | MOD_SHIFT))
        );
        assert_eq!(virtual_key("shift+Digit4"), Some((0x34, MOD_SHIFT)));
        assert_eq!(virtual_key("super+F12"), Some((0x7B, MOD_WIN)));
        assert_eq!(virtual_key("F1"), Some((0x70, 0)));
        assert_eq!(virtual_key("alt+Numpad7"), Some((0x67, MOD_ALT)));
        assert_eq!(virtual_key("Pause"), Some((0x13, 0)));
        assert_eq!(virtual_key("ctrl+KeyAB"), None);
        assert_eq!(virtual_key("ctrl+F25"), None);
        assert_eq!(virtual_key("ctrl+shift"), None);
        for name in FALLBACK_HOTKEYS {
            assert!(virtual_key(name).is_some(), "{name}");
        }
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
