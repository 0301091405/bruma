//! bruma: freeze, select, annotate, done.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod cli;
mod config;
mod history;
mod lab;
mod overlay;
mod pin;
mod platform;
mod rewind;
mod scroll;
mod stage;
mod toast;
mod tray;
mod traymenu;

use std::path::PathBuf;
use std::process::ExitCode;

use bruma_core::Rect;

use crate::cli::{Command, Region};
use crate::config::Config;

/// Whether `BRUMA_TRACE` asks for timings.
fn tracing() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("BRUMA_TRACE").is_some())
}

/// Lines waiting to be printed. Printing takes milliseconds; doing it while
/// timing would measure the printing.
static TRACE: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());

/// Records a timing line when `BRUMA_TRACE` is set; [`flush_trace`] prints it.
pub fn trace(label: &str, since: std::time::Instant) {
    if tracing() {
        let line = format!(
            "[bruma] {label}: {:.1} ms",
            since.elapsed().as_secs_f64() * 1000.0
        );
        if let Ok(mut lines) = TRACE.lock() {
            lines.push(line);
        }
    }
}

/// Records a diagnostic line when `BRUMA_TRACE` is set.
pub fn note(message: impl FnOnce() -> String) {
    if tracing() {
        let line = format!("[bruma] {}", message());
        if let Ok(mut lines) = TRACE.lock() {
            lines.push(line);
        }
    }
}

/// Prints what [`trace`] and [`note`] recorded.
pub fn flush_trace() {
    if let Ok(mut lines) = TRACE.lock() {
        for line in lines.drain(..) {
            eprintln!("{line}");
        }
    }
}

/// Writes an image as PNG, creating the folder if needed.
pub fn write_png(image: &bruma_core::Image, path: &std::path::Path) -> Result<(), String> {
    write_png_with(image, path, None)
}

/// [`write_png`] with a text field inside the file: what Bruma needs to edit
/// the capture again.
pub fn write_png_with(
    image: &bruma_core::Image,
    path: &std::path::Path,
    meta: Option<&str>,
) -> Result<(), String> {
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    }
    let png = bruma_core::export::encode_png_with(image, meta).map_err(|e| e.to_string())?;
    std::fs::write(path, png).map_err(|e| format!("{}: {e}", path.display()))
}

/// Reads a capture to edit: the picture as it was and its drawings, or for
/// any other PNG just the picture.
fn read_editable(file: &std::path::Path) -> Result<app::EditJob, String> {
    let bytes = std::fs::read(file).map_err(|e| format!("{}: {e}", file.display()))?;
    let (image, meta) = bruma_core::export::decode_png_meta(&bytes)
        .map_err(|e| format!("{}: {e}", file.display()))?;
    let (original, items) = match meta
        .as_deref()
        .and_then(bruma_core::export::unpack_editable)
    {
        Some(editable) => (editable.original, editable.items),
        None => (image, Vec::new()),
    };
    Ok(app::EditJob {
        path: file.to_path_buf(),
        original,
        items,
    })
}

