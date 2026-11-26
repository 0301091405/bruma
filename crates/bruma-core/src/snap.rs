//! Snapping: finding the thing you meant.
//!
//! Two questions are answered here, both from pixels alone, so they work on
//! anything that is visible: a web page, a game, a photo, a drawing.
//!
//! * [`element_at`]: what box, card, button, circle or picture is under the
//!   pointer?
//! * [`snap_x`] / [`snap_y`]: is there a real edge near the selection edge
//!   being dragged?

use crate::geom::{Point, Rect};
use crate::image::Image;

/// Channels may differ by this much and still be "the same colour".
const SAME: i32 = 10;
/// A step between neighbours larger than this is an edge.
const EDGE: i32 = 28;
/// Nothing smaller than this on a side counts as an element.
const MIN_SIDE: f32 = 12.0;
/// A flat region covering more of the screen than this is background.
const MAX_SHARE: f32 = 0.45;
/// How far the search for the border of a busy element reaches.
const REACH: i32 = 1400;

fn near(a: &[u8], b: &[u8]) -> bool {
    (a[0] as i32 - b[0] as i32).abs() <= SAME
        && (a[1] as i32 - b[1] as i32).abs() <= SAME
        && (a[2] as i32 - b[2] as i32).abs() <= SAME
}

fn steps(a: &[u8], b: &[u8]) -> bool {
    (a[0] as i32 - b[0] as i32).abs() > EDGE
        || (a[1] as i32 - b[1] as i32).abs() > EDGE
        || (a[2] as i32 - b[2] as i32).abs() > EDGE
}

fn px(image: &Image, x: i32, y: i32) -> &[u8] {
    let i = (y as usize * image.width as usize + x as usize) * 4;
    &image.rgba[i..i + 4]
}

/// The flat-coloured area around `p`: a button, a card, a filled circle.
/// `None` when it is tiny (text, detail) or so large it must be background.
pub fn flat_region(image: &Image, p: Point) -> Option<Rect> {
    let (w, h) = (image.width as i32, image.height as i32);
    let (sx, sy) = (p.x.floor() as i32, p.y.floor() as i32);
    if sx < 0 || sy < 0 || sx >= w || sy >= h {
        return None;
    }
    let seed = [
        px(image, sx, sy)[0],
        px(image, sx, sy)[1],
        px(image, sx, sy)[2],
    ];
    // A colour found at half of a grid of sample points is the background;
    // there is no need to fill half the screen to learn that.
    let (cols, rows) = (48, 27);
    let same = (0..rows)
        .flat_map(|j| (0..cols).map(move |i| (i, j)))
        .filter(|&(i, j)| {
            near(
                px(image, (i * w + w / 2) / cols, (j * h + h / 2) / rows),
                &seed,
            )
        })
        .count();
    if same * 2 >= (cols * rows) as usize {
        return None;
    }
    let limit = (w as f32 * h as f32 * MAX_SHARE) as usize;
    let mut seen = vec![false; (w * h) as usize];
    let mut stack = vec![(sx, sy)];
    let (mut x0, mut y0, mut x1, mut y1) = (sx, sy, sx, sy);
    let mut count = 0usize;
    while let Some((x, y)) = stack.pop() {
        let at = |x: i32| (y * w + x) as usize;
        if seen[at(x)] || !near(px(image, x, y), &seed) {
            continue;
        }
        // Fill the whole run of matching pixels on this row.
        let (mut l, mut r) = (x, x);
        while l > 0 && !seen[at(l - 1)] && near(px(image, l - 1, y), &seed) {
            l -= 1;
        }
        while r + 1 < w && !seen[at(r + 1)] && near(px(image, r + 1, y), &seed) {
            r += 1;
        }
        seen[at(l)..=at(r)].fill(true);
        count += (r - l + 1) as usize;
        if count > limit {
            return None;
        }
        (x0, y0, x1, y1) = (x0.min(l), y0.min(y), x1.max(r), y1.max(y));
        // Queue one pixel for each matching run on the rows above and below.
        for ny in [y - 1, y + 1] {
            if ny < 0 || ny >= h {
                continue;
            }
            let mut inside = false;
            for nx in l..=r {
                let open = !seen[(ny * w + nx) as usize] && near(px(image, nx, ny), &seed);
                if open && !inside {
                    stack.push((nx, ny));
                }
                inside = open;
            }
        }
    }
    let rect = Rect::new(
        x0 as f32,
        y0 as f32,
        (x1 - x0 + 1) as f32,
        (y1 - y0 + 1) as f32,
    );
    (rect.w >= MIN_SIDE && rect.h >= MIN_SIDE).then_some(rect)
}

