//! The icon set. Every glyph is drawn here, in code, on a 16×16 grid with a
//! 1.5-unit stroke. No icon font, no bitmaps.

use tiny_skia::{LineCap, LineJoin, Path, PathBuilder, Pixmap, Rect as SkRect, Stroke, Transform};

use crate::draw::solid;
use crate::geom::Point;
use crate::session::Tool;
use crate::strip::StripItem;

/// Side of the design grid.
pub const GRID: f32 = 16.0;

enum Ink {
    Stroke,
    /// A heavier, softer stroke: the highlighter.
    Thick,
}

fn path(build: impl FnOnce(&mut PathBuilder)) -> Option<Path> {
    let mut pb = PathBuilder::new();
    build(&mut pb);
    pb.finish()
}

fn polyline(pb: &mut PathBuilder, points: &[(f32, f32)]) {
    if let Some(((x, y), rest)) = points.split_first() {
        pb.move_to(*x, *y);
        for (x, y) in rest {
            pb.line_to(*x, *y);
        }
    }
}

fn rect(pb: &mut PathBuilder, x: f32, y: f32, w: f32, h: f32) {
    if let Some(r) = SkRect::from_xywh(x, y, w, h) {
        pb.push_rect(r);
    }
}

fn parts(item: StripItem) -> Vec<(Option<Path>, Ink)> {
    use Ink::*;
    match item {
        StripItem::Tool(Tool::Select) => vec![(
            path(|pb| {
                polyline(
                    pb,
                    &[
                        (3.5, 2.0),
                        (3.5, 13.0),
                        (6.4, 10.2),
                        (8.6, 14.6),
                        (10.2, 13.8),
                        (8.0, 9.6),
                        (12.0, 9.6),
                    ],
                );
                pb.close();
            }),
            Stroke,
        )],
        StripItem::Tool(Tool::Arrow) => vec![(
            path(|pb| {
                polyline(pb, &[(3.0, 13.0), (13.0, 3.0)]);
                polyline(pb, &[(7.0, 3.0), (13.0, 3.0), (13.0, 9.0)]);
            }),
            Stroke,
        )],
        StripItem::Tool(Tool::Line) => {
            vec![(path(|pb| polyline(pb, &[(3.0, 13.0), (13.0, 3.0)])), Stroke)]
        }
        StripItem::Tool(Tool::Rect) => vec![(path(|pb| rect(pb, 2.5, 4.0, 11.0, 8.0)), Stroke)],
        StripItem::Tool(Tool::Ellipse) => vec![(
            SkRect::from_xywh(2.0, 3.5, 12.0, 9.0).and_then(PathBuilder::from_oval),
            Stroke,
        )],
        StripItem::Tool(Tool::Pen) => vec![(
            path(|pb| {
                pb.move_to(2.0, 11.5);
                pb.cubic_to(4.0, 1.5, 6.5, 14.5, 8.5, 7.5);
                pb.cubic_to(9.5, 4.0, 12.5, 4.0, 14.0, 6.5);
            }),
            Stroke,
        )],
        StripItem::Tool(Tool::Marker) => {
            vec![(path(|pb| polyline(pb, &[(4.5, 11.5), (11.5, 4.5)])), Thick)]
        }
        StripItem::Tool(Tool::Text) => vec![(
            path(|pb| {
                polyline(pb, &[(3.0, 3.5), (13.0, 3.5)]);
                polyline(pb, &[(8.0, 3.5), (8.0, 13.0)]);
            }),
            Stroke,
        )],
        StripItem::Tool(Tool::Step) => vec![
            (PathBuilder::from_circle(8.0, 8.0, 6.0), Stroke),
            (
                path(|pb| polyline(pb, &[(6.8, 6.4), (8.4, 5.4), (8.4, 10.8)])),
                Stroke,
            ),
        ],
        // A drop: the mist tool.
        StripItem::Tool(Tool::Blur) => vec![(
            path(|pb| {
                pb.move_to(8.0, 2.2);
                pb.cubic_to(5.6, 5.4, 3.6, 7.9, 3.6, 10.0);
                pb.cubic_to(3.6, 12.5, 5.6, 14.2, 8.0, 14.2);
                pb.cubic_to(10.4, 14.2, 12.4, 12.5, 12.4, 10.0);
                pb.cubic_to(12.4, 7.9, 10.4, 5.4, 8.0, 2.2);
                pb.close();
            }),
            Stroke,
        )],
        // A viewfinder with a point in the middle: the focus tool.
        StripItem::Tool(Tool::Focus) => vec![
            (
                path(|pb| {
                    polyline(pb, &[(2.5, 6.0), (2.5, 2.5), (6.0, 2.5)]);
                    polyline(pb, &[(10.0, 2.5), (13.5, 2.5), (13.5, 6.0)]);
                    polyline(pb, &[(13.5, 10.0), (13.5, 13.5), (10.0, 13.5)]);
                    polyline(pb, &[(6.0, 13.5), (2.5, 13.5), (2.5, 10.0)]);
                }),
                Stroke,
            ),
            (PathBuilder::from_circle(8.0, 8.0, 1.6), Stroke),
        ],
        StripItem::Tool(Tool::Measure) => vec![(
            path(|pb| {
                polyline(pb, &[(2.5, 8.0), (13.5, 8.0)]);
                polyline(pb, &[(2.5, 4.5), (2.5, 11.5)]);
                polyline(pb, &[(13.5, 4.5), (13.5, 11.5)]);
            }),
            Stroke,
        )],
        StripItem::Color => vec![],
        StripItem::Undo => vec![(
            path(|pb| {
                polyline(pb, &[(6.5, 3.5), (4.0, 6.0), (6.5, 8.5)]);
                polyline(pb, &[(4.0, 6.0), (9.5, 6.0)]);
                pb.cubic_to(13.5, 6.0, 13.5, 12.0, 9.5, 12.0);
                pb.line_to(6.0, 12.0);
            }),
            Stroke,
        )],
        StripItem::Redo => vec![(
            path(|pb| {
                polyline(pb, &[(9.5, 3.5), (12.0, 6.0), (9.5, 8.5)]);
                polyline(pb, &[(12.0, 6.0), (6.5, 6.0)]);
                pb.cubic_to(2.5, 6.0, 2.5, 12.0, 6.5, 12.0);
                pb.line_to(10.0, 12.0);
            }),
            Stroke,
        )],
        StripItem::Copy => vec![(
            path(|pb| {
                rect(pb, 5.5, 5.5, 8.0, 8.0);
                polyline(
                    pb,
                    &[
                        (10.5, 5.5),
                        (10.5, 2.5),
                        (2.5, 2.5),
                        (2.5, 10.5),
                        (5.5, 10.5),
                    ],
                );
            }),
            Stroke,
        )],
        // A T in the corners of a frame: the text read from a picture.
        StripItem::CopyText => vec![(
            path(|pb| {
                polyline(pb, &[(2.5, 5.5), (2.5, 2.5), (5.5, 2.5)]);
                polyline(pb, &[(10.5, 2.5), (13.5, 2.5), (13.5, 5.5)]);
                polyline(pb, &[(13.5, 10.5), (13.5, 13.5), (10.5, 13.5)]);
                polyline(pb, &[(5.5, 13.5), (2.5, 13.5), (2.5, 10.5)]);
                polyline(pb, &[(5.5, 5.5), (10.5, 5.5)]);
                polyline(pb, &[(8.0, 5.5), (8.0, 10.5)]);
            }),
            Stroke,
        )],
        StripItem::Save => vec![(
            path(|pb| {
                polyline(pb, &[(8.0, 2.5), (8.0, 10.0)]);
                polyline(pb, &[(4.5, 6.5), (8.0, 10.0), (11.5, 6.5)]);
                polyline(pb, &[(2.5, 13.5), (13.5, 13.5)]);
            }),
            Stroke,
        )],
        StripItem::Pin => vec![(
            path(|pb| {
                polyline(pb, &[(5.8, 9.0), (6.6, 2.8), (9.4, 2.8), (10.2, 9.0)]);
                polyline(pb, &[(3.8, 9.0), (12.2, 9.0)]);
                polyline(pb, &[(8.0, 9.0), (8.0, 14.0)]);
            }),
            Stroke,
        )],
    }
}

