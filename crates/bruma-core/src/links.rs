//! Links on the screen: web addresses written as text (found in the words
//! text recognition reads) and QR codes. Bruma offers them to be copied; it
//! never opens them, and nothing here touches the network.

use crate::geom::Rect;
use crate::redact::Word;

/// What a link holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// A web address.
    Web,
    /// Other text, from a QR code (Wi-Fi settings, a note).
    Text,
}

/// A link found on the screen, and where.
#[derive(Clone, Debug, PartialEq)]
pub struct Link {
    /// The address to copy (or the text, for [`Kind::Text`]).
    pub url: String,
    /// Where it is, in the picture's coordinates.
    pub rect: Rect,
    pub kind: Kind,
    /// Read from a QR code rather than from written text.
    pub qr: bool,
}

/// Top-level domains a bare address (without `www.` or `https://`) may
/// end in. Others would turn too many ordinary words with a dot into links.
const DOMAINS: &[&str] = &[
    "com", "org", "net", "io", "dev", "app", "gg", "tv", "co", "me", "edu", "gov", "info", "xyz",
    "ai", "so", "tr", "de", "uk", "fr", "nl", "ru", "jp", "eu", "us", "ca", "it", "es",
];

/// The web address `text` stands for, ready to paste: `https://` put in
/// front of `www.` and bare domains. `None` when it is not an address.
pub fn as_link(text: &str) -> Option<String> {
    let mut text = text.trim_start_matches(['(', '[', '<', '"', '\'']);
    // Punctuation after an address belongs to the sentence; a closing
    // bracket only when the address did not open it.
    while let Some(last) = text.chars().last() {
        let strip = match last {
            '.' | ',' | ';' | ':' | '!' | '?' | ']' | '>' | '"' | '\'' => true,
            ')' => text.matches('(').count() < text.matches(')').count(),
            _ => false,
        };
        if !strip {
            break;
        }
        text = &text[..text.len() - last.len_utf8()];
    }
    if text.is_empty() || text.chars().any(char::is_whitespace) {
        return None;
    }
    let lower = text.to_ascii_lowercase();
    let (scheme, rest) = if let Some(rest) = lower.strip_prefix("https://") {
        (true, rest)
    } else if let Some(rest) = lower.strip_prefix("http://") {
        (true, rest)
    } else {
        (false, lower.as_str())
    };
    let host = rest.split(['/', '?', '#']).next().unwrap_or("");
    // An e-mail address has its `@` before any path; a handle after it.
    if host.contains('@') {
        return None;
    }
    let host = match host.split_once(':') {
        Some((name, port)) if !port.is_empty() && port.chars().all(|c| c.is_ascii_digit()) => name,
        Some(_) => return None,
        None => host,
    };
    let labels: Vec<&str> = host.split('.').collect();
    let well_formed = labels.len() >= 2
        && labels
            .iter()
            .all(|l| !l.is_empty() && l.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'))
        && host.chars().any(|c| c.is_ascii_alphabetic());
    if !well_formed {
        return None;
    }
    if scheme {
        return Some(text.to_string());
    }
    let top = labels.last().copied().unwrap_or("");
    let named = labels[..labels.len() - 1]
        .iter()
        .any(|l| l.chars().any(|c| c.is_ascii_alphabetic()));
    if (host.starts_with("www.") || DOMAINS.contains(&top)) && named {
        Some(format!("https://{text}"))
    } else {
        None
    }
}

/// Whether text recognition probably split one address between `left` and
/// `right`: no space between them to speak of, and the cut at a character
/// an address is made of.
fn joined(left: &Word, right: &Word) -> bool {
    let gap = right.rect.x - left.rect.right();
    let height = left.rect.h.max(right.rect.h);
    // Recognition splits an address with next to no gap; an ordinary space
    // is about a third of the text's height.
    left.line == right.line
        && gap < height * 0.15
        && (left.text.ends_with(['/', '.', '-', '_']) || right.text.starts_with(['/', '.']))
}

/// The web addresses among recognised `words`, joining the pieces text
/// recognition splits an address into.
pub fn in_words(words: &[Word]) -> Vec<Link> {
    let mut order: Vec<&Word> = words.iter().collect();
    order.sort_by(|a, b| a.line.cmp(&b.line).then(a.rect.x.total_cmp(&b.rect.x)));
    let mut links = Vec::new();
    let mut i = 0;
    while i < order.len() {
        let mut text = order[i].text.clone();
        let mut rect = order[i].rect;
        let mut j = i;
        while j + 1 < order.len() && joined(order[j], order[j + 1]) {
            j += 1;
            text.push_str(&order[j].text);
            rect = rect.union(&order[j].rect);
        }
        if let Some(url) = as_link(&text) {
            links.push(Link {
                url,
                rect,
                kind: Kind::Web,
                qr: false,
            });
        } else if j > i {
            // Joined wrongly after all: the pieces on their own.
            for piece in &order[i..=j] {
                if let Some(url) = as_link(&piece.text) {
                    links.push(Link {
                        url,
                        rect: piece.rect,
                        kind: Kind::Web,
                        qr: false,
                    });
                }
            }
        }
        i = j + 1;
    }
    links
}

/// The QR codes in `image`: a web address as [`Kind::Web`], anything else
/// as [`Kind::Text`].
pub fn in_qr_codes(image: &crate::Image) -> Vec<Link> {
    let (w, h) = (image.width as usize, image.height as usize);
    if w == 0 || h == 0 {
        return Vec::new();
    }
    let luma: Vec<u8> = image
        .rgba
        .chunks_exact(4)
        .map(|p| ((p[0] as u32 * 30 + p[1] as u32 * 59 + p[2] as u32 * 11) / 100) as u8)
        .collect();
    let mut prepared = rqrr::PreparedImage::prepare_from_greyscale(w, h, |x, y| luma[y * w + x]);
    prepared
        .detect_grids()
        .into_iter()
        .filter_map(|grid| {
            let (_, content) = grid.decode().ok()?;
            let xs = grid.bounds.iter().map(|p| p.x as f32);
            let ys = grid.bounds.iter().map(|p| p.y as f32);
            let (x0, x1) = xs.fold((f32::MAX, f32::MIN), |(a, b), v| (a.min(v), b.max(v)));
            let (y0, y1) = ys.fold((f32::MAX, f32::MIN), |(a, b), v| (a.min(v), b.max(v)));
            let rect = Rect::new(x0, y0, x1 - x0, y1 - y0);
            Some(match as_link(content.trim()) {
                Some(url) => Link {
                    url,
                    rect,
                    kind: Kind::Web,
                    qr: true,
                },
                None => Link {
                    url: content,
                    rect,
                    kind: Kind::Text,
                    qr: true,
                },
            })
        })
        .collect()
}

/// The links on `image`, read one part at a time (one monitor each):
/// text recognition is given each part on its own (`read`, which returns
/// the part's words in the part's coordinates) and QR codes are looked for
/// in each part. Rectangles come back in `image`'s coordinates. A whole
/// multi-monitor desktop can be wider than text recognition accepts.
pub fn in_parts(
    image: &crate::Image,
    parts: &[Rect],
    mut read: impl FnMut(&crate::Image) -> Vec<Word>,
) -> Vec<Link> {
    let bounds = image.bounds();
    let mut areas: Vec<Rect> = parts
        .iter()
        .map(|p| p.round().intersect(&bounds))
        .filter(|p| !p.is_empty())
        .collect();
    if areas.is_empty() {
        areas.push(bounds);
    }
    let mut links = Vec::new();
    for area in areas {
        let part = image.crop(area);
        let mut found = in_words(&read(&part));
        found.extend(in_qr_codes(&part));
        for link in &mut found {
            link.rect = link.rect.translate(area.x, area.y);
        }
        links.extend(found);
    }
    links
}

/// The link a preview card offers for `area`: of the links whose middle is
/// inside it, a web address before other text, and of those the one nearest
/// the area's middle.
pub fn for_card(links: &[Link], area: Rect) -> Option<&Link> {
    let middle = |r: &Rect| crate::Point::new(r.x + r.w / 2.0, r.y + r.h / 2.0);
    let centre = middle(&area);
    let distance = |l: &Link| {
        let m = middle(&l.rect);
        (m.x - centre.x).powi(2) + (m.y - centre.y).powi(2)
    };
    links
        .iter()
        .filter(|l| area.contains(middle(&l.rect)))
        .min_by(|a, b| {
            (a.kind != Kind::Web)
                .cmp(&(b.kind != Kind::Web))
                .then(distance(a).total_cmp(&distance(b)))
        })
}

/// The line a preview card shows for `link`.
pub fn card_line(link: &Link) -> String {
    let (label, text) = match link.kind {
        Kind::Web => (
            "Copy link",
            link.url
                .strip_prefix("https://")
                .or_else(|| link.url.strip_prefix("http://"))
                .unwrap_or(&link.url),
        ),
        Kind::Text => ("Copy text", link.url.as_str()),
    };
    let chars: Vec<char> = text.chars().collect();
    let shown: String = if chars.len() > 40 {
        // The start says where, the end says what.
        let head: String = chars[..18].iter().collect();
        let tail: String = chars[chars.len() - 21..].iter().collect();
        format!("{head}\u{2026}{tail}")
    } else {
        text.to_string()
    };
    format!("{label}  {shown}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn word(text: &str, x: f32, y: f32, w: f32, h: f32, line: u32) -> Word {
        Word {
            text: text.into(),
            rect: Rect::new(x, y, w, h),
            line,
        }
    }

    /// A white picture with `content` as a QR code at (40, 30), four
    /// pixels a module; and the code's side in pixels.
    fn picture_of_qr(content: &str) -> (crate::Image, f32) {
        let code = qrcode::QrCode::new(content.as_bytes()).unwrap();
        let n = code.width();
        let (w, h) = (400u32, 300u32);
        let mut rgba = vec![255u8; (w * h * 4) as usize];
        // A quiet zone of four modules is part of the picture already: it
        // is white all round.
        for my in 0..n {
            for mx in 0..n {
                if code[(mx, my)] == qrcode::Color::Dark {
                    for dy in 0..4 {
                        for dx in 0..4 {
                            let (x, y) = (40 + mx * 4 + dx, 30 + my * 4 + dy);
                            let i = (y * w as usize + x) * 4;
                            rgba[i..i + 3].copy_from_slice(&[0, 0, 0]);
                        }
                    }
                }
            }
        }
        (crate::Image::new(w, h, rgba), (n * 4) as f32)
    }

    #[test]
    fn a_qr_code_is_read_with_where_it_is() {
        let (picture, side) = picture_of_qr("https://example.com/q");
        let links = in_qr_codes(&picture);
        assert_eq!(links.len(), 1, "{links:?}");
        let link = &links[0];
        assert_eq!(link.url, "https://example.com/q");
        assert_eq!(link.kind, Kind::Web);
        assert!(link.qr);
        assert!(
            link.rect.contains(crate::Point::new(60.0, 50.0)),
            "{:?}",
            link.rect
        );
        // The decoder's outline reaches the far edge of the last module:
        // one module (four pixels) of slack.
        let code = Rect::new(40.0, 30.0, side, side);
        assert!(
            link.rect.x >= code.x - 4.0
                && link.rect.y >= code.y - 4.0
                && link.rect.right() <= code.right() + 4.0
                && link.rect.bottom() <= code.bottom() + 4.0,
            "{:?} in {code:?}",
            link.rect
        );
    }

    #[test]
    fn qr_text_that_is_not_a_link_is_text() {
        let content = "WIFI:S:home;T:WPA;P:secret;;";
        let (picture, _) = picture_of_qr(content);
        let links = in_qr_codes(&picture);
        assert_eq!(links.len(), 1, "{links:?}");
        assert_eq!(links[0].url, content);
        assert_eq!(links[0].kind, Kind::Text);
    }

    #[test]
    fn the_card_line_names_the_link_and_shortens_it() {
        let web = Link {
            url: "https://github.com/0301091405/bruma".into(),
            rect: Rect::new(0.0, 0.0, 1.0, 1.0),
            kind: Kind::Web,
            qr: false,
        };
        assert_eq!(card_line(&web), "Copy link  github.com/0301091405/bruma");
        let long = Link {
            url: "https://example.com/a/very/long/path/that/goes/on/and/on/index.html".into(),
            ..web.clone()
        };
        let line = card_line(&long);
        assert!(line.starts_with("Copy link  example.com/"), "{line}");
        assert!(
            line.contains('\u{2026}') && line.ends_with("index.html"),
            "{line}"
        );
        assert!(line.chars().count() <= "Copy link  ".len() + 40, "{line}");
        let text = Link {
            url: "WIFI:S:home;;".into(),
            kind: Kind::Text,
            ..web
        };
        assert_eq!(card_line(&text), "Copy text  WIFI:S:home;;");
    }

    #[test]
    fn each_monitor_is_read_on_its_own_and_placed_back() {
        let image = crate::Image::filled(400, 100, [255, 255, 255, 255]);
        let parts = [
            Rect::new(0.0, 0.0, 200.0, 100.0),
            Rect::new(200.0, 0.0, 200.0, 100.0),
        ];
        let mut sizes = Vec::new();
        let links = in_parts(&image, &parts, |part| {
            sizes.push((part.width, part.height));
            vec![word("example.com", 10.0, 10.0, 50.0, 10.0, 0)]
        });
        assert_eq!(sizes, [(200, 100), (200, 100)]);
        let xs: Vec<f32> = links.iter().map(|l| l.rect.x).collect();
        assert_eq!(xs, [10.0, 210.0]);
        // No parts given: the whole picture is one.
        let mut whole = Vec::new();
        in_parts(&image, &[], |part| {
            whole.push(part.width);
            Vec::new()
        });
        assert_eq!(whole, [400]);
    }

    #[test]
    fn a_card_prefers_a_web_link_then_the_nearest() {
        let link = |url: &str, x: f32, y: f32, kind: Kind| Link {
            url: url.into(),
            rect: Rect::new(x, y, 40.0, 10.0),
            kind,
            qr: false,
        };
        let area = Rect::new(0.0, 0.0, 400.0, 300.0);
        let links = [
            link("https://far.com", 10.0, 10.0, Kind::Web),
            link("https://near.com", 170.0, 140.0, Kind::Web),
            link("note", 180.0, 145.0, Kind::Text),
            link("https://outside.com", 500.0, 500.0, Kind::Web),
        ];
        assert_eq!(
            for_card(&links, area).map(|l| l.url.as_str()),
            Some("https://near.com")
        );
        assert_eq!(
            for_card(&links[2..3], area).map(|l| l.url.as_str()),
            Some("note")
        );
        assert_eq!(for_card(&links[3..], area), None);
    }

    #[test]
    fn addresses_of_every_kind_are_found() {
        assert_eq!(
            as_link("https://example.com/a.").as_deref(),
            Some("https://example.com/a")
        );
        assert_eq!(
            as_link("www.rust-lang.org").as_deref(),
            Some("https://www.rust-lang.org")
        );
        assert_eq!(
            as_link("github.com/0301091405/bruma").as_deref(),
            Some("https://github.com/0301091405/bruma")
        );
        assert_eq!(as_link("(http://x.io)").as_deref(), Some("http://x.io"));
    }

    #[test]
    fn things_that_are_not_links_are_left_alone() {
        for text in [
            "report.txt",
            "setup.exe",
            "v1.2.3",
            "1.2.3.4",
            "ada@example.com",
            "e.g.",
            "...",
            "hello",
        ] {
            assert_eq!(as_link(text), None, "{text}");
        }
    }

    #[test]
    fn an_address_split_by_recognition_is_one_link() {
        let words = [
            word("github.com/", 100.0, 50.0, 90.0, 16.0, 0),
            word("0301091405/", 192.0, 50.0, 98.0, 16.0, 0),
            word("bruma", 292.0, 50.0, 48.0, 16.0, 0),
        ];
        let links = in_words(&words);
        assert_eq!(links.len(), 1, "{links:?}");
        assert_eq!(links[0].url, "https://github.com/0301091405/bruma");
        assert_eq!(links[0].rect, Rect::new(100.0, 50.0, 240.0, 16.0));
        assert_eq!(links[0].kind, Kind::Web);
        assert!(!links[0].qr);
    }

    #[test]
    fn an_address_at_the_end_of_a_sentence_is_found() {
        // An ordinary space between words is about a third of the text's
        // height: the next word is not part of the address.
        let found = |a: &str, b: &str| {
            let words = [
                word(a, 100.0, 50.0, 90.0, 16.0, 0),
                word(b, 195.0, 50.0, 40.0, 16.0, 0),
            ];
            in_words(&words)
                .into_iter()
                .map(|l| l.url)
                .collect::<Vec<_>>()
        };
        assert_eq!(found("example.com.", "Then"), ["https://example.com"]);
        assert_eq!(found("https://x.com/docs.", "See"), ["https://x.com/docs"]);
        assert_eq!(found("1.", "example.com"), ["https://example.com"]);
    }

    #[test]
    fn addresses_with_handles_ports_and_brackets_are_kept_whole() {
        assert_eq!(
            as_link("youtube.com/@name").as_deref(),
            Some("https://youtube.com/@name")
        );
        assert_eq!(
            as_link("medium.com/@user/post").as_deref(),
            Some("https://medium.com/@user/post")
        );
        assert_eq!(
            as_link("example.com:8080/x").as_deref(),
            Some("https://example.com:8080/x")
        );
        assert_eq!(
            as_link("wikipedia.org/wiki/Rust_(programming_language)").as_deref(),
            Some("https://wikipedia.org/wiki/Rust_(programming_language)")
        );
        assert_eq!(
            as_link("(example.com)").as_deref(),
            Some("https://example.com")
        );
        assert_eq!(as_link("ada@example.com"), None);
    }

    #[test]
    fn words_far_apart_are_not_joined() {
        let words = [
            word("example.com", 100.0, 50.0, 90.0, 16.0, 0),
            word("next", 250.0, 50.0, 30.0, 16.0, 0),
        ];
        let links = in_words(&words);
        assert_eq!(links.len(), 1, "{links:?}");
        assert_eq!(links[0].url, "https://example.com");
        assert_eq!(links[0].rect, Rect::new(100.0, 50.0, 90.0, 16.0));
    }
}