/// Whether column `x` is one flat colour from row `y0` to `y1` (inclusive).
fn column_is_flat(image: &Image, x: i32, y0: i32, y1: i32) -> bool {
    let first = px(image, x, y0);
    (y0 + 1..=y1).all(|y| near(px(image, x, y), first))
}

fn row_is_flat(image: &Image, y: i32, x0: i32, x1: i32) -> bool {
    let first = px(image, x0, y);
    (x0 + 1..=x1).all(|x| near(px(image, x, y), first))
}

/// The box around busy content at `p`: a photo, a chart, a card full of text.
///
/// Walks outwards from the pointer until each side reaches a line of flat
/// background as long as the box itself, widening the box until it is stable.
/// The edge of the screen counts as a border too.
pub fn busy_box(image: &Image, p: Point) -> Option<Rect> {
    let (w, h) = (image.width as i32, image.height as i32);
    let (cx, cy) = (p.x.floor() as i32, p.y.floor() as i32);
    if cx < 1 || cy < 1 || cx >= w - 1 || cy >= h - 1 {
        return None;
    }
    // Start from a small cross around the pointer and let it grow.
    let (mut l, mut r, mut t, mut b) = (cx - 8, cx + 8, cy - 8, cy + 8);
    for _ in 0..8 {
        let before = (l, r, t, b);
        let (top, bottom) = (t.max(0), b.min(h - 1));
        // Nearest flat column on each side, judged over the box's height.
        l = (0.max(cx - REACH)..cx)
            .rev()
            .find(|&x| column_is_flat(image, x, top, bottom))
            .unwrap_or(-1);
        r = (cx + 1..w.min(cx + REACH))
            .find(|&x| column_is_flat(image, x, top, bottom))
            .unwrap_or(w);
        // Rows are judged across the box *and* the border columns just found:
        // a real border row runs into them, a calm band inside a photo does not.
        let (left, right) = (l.max(0), r.min(w - 1));
        t = (0.max(cy - REACH)..cy)
            .rev()
            .find(|&y| row_is_flat(image, y, left, right))
            .unwrap_or(-1);
        b = (cy + 1..h.min(cy + REACH))
            .find(|&y| row_is_flat(image, y, left, right))
            .unwrap_or(h);
        if (l, r, t, b) == before {
            break;
        }
    }
    // The flat lines found are background; the box lies strictly between them.
    let rect = Rect::new(
        (l + 1) as f32,
        (t + 1) as f32,
        (r - l - 1) as f32,
        (b - t - 1) as f32,
    );
    let share = rect.w * rect.h / (w as f32 * h as f32);
    (rect.w >= 2.0 * MIN_SIDE && rect.h >= 2.0 * MIN_SIDE && share <= MAX_SHARE).then_some(rect)
}

/// The visual element under `p`, if there is one worth selecting.
pub fn element_at(image: &Image, p: Point) -> Option<Rect> {
    flat_region(image, p).or_else(|| busy_box(image, p))
}

/// How many of the rows `y0..y1` have an edge between columns `b - 1` and `b`.
fn column_edge(image: &Image, b: i32, y0: i32, y1: i32) -> i32 {
    (y0..y1)
        .filter(|&y| steps(px(image, b - 1, y), px(image, b, y)))
        .count() as i32
}

fn row_edge(image: &Image, b: i32, x0: i32, x1: i32) -> i32 {
    (x0..x1)
        .filter(|&x| steps(px(image, x, b - 1), px(image, x, b)))
        .count() as i32
}

/// Picks the strongest edge within `reach` of `at`; nearer wins a tie. An
/// edge must run along a good part of the span to count.
fn strongest(at: i32, reach: i32, limit: i32, span: i32, strength: impl Fn(i32) -> i32) -> i32 {
    let needed = (span * 35 / 100).max(MIN_SIDE as i32);
    let mut best = (0, at);
    for offset in 0..=reach {
        for b in [at - offset, at + offset] {
            if b >= 1 && b < limit {
                let s = strength(b);
                if s >= needed && s > best.0 {
                    best = (s, b);
                }
            }
        }
    }
    best.1
}

