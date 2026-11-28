//! Settings: one small TOML file, every key optional.

use std::path::{Path, PathBuf};

use serde::Deserialize;

#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(default)]
pub struct Config {
    /// Global hotkey, e.g. `"PrintScreen"` or `"ctrl+shift+KeyS"`.
    pub hotkey: String,
    /// Folder for saved screenshots. A leading `~` is the home directory.
    pub save_dir: String,
    /// File name pattern: `%Y %m %d %H %M %S` are replaced with the local time.
    pub filename: String,
    /// Default annotation ink, 1–6.
    pub color: u8,
    /// Default stroke width in logical pixels.
    pub stroke: f32,
    /// Snap selections to the boxes and edges on screen.
    pub snap: bool,
    /// Also save a file every time a capture is copied.
    pub autosave: bool,
    /// Seconds the preview card stays after a capture; 0 shows a short
    /// message instead.
    pub preview: f32,
    /// Seconds of screen history kept in memory for rewinding; 0 switches it
    /// off. Only the background process records.
    pub rewind: f32,
    /// Seconds of screen kept for GIFs, at up to thirty pictures a second; 0
    /// switches it off.
    pub clip: f32,
    /// Copy the moment an area is chosen, without stopping to draw.
    pub quick: bool,
    /// Run as administrator, through a task made once with consent (setup
    /// asks), so that the hotkey and the overlay also work over games that
    /// run that way. Without the task Bruma runs as it is.
    pub games: bool,
    /// Find links and QR codes on the frozen screen, to copy them.
    pub links: bool,
    /// Keep recording for rewind while a game or a film fills the screen.
    /// Off, so that a game never pays for a feature it is not using.
    pub rewind_fullscreen: bool,
    /// Read the screen through Desktop Duplication where it is to be had:
    /// several times quicker than GDI. Off, the screen is always read with
    /// GDI.
    pub fast_capture: bool,
    /// Include the mouse cursor in captures.
    pub cursor: bool,
    /// How many captures history keeps; 0 switches history off.
    pub history: usize,
    /// A command to run after a file is saved; `{file}` is the file. If it
    /// prints a web address, that address is copied.
    pub run_after: String,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            hotkey: "PrintScreen".into(),
            save_dir: String::new(),
            filename: "bruma-%Y%m%d-%H%M%S.png".into(),
            color: 1,
            stroke: 3.0,
            snap: true,
            autosave: false,
            preview: 4.0,
            rewind: 4.0,
            clip: 10.0,
            quick: true,
            games: true,
            links: true,
            rewind_fullscreen: false,
            fast_capture: true,
            cursor: false,
            history: 200,
            run_after: String::new(),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct LocalTime {
    pub year: u32,
    pub month: u32,
    pub day: u32,
    pub hour: u32,
    pub minute: u32,
    pub second: u32,
}

impl Config {
    /// The settings file for these values: every key, with a note on what it
    /// does.
    pub fn to_toml(&self) -> String {
        let text = |v: &str| toml::Value::String(v.to_string()).to_string();
        let number = |v: f32| {
            if v.fract() == 0.0 {
                format!("{}", v as i64)
            } else {
                format!("{v}")
            }
        };
        let lines = [
            ("hotkey", text(&self.hotkey), "or e.g. \"ctrl+shift+KeyS\""),
            (
                "save_dir",
                text(&self.save_dir),
                "empty = Pictures\\bruma; \"~\" is your home folder",
            ),
            (
                "filename",
                text(&self.filename),
                "%Y %m %d %H %M %S are the date and time",
            ),
            ("color", self.color.to_string(), "default ink, 1-6"),
            ("stroke", number(self.stroke), "default stroke width"),
            (
                "snap",
                self.snap.to_string(),
                "snap selections to boxes and edges (G toggles it)",
            ),
            (
                "autosave",
                self.autosave.to_string(),
                "also save a file whenever a capture is copied",
            ),
            (
                "preview",
                number(self.preview),
                "seconds the preview card stays; 0 = a short message",
            ),
            (
                "rewind",
                number(self.rewind),
                "seconds of screen history; 0 turns rewind off",
            ),
            (
                "clip",
                number(self.clip),
                "seconds kept for GIFs, 30 pictures a second; 0 turns it off",
            ),
            (
                "quick",
                self.quick.to_string(),
                "copy as soon as an area is chosen; Shift keeps it open to draw",
            ),
            (
                "links",
                self.links.to_string(),
                "find links and QR codes in captures",
            ),
            (
                "games",
                self.games.to_string(),
                "run as administrator, so the hotkey works over games that do",
            ),
            (
                "rewind_fullscreen",
                self.rewind_fullscreen.to_string(),
                "keep recording while a game or a film fills the screen",
            ),
            (
                "fast_capture",
                self.fast_capture.to_string(),
                "read the screen the quick way where possible; false = always GDI",
            ),
            (
                "cursor",
                self.cursor.to_string(),
                "include the mouse cursor in captures",
            ),
            (
                "history",
                self.history.to_string(),
                "captures kept in history; 0 turns history off",
            ),
            (
                "run_after",
                text(&self.run_after),
                "run after saving, e.g. 'curl -s -F \"file=@{file}\" https://example.com/upload'",
            ),
        ];
        let mut out = String::from(
            "# Bruma settings. Every line is optional: delete one to get its default back.\n\
             # Settings in the tray menu change most of these at once; changes made\n\
             # here take effect the next time Bruma starts.\n\n",
        );
        for (key, value, note) in lines {
            out.push_str(&format!("{key:<9} = {value:<27} # {note}\n"));
        }
        out
    }

