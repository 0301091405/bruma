//! Command-line parsing. Small enough not to need a dependency.

use std::path::PathBuf;

#[derive(Clone, Debug, PartialEq)]
pub enum Command {
    /// Stay in the tray and wait for the hotkey.
    Background,
    /// Open the interactive overlay once.
    Capture,
    /// Open the overlay after this many seconds.
    CaptureIn(f32),
    /// Write the screen, or a region of it, to a file without any UI.
    Headless {
        region: Option<Region>,
        out: Option<PathBuf>,
    },
    /// Open the test lab page in the browser.
    Lab,
    /// Start with Windows: switch on, off, or (with `None`) report.
    Autostart(Option<bool>),
    /// Print the text recognised in a PNG file.
    Ocr(PathBuf),
    /// Blur the sensitive text in a PNG file.
    /// Open the history page in the browser.
    History,
    /// Print the captures whose text matches.
    Find(String),
    /// Open a saved capture and edit it again.
    Edit(PathBuf),
    Mask {
        file: PathBuf,
        out: Option<PathBuf>,
    },
    Help,
    /// Time the two ways of reading the screen and compare what they see.
    Probe,
    /// Ask the running Bruma to show its settings.
    Settings,
    /// Ask the running Bruma to quit.
    Quit,
    /// Make (`true`) or remove the task that starts Bruma as administrator.
    /// Run as administrator by Bruma itself, after asking.
    Games(bool),
    /// Run in the background, started by that task: the Bruma that started
    /// it is given a moment to quit first.
    Takeover,
    Version,
}

/// A rectangle in desktop coordinates.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Region {
    pub x: i32,
    pub y: i32,
    pub w: u32,
    pub h: u32,
}

pub const HELP: &str = "\
bruma: freeze, select, annotate, done.

USAGE
  bruma                               run in the background (tray + hotkey)
  bruma capture                       open the capture overlay now
  bruma capture --delay 3             open it in 3 seconds
  bruma capture --full [-o FILE]      save the whole screen, no UI
  bruma capture --region X,Y,W,H [-o FILE]
                                       save a region, no UI
  bruma lab                            open the test lab: a page to try every feature on
  bruma autostart on|off               start with Windows, or not
  bruma ocr FILE.png                   print the text in a PNG
  bruma mask FILE.png [-o OUT.png]     blur the sensitive text in a PNG
  bruma edit FILE.png                  open a saved capture and edit it again
  bruma history                        browse and search past captures
  bruma find WORDS                     list past captures containing the words
  bruma probe                          time the ways of reading the screen
  bruma settings                       show the settings of the running Bruma
  bruma quit                           stop the running Bruma
  bruma --help | --version

Without -o, files go to the configured save folder.

IN THE OVERLAY
  drag            select a region        click     select the box, picture or window
  G               snap to boxes and edges on / off (hold Alt to suspend it)
  F1              show every key
  F               whole monitor          S         shrink selection to its content
  C               copy the colour under the cursor (before selecting)
  PgUp PgDn       rewind: step back and forward through the last seconds
                  (before selecting, the arrow keys and the wheel do the same)
  W               select what changed (what just appeared or vanished)
  X               copy the text in the selection
  M               mask sensitive text: addresses, numbers, keys
  V A L R E       select, arrow, line, rectangle, ellipse
  D H T N B O     pen, highlighter, text, numbered step, blur, focus
  1-6             ink                    [ ] wheel stroke width
  arrows          nudge (Shift = 10 px, Ctrl = resize)
  Ctrl+Z / Ctrl+Y undo / redo            Del       delete the selected annotation
  Enter, Ctrl+C   copy                   Ctrl+S    save        P    pin to screen
  Esc             step back, then close
";

fn parse_region(text: &str) -> Result<Region, String> {
    let parts: Vec<&str> = text.split(',').map(str::trim).collect();
    let [x, y, w, h] = parts[..] else {
        return Err(format!("--region expects X,Y,W,H, got '{text}'"));
    };
    let bad = |what: &str| format!("--region: '{what}' is not a whole number");
    let region = Region {
        x: x.parse().map_err(|_| bad(x))?,
        y: y.parse().map_err(|_| bad(y))?,
        w: w.parse().map_err(|_| bad(w))?,
        h: h.parse().map_err(|_| bad(h))?,
    };
    if region.w == 0 || region.h == 0 {
        return Err("--region: width and height must be greater than zero".into());
    }
    Ok(region)
}