/// Snaps a vertical selection edge at `x` (the boundary between columns
/// `x - 1` and `x`) to a real edge of the picture that runs along rows
/// `y0..y1`. Returns `x` unchanged when there is none within `reach`.
pub fn snap_x(image: &Image, x: f32, y0: f32, y1: f32, reach: f32) -> f32 {
    let (w, h) = (image.width as i32, image.height as i32);
    let (ya, yb) = (
        (y0.min(y1).round() as i32).clamp(0, h),
        (y0.max(y1).round() as i32).clamp(0, h),
    );
    if yb - ya < MIN_SIDE as i32 {
        return x;
    }
    strongest(x.round() as i32, reach.round() as i32, w, yb - ya, |b| {
        column_edge(image, b, ya, yb)
    }) as f32
}

/// [`snap_x`] for a horizontal edge at `y` running along columns `x0..x1`.
pub fn snap_y(image: &Image, y: f32, x0: f32, x1: f32, reach: f32) -> f32 {
    let (w, h) = (image.width as i32, image.height as i32);
    let (xa, xb) = (
        (x0.min(x1).round() as i32).clamp(0, w),
        (x0.max(x1).round() as i32).clamp(0, w),
    );
    if xb - xa < MIN_SIDE as i32 {
        return y;
    }
    strongest(y.round() as i32, reach.round() as i32, h, xb - xa, |b| {
        row_edge(image, b, xa, xb)
    }) as f32
}

#[cfg(test)]
mod tests {
    use super::*;

    const WHITE: [u8; 4] = [255, 255, 255, 255];
    const DARK: [u8; 4] = [30, 30, 30, 255];

    fn page() -> Image {
        Image::filled(600, 400, WHITE)
    }

    /// A photo-like patch: every pixel differs from its neighbours.
    fn noise(image: &mut Image, rect: Rect) {
        for y in rect.y as u32..rect.bottom() as u32 {
            for x in rect.x as u32..rect.right() as u32 {
                let v = ((x * 37 + y * 91) % 200) as u8;
                image.set_pixel(x, y, [v, 255 - v, (x * 3 % 255) as u8, 255]);
            }
        }
    }

    #[test]
    fn a_flat_box_is_found_exactly() {
        let mut img = page();
        img.fill_rect(Rect::new(100.0, 80.0, 200.0, 120.0), DARK);
        assert_eq!(
            element_at(&img, Point::new(150.0, 100.0)),
            Some(Rect::new(100.0, 80.0, 200.0, 120.0))
        );
        assert_eq!(
            element_at(&img, Point::new(299.0, 199.0)),
            Some(Rect::new(100.0, 80.0, 200.0, 120.0))
        );
    }

    #[test]
    fn a_circle_gives_its_bounding_square() {
        let mut img = page();
        for y in 0..400 {
            for x in 0..600 {
                let (dx, dy) = (x as f32 - 300.0, y as f32 - 200.0);
                if dx * dx + dy * dy <= 60.0 * 60.0 {
                    img.set_pixel(x, y, DARK);
                }
            }
        }
        assert_eq!(
            element_at(&img, Point::new(300.0, 200.0)),
            Some(Rect::new(240.0, 140.0, 121.0, 121.0))
        );
    }

    #[test]
    fn the_background_is_not_an_element() {
        let mut img = page();
        img.fill_rect(Rect::new(100.0, 80.0, 60.0, 60.0), DARK);
        assert_eq!(element_at(&img, Point::new(400.0, 300.0)), None);
    }

    #[test]
    fn text_sized_specks_are_not_elements() {
        let mut img = page();
        img.fill_rect(Rect::new(100.0, 100.0, 6.0, 9.0), DARK);
        assert_eq!(flat_region(&img, Point::new(102.0, 104.0)), None);
    }

    #[test]
    fn the_box_keeps_its_shape_around_what_is_inside_it() {
        // A card with "text" on it: pointing at the card's own colour selects
        // the card, not the gaps between the letters.
        let mut img = page();
        img.fill_rect(Rect::new(100.0, 80.0, 300.0, 200.0), [240, 238, 230, 255]);
        for i in 0..6 {
            img.fill_rect(Rect::new(120.0, 100.0 + i as f32 * 20.0, 200.0, 8.0), DARK);
        }
        assert_eq!(
            element_at(&img, Point::new(380.0, 260.0)),
            Some(Rect::new(100.0, 80.0, 300.0, 200.0))
        );
    }

    #[test]
    fn a_picture_on_a_flat_page_is_boxed() {
        let mut img = page();
        let photo = Rect::new(150.0, 90.0, 240.0, 160.0);
        noise(&mut img, photo);
        assert_eq!(
            flat_region(&img, Point::new(270.0, 170.0)),
            None,
            "a photo has no flat area"
        );
        assert_eq!(element_at(&img, Point::new(270.0, 170.0)), Some(photo));
        assert_eq!(
            element_at(&img, Point::new(152.0, 92.0)),
            Some(photo),
            "near its corner too"
        );
    }

