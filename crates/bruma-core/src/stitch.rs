//! Joining the pictures of a scrolling capture into one tall image.
//!
//! Each new picture shows the same area a little further down the page. The
//! work here is finding out how far it moved, by lining up rows.

use crate::image::Image;

/// At least this share of the overlapping rows must line up.
const AGREEMENT: f32 = 0.8;

/// A fingerprint of each row, ignoring the outer tenth on both sides where a
/// scrollbar would be.
fn row_marks(image: &Image) -> Vec<u64> {
    let w = image.width as usize;
    let (from, to) = (w / 10, w - w / 10);
    (0..image.height as usize)
        .map(|y| {
            let row = &image.rgba[(y * w + from) * 4..(y * w + to) * 4];
            // FNV-1a: fast, and good enough to tell rows apart.
            row.iter().fold(0xcbf29ce484222325u64, |h, &b| {
                (h ^ b as u64).wrapping_mul(0x100000001b3)
            })
        })
        .collect()
}

/// How many rows the page moved between `prev` and `next`, two pictures of the
/// same area. `Some(0)` means it did not move (the end of the page); `None`
/// means the two pictures cannot be lined up at all.
pub fn scroll_offset(prev: &Image, next: &Image) -> Option<usize> {
    if prev.width != next.width || prev.height != next.height || prev.height < 16 || prev.width < 20
    {
        return None;
    }
    let (a, b) = (row_marks(prev), row_marks(next));
    let h = a.len();
    let agree = |shift: usize| -> f32 {
        let overlap = h - shift;
        let same = (0..overlap).filter(|&i| a[i + shift] == b[i]).count();
        same as f32 / overlap as f32
    };
    if agree(0) >= 0.98 {
        return Some(0);
    }
    // Rows that look like many others (blank lines) prove nothing, so a shift
    // only counts if it also lines up rows that are rare.
    let mut seen = std::collections::HashMap::new();
    for mark in &b {
        *seen.entry(*mark).or_insert(0usize) += 1;
    }
    let distinct = |shift: usize| {
        (0..h - shift)
            .filter(|&i| a[i + shift] == b[i] && seen[&b[i]] <= 2)
            .count()
    };
    let mut best: Option<(usize, usize)> = None;
    // Keep at least a fifth of the picture as overlap.
    for shift in 1..h - h / 5 {
        if agree(shift) >= AGREEMENT {
            let weight = distinct(shift);
            if weight >= 4 && best.is_none_or(|(_, w)| weight > w) {
                best = Some((shift, weight));
            }
        }
    }
    best.map(|(shift, _)| shift)
}

/// Adds the part of `next` that `total` does not have yet: its last `offset`
/// rows.
pub fn append(total: &mut Image, next: &Image, offset: usize) {
    if next.width != total.width || offset == 0 {
        return;
    }
    let offset = offset.min(next.height as usize);
    let start = (next.height as usize - offset) * next.width as usize * 4;
    total.rgba.extend_from_slice(&next.rgba[start..]);
    total.height += offset as u32;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A tall "page" where every row is different.
    fn page(height: u32) -> Image {
        let mut img = Image::filled(200, height, [255, 255, 255, 255]);
        for y in 0..height {
            let v = ((y * 53) % 251) as u8;
            for x in 20..180 {
                img.set_pixel(x, y, [v, (x % 200) as u8, (y % 256) as u8, 255]);
            }
        }
        img
    }

    fn view(page: &Image, top: u32) -> Image {
        page.crop(crate::geom::Rect::new(0.0, top as f32, 200.0, 120.0))
    }

    #[test]
    fn finds_how_far_the_page_moved() {
        let p = page(600);
        assert_eq!(scroll_offset(&view(&p, 0), &view(&p, 40)), Some(40));
        assert_eq!(scroll_offset(&view(&p, 100), &view(&p, 187)), Some(87));
    }

    #[test]
    fn no_movement_is_zero() {
        let p = page(600);
        assert_eq!(scroll_offset(&view(&p, 50), &view(&p, 50)), Some(0));
    }

    #[test]
    fn unrelated_pictures_do_not_line_up() {
        let p = page(600);
        let other = Image::filled(200, 120, [9, 9, 9, 255]);
        assert_eq!(scroll_offset(&view(&p, 0), &other), None);
        assert_eq!(
            scroll_offset(&view(&p, 0), &view(&p, 300)),
            None,
            "moved further than the overlap allows"
        );
    }

    #[test]
    fn a_moving_scrollbar_and_a_fixed_header_are_tolerated() {
        let p = page(600);
        let (mut a, mut b) = (view(&p, 0), view(&p, 40));
        for img in [&mut a, &mut b] {
            // A header that stays put at the top of the view.
            img.fill_rect(
                crate::geom::Rect::new(0.0, 0.0, 200.0, 10.0),
                [40, 40, 40, 255],
            );
        }
        // A scrollbar thumb at the right edge, in a different place each time.
        a.fill_rect(
            crate::geom::Rect::new(190.0, 10.0, 8.0, 30.0),
            [120, 120, 120, 255],
        );
        b.fill_rect(
            crate::geom::Rect::new(190.0, 50.0, 8.0, 30.0),
            [120, 120, 120, 255],
        );
        assert_eq!(scroll_offset(&a, &b), Some(40));
    }

    #[test]
    fn blank_pages_are_not_guessed_at() {
        let blank = Image::filled(200, 120, [255, 255, 255, 255]);
        assert_eq!(scroll_offset(&blank, &blank), Some(0));
        let mut one_line = blank.clone();
        one_line.fill_rect(
            crate::geom::Rect::new(30.0, 100.0, 100.0, 10.0),
            [0, 0, 0, 255],
        );
        assert_eq!(
            scroll_offset(&one_line, &blank),
            None,
            "nothing distinctive to line up"
        );
    }

    #[test]
    fn appending_rebuilds_the_page() {
        let p = page(300);
        let tops = [0u32, 40, 90, 150, 180];
        let mut total = view(&p, 0);
        for pair in tops.windows(2) {
            let (prev, next) = (view(&p, pair[0]), view(&p, pair[1]));
            let shift = scroll_offset(&prev, &next).unwrap();
            assert_eq!(shift as u32, pair[1] - pair[0]);
            append(&mut total, &next, shift);
        }
        assert_eq!(total, p);
    }

    #[test]
    fn tiny_or_mismatched_pictures_are_refused() {
        let small = Image::filled(10, 10, [0, 0, 0, 255]);
        assert_eq!(scroll_offset(&small, &small), None);
        let p = page(300);
        assert_eq!(scroll_offset(&view(&p, 0), &p), None);
    }
}