/// Reads the screen both ways, a few times each, and reports how long they
/// take and whether they show the same picture.
fn probe() -> Result<String, String> {
    use std::fmt::Write;
    use std::time::{Duration, Instant};
    let ms = |since: Instant| since.elapsed().as_secs_f64() * 1000.0;
    let median = |mut times: Vec<f64>| {
        times.sort_by(|a, b| a.total_cmp(b));
        times[times.len() / 2]
    };
    let image = |raw: platform::Raw| bruma_core::Image::from_bgrx(raw.width, raw.height, raw.bytes);
    let mut out = String::new();

    let mut grabber = platform::Grabber::new()?;
    let mut times = Vec::new();
    for _ in 0..5 {
        let started = Instant::now();
        grabber.grab()?;
        times.push(ms(started));
    }
    let _ = writeln!(out, "GDI: {:.1} ms a read", median(times));

    let Some(mut duplicator) = platform::Duplicator::new() else {
        let _ = writeln!(out, "Desktop Duplication: not to be had here; GDI is used");
        return Ok(out);
    };
    let lost = "Desktop Duplication: lost while reading; GDI is used";
    let started = Instant::now();
    duplicator.refresh().map_err(|()| lost)?;
    let _ = writeln!(
        out,
        "Desktop Duplication: first picture after {:.1} ms",
        ms(started)
    );
    let (mut same, mut changed) = (Vec::new(), Vec::new());
    for _ in 0..10 {
        std::thread::sleep(Duration::from_millis(100));
        let started = Instant::now();
        match duplicator.refresh().map_err(|()| lost)? {
            true => changed.push(ms(started)),
            false => same.push(ms(started)),
        }
    }
    if !changed.is_empty() {
        let n = changed.len();
        let _ = writeln!(
            out,
            "  {:.1} ms a read when the screen changed ({n} of 10)",
            median(changed)
        );
    }
    if !same.is_empty() {
        let n = same.len();
        let _ = writeln!(
            out,
            "  {:.2} ms a read when it had not ({n} of 10)",
            median(same)
        );
    }
    // The same moment, as nearly as two reads can be, seen both ways.
    duplicator.refresh().map_err(|()| lost)?;
    let one = image(duplicator.raw());
    let other = image(grabber.grab()?);
    let verdict = rewind::difference(&one, &other);
    let verdict = verdict.strip_prefix("changed: ").unwrap_or(&verdict);
    let _ = writeln!(out, "  against GDI a moment later: {verdict}");
    Ok(out)
}

fn headless(
    region: Option<Region>,
    out: Option<PathBuf>,
    config: &Config,
) -> Result<PathBuf, String> {
    let started = std::time::Instant::now();
    let capture = platform::capture()?;
    trace("capture", started);
    let image = match region {
        None => capture.image,
        Some(r) => {
            let rect = Rect::new(
                (r.x - capture.origin.0) as f32,
                (r.y - capture.origin.1) as f32,
                r.w as f32,
                r.h as f32,
            );
            capture.image.crop(rect)
        }
    };
    if image.width == 0 || image.height == 0 {
        return Err("the region lies outside the screen".into());
    }
    let path = out.unwrap_or_else(|| config.next_path(platform::local_time()));
    write_png(&image, &path)?;
    trace("capture + encode + write", started);
    Ok(path)
}

fn read_png(file: &std::path::Path) -> Result<bruma_core::Image, String> {
    let bytes = std::fs::read(file).map_err(|e| format!("{}: {e}", file.display()))?;
    bruma_core::export::decode_png(&bytes).map_err(|e| format!("{}: {e}", file.display()))
}

/// Blurs the sensitive text found in a PNG and writes the result.
fn mask(file: &std::path::Path, out: Option<PathBuf>) -> Result<String, String> {
    use bruma_core::doc::{Annotation, Shape};
    let image = read_png(file)?;
    let words = platform::ocr(&image)?;
    let found = bruma_core::redact::sensitive(&words);
    let masks: Vec<Annotation> = found
        .iter()
        .map(|rect| Annotation {
            shape: Shape::Blur {
                rect: rect.inflate(2.0),
            },
            color: 0,
            stroke: 3.0,
        })
        .collect();
    let fonts = bruma_core::text::Fonts::empty();
    let result = bruma_core::export::flatten(&image, &masks, image.bounds(), &fonts);
    let out = out.unwrap_or_else(|| {
        let stem = file
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        file.with_file_name(format!("{stem}-masked.png"))
    });
    write_png(&result, &out)?;
    Ok(format!(
        "{} masked, written to {}",
        found.len(),
        out.display()
    ))
}

