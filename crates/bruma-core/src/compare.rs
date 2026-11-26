//! Before and after: two moments of the same area, side by side, with what
//! changed outlined. The picture a bug report wants.

use tiny_skia::{IntSize, Pixmap};

use crate::draw::fill_rect;
use crate::geom::{Point, Rect};
use crate::image::Image;
use crate::text::{Face, Fonts};
use crate::theme::{ACCENT, INK, PAPER};

/// Composes `before` and `after`, both cropped to `area`, into one image with
/// a caption over each half. `marks` are the changed areas, in the same
/// coordinates as `area`; they are outlined on both halves.
pub fn side_by_side(
    before: &Image,
    after: &Image,
    area: Rect,
    marks: &[Rect],
    captions: (&str, &str),
    fonts: &Fonts,
    scale: f32,
) -> Image {
    let (left, right) = (before.crop(area), after.crop(area));
    let (w, h) = (left.width.min(right.width), left.height.min(right.height));
    if w == 0 || h == 0 {
        return left;
    }
    let gap = (12.0 * scale).round() as u32;
    let bar = (30.0 * scale).round() as u32;
    let (total_w, total_h) = (2 * w + gap, h + bar);
    let mut canvas = Image::filled(total_w, total_h, [INK[0], INK[1], INK[2], 255]);
    let origin = area.round().intersect(&before.bounds());
    for (half, image) in [(0u32, &left), (1, &right)] {
        let x0 = half * (w + gap);
        for y in 0..h {
            let from = (y * image.width) as usize * 4;
            let to = ((y + bar) * total_w + x0) as usize * 4;
            canvas.rgba[to..to + w as usize * 4]
                .copy_from_slice(&image.rgba[from..from + w as usize * 4]);
        }
    }
    let Some(mut pixmap) =
        IntSize::from_wh(total_w, total_h).and_then(|s| Pixmap::from_vec(canvas.rgba.clone(), s))
    else {
        return canvas;
    };
    let px = (12.0 * scale).round();
    let line = (2.0 * scale).round().max(1.0);
    for (half, caption) in [(0u32, captions.0), (1, captions.1)] {
        let x0 = (half * (w + gap)) as f32;
        let (_, th) = fonts.measure(Face::Mono, caption, px);
        let dot = (6.0 * scale).round();
        let cy = bar as f32 / 2.0;
        fill_rect(
            &mut pixmap,
            Rect::new(
                x0 + (10.0 * scale).round(),
                (cy - dot / 2.0).round(),
                dot,
                dot,
            ),
            ACCENT,
            1.0,
        );
        let at = Point::new(
            x0 + (10.0 * scale).round() + dot + (8.0 * scale).round(),
            (cy - th / 2.0).round(),
        );
        fonts.draw(&mut pixmap, Face::Mono, caption, px, at, PAPER);
        // Outline what changed, clipped to this half.
        let half_rect = Rect::new(x0, bar as f32, w as f32, h as f32);
        for mark in marks {
            let r = mark
                .translate(-origin.x + x0, -origin.y + bar as f32)
                .inflate(line)
                .round()
                .intersect(&half_rect);
            if r.w > 2.0 * line && r.h > 2.0 * line {
                fill_rect(&mut pixmap, Rect::new(r.x, r.y, r.w, line), ACCENT, 1.0);
                fill_rect(
                    &mut pixmap,
                    Rect::new(r.x, r.bottom() - line, r.w, line),
                    ACCENT,
                    1.0,
                );
                fill_rect(&mut pixmap, Rect::new(r.x, r.y, line, r.h), ACCENT, 1.0);
                fill_rect(
                    &mut pixmap,
                    Rect::new(r.right() - line, r.y, line, r.h),
                    ACCENT,
                    1.0,
                );
            }
        }
    }
    Image::new(total_w, total_h, pixmap.take())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn two_halves_with_a_caption_bar_and_the_change_outlined() {
        let before = Image::filled(300, 200, [255, 255, 255, 255]);
        let mut after = before.clone();
        let change = Rect::new(100.0, 80.0, 60.0, 40.0);
        after.fill_rect(change, [20, 20, 20, 255]);
        let out = side_by_side(
            &before,
            &after,
            Rect::new(50.0, 50.0, 200.0, 120.0),
            &[change],
            ("before", "after"),
            &Fonts::empty(),
            1.0,
        );
        assert_eq!((out.width, out.height), (2 * 200 + 12, 120 + 30));
        assert_eq!(
            out.pixel(100, 30 + 60),
            [255, 255, 255, 255],
            "left half: before, still blank"
        );
        assert_eq!(
            out.pixel(212 + 80, 30 + 50),
            [20, 20, 20, 255],
            "right half: after, with the box"
        );
        // The change sits at (50, 30) within the area; its outline is just outside it.
        assert_eq!(
            out.pixel(212 + 49, 30 + 50)[..3],
            ACCENT,
            "outline on the after side"
        );
        assert_eq!(
            out.pixel(49, 30 + 50)[..3],
            ACCENT,
            "and on the before side"
        );
        assert_eq!(out.pixel(205, 100)[..3], INK, "the gap between the halves");
    }

    #[test]
    fn an_area_off_the_image_gives_an_empty_picture() {
        let img = Image::filled(50, 50, [0, 0, 0, 255]);
        let out = side_by_side(
            &img,
            &img,
            Rect::new(500.0, 500.0, 10.0, 10.0),
            &[],
            ("a", "b"),
            &Fonts::empty(),
            1.0,
        );
        assert_eq!((out.width, out.height), (0, 0));
    }

    #[test]
    fn marks_outside_the_area_are_ignored() {
        let img = Image::filled(100, 100, [200, 200, 200, 255]);
        let out = side_by_side(
            &img,
            &img,
            Rect::new(0.0, 0.0, 50.0, 50.0),
            &[Rect::new(80.0, 80.0, 10.0, 10.0)],
            ("a", "b"),
            &Fonts::empty(),
            2.0,
        );
        assert_eq!(out.width, 2 * 50 + 24);
    }
}
