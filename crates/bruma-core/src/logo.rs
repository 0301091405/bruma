//! The Bruma mark: four crop marks around an accent dot, on an ink plate.
//!
//! It is drawn in code at any size and in pieces, so that the intro can take
//! it apart. The proportions are the tray icon's.

use tiny_skia::{PathBuilder, Pixmap};

use crate::draw::fill_path;
use crate::geom::{Point, Rect};
use crate::render::rounded_rect;
use crate::theme::{ACCENT, INK, PAPER};

/// Where the parts of a mark of plate side `size` sit when it is whole.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Parts {
    /// The outer corner of each crop mark: top-left, top-right, bottom-left,
    /// bottom-right.
    pub corners: [Point; 4],
    /// For each mark, the direction its arms point in, towards the middle.
    pub inward: [(f32, f32); 4],
    pub arm: f32,
    pub thickness: f32,
    pub dot: f32,
}

pub fn parts(centre: Point, size: f32) -> Parts {
    let half = size / 2.0;
    let inset = size * 0.22;
    let near = half - inset;
    Parts {
        corners: [
            Point::new(centre.x - near, centre.y - near),
            Point::new(centre.x + near, centre.y - near),
            Point::new(centre.x - near, centre.y + near),
            Point::new(centre.x + near, centre.y + near),
        ],
        inward: [(1.0, 1.0), (-1.0, 1.0), (1.0, -1.0), (-1.0, -1.0)],
        arm: size * 0.22,
        thickness: (size / 10.0).max(1.0),
        dot: (size * 0.16).max(2.0),
    }
}

/// The rounded ink square behind the marks.
pub fn plate(pixmap: &mut Pixmap, centre: Point, size: f32, alpha: f32) {
    let rect = Rect::new(centre.x - size / 2.0, centre.y - size / 2.0, size, size);
    if let Some(path) = rounded_rect(rect, size * 0.19) {
        fill_path(pixmap, &path, INK, alpha);
    }
}

/// One crop mark: an L whose outer corner is `corner` and whose arms run
/// `arm` pixels in the direction `inward`.
pub fn mark(
    pixmap: &mut Pixmap,
    corner: Point,
    inward: (f32, f32),
    arm: f32,
    thickness: f32,
    alpha: f32,
) {
    let span = |from: f32, dir: f32, len: f32| {
        let to = from + dir * len;
        (from.min(to), (from - to).abs())
    };
    let (hx, hw) = span(corner.x, inward.0, arm);
    let (hy, hh) = span(corner.y, inward.1, thickness);
    let (vx, vw) = span(corner.x, inward.0, thickness);
    let (vy, vh) = span(corner.y, inward.1, arm);
    let mut pb = PathBuilder::new();
    for (x, y, w, h) in [(hx, hy, hw, hh), (vx, vy, vw, vh)] {
        if let Some(r) = tiny_skia::Rect::from_xywh(x, y, w, h) {
            pb.push_rect(r);
        }
    }
    if let Some(path) = pb.finish() {
        fill_path(pixmap, &path, PAPER, alpha);
    }
}

/// The accent dot, `w` by `h` pixels, its corners rounded by `round` (0.22
/// is the mark's, 0.5 a circle). Squashed, it reads as an eye closing.
pub fn dot(pixmap: &mut Pixmap, centre: Point, w: f32, h: f32, round: f32, alpha: f32) {
    if w <= 0.0 || h <= 0.0 {
        return;
    }
    let rect = Rect::new(centre.x - w / 2.0, centre.y - h / 2.0, w, h);
    if let Some(path) = rounded_rect(rect, w.min(h) * round.clamp(0.0, 0.5)) {
        fill_path(pixmap, &path, ACCENT, alpha);
    }
}

/// The whole mark, plate side `size`, centred on `centre`.
pub fn draw(pixmap: &mut Pixmap, centre: Point, size: f32) {
    let p = parts(centre, size);
    plate(pixmap, centre, size, 1.0);
    for (corner, inward) in p.corners.into_iter().zip(p.inward) {
        mark(pixmap, corner, inward, p.arm, p.thickness, 1.0);
    }
    dot(pixmap, centre, p.dot, p.dot, 0.22, 1.0);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(pixmap: &Pixmap, x: u32, y: u32) -> [u8; 4] {
        let i = ((y * pixmap.width() + x) * 4) as usize;
        let d = pixmap.data();
        [d[i], d[i + 1], d[i + 2], d[i + 3]]
    }

    #[test]
    fn the_mark_has_ink_corners_paper_marks_and_an_accent_middle() {
        let mut pixmap = Pixmap::new(100, 100).unwrap();
        draw(&mut pixmap, Point::new(50.0, 50.0), 100.0);
        assert_eq!(at(&pixmap, 50, 50)[..3], ACCENT);
        // Just inside the top-left mark's corner.
        assert_eq!(at(&pixmap, 24, 24)[..3], PAPER);
        // Between the marks and the dot: the plate.
        assert_eq!(at(&pixmap, 38, 38)[..3], INK);
        // Outside the rounded plate: nothing.
        assert_eq!(at(&pixmap, 0, 0)[3], 0);
    }

    #[test]
    fn marks_point_inwards_from_their_corners() {
        let p = parts(Point::new(0.0, 0.0), 100.0);
        for (corner, (dx, dy)) in p.corners.iter().zip(p.inward) {
            // Moving along the arms goes towards the middle.
            assert!((corner.x + dx * p.arm).abs() < corner.x.abs());
            assert!((corner.y + dy * p.arm).abs() < corner.y.abs());
        }
    }
}
