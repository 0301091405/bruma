//! `bruma doctor`: what Bruma sees of this computer, for a bug report. It
//! reads and reports; it changes nothing and sends nothing anywhere. The
//! person's name and home folder are left out, so the report can be pasted
//! where others will read it.

use std::fmt::Write;

use crate::config::Config;
use crate::platform;

/// The report, as plain text.
pub fn report() -> String {
    let mut out = String::new();
    let (config, _) = Config::load();
    let exe = std::env::current_exe().ok();
    let line = |out: &mut String, text: String| {
        let _ = writeln!(out, "{text}");
    };

    line(&mut out, format!("Bruma {}", env!("CARGO_PKG_VERSION")));
    line(&mut out, platform::windows_version());
    line(
        &mut out,
        format!(
            "memory {} GB, power: {:?}",
            (platform::installed_memory() + (1 << 29)) >> 30,
            platform::power()
        ),
    );
    if let Some(exe) = &exe {
        line(&mut out, format!("program: {}", exe.display()));
    }

    // Running, and how.
    let others = platform::other_brumas();
    line(
        &mut out,
        match others.as_slice() {
            [] => "running: no".into(),
            list => format!(
                "running: {}",
                list.iter()
                    .map(|(_, admin)| if *admin {
                        "as administrator"
                    } else {
                        "as an ordinary program"
                    })
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        },
    );
    let task = exe.as_deref().is_some_and(platform::games_task_starts);
    line(
        &mut out,
        format!(
            "over games run as administrator: setting {}, task \"{}\" {}",
            if config.games { "on" } else { "off" },
            platform::games_task_name(),
            if task {
                "there, for this program"
            } else {
                "not there"
            }
        ),
    );

    // The hotkey.
    line(&mut out, format!("hotkey: {}", config.hotkey));
    line(
        &mut out,
        format!(
            "Windows uses PrintScreen for its Snipping Tool: {}",
            if platform::windows_takes_printscreen() {
                "yes"
            } else {
                "no"
            }
        ),
    );

    // The screen.
    for monitor in platform::describe_monitors() {
        line(&mut out, monitor);
    }
    match platform::Grabber::new().and_then(|mut g| g.grab().map(|r| (r.width, r.height))) {
        Ok((w, h)) => line(&mut out, format!("GDI reads {w}x{h}")),
        Err(e) => line(&mut out, format!("GDI: {e}")),
    }

    // What is in front.
    let front = platform::foreground_window();
    line(
        &mut out,
        format!(
            "in front: {}, {}; Windows says {}",
            platform::describe_window(front),
            if platform::window_is_elevated(front) {
                "run as administrator"
            } else {
                "ordinary"
            },
            match platform::attention_state() {
                1 => "away".to_string(),
                2 => "full screen".to_string(),
                3 => "Direct3D exclusive full screen".to_string(),
                4 => "presenting".to_string(),
                5 => "normal".to_string(),
                n => format!("state {n}"),
            }
        ),
    );
    line(
        &mut out,
        format!(
            "Bruma takes it as: {}",
            match platform::attention() {
                platform::Attention::Desktop => "the desktop",
                platform::Attention::FullScreen => "a game or a film in full screen",
                platform::Attention::Away => "nobody at the computer",
            }
        ),
    );

    // Settings that decide what runs in the background.
    line(
        &mut out,
        format!(
            "settings: rewind {} s, GIF {} s, quick copy {}, fast capture {}, cursor {}, history {}",
            config.rewind,
            config.clip,
            config.quick,
            config.fast_capture,
            config.cursor,
            config.history
        ),
    );
    let folder = config.save_dir();
    line(
        &mut out,
        format!(
            "save folder: {} ({})",
            folder.display(),
            match platform::writable_folder(&folder) {
                Ok(()) => "can be written to".to_string(),
                Err(why) => why,
            }
        ),
    );

    // The last time Bruma stopped unexpectedly.
    if let Some(crash) = crate::crash::last() {
        line(&mut out, "last unexpected stop:".into());
        for text in crash.lines().take(8) {
            line(&mut out, format!("  {text}"));
        }
    }
    private(&out)
}

/// The report without the person's name or home folder.
fn private(text: &str) -> String {
    let mut text = text.to_string();
    if let Some(home) = std::env::var_os("USERPROFILE").map(|h| h.to_string_lossy().into_owned()) {
        if !home.is_empty() {
            text = replace_ignoring_case(&text, &home, "%USERPROFILE%");
        }
    }
    if let Ok(user) = std::env::var("USERNAME") {
        if user.len() > 1 {
            text = replace_ignoring_case(&text, &user, "<user>");
        }
    }
    text
}

fn replace_ignoring_case(text: &str, what: &str, with: &str) -> String {
    let lower = text.to_lowercase();
    let what_lower = what.to_lowercase();
    // Lower-casing can change lengths outside ASCII; then only exact matches go.
    if lower.len() != text.len() || what_lower.len() != what.len() {
        return text.replace(what, with);
    }
    let mut out = String::with_capacity(text.len());
    let mut rest = 0;
    let mut from = 0;
    while let Some(at) = lower[from..].find(&what_lower) {
        let at = from + at;
        out.push_str(&text[rest..at]);
        out.push_str(with);
        rest = at + what.len();
        from = rest;
    }
    out.push_str(&text[rest..]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_name_and_home_folder_are_left_out_whatever_their_case() {
        assert_eq!(
            replace_ignoring_case(
                r"C:\USERS\Ada\x and c:\users\ada\y",
                r"C:\Users\Ada",
                "%USERPROFILE%"
            ),
            r"%USERPROFILE%\x and %USERPROFILE%\y"
        );
        assert_eq!(
            replace_ignoring_case("Bruma - Ada", "ada", "<user>"),
            "Bruma - <user>"
        );
        assert_eq!(
            replace_ignoring_case("nothing here", "ada", "<user>"),
            "nothing here"
        );
    }
}
