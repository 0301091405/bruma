//! History: the last captures, kept on disk and searchable by the text in
//! them.
//!
//! Each capture is a PNG in the history folder plus one line in `index.tsv`.
//! The text comes from the same recognition that masking uses, so "invoice"
//! finds the screenshot of the invoice. Everything stays on this machine.

use std::io::Write;
use std::path::PathBuf;

use bruma_core::Image;

use crate::config::LocalTime;
use crate::platform;

#[derive(Clone, Debug, PartialEq)]
pub struct Entry {
    pub file: String,
    pub width: u32,
    pub height: u32,
    /// `2026-11-26 14:03`, local time.
    pub when: String,
    /// The text recognised in the capture, on one line.
    pub text: String,
}

/// Where history lives: `%LOCALAPPDATA%\Bruma\history`, or the package's
/// own folder for a packaged install.
pub fn dir() -> Option<PathBuf> {
    if let Some(folder) = crate::platform::package_folder() {
        return Some(folder.join("history"));
    }
    Some(
        directories::BaseDirs::new()?
            .data_local_dir()
            .join("Bruma")
            .join("history"),
    )
}

fn one_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn line(entry: &Entry) -> String {
    format!(
        "{}\t{}\t{}\t{}\t{}",
        entry.file,
        entry.width,
        entry.height,
        entry.when,
        one_line(&entry.text)
    )
}

fn parse_line(line: &str) -> Option<Entry> {
    let mut parts = line.splitn(5, '\t');
    Some(Entry {
        file: parts.next().filter(|f| !f.is_empty())?.to_string(),
        width: parts.next()?.parse().ok()?,
        height: parts.next()?.parse().ok()?,
        when: parts.next()?.to_string(),
        text: parts.next().unwrap_or("").to_string(),
    })
}

/// Every capture in history, newest first.
pub fn entries() -> Vec<Entry> {
    let Some(dir) = dir() else {
        return Vec::new();
    };
    let text = std::fs::read_to_string(dir.join("index.tsv")).unwrap_or_default();
    let mut all: Vec<Entry> = text.lines().filter_map(parse_line).collect();
    all.reverse();
    all
}

/// Whether every word of `query` appears in the entry's text or date.
fn matches(entry: &Entry, query: &str) -> bool {
    let hay = format!("{} {}", entry.text, entry.when).to_lowercase();
    query
        .split_whitespace()
        .all(|word| hay.contains(&word.to_lowercase()))
}

/// Captures whose text or date contains every word of `query`, newest first.
pub fn find(query: &str) -> Vec<Entry> {
    entries()
        .into_iter()
        .filter(|e| matches(e, query))
        .collect()
}

/// Adds a capture to history in the background and drops the oldest ones
/// beyond `keep`. With `keep` at zero, history is off and nothing is written.
pub fn record(image: Image, keep: usize, now: LocalTime) {
    if keep == 0 || image.width == 0 || image.height == 0 {
        return;
    }
    let Some(dir) = dir() else {
        return;
    };
    let _ = std::thread::Builder::new()
        .name("bruma-history".into())
        .spawn(move || {
            bruma_core::par::keep_jobs_on_this_thread();
            if std::fs::create_dir_all(&dir).is_err() {
                return;
            }
            let stamp = format!(
                "{:04}{:02}{:02}-{:02}{:02}{:02}",
                now.year, now.month, now.day, now.hour, now.minute, now.second
            );
            let mut file = format!("{stamp}.png");
            let mut n = 2;
            while dir.join(&file).exists() {
                file = format!("{stamp}-{n}.png");
                n += 1;
            }
            if crate::write_png(&image, &dir.join(&file)).is_err() {
                return;
            }
            let text = platform::ocr(&image)
                .map(|words| bruma_core::redact::text(&words))
                .unwrap_or_default();
            let entry = Entry {
                file,
                width: image.width,
                height: image.height,
                when: format!(
                    "{:04}-{:02}-{:02} {:02}:{:02}",
                    now.year, now.month, now.day, now.hour, now.minute
                ),
                text,
            };
            let index = dir.join("index.tsv");
            if let Ok(mut out) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&index)
            {
                let _ = writeln!(out, "{}", line(&entry));
            }
            // Keep only the newest `keep` captures.
            let all = std::fs::read_to_string(&index).unwrap_or_default();
            let lines: Vec<&str> = all.lines().collect();
            if lines.len() > keep {
                let (old, kept) = lines.split_at(lines.len() - keep);
                for entry in old.iter().filter_map(|l| parse_line(l)) {
                    let _ = std::fs::remove_file(dir.join(entry.file));
                }
                let _ = std::fs::write(&index, kept.join("\n") + "\n");
            }
        });
}