    /// Writes these values to the settings file.
    pub fn save(&self) -> Result<PathBuf, String> {
        let path = Self::path().ok_or("no settings folder on this system")?;
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        }
        std::fs::write(&path, self.to_toml()).map_err(|e| format!("{}: {e}", path.display()))?;
        Ok(path)
    }

    /// Opens the settings file in Notepad, creating it with the defaults
    /// first if there is none.
    pub fn open_in_editor() -> Result<PathBuf, String> {
        let path = Self::path().ok_or("no settings folder on this system")?;
        if !path.exists() {
            if let Some(dir) = path.parent() {
                std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
            }
            std::fs::write(&path, Self::default().to_toml())
                .map_err(|e| format!("{}: {e}", path.display()))?;
        }
        std::process::Command::new("notepad")
            .arg(&path)
            .spawn()
            .map_err(|e| format!("could not open the settings file: {e}"))?;
        Ok(path)
    }

    /// `BRUMA_CONFIG` overrides the default location (handy for portable use).
    pub fn path() -> Option<PathBuf> {
        if let Some(custom) = std::env::var_os("BRUMA_CONFIG").filter(|v| !v.is_empty()) {
            return Some(PathBuf::from(custom));
        }
        if let Some(folder) = crate::platform::package_folder() {
            return Some(folder.join("config.toml"));
        }
        let base = directories::BaseDirs::new()?;
        Some(base.config_dir().join("bruma").join("config.toml"))
    }

    /// Reads the settings file. A missing file gives the defaults silently; a
    /// broken one gives the defaults and the reason.
    pub fn load() -> (Self, Option<String>) {
        let Some(path) = Self::path() else {
            return (Self::default(), None);
        };
        match std::fs::read_to_string(&path) {
            Ok(text) => match Self::parse(&text) {
                Ok(config) => (config, None),
                Err(e) => (Self::default(), Some(format!("{}: {e}", path.display()))),
            },
            Err(_) => (Self::default(), None),
        }
    }

    pub fn parse(text: &str) -> Result<Self, String> {
        toml::from_str(text).map_err(|e| e.to_string())
    }

    /// Palette index (0-based) for the configured ink.
    pub fn color_index(&self) -> u8 {
        self.color.clamp(1, 6) - 1
    }

    pub fn save_dir(&self) -> PathBuf {
        let dirs = directories::UserDirs::new();
        if self.save_dir.trim().is_empty() {
            return dirs
                .as_ref()
                .and_then(|d| d.picture_dir().map(Path::to_path_buf))
                .unwrap_or_else(|| PathBuf::from("."))
                .join("bruma");
        }
        let dir = match (self.save_dir.strip_prefix('~'), &dirs) {
            (Some(rest), Some(d)) => d.home_dir().join(rest.trim_start_matches(['/', '\\'])),
            _ => PathBuf::from(&self.save_dir),
        };
        crate::platform::reachable(&dir)
    }