/// Draws a strip glyph into the square of `size` pixels whose top-left is `origin`.
pub fn draw(pixmap: &mut Pixmap, item: StripItem, origin: Point, size: f32, color: [u8; 3]) {
    let k = size / GRID;
    let transform = Transform::from_scale(k, k).post_translate(origin.x, origin.y);
    for (path, ink) in parts(item) {
        let Some(path) = path else { continue };
        let thick = matches!(ink, Ink::Thick);
        let stroke = Stroke {
            width: if thick { 4.5 } else { 1.5 },
            line_cap: if thick { LineCap::Butt } else { LineCap::Round },
            line_join: LineJoin::Round,
            ..Stroke::default()
        };
        let alpha = if thick { 0.75 } else { 1.0 };
        pixmap.stroke_path(&path, &solid(color, alpha), &stroke, transform, None);
    }
}

/// The "return" arrow used as the key hint for Enter, in a `size`-pixel square.
pub fn draw_enter(pixmap: &mut Pixmap, origin: Point, size: f32, color: [u8; 3]) {
    let k = size / GRID;
    let transform = Transform::from_scale(k, k).post_translate(origin.x, origin.y);
    let Some(path) = path(|pb| {
        polyline(pb, &[(13.0, 3.0), (13.0, 9.5), (3.5, 9.5)]);
        polyline(pb, &[(6.5, 6.5), (3.5, 9.5), (6.5, 12.5)]);
    }) else {
        return;
    };
    let stroke = Stroke {
        width: 1.8,
        line_cap: LineCap::Round,
        line_join: LineJoin::Round,
        ..Stroke::default()
    };
    pixmap.stroke_path(&path, &solid(color, 1.0), &stroke, transform, None);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::strip;

    #[test]
    fn every_strip_item_draws_something_except_the_colour_dot() {
        let layout = strip::layout(
            crate::geom::Rect::new(0.0, 0.0, 600.0, 300.0),
            crate::geom::Rect::new(0.0, 0.0, 1920.0, 1080.0),
            1.0,
        );
        for (item, _) in layout.cells {
            let mut pixmap = Pixmap::new(16, 16).unwrap();
            draw(
                &mut pixmap,
                item,
                Point::new(0.0, 0.0),
                16.0,
                [255, 255, 255],
            );
            let inked = pixmap.data().iter().any(|&b| b != 0);
            assert_eq!(inked, item != StripItem::Color, "{item:?}");
        }
    }
}
