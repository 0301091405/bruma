//! When Bruma stops unexpectedly. Without this it would simply be gone: no
//! tray icon, no hotkey, nothing to say why. Now a line is left in a file
//! next to the history (on this computer only), and the background process
//! starts again, unless it stopped the same way a moment ago.

use std::io::Write as _;
use std::path::PathBuf;

/// The file the notes go in.
fn file() -> Option<PathBuf> {
    Some(crate::history::dir()?.parent()?.join("crash.log"))
}

/// Notes kept from earlier stops; older ones are dropped beyond this.
const KEEP_BYTES: usize = 16 * 1024;

/// Starting again is skipped when the last stop was this recent, so that a
/// fault that comes back at once does not loop.
const AGAIN_AFTER_SECONDS: u64 = 120;

/// Seconds since 1970, for the notes.
fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// When the last note was written, from its first line.
fn last_at(text: &str) -> Option<u64> {
    text.rsplit("--- ")
        .next()?
        .split_whitespace()
        .next()?
        .parse()
        .ok()
}

/// Leaves a note and, for the background process, starts it again when
/// anything in Bruma goes wrong past repair.
pub fn install(background: bool) {
    std::panic::set_hook(Box::new(move |info| {
        let thread = std::thread::current();
        let place = info
            .location()
            .map(|l| format!("{}:{}", l.file(), l.line()))
            .unwrap_or_default();
        let what = info
            .payload()
            .downcast_ref::<&str>()
            .map(|s| s.to_string())
            .or_else(|| info.payload().downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "unknown".into());
        let t = crate::platform::local_time();
        let note = format!(
            "--- {} {:04}-{:02}-{:02} {:02}:{:02}:{:02} Bruma {}\nthread {}: {what}\nat {place}\n",
            now(),
            t.year,
            t.month,
            t.day,
            t.hour,
            t.minute,
            t.second,
            env!("CARGO_PKG_VERSION"),
            thread.name().unwrap_or("unnamed"),
        );
        eprintln!("bruma: {}", note.trim_end());
        let Some(path) = file() else {
            return;
        };
        let before = std::fs::read_to_string(&path).unwrap_or_default();
        let recent =
            last_at(&before).is_some_and(|at| now().saturating_sub(at) < AGAIN_AFTER_SECONDS);
        let mut kept = before;
        if kept.len() > KEEP_BYTES {
            let mut cut = kept.len() - KEEP_BYTES;
            while !kept.is_char_boundary(cut) {
                cut += 1;
            }
            let start = kept[cut..].find("--- ").map_or(kept.len(), |i| cut + i);
            kept = kept[start..].to_string();
        }
        kept.push_str(&note);
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Ok(mut f) = std::fs::File::create(&path) {
            let _ = f.write_all(kept.as_bytes());
        }
        // Back in the tray, waiting for this one to be gone first.
        if background && !recent {
            if let Ok(exe) = std::env::current_exe() {
                let _ = std::process::Command::new(exe).arg("takeover").spawn();
            }
        }
    }));
}

/// The last note, if Bruma ever stopped unexpectedly here.
pub fn last() -> Option<String> {
    let text = std::fs::read_to_string(file()?).ok()?;
    let start = text.rfind("--- ")?;
    let note = &text[start + 4..];
    // The first word is the machine time; the rest is for people.
    Some(
        note.split_once(' ')
            .map_or(note, |(_, rest)| rest)
            .trim_end()
            .to_string(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_time_of_the_last_note_is_read_back() {
        let text =
            "--- 100 2026-01-01 00:00:00 Bruma 1\nx\n--- 250 2026-01-01 00:00:02 Bruma 1\ny\n";
        assert_eq!(last_at(text), Some(250));
        assert_eq!(last_at(""), None);
    }
}