    /// A path in the save folder that does not exist yet.
    pub fn next_path(&self, now: LocalTime) -> PathBuf {
        let dir = self.save_dir();
        let name = expand(&self.filename, now);
        let first = dir.join(&name);
        if !first.exists() {
            return first;
        }
        let (stem, ext) = match name.rsplit_once('.') {
            Some((s, e)) => (s.to_string(), format!(".{e}")),
            None => (name.clone(), String::new()),
        };
        (2..)
            .map(|n| dir.join(format!("{stem}-{n}{ext}")))
            .find(|p| !p.exists())
            .unwrap_or(first)
    }
}

/// Replaces `%Y %m %d %H %M %S` (and `%%`) in a file name pattern.
pub fn expand(pattern: &str, t: LocalTime) -> String {
    let mut out = String::with_capacity(pattern.len() + 8);
    let mut chars = pattern.chars();
    while let Some(c) = chars.next() {
        if c != '%' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('Y') => out.push_str(&format!("{:04}", t.year)),
            Some('m') => out.push_str(&format!("{:02}", t.month)),
            Some('d') => out.push_str(&format!("{:02}", t.day)),
            Some('H') => out.push_str(&format!("{:02}", t.hour)),
            Some('M') => out.push_str(&format!("{:02}", t.minute)),
            Some('S') => out.push_str(&format!("{:02}", t.second)),
            Some('%') => out.push('%'),
            Some(other) => {
                out.push('%');
                out.push(other);
            }
            None => out.push('%'),
        }
    }
    // Characters Windows refuses in file names.
    out.chars()
        .map(|c| {
            if matches!(c, '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*') {
                '-'
            } else {
                c
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const T: LocalTime = LocalTime {
        year: 2026,
        month: 11,
        day: 5,
        hour: 9,
        minute: 3,
        second: 7,
    };

    #[test]
    fn a_written_file_reads_back_the_same() {
        assert_eq!(
            Config::parse(&Config::default().to_toml()).unwrap(),
            Config::default()
        );
        let odd = Config {
            hotkey: "ctrl+shift+KeyS".into(),
            save_dir: r#"C:\Users\someone\Pictures\shots "new""#.into(),
            filename: "shot-%H%M.png".into(),
            stroke: 2.5,
            preview: 0.0,
            rewind: 30.0,
            history: 0,
            run_after: r#"curl -s -F "file=@{file}" https://example.com"#.into(),
            ..Config::default()
        };
        assert_eq!(Config::parse(&odd.to_toml()).unwrap(), odd);
    }

    #[test]
    fn empty_file_gives_defaults() {
        assert_eq!(Config::parse("").unwrap(), Config::default());
    }

    #[test]
    fn partial_file_overrides_only_given_keys() {
        let c = Config::parse("hotkey = \"ctrl+shift+KeyS\"\nstroke = 5").unwrap();
        assert_eq!(c.hotkey, "ctrl+shift+KeyS");
        assert_eq!(c.stroke, 5.0);
        assert_eq!(c.filename, Config::default().filename);
    }

    #[test]
    fn broken_file_is_an_error() {
        assert!(Config::parse("hotkey = [").is_err());
        assert!(Config::parse("stroke = \"thick\"").is_err());
    }

    #[test]
    fn colour_is_clamped_to_palette() {
        let c = Config {
            color: 0,
            ..Config::default()
        };
        assert_eq!(c.color_index(), 0);
        let c = Config {
            color: 99,
            ..Config::default()
        };
        assert_eq!(c.color_index(), 5);
    }

    #[test]
    fn pattern_expands_with_zero_padding() {
        assert_eq!(
            expand("bruma-%Y%m%d-%H%M%S.png", T),
            "bruma-20261105-090307.png"
        );
    }

    #[test]
    fn pattern_keeps_unknown_and_literal_percent() {
        assert_eq!(expand("a%%b%qc%", T), "a%b%qc%");
    }

    #[test]
    fn pattern_strips_path_characters() {
        assert_eq!(expand("%H:%M/x?.png", T), "09-03-x-.png");
    }
}
