//! Finding text that should not leave your screen: addresses, numbers, keys.
//!
//! The words come from text recognition, which lives outside this crate. What
//! counts as sensitive is decided here, by a handful of plain rules. They are
//! heuristics: they catch the common cases, not everything.

use crate::geom::Rect;

/// One recognised word and where it is.
#[derive(Clone, Debug, PartialEq)]
pub struct Word {
    pub text: String,
    pub rect: Rect,
    /// Which line of text the word belongs to.
    pub line: u32,
}

/// Puts words into reading order: rows by vertical position, then left to
/// right, and numbers the rows.
///
/// Text recognition groups words into "lines" of its own, and a form with a
/// column of labels and a column of values often comes back as two columns.
/// Position on screen is what a reader sees, so position is what counts here.
pub fn arrange(words: &[Word]) -> Vec<Word> {
    let mut sorted = words.to_vec();
    sorted.sort_by(|a, b| a.rect.center().y.total_cmp(&b.rect.center().y));
    let mut rows: Vec<Vec<Word>> = Vec::new();
    for word in sorted {
        let centre = word.rect.center().y;
        match rows.last_mut() {
            Some(row)
                if (centre - row[0].rect.center().y).abs()
                    <= 0.6 * row[0].rect.h.max(word.rect.h) =>
            {
                row.push(word)
            }
            _ => rows.push(vec![word]),
        }
    }
    let mut out = Vec::with_capacity(words.len());
    for (line, mut row) in rows.into_iter().enumerate() {
        row.sort_by(|a, b| a.rect.x.total_cmp(&b.rect.x));
        out.extend(row.into_iter().map(|mut word| {
            word.line = line as u32;
            word
        }));
    }
    out
}

/// The recognised text in reading order, one row per line.
pub fn text(words: &[Word]) -> String {
    let mut out = String::new();
    let mut line = None;
    for word in arrange(words) {
        match line {
            Some(l) if l == word.line => out.push(' '),
            Some(_) => out.push('\n'),
            None => {}
        }
        out.push_str(&word.text);
        line = Some(word.line);
    }
    out
}

const SECRET_PREFIXES: &[&str] = &[
    "sk-",
    "sk_",
    "pk_",
    "rk_",
    "ghp_",
    "gho_",
    "ghs_",
    "github_pat_",
    "xoxb-",
    "xoxp-",
    "AKIA",
    "eyJ",
    "AIza",
    "glpat-",
];

/// Words that announce a secret: the word after them is masked.
const LABELS: &[&str] = &[
    "password",
    "passwd",
    "pwd",
    "passwort",
    "kennwort",
    "contraseña",
    "contrasena",
    "senha",
    "wachtwoord",
    "hasło",
    "haslo",
    "şifre",
    "sifre",
    "parola",
    "token",
    "secret",
    "apikey",
    "api-key",
    "api_key",
    "pin",
];

fn trimmed(word: &str) -> &str {
    word.trim_matches(|c: char| ",;:()[]{}<>\"'`".contains(c))
}

fn is_email(t: &str) -> bool {
    match t.split_once('@') {
        // Recognition may cut the address after a dot ("name@example. com"),
        // so a host that merely contains a dot is enough.
        Some((name, host)) => !name.is_empty() && host.len() >= 3 && host[1..].contains('.'),
        None => false,
    }
}

fn is_ipv4(t: &str) -> bool {
    let parts: Vec<&str> = t.split('.').collect();
    parts.len() == 4
        && parts.iter().all(|p| {
            !p.is_empty()
                && p.len() <= 3
                && p.bytes().all(|b| b.is_ascii_digit())
                && p.parse::<u16>().is_ok_and(|n| n <= 255)
        })
}

/// Phone numbers, card numbers, national IDs written without spaces.
fn is_long_number(t: &str) -> bool {
    let digits = t.bytes().filter(u8::is_ascii_digit).count();
    digits >= 9
        && t.chars()
            .all(|c| c.is_ascii_digit() || "+-.()/".contains(c))
}

fn is_iban(t: &str) -> bool {
    let bytes = t.as_bytes();
    bytes.len() >= 15
        && bytes[..2].iter().all(u8::is_ascii_uppercase)
        && bytes.iter().all(u8::is_ascii_alphanumeric)
        && bytes.iter().filter(|b| b.is_ascii_digit()).count() >= 13
}