fn main() -> ExitCode {
    platform::attach_console();
    platform::init_dpi();

    let args: Vec<String> = std::env::args().skip(1).collect();
    let command = match cli::parse(&args) {
        Ok(command) => command,
        Err(message) => {
            eprintln!("bruma: {message}\nTry 'bruma --help'.");
            return ExitCode::from(2);
        }
    };

    let (config, config_error) = Config::load();
    if let Some(problem) = &config_error {
        eprintln!("bruma: ignoring settings file, using defaults ({problem})");
    }

    platform::set_capture_cursor(config.cursor);
    let result = match command {
        Command::Help => {
            print!("{}", cli::HELP);
            Ok(())
        }
        Command::Version => {
            println!("bruma {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        Command::Probe => probe().map(|report| print!("{report}")),
        Command::Settings => {
            if platform::request_settings() {
                Ok(())
            } else {
                Err("Bruma is not running; start it, then try again".into())
            }
        }
        Command::Lab => lab::open().map(|path| println!("{}", path.display())),
        Command::Autostart(None) => {
            let state = if platform::autostart_enabled() {
                "on"
            } else {
                "off"
            };
            println!("start with Windows: {state}");
            Ok(())
        }
        Command::Autostart(Some(on)) => platform::set_autostart(on).map(|()| {
            println!("start with Windows: {}", if on { "on" } else { "off" });
        }),
        Command::Ocr(file) => read_png(&file)
            .and_then(|image| platform::ocr(&image))
            .map(|words| println!("{}", bruma_core::redact::text(&words))),
        Command::Mask { file, out } => mask(&file, out).map(|report| println!("{report}")),
        Command::History => history::open().map(|path| println!("{}", path.display())),
        Command::Find(query) => {
            let dir = history::dir().unwrap_or_default();
            for entry in history::find(&query) {
                println!("{}\t{}", dir.join(&entry.file).display(), entry.when);
            }
            Ok(())
        }
        Command::Edit(file) => {
            read_editable(&file).and_then(|job| app::run(false, config, Some(job)))
        }
        Command::Headless { region, out } => {
            headless(region, out, &config).map(|path| println!("{}", path.display()))
        }
        // A running background process takes the request: it has the
        // screen's recent past. Otherwise capture on our own.
        // (`BRUMA_STANDALONE` keeps the capture in this process, for testing.)
        Command::Capture
            if std::env::var_os("BRUMA_STANDALONE").is_none() && platform::request_capture() =>
        {
            Ok(())
        }
        Command::CaptureIn(seconds) => {
            std::thread::sleep(std::time::Duration::from_secs_f32(
                seconds.clamp(0.0, 120.0),
            ));
            if platform::request_capture() {
                Ok(())
            } else {
                app::run(false, config, None)
            }
        }
        Command::Capture => app::run(false, config, None),
        Command::Takeover => {
            // The Bruma that started the task may take a moment to quit.
            let mut lock = None;
            for _ in 0..50 {
                lock = platform::SingleInstance::acquire();
                if lock.is_some() {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
            match lock {
                Some(_lock) => app::run(true, config, None),
                None => Err("already running in the background (look for the tray icon)".into()),
            }
        }
        Command::Quit => {
            if platform::request_quit() {
                // Done when it has let go of being the one in the background.
                for _ in 0..50 {
                    if platform::SingleInstance::acquire().is_some() {
                        break;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(100));
                }
            }
            Ok(())
        }
        Command::Games(true) => std::env::current_exe()
            .map_err(|e| e.to_string())
            .and_then(|exe| platform::create_games_task(&exe)),
        Command::Games(false) => {
            platform::delete_games_task();
            Ok(())
        }
        Command::Background => match platform::SingleInstance::acquire() {
            // Set to run as administrator: the task starts that Bruma, and
            // this one makes way for it.
            Some(lock)
                if config.games
                    && !platform::running_as_administrator()
                    && std::env::current_exe()
                        .is_ok_and(|exe| platform::games_task_starts(&exe)) =>
            {
                drop(lock);
                if platform::start_games_task() {
                    Ok(())
                } else {
                    match platform::SingleInstance::acquire() {
                        Some(_lock) => app::run(true, config, None),
                        None => Ok(()),
                    }
                }
            }
            Some(_lock) => app::run(true, config, None),
            // Already running. Starting it again, from the Start menu say,
            // is someone asking for it: open a capture.
            None if platform::request_capture() => Ok(()),
            None => Err("already running in the background (look for the tray icon)".into()),
        },
    };

    flush_trace();
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("bruma: {message}");
            ExitCode::FAILURE
        }
    }
}
