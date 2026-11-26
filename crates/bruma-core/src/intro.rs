//! The first thing Bruma shows, once: the screen goes to mist, the four
//! crop marks of the mark and its dot appear in the middle, the crop marks
//! are pulled out to the corners of the screen and off it, and the dot that
//! is left blinks like an eye and closes. Then the mist clears.
//!
//! Pure: a time in, a frame out.

use tiny_skia::Pixmap;

use crate::backdrop::Backdrop;
use crate::geom::Point;
use crate::logo;

/// How long the whole thing lasts, in seconds.
pub const LENGTH: f32 = 2.6;
/// Where a skipped intro carries on from: the eye shut, the mist clearing.
pub const OUTRO: f32 = 2.16;

/// The size of the mark at a scale of 1: the square its crop marks frame.
const SIZE: f32 = 76.0;

/// 0 before `start`, 1 after `start + length`, smooth in between.
fn ease(t: f32, start: f32, length: f32) -> f32 {
    let x = ((t - start) / length).clamp(0.0, 1.0);
    x * x * (3.0 - 2.0 * x)
}

/// Like [`ease`], but slow to leave and quick to arrive: things being pulled.
fn pull(t: f32, start: f32, length: f32) -> f32 {
    let x = ((t - start) / length).clamp(0.0, 1.0);
    if x < 0.5 {
        4.0 * x * x * x
    } else {
        1.0 - (-2.0 * x + 2.0).powi(3) / 2.0
    }
}

/// How open the eye is: 1 open, near 0 shut. One blink that starts at
/// `start`, closing quickly and opening a little slower.
fn blink(t: f32, start: f32) -> f32 {
    const SHUT: f32 = 0.06;
    let (close, open) = (0.09, 0.13);
    let u = t - start;
    if !(0.0..close + open).contains(&u) {
        return 1.0;
    }
    if u < close {
        1.0 - (1.0 - SHUT) * ease(u, 0.0, close)
    } else {
        SHUT + (1.0 - SHUT) * ease(u, close, open)
    }
}

/// How far into mist the screen is at `t`.
pub fn mist(t: f32) -> f32 {
    ease(t, 0.0, 0.35) * (1.0 - ease(t, OUTRO - 0.05, LENGTH - OUTRO + 0.05))
}