/// API keys, tokens, hashes: long, unbroken, letters and digits mixed.
fn is_secret(t: &str) -> bool {
    if t.len() >= 12 && SECRET_PREFIXES.iter().any(|p| t.starts_with(p)) {
        return true;
    }
    let digits = t.bytes().filter(u8::is_ascii_digit).count();
    let letters = t.bytes().filter(u8::is_ascii_alphabetic).count();
    t.len() >= 24
        && digits >= 4
        && letters >= 4
        && t.chars()
            .all(|c| c.is_ascii_alphanumeric() || "_-+/=.".contains(c))
}

/// One piece of a number written with spaces: `0207`, `45`, `+44`.
/// Returns how many digits it has.
fn digit_group(t: &str) -> Option<usize> {
    let digits = t.strip_prefix('+').unwrap_or(t);
    ((2..=4).contains(&digits.len()) && digits.bytes().all(|b| b.is_ascii_digit()))
        .then_some(digits.len())
}

/// `Password:` or `token=`: a known label that ends in a colon or equals sign.
fn is_label(raw: &str) -> bool {
    let stem = raw.trim_end_matches(['=', ':']);
    stem.len() < raw.len() && LABELS.contains(&trimmed(stem).to_lowercase().as_str())
}

/// Rectangles covering the sensitive words, with neighbours on a row merged.
pub fn sensitive(words: &[Word]) -> Vec<Rect> {
    let words = arrange(words);
    let same_row = |i: usize, j: usize| words.get(j).is_some_and(|w| w.line == words[i].line);
    let mut hit = vec![false; words.len()];
    for (i, word) in words.iter().enumerate() {
        let t = trimmed(&word.text);
        if is_email(t) {
            hit[i] = true;
            // "name@example." then "com": the address continues in the next word.
            if t.ends_with('.') && same_row(i, i + 1) {
                hit[i + 1] = true;
            }
        } else if is_ipv4(t) || is_long_number(t) || is_iban(t) || is_secret(t) {
            hit[i] = true;
        }
        // "Password: hunter2" hides the word after the label. Recognition may
        // return the colon as a word of its own.
        let after_label = if is_label(&word.text) {
            Some(i + 1)
        } else if is_label(&format!("{}:", word.text))
            && same_row(i, i + 1)
            && matches!(words[i + 1].text.as_str(), ":" | "=")
        {
            Some(i + 2)
        } else {
            None
        };
        if let Some(j) = after_label {
            if same_row(i, j) && !is_label(&words[j].text) {
                hit[j] = true;
            }
        }
    }
    // "4242 4242 4242 4242", "0207 946 09 58": a run of short digit groups
    // that adds up to a phone or card number.
    let mut i = 0;
    while i < words.len() {
        let (mut j, mut digits) = (i, 0);
        while same_row(i, j) {
            match digit_group(trimmed(&words[j].text)) {
                Some(n) => digits += n,
                None => break,
            }
            j += 1;
        }
        if j - i >= 3 && digits >= 10 {
            hit[i..j].fill(true);
        }
        i = j.max(i + 1);
    }

    let mut out: Vec<(u32, Rect)> = Vec::new();
    for (word, _) in words.iter().zip(&hit).filter(|(_, h)| **h) {
        match out.last_mut() {
            Some((line, rect))
                if *line == word.line && word.rect.x - rect.right() <= rect.h.max(word.rect.h) =>
            {
                *rect = rect.union(&word.rect);
            }
            _ => out.push((word.line, word.rect)),
        }
    }
    out.into_iter().map(|(_, rect)| rect).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Lays words out left to right, 10 px per character, lines 20 px apart.
    fn words(lines: &[&str]) -> Vec<Word> {
        let mut out = Vec::new();
        for (line, text) in lines.iter().enumerate() {
            let mut x = 0.0;
            for token in text.split_whitespace() {
                let w = token.chars().count() as f32 * 10.0;
                out.push(Word {
                    text: token.to_string(),
                    rect: Rect::new(x, line as f32 * 20.0, w, 14.0),
                    line: line as u32,
                });
                x += w + 10.0;
            }
        }
        out
    }

    fn masked(lines: &[&str]) -> Vec<String> {
        let all = words(lines);
        sensitive(&all)
            .iter()
            .map(|r| {
                all.iter()
                    .filter(|w| !w.rect.intersect(r).is_empty())
                    .map(|w| w.text.as_str())
                    .collect::<Vec<_>>()
                    .join(" ")
            })
            .collect()
    }

    #[test]
    fn text_keeps_lines() {
        assert_eq!(
            text(&words(&["hello there", "second line"])),
            "hello there\nsecond line"
        );
        assert_eq!(text(&[]), "");
    }

    #[test]
    fn words_returned_column_by_column_are_put_back_into_rows() {
        // What recognition gives for a form: every label, then every value.
        let w = |text: &str, x: f32, y: f32, line: u32| Word {
            text: text.into(),
            rect: Rect::new(x, y, text.len() as f32 * 10.0, 14.0),
            line,
        };
        let scrambled = [
            w("Name", 0.0, 0.0, 0),
            w("Password", 0.0, 21.0, 1),
            w(":", 85.0, 22.0, 1),
            w("Alex", 120.0, 1.0, 2),
            w("hunter2", 120.0, 20.0, 3),
        ];
        assert_eq!(text(&scrambled), "Name Alex\nPassword : hunter2");
        let rects = sensitive(&scrambled);
        assert_eq!(
            rects,
            vec![Rect::new(120.0, 20.0, 70.0, 14.0)],
            "only the password is masked"
        );
    }

    #[test]
    fn an_address_cut_in_two_by_recognition_is_masked_whole() {
        assert_eq!(
            masked(&["E-mail alex.Morgan@exampte. com today"]),
            ["alex.Morgan@exampte. com"]
        );
    }

    #[test]
    fn ordinary_text_is_left_alone() {
        assert!(
            masked(&["The quick brown fox, version 2.4.1, on 2026-11-25 at 14:30."]).is_empty()
        );
        assert!(
            masked(&[
                "Page 12 of 340 costs 1999 lira",
                "https://example.com/docs/getting-started"
            ])
            .is_empty()
        );
    }

    #[test]
    fn emails_are_masked() {
        assert_eq!(
            masked(&["Contact: jane.doe@example.com, thanks"]),
            ["jane.doe@example.com,"]
        );
        assert!(masked(&["twitter @handle and a@b"]).is_empty());
    }

    #[test]
    fn numbers_that_identify_people_are_masked() {
        assert_eq!(masked(&["Call +44-207-946-0958 now"]), ["+44-207-946-0958"]);
        assert_eq!(masked(&["ID 12345678901"]), ["12345678901"]);
        assert_eq!(
            masked(&["Card 4242 4242 4242 4242 exp 12/28"]),
            ["4242 4242 4242 4242"]
        );
        assert_eq!(masked(&["Tel 0207 946 09 58 home"]), ["0207 946 09 58"]);
        assert_eq!(masked(&["Tel +44 207 946 09 58"]), ["+44 207 946 09 58"]);
        assert!(
            masked(&["sizes 120 340 560 and 12 34 56"]).is_empty(),
            "short runs are not numbers"
        );
        assert_eq!(
            masked(&["IBAN DE89370400440532013000"]),
            ["DE89370400440532013000"]
        );
    }

    #[test]
    fn addresses_and_keys_are_masked() {
        assert_eq!(masked(&["server 192.168.1.42 is up"]), ["192.168.1.42"]);
        assert!(masked(&["version 1.2.3.4567"]).is_empty());
        assert_eq!(
            masked(&["key sk-proj-a1B2c3D4e5 ok"]),
            ["sk-proj-a1B2c3D4e5"]
        );
        assert_eq!(
            masked(&["hash 9f86d081884c7d659a2feaa0c55ad015a3bf4f1b"]),
            ["9f86d081884c7d659a2feaa0c55ad015a3bf4f1b"]
        );
    }

    #[test]
    fn the_word_after_a_label_is_masked() {
        assert_eq!(masked(&["Password: hunter2"]), ["hunter2"]);
        // The label can be in another language.
        for label in ["Passwort", "Contraseña", "Senha", "Hasło", "Şifre"] {
            let line = format!("{label}: geheim123 and more");
            assert_eq!(masked(&[line.as_str()]), ["geheim123"], "{label}");
        }
        assert!(
            masked(&["Password:"]).is_empty(),
            "nothing follows the label"
        );
        assert!(
            masked(&["Password:", "next line"]).is_empty(),
            "only on the same line"
        );
    }

    #[test]
    fn neighbours_merge_but_lines_and_distant_words_do_not() {
        let all = words(&["a@b.co c@d.co", "e@f.co                      192.168.0.1"]);
        let rects = sensitive(&all);
        assert_eq!(
            rects.len(),
            2,
            "two emails side by side are one box; the next line is another"
        );
        let far = vec![
            Word {
                text: "a@b.co".into(),
                rect: Rect::new(0.0, 0.0, 60.0, 14.0),
                line: 0,
            },
            Word {
                text: "c@d.co".into(),
                rect: Rect::new(400.0, 0.0, 60.0, 14.0),
                line: 0,
            },
        ];
        assert_eq!(sensitive(&far).len(), 2);
    }
}