/// Text made safe to sit inside a JavaScript string in an HTML page.
fn js(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    for c in text.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '<' => out.push_str("\\u003c"),
            '>' => out.push_str("\\u003e"),
            '&' => out.push_str("\\u0026"),
            c if c.is_control() => out.push(' '),
            c => out.push(c),
        }
    }
    out
}

const PAGE_HEAD: &str = r#"<!doctype html>
<html lang="en"><head><meta charset="utf-8"><title>Bruma history</title>
<style>
  :root { --ink:#0e0e0e; --panel:#171717; --line:#2a2a28; --paper:#f2f0eb; --faint:#9a988f; --accent:#ff4f1f;
          --mono:"Cascadia Mono",Consolas,monospace; --sans:"Segoe UI Variable","Segoe UI",system-ui,sans-serif; }
  * { box-sizing:border-box; } body { margin:0; background:var(--ink); color:var(--paper); font:15px/1.5 var(--sans); }
  header { position:sticky; top:0; background:#0e0e0eee; border-bottom:1px solid var(--line); padding:18px 28px; display:flex; gap:18px; align-items:center; }
  h1 { font-size:18px; font-weight:600; margin:0; } .n { font:12px var(--mono); color:var(--faint); }
  input { flex:1; max-width:520px; background:#101010; border:1px solid var(--line); border-radius:9px; padding:9px 13px; color:inherit; font:inherit; outline:none; }
  input:focus { border-color:var(--faint); }
  main { display:grid; grid-template-columns:repeat(auto-fill,minmax(260px,1fr)); gap:18px; padding:24px 28px 80px; }
  a.card { display:block; background:var(--panel); border:1px solid var(--line); border-radius:12px; overflow:hidden; color:inherit; text-decoration:none; }
  a.card:hover { border-color:var(--faint); }
  .pic { height:170px; background:#0b0b0b; display:grid; place-items:center; } .pic img { max-width:100%; max-height:170px; display:block; }
  .meta { padding:10px 12px; } .when { font:12px var(--mono); color:var(--faint); display:flex; justify-content:space-between; }
  .text { font-size:13px; color:#cfcdc6; margin-top:6px; display:-webkit-box; -webkit-line-clamp:2; -webkit-box-orient:vertical; overflow:hidden; }
  mark { background:none; color:var(--accent); } .none { color:var(--faint); padding:40px 28px; }
</style></head><body>
<header><h1>Bruma history</h1><input id="q" placeholder="Search the text in your captures" autofocus><span class="n" id="n"></span></header>
<main id="grid"></main><div class="none" id="none" hidden>Nothing matches.</div>
<script>
const items = ["#;

const PAGE_TAIL: &str = r#"];
const grid = document.getElementById('grid'), q = document.getElementById('q');
const esc = (s) => s.replace(/[&<>"]/g, (c) => ({'&':'&amp;','<':'&lt;','>':'&gt;','"':'&quot;'}[c]));
function draw() {
  const words = q.value.toLowerCase().split(/\s+/).filter(Boolean);
  const shown = items.filter((i) => words.every((w) => (i.t + ' ' + i.d).toLowerCase().includes(w)));
  grid.innerHTML = shown.map((i) => {
    let text = esc(i.t);
    for (const w of words) text = text.replace(new RegExp(w.replace(/[.*+?^${}()|[\]\\]/g, '\\$&'), 'gi'), (m) => '<mark>' + m + '</mark>');
    return '<a class="card" href="' + esc(i.f) + '"><div class="pic"><img loading="lazy" src="' + esc(i.f) + '"></div>' +
      '<div class="meta"><div class="when"><span>' + esc(i.d) + '</span><span>' + i.w + ' × ' + i.h + '</span></div>' +
      '<div class="text">' + (text || '&nbsp;') + '</div></div></a>';
  }).join('');
  document.getElementById('n').textContent = shown.length + ' of ' + items.length;
  document.getElementById('none').hidden = shown.length > 0;
}
q.addEventListener('input', draw); draw();
</script></body></html>
"#;

/// The history page: every capture as a card, with a search box.
fn page(entries: &[Entry]) -> String {
    let rows: Vec<String> = entries
        .iter()
        .map(|e| {
            format!(
                "{{f:\"{}\",w:{},h:{},d:\"{}\",t:\"{}\"}}",
                js(&e.file),
                e.width,
                e.height,
                js(&e.when),
                js(&e.text)
            )
        })
        .collect();
    format!("{PAGE_HEAD}{}{PAGE_TAIL}", rows.join(","))
}

/// Writes the history page next to the captures and opens it in the browser.
pub fn open() -> Result<PathBuf, String> {
    let dir = dir().ok_or("no folder for history on this system")?;
    std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let path = dir.join("history.html");
    std::fs::write(&path, page(&entries())).map_err(|e| format!("{}: {e}", path.display()))?;
    std::process::Command::new("explorer")
        .arg(&path)
        .spawn()
        .map_err(|e| format!("could not open the history page: {e}"))?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(text: &str) -> Entry {
        Entry {
            file: "20261126-140307.png".into(),
            width: 640,
            height: 480,
            when: "2026-11-26 14:03".into(),
            text: text.into(),
        }
    }

    #[test]
    fn an_index_line_reads_back_as_it_was_written() {
        let e = entry("Invoice 2041 total 380");
        assert_eq!(parse_line(&line(&e)), Some(e));
        assert_eq!(parse_line(&line(&entry(""))), Some(entry("")));
    }

    #[test]
    fn text_with_tabs_and_line_breaks_stays_on_one_line() {
        let written = line(&entry("first\tsecond\nthird   fourth"));
        assert_eq!(written.lines().count(), 1);
        assert_eq!(
            parse_line(&written).unwrap().text,
            "first second third fourth"
        );
    }

    #[test]
    fn broken_index_lines_are_skipped() {
        assert_eq!(parse_line(""), None);
        assert_eq!(parse_line("a.png\twide\t10\twhen\ttext"), None);
        assert_eq!(parse_line("a.png\t10"), None);
    }

    #[test]
    fn search_needs_every_word_and_ignores_case() {
        let e = entry("Invoice 2041 Total 380");
        assert!(matches(&e, "invoice"));
        assert!(matches(&e, "TOTAL invoice"));
        assert!(matches(&e, "2026-11-26"), "dates are searchable too");
        assert!(matches(&e, ""));
        assert!(!matches(&e, "invoice receipt"));
    }

    #[test]
    fn the_page_cannot_be_broken_by_what_a_capture_says() {
        let nasty = entry("</script><script>alert(1)</script> \"quoted\" back\\slash");
        let html = page(&[nasty]);
        assert_eq!(
            html.matches("</script>").count(),
            1,
            "only the page's own closing tag"
        );
        assert!(html.contains("\\u003c/script\\u003e"));
        assert!(html.contains("\\\"quoted\\\""));
        assert!(
            !html.contains("http://") && !html.contains("https://"),
            "no network resources"
        );
    }

    #[test]
    fn an_empty_history_still_makes_a_page() {
        assert!(page(&[]).contains("const items = [];"));
    }
}