/// Draws the frame at `t` seconds over `backdrop`, on a pixmap the size of
/// the screen. `scale` is the monitor's.
pub fn draw(pixmap: &mut Pixmap, backdrop: &Backdrop, scale: f32, t: f32) {
    backdrop.paint(pixmap, mist(t));
    if t >= OUTRO {
        return;
    }
    let (w, h) = (pixmap.width() as f32, pixmap.height() as f32);
    let centre = Point::new(w / 2.0, h / 2.0);

    // The mark comes in, a little small and see-through, and settles.
    let appear = ease(t, 0.08, 0.4);
    let size = SIZE * scale * (0.9 + 0.1 * appear);
    let rest = logo::parts(centre, size);
    let leave = 0.9;

    // The crop marks are pulled out to the corners of the screen, growing on
    // the way, until they are off it.
    let fly = pull(t, leave, 0.62);
    let arm = rest.arm * (1.0 + 2.5 * fly);
    let thickness = rest.thickness * (1.0 + 0.6 * fly);
    let screen_corners = [
        Point::new(0.0, 0.0),
        Point::new(w, 0.0),
        Point::new(0.0, h),
        Point::new(w, h),
    ];
    for ((from, inward), corner) in rest.corners.iter().zip(rest.inward).zip(screen_corners) {
        // Just past the screen's corner, so that the whole mark is outside.
        let beyond = arm + thickness + 2.0;
        let to = Point::new(corner.x - inward.0 * beyond, corner.y - inward.1 * beyond);
        let at = Point::new(
            from.x + (to.x - from.x) * fly,
            from.y + (to.y - from.y) * fly,
        );
        if fly < 1.0 {
            logo::mark(pixmap, at, inward, arm, thickness, appear);
        }
    }

    // The dot is left alone and becomes an eye: it opens wide and round,
    // blinks once, and shuts.
    let awake = ease(t, leave, 0.5);
    let wide = rest.dot * (1.0 + 1.0 * awake);
    let round = 0.22 + 0.28 * awake;
    let shut = ease(t, 1.96, OUTRO - 1.96);
    let open = blink(t, 1.6) * (1.0 - shut);
    let dot_w = wide * (1.0 + 0.3 * (1.0 - open)) * (1.0 - 0.4 * shut);
    let dot_h = (wide * open).max(if shut < 1.0 { scale } else { 0.0 });
    logo::dot(pixmap, centre, dot_w, dot_h, round, appear);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::image::Image;
    use crate::theme::{ACCENT, PAPER};

    fn backdrop() -> (Image, Backdrop) {
        let img = Image::filled(400, 240, [120, 130, 140, 255]);
        let b = Backdrop::new(&img, 1.0).unwrap();
        (img, b)
    }

    fn frame(b: &Backdrop, t: f32) -> Pixmap {
        let mut pixmap = Pixmap::new(400, 240).unwrap();
        draw(&mut pixmap, b, 0.5, t);
        pixmap
    }

    fn count(pixmap: &Pixmap, color: [u8; 3]) -> usize {
        pixmap
            .data()
            .chunks_exact(4)
            .filter(|p| p[..3] == color)
            .count()
    }

    #[test]
    fn it_starts_and_ends_on_the_screen_as_it_was() {
        let (img, b) = backdrop();
        assert_eq!(frame(&b, 0.0).data(), &img.rgba[..]);
        assert_eq!(frame(&b, LENGTH).data(), &img.rgba[..]);
    }

    #[test]
    fn the_whole_mark_shows_before_it_comes_apart() {
        let (_, b) = backdrop();
        let whole = frame(&b, 0.7);
        assert!(count(&whole, ACCENT) > 0);
        assert!(count(&whole, PAPER) > 0);
    }

    #[test]
    fn the_crop_marks_leave_and_the_dot_stays_until_the_eye_shuts() {
        let (_, b) = backdrop();
        let alone = frame(&b, 1.7);
        assert_eq!(count(&alone, PAPER), 0, "the crop marks are off the screen");
        let open = frame(&b, 1.85);
        assert!(count(&open, ACCENT) > 0, "the eye is open after the blink");
        let blinking = frame(&b, 1.69);
        assert!(count(&blinking, ACCENT) < count(&open, ACCENT));
        let gone = frame(&b, OUTRO + 0.01);
        assert_eq!(count(&gone, ACCENT), 0);
    }

    #[test]
    fn the_crop_marks_fly_towards_the_corners() {
        let (_, b) = backdrop();
        // Half way out, the paper is nearer the corners than the middle.
        let mid = frame(&b, 1.2);
        let (mut near_corners, mut near_middle) = (0, 0);
        for (i, p) in mid.data().chunks_exact(4).enumerate() {
            if p[..3] != PAPER {
                continue;
            }
            let (x, y) = ((i % 400) as f32, (i / 400) as f32);
            let from_middle = ((x - 200.0).powi(2) + (y - 120.0).powi(2)).sqrt();
            if from_middle > 80.0 {
                near_corners += 1;
            } else {
                near_middle += 1;
            }
        }
        assert!(
            near_corners > near_middle,
            "{near_corners} vs {near_middle}"
        );
    }

    #[test]
    fn the_mist_comes_and_goes() {
        assert_eq!(mist(0.0), 0.0);
        assert_eq!(mist(1.0), 1.0);
        assert_eq!(mist(LENGTH), 0.0);
        assert!(mist(OUTRO + 0.2) < 1.0 && mist(OUTRO + 0.2) > 0.0);
    }
}