pub fn parse(args: &[String]) -> Result<Command, String> {
    let mut it = args.iter().map(String::as_str);
    match it.next() {
        None => Ok(Command::Background),
        Some("-h" | "--help" | "help") => Ok(Command::Help),
        Some("-V" | "--version" | "version") => Ok(Command::Version),
        Some("probe") => Ok(Command::Probe),
        Some("settings") => Ok(Command::Settings),
        Some("quit") => Ok(Command::Quit),
        Some("takeover") => Ok(Command::Takeover),
        Some("games") => match it.next() {
            Some("on") => Ok(Command::Games(true)),
            Some("off") => Ok(Command::Games(false)),
            _ => Err("games takes 'on' or 'off'".into()),
        },
        Some("lab") => Ok(Command::Lab),
        Some("autostart") => match it.next() {
            None | Some("status") => Ok(Command::Autostart(None)),
            Some("on") => Ok(Command::Autostart(Some(true))),
            Some("off") => Ok(Command::Autostart(Some(false))),
            Some(other) => Err(format!("autostart takes 'on' or 'off', not '{other}'")),
        },
        Some("ocr") => match it.next() {
            Some(file) => Ok(Command::Ocr(PathBuf::from(file))),
            None => Err("ocr needs a PNG file".into()),
        },
        Some("history") => Ok(Command::History),
        Some("find") => {
            let words: Vec<&str> = it.collect();
            if words.is_empty() {
                return Err("find needs something to look for".into());
            }
            Ok(Command::Find(words.join(" ")))
        }
        Some("edit") => match it.next() {
            Some(file) => Ok(Command::Edit(PathBuf::from(file))),
            None => Err("edit needs a PNG file".into()),
        },
        Some("mask") => {
            let file = it.next().ok_or("mask needs a PNG file")?;
            let out = match (it.next(), it.next()) {
                (None, _) => None,
                (Some("-o" | "--output"), Some(path)) => Some(PathBuf::from(path)),
                (Some("-o" | "--output"), None) => return Err("-o needs a file path".into()),
                (Some(other), _) => return Err(format!("unknown option '{other}'")),
            };
            Ok(Command::Mask {
                file: PathBuf::from(file),
                out,
            })
        }
        Some("capture") => {
            let mut full = false;
            let mut region = None;
            let mut out = None;
            let mut delay = None;
            while let Some(arg) = it.next() {
                match arg {
                    "--full" => full = true,
                    "--delay" => {
                        let value = it.next().ok_or("--delay needs a number of seconds")?;
                        let seconds: f32 = value
                            .parse()
                            .map_err(|_| format!("--delay: '{value}' is not a number"))?;
                        if !(0.0..=120.0).contains(&seconds) {
                            return Err("--delay must be between 0 and 120 seconds".into());
                        }
                        delay = Some(seconds);
                    }
                    "--region" => {
                        let value = it.next().ok_or("--region needs a value: X,Y,W,H")?;
                        region = Some(parse_region(value)?);
                    }
                    "-o" | "--output" => {
                        let value = it.next().ok_or("-o needs a file path")?;
                        out = Some(PathBuf::from(value));
                    }
                    other => return Err(format!("unknown option '{other}'")),
                }
            }
            match (full, region, out) {
                (true, Some(_), _) => Err("use either --full or --region, not both".into()),
                (false, None, Some(_)) => Err("-o needs --full or --region".into()),
                (false, None, None) => Ok(delay.map_or(Command::Capture, Command::CaptureIn)),
                (_, region, out) => Ok(Command::Headless { region, out }),
            }
        }
        Some(other) => Err(format!("unknown command '{other}'")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(s: &str) -> Vec<String> {
        s.split_whitespace().map(String::from).collect()
    }

    #[test]
    fn no_arguments_runs_in_background() {
        assert_eq!(parse(&[]), Ok(Command::Background));
    }

    #[test]
    fn capture_alone_is_interactive() {
        assert_eq!(parse(&args("capture")), Ok(Command::Capture));
    }

    #[test]
    fn full_with_output() {
        assert_eq!(
            parse(&args("capture --full -o a.png")),
            Ok(Command::Headless {
                region: None,
                out: Some("a.png".into())
            })
        );
    }

    #[test]
    fn region_is_parsed_including_negative_origin() {
        assert_eq!(
            parse(&args("capture --region -1920,0,800,600")),
            Ok(Command::Headless {
                region: Some(Region {
                    x: -1920,
                    y: 0,
                    w: 800,
                    h: 600
                }),
                out: None
            })
        );
    }

    #[test]
    fn bad_input_is_reported() {
        assert!(parse(&args("capture --region 1,2,3")).is_err());
        assert!(parse(&args("capture --region 1,2,0,5")).is_err());
        assert!(parse(&args("capture --region a,b,c,d")).is_err());
        assert!(parse(&args("capture --full --region 0,0,1,1")).is_err());
        assert!(parse(&args("capture -o a.png")).is_err());
        assert!(parse(&args("capture -o")).is_err());
        assert!(parse(&args("capture --wat")).is_err());
        assert!(parse(&args("launch")).is_err());
    }

    #[test]
    fn lab_autostart_and_ocr() {
        assert_eq!(parse(&args("lab")), Ok(Command::Lab));
        assert_eq!(parse(&args("quit")), Ok(Command::Quit));
        assert_eq!(parse(&args("takeover")), Ok(Command::Takeover));
        assert_eq!(parse(&args("games on")), Ok(Command::Games(true)));
        assert_eq!(parse(&args("games off")), Ok(Command::Games(false)));
        assert!(parse(&args("games")).is_err());
        assert_eq!(parse(&args("autostart")), Ok(Command::Autostart(None)));
        assert_eq!(
            parse(&args("autostart on")),
            Ok(Command::Autostart(Some(true)))
        );
        assert_eq!(
            parse(&args("autostart off")),
            Ok(Command::Autostart(Some(false)))
        );
        assert!(parse(&args("autostart maybe")).is_err());
        assert_eq!(parse(&args("ocr a.png")), Ok(Command::Ocr("a.png".into())));
        assert!(parse(&args("ocr")).is_err());
        assert_eq!(
            parse(&args("mask a.png")),
            Ok(Command::Mask {
                file: "a.png".into(),
                out: None
            })
        );
        assert_eq!(
            parse(&args("mask a.png -o b.png")),
            Ok(Command::Mask {
                file: "a.png".into(),
                out: Some("b.png".into())
            })
        );
        assert!(parse(&args("mask")).is_err());
        assert!(parse(&args("mask a.png -o")).is_err());
        assert!(parse(&args("mask a.png --wat")).is_err());
    }

    #[test]
    fn history_find_and_edit() {
        assert_eq!(parse(&args("history")), Ok(Command::History));
        assert_eq!(
            parse(&args("find invoice 2041")),
            Ok(Command::Find("invoice 2041".into()))
        );
        assert!(parse(&args("find")).is_err());
        assert_eq!(
            parse(&args("edit a.png")),
            Ok(Command::Edit("a.png".into()))
        );
        assert!(parse(&args("edit")).is_err());
    }

    #[test]
    fn capture_can_be_delayed() {
        assert_eq!(
            parse(&args("capture --delay 3")),
            Ok(Command::CaptureIn(3.0))
        );
        assert!(parse(&args("capture --delay")).is_err());
        assert!(parse(&args("capture --delay soon")).is_err());
        assert!(parse(&args("capture --delay 900")).is_err());
    }

    #[test]
    fn help_and_version() {
        assert_eq!(parse(&args("--help")), Ok(Command::Help));
        assert_eq!(parse(&args("-V")), Ok(Command::Version));
        assert_eq!(parse(&args("probe")), Ok(Command::Probe));
        assert_eq!(parse(&args("settings")), Ok(Command::Settings));
    }
}
