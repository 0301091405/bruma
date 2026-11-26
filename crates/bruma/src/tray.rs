//! Tray icon, its menu, and the global hotkey.

use bruma_core::theme::{ACCENT, INK, PAPER};
use bruma_core::tiny_skia::{Color, FillRule, Paint, PathBuilder, Pixmap, Rect, Transform};
use global_hotkey::hotkey::HotKey;
use global_hotkey::{GlobalHotKeyEvent, GlobalHotKeyManager, HotKeyState};
use tray_icon::menu::{CheckMenuItem, Menu, MenuEvent, MenuId, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon, MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};
use winit::event_loop::EventLoopProxy;

use crate::app::UserEvent;

pub struct Tray {
    _icon: TrayIcon,
    _hotkeys: Option<GlobalHotKeyManager>,
    pub capture: MenuId,
    pub folder: MenuId,
    pub lab: MenuId,
    pub later: MenuId,
    pub history: MenuId,
    pub settings: MenuId,
    /// The "Start with Windows" tick.
    pub autostart: CheckMenuItem,
    pub quit: MenuId,
    /// The hotkey that takes a capture, in plain words; `None` when neither
    /// the configured one nor a fallback could be had.
    pub hotkey: Option<String>,
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
    let inset = (s * 0.22).round();
    let len = (s * 0.22).round();
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
    let dot = (s * 0.16).round().max(2.0);
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
    /// Creates the tray icon and registers `hotkey`. A hotkey that cannot be
    /// registered is reported in the menu instead of failing.
    pub fn new(
        proxy: EventLoopProxy<UserEvent>,
        hotkey: &str,
        autostart_on: bool,
    ) -> Result<Self, String> {
        let registration = register(hotkey);
        let label = match &registration.active {
            Some(active) => format!("Capture\t{active}"),
            None => "Capture".to_string(),
        };
        let capture = MenuItem::new(label, true, None);
        let folder = MenuItem::new("Open screenshots folder", true, None);
        let lab = MenuItem::new("Open test lab", true, None);
        let later = MenuItem::new("Capture in 3 seconds", true, None);
        let history = MenuItem::new("History", true, None);
        let settings = MenuItem::new("Settings", true, None);
        let autostart = CheckMenuItem::new("Start with Windows", true, autostart_on, None);
        let quit = MenuItem::new("Quit bruma", true, None);
        let menu = Menu::new();
        let separator = PredefinedMenuItem::separator();
        let mut build = menu.append(&capture).and_then(|_| menu.append(&later));
        if let Some(problem) = &registration.problem {
            eprintln!("bruma: {problem}");
            let note = MenuItem::new(
                format!("{} is taken by another app", pretty(hotkey)),
                false,
                None,
            );
            build = build.and_then(|_| menu.append(&note));
        }
        let hotkey = registration.active.clone();
        let hotkeys = registration.manager;
        build
            .and_then(|_| menu.append(&folder))
            .and_then(|_| menu.append(&lab))
            .and_then(|_| menu.append(&history))
            .and_then(|_| menu.append(&settings))
            .and_then(|_| menu.append(&autostart))
            .and_then(|_| menu.append(&separator))
            .and_then(|_| menu.append(&quit))
            .map_err(|e| e.to_string())?;

        let icon = Icon::from_rgba(mark(32), 32, 32).map_err(|e| e.to_string())?;
        let tray = TrayIconBuilder::new()
            .with_icon(icon)
            .with_tooltip("bruma")
            .with_menu(Box::new(menu))
            .with_menu_on_left_click(false)
            .build()
            .map_err(|e| e.to_string())?;

        let menu_proxy = proxy.clone();
        MenuEvent::set_event_handler(Some(std::sync::Arc::new(move |event: MenuEvent| {
            let _ = menu_proxy.send_event(UserEvent::Menu(event.id));
        })));
        let click_proxy = proxy.clone();
        TrayIconEvent::set_event_handler(Some(move |event: TrayIconEvent| {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                let _ = click_proxy.send_event(UserEvent::Capture);
            }
        }));
        GlobalHotKeyEvent::set_event_handler(Some(move |event: GlobalHotKeyEvent| {
            if event.state() == HotKeyState::Pressed {
                let _ = proxy.send_event(UserEvent::Capture);
            }
        }));

        Ok(Self {
            _icon: tray,
            _hotkeys: hotkeys,
            capture: capture.id().clone(),
            folder: folder.id().clone(),
            lab: lab.id().clone(),
            later: later.id().clone(),
            history: history.id().clone(),
            settings: settings.id().clone(),
            autostart,
            quit: quit.id().clone(),
            hotkey,
        })
    }
}

/// Tried in order when the configured hotkey belongs to another program.
const FALLBACK_HOTKEYS: &[&str] = &["ctrl+PrintScreen", "ctrl+alt+KeyB"];

struct Registration {
    manager: Option<GlobalHotKeyManager>,
    /// The hotkey that actually works, if any.
    active: Option<String>,
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
    let attempt = |name: &str| -> Result<(), String> {
        let parsed: HotKey = name
            .parse()
            .map_err(|_| format!("hotkey '{name}' is not a key combination bruma understands"))?;
        manager
            .register(parsed)
            .map_err(|_| format!("hotkey {} is in use by another program", pretty(name)))
    };
    let problem = match attempt(wanted) {
        Ok(()) => {
            return Registration {
                manager: Some(manager),
                active: Some(pretty(wanted)),
                problem: None,
            };
        }
        Err(problem) => problem,
    };
    let active = FALLBACK_HOTKEYS
        .iter()
        .find(|name| !name.eq_ignore_ascii_case(wanted) && attempt(name).is_ok())
        .map(|name| pretty(name));
    let problem = match &active {
        Some(active) => format!("{problem}; using {active} instead"),
        None => format!("{problem}, and so are the fallbacks; set 'hotkey' in the settings file"),
    };
    Registration {
        manager: Some(manager),
        active,
        problem: Some(problem),
    }
}

/// `ctrl+alt+KeyB` → `Ctrl+Alt+B`, for showing to people.
pub fn pretty(hotkey: &str) -> String {
    hotkey
        .split('+')
        .map(|part| {
            let part = part.trim();
            let lower = part.to_ascii_lowercase();
            match lower.as_str() {
                "ctrl" | "control" => "Ctrl".to_string(),
                "alt" | "option" => "Alt".to_string(),
                "shift" => "Shift".to_string(),
                "super" | "cmd" | "command" | "win" => "Win".to_string(),
                _ => match (lower.strip_prefix("key"), lower.strip_prefix("digit")) {
                    (Some(letter), _) if letter.len() == 1 => letter.to_ascii_uppercase(),
                    (_, Some(digit)) if digit.len() == 1 => digit.to_string(),
                    _ => part.to_string(),
                },
            }
        })
        .collect::<Vec<_>>()
        .join("+")
}

#[cfg(test)]
mod tests {
    use super::*;

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