    #[test]
    fn a_picture_with_a_calm_band_through_it_is_still_boxed_whole() {
        // A flat stripe across the photo right where the pointer is must not
        // be mistaken for the photo's border.
        let mut img = page();
        let photo = Rect::new(150.0, 90.0, 240.0, 160.0);
        noise(&mut img, photo);
        img.fill_rect(Rect::new(150.0, 150.0, 240.0, 30.0), [90, 140, 200, 255]);
        assert_eq!(busy_box(&img, Point::new(200.0, 120.0)), Some(photo));
    }

    #[test]
    fn a_picture_touching_the_screen_edge_is_boxed_to_the_edge() {
        let mut img = page();
        let photo = Rect::new(0.0, 0.0, 200.0, 150.0);
        noise(&mut img, photo);
        assert_eq!(element_at(&img, Point::new(100.0, 75.0)), Some(photo));
    }

    #[test]
    fn busy_content_filling_the_screen_is_not_an_element() {
        let mut img = page();
        noise(&mut img, Rect::new(0.0, 0.0, 600.0, 400.0));
        assert_eq!(element_at(&img, Point::new(300.0, 200.0)), None);
    }

    #[test]
    fn points_outside_the_image_are_ignored() {
        let img = page();
        assert_eq!(element_at(&img, Point::new(-5.0, 10.0)), None);
        assert_eq!(element_at(&img, Point::new(10.0, 4000.0)), None);
    }

    #[test]
    fn an_edge_being_dragged_snaps_to_a_nearby_real_edge() {
        let mut img = page();
        img.fill_rect(Rect::new(200.0, 100.0, 200.0, 150.0), DARK);
        // Left edge of the box is the boundary at x = 200; right edge at 400.
        assert_eq!(snap_x(&img, 195.0, 100.0, 250.0, 8.0), 200.0);
        assert_eq!(snap_x(&img, 206.0, 100.0, 250.0, 8.0), 200.0);
        assert_eq!(snap_x(&img, 404.0, 120.0, 240.0, 8.0), 400.0);
        assert_eq!(snap_y(&img, 97.0, 200.0, 400.0, 8.0), 100.0);
        assert_eq!(snap_y(&img, 255.0, 200.0, 400.0, 8.0), 250.0);
    }

    #[test]
    fn nothing_snaps_without_an_edge_in_reach() {
        let mut img = page();
        img.fill_rect(Rect::new(200.0, 100.0, 200.0, 150.0), DARK);
        assert_eq!(snap_x(&img, 180.0, 100.0, 250.0, 8.0), 180.0, "too far");
        assert_eq!(snap_x(&img, 500.0, 100.0, 250.0, 8.0), 500.0, "blank page");
        assert_eq!(
            snap_x(&img, 198.0, 300.0, 380.0, 8.0),
            198.0,
            "the edge does not run along this span"
        );
        assert_eq!(
            snap_x(&img, 198.0, 100.0, 105.0, 8.0),
            198.0,
            "span too short to judge"
        );
    }

    #[test]
    fn a_short_edge_does_not_capture_a_long_selection_side() {
        // A 20 px tall mark next to a 300 px tall selection side is noise.
        let mut img = page();
        img.fill_rect(Rect::new(200.0, 100.0, 50.0, 20.0), DARK);
        assert_eq!(snap_x(&img, 203.0, 50.0, 350.0, 8.0), 203.0);
    }

    #[test]
    fn the_stronger_of_two_edges_wins() {
        let mut img = page();
        img.fill_rect(Rect::new(200.0, 100.0, 4.0, 60.0), DARK); // short edges at 200 and 204
        img.fill_rect(Rect::new(207.0, 100.0, 100.0, 150.0), DARK); // full edge at 207
        assert_eq!(snap_x(&img, 203.0, 100.0, 250.0, 8.0), 207.0);
    }

    #[test]
    fn snapping_at_the_screen_border_is_safe() {
        let img = page();
        assert_eq!(snap_x(&img, 0.0, 0.0, 400.0, 8.0), 0.0);
        assert_eq!(snap_x(&img, 600.0, 0.0, 400.0, 8.0), 600.0);
        assert_eq!(snap_y(&img, 400.0, -50.0, 900.0, 8.0), 400.0);
    }
}
