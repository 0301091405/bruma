//! Drawing annotations onto a pixmap.

use tiny_skia::{
    FillRule, FilterQuality, IntSize, LineCap, LineJoin, Paint, Path, PathBuilder, Pattern, Pixmap,
    Shader, SpreadMode, Stroke, Transform,
};

use crate::doc::{Annotation, Shape, marker_width, step_radius};
use crate::geom::{Point, Rect};
use crate::image::Image;
use crate::text::{Face, Fonts};
use crate::theme;

pub(crate) fn solid(color: [u8; 3], alpha: f32) -> Paint<'static> {
    let a = (alpha.clamp(0.0, 1.0) * 255.0).round() as u8;
    Paint {
        shader: Shader::SolidColor(tiny_skia::Color::from_rgba8(
            color[0], color[1], color[2], a,
        )),
        anti_alias: true,
        ..Paint::default()
    }
}

/// Fills a pixel-aligned rectangle without anti-aliasing.
pub(crate) fn fill_rect(pixmap: &mut Pixmap, rect: Rect, color: [u8; 3], alpha: f32) {
    if rect.is_empty() {
        return;
    }
    let Some(r) = tiny_skia::Rect::from_xywh(rect.x, rect.y, rect.w, rect.h) else {
        return;
    };
    let mut paint = solid(color, alpha);
    paint.anti_alias = false;
    pixmap.fill_rect(r, &paint, Transform::identity(), None);
}

pub(crate) fn fill_path(pixmap: &mut Pixmap, path: &Path, color: [u8; 3], alpha: f32) {
    pixmap.fill_path(
        path,
        &solid(color, alpha),
        FillRule::Winding,
        Transform::identity(),
        None,
    );
}

pub(crate) fn stroke_path(
    pixmap: &mut Pixmap,
    path: &Path,
    color: [u8; 3],
    alpha: f32,
    width: f32,
    cap: LineCap,
    join: LineJoin,
) {
    let stroke = Stroke {
        width,
        line_cap: cap,
        line_join: join,
        ..Stroke::default()
    };
    pixmap.stroke_path(
        path,
        &solid(color, alpha),
        &stroke,
        Transform::identity(),
        None,
    );
}

fn segment(a: Point, b: Point) -> Option<Path> {
    let mut pb = PathBuilder::new();
    pb.move_to(a.x, a.y);
    pb.line_to(b.x, b.y);
    pb.finish()
}

/// A polyline smoothed with quadratic curves through segment midpoints.
fn smooth_path(points: &[Point]) -> Option<Path> {
    let first = points.first()?;
    let mut pb = PathBuilder::new();
    pb.move_to(first.x, first.y);
    match points.len() {
        1 => pb.line_to(first.x + 0.01, first.y + 0.01),
        2 => pb.line_to(points[1].x, points[1].y),
        n => {
            for i in 1..n - 1 {
                let (p, next) = (points[i], points[i + 1]);
                pb.quad_to(p.x, p.y, (p.x + next.x) / 2.0, (p.y + next.y) / 2.0);
            }
            pb.line_to(points[n - 1].x, points[n - 1].y);
        }
    }
    pb.finish()
}

fn draw_arrow(pixmap: &mut Pixmap, from: Point, to: Point, color: [u8; 3], stroke: f32) {
    let len = from.distance(to);
    if len < 0.5 {
        return;
    }
    let (ux, uy) = ((to.x - from.x) / len, (to.y - from.y) / len);
    let head = (stroke * 4.0).max(10.0).min(len);
    let half = head * 0.5;
    let base = Point::new(to.x - ux * head, to.y - uy * head);
    if let Some(shaft) = segment(
        from,
        Point::new(to.x - ux * head * 0.9, to.y - uy * head * 0.9),
    ) {
        stroke_path(
            pixmap,
            &shaft,
            color,
            1.0,
            stroke,
            LineCap::Round,
            LineJoin::Round,
        );
    }
    let mut pb = PathBuilder::new();
    pb.move_to(to.x, to.y);
    pb.line_to(base.x - uy * half, base.y + ux * half);
    pb.line_to(base.x + uy * half, base.y - ux * half);
    pb.close();
    if let Some(tip) = pb.finish() {
        fill_path(pixmap, &tip, color, 1.0);
    }
}

/// Frosts `rect` (screen coordinates): the area is reduced to block averages
/// and smoothed back up, so what was there can be neither read nor recovered.
fn draw_blur(pixmap: &mut Pixmap, rect: Rect, offset: Point, screen: &Image, stroke: f32) {
    let (x0, y0, x1, y1) = screen.pixel_span(rect);
    if x1 <= x0 || y1 <= y0 {
        return;
    }
    let block = (stroke * 3.0).round().max(6.0) as usize;
    let (w, h) = ((x1 - x0) as usize, (y1 - y0) as usize);
    let mut region = Vec::with_capacity(w * h * 4);
    for y in y0..y1 {
        let start = (y as usize * screen.width as usize + x0 as usize) * 4;
        region.extend_from_slice(&screen.rgba[start..start + w * 4]);
    }
    let mut small = crate::blur::downscale(&region, w, h, block);
    crate::blur::blur(&mut small, 1, 2);
    let Some(tile) = IntSize::from_wh(small.w as u32, small.h as u32)
        .and_then(|size| Pixmap::from_vec(small.px, size))
    else {
        return;
    };
    let (tx, ty) = (x0 as f32 + offset.x, y0 as f32 + offset.y);
    let Some(target) = tiny_skia::Rect::from_xywh(tx, ty, w as f32, h as f32) else {
        return;
    };
    let paint = Paint {
        shader: Pattern::new(
            tile.as_ref(),
            SpreadMode::Pad,
            FilterQuality::Bilinear,
            1.0,
            Transform::from_scale(block as f32, block as f32).post_translate(tx, ty),
        ),
        anti_alias: false,
        ..Paint::default()
    };
    pixmap.fill_rect(target, &paint, Transform::identity(), None);
}

/// A ruler: a thin line with a tick at each end and its length on a label.
fn draw_measure(
    pixmap: &mut Pixmap,
    from: Point,
    to: Point,
    color: [u8; 3],
    stroke: f32,
    fonts: &Fonts,
) {
    let len = from.distance(to);
    if len < 0.5 {
        return;
    }
    let width = (stroke * 0.6).max(1.5);
    let (ux, uy) = ((to.x - from.x) / len, (to.y - from.y) / len);
    let tick = 5.0 + stroke;
    if let Some(path) = segment(from, to) {
        stroke_path(
            pixmap,
            &path,
            color,
            1.0,
            width,
            LineCap::Butt,
            LineJoin::Round,
        );
    }
    for end in [from, to] {
        let (a, b) = (
            Point::new(end.x - uy * tick, end.y + ux * tick),
            Point::new(end.x + uy * tick, end.y - ux * tick),
        );
        if let Some(path) = segment(a, b) {
            stroke_path(
                pixmap,
                &path,
                color,
                1.0,
                width,
                LineCap::Butt,
                LineJoin::Round,
            );
        }
    }
    let label = format!("{} px", len.round() as i64);
    let px = (11.0 + stroke).round();
    let (tw, th) = fonts.measure(Face::Mono, &label, px);
    let (w, h) = (tw + 12.0, th + 6.0);
    let middle = Point::new((from.x + to.x) / 2.0, (from.y + to.y) / 2.0);
    let plate = Rect::new(
        (middle.x - w / 2.0).round(),
        (middle.y - h / 2.0).round(),
        w,
        h,
    );
    fill_rect(pixmap, plate, color, 1.0);
    fonts.draw(
        pixmap,
        Face::Mono,
        &label,
        px,
        Point::new(plate.x + 6.0, plate.y + 3.0),
        theme::contrast(color),
    );
}

/// Draws one annotation. `offset` is added to every coordinate, so exporting a
/// selection passes its negated origin. The blur tool samples `screen`.
pub fn draw_annotation(
    pixmap: &mut Pixmap,
    annotation: &Annotation,
    offset: Point,
    screen: &Image,
    fonts: &Fonts,
) {
    let color = theme::ink(annotation.color);
    let stroke = annotation.stroke;
    let shift = |p: Point| p.offset(offset.x, offset.y);
    match &annotation.shape {
        Shape::Arrow { from, to } => draw_arrow(pixmap, shift(*from), shift(*to), color, stroke),
        Shape::Line { from, to } => {
            if let Some(path) = segment(shift(*from), shift(*to)) {
                stroke_path(
                    pixmap,
                    &path,
                    color,
                    1.0,
                    stroke,
                    LineCap::Round,
                    LineJoin::Round,
                );
            }
        }
        Shape::Rect { rect } => {
            let r = rect.translate(offset.x, offset.y);
            if let Some(tr) = tiny_skia::Rect::from_xywh(r.x, r.y, r.w.max(0.01), r.h.max(0.01)) {
                let path = PathBuilder::from_rect(tr);
                stroke_path(
                    pixmap,
                    &path,
                    color,
                    1.0,
                    stroke,
                    LineCap::Butt,
                    LineJoin::Miter,
                );
            }
        }
        Shape::Ellipse { rect } => {
            let r = rect.translate(offset.x, offset.y);
            if let Some(path) = tiny_skia::Rect::from_xywh(r.x, r.y, r.w.max(0.01), r.h.max(0.01))
                .and_then(PathBuilder::from_oval)
            {
                stroke_path(
                    pixmap,
                    &path,
                    color,
                    1.0,
                    stroke,
                    LineCap::Butt,
                    LineJoin::Round,
                );
            }
        }
        Shape::Pen { points } => {
            let shifted: Vec<Point> = points.iter().map(|p| shift(*p)).collect();
            if let Some(path) = smooth_path(&shifted) {
                stroke_path(
                    pixmap,
                    &path,
                    color,
                    1.0,
                    stroke,
                    LineCap::Round,
                    LineJoin::Round,
                );
            }
        }
        Shape::Marker { points } => {
            let shifted: Vec<Point> = points.iter().map(|p| shift(*p)).collect();
            if let Some(path) = smooth_path(&shifted) {
                let width = marker_width(stroke);
                stroke_path(
                    pixmap,
                    &path,
                    color,
                    0.4,
                    width,
                    LineCap::Butt,
                    LineJoin::Round,
                );
            }
        }
        Shape::Text { at, text, size, .. } => {
            fonts.draw(pixmap, Face::Ui, text, *size, shift(*at), color);
        }
        Shape::Step { at, n } => {
            let c = shift(*at);
            let radius = step_radius(stroke);
            if let Some(path) = PathBuilder::from_circle(c.x, c.y, radius) {
                fill_path(pixmap, &path, color, 1.0);
            }
            let label = n.to_string();
            let px = (radius * 1.15).round();
            let (w, h) = fonts.measure(Face::Mono, &label, px);
            let origin = Point::new((c.x - w / 2.0).round(), (c.y - h / 2.0).round());
            fonts.draw(
                pixmap,
                Face::Mono,
                &label,
                px,
                origin,
                theme::contrast(color),
            );
        }
        Shape::Blur { rect } => draw_blur(pixmap, *rect, offset, screen, stroke),
        // Focus changes the layer underneath; it is applied before any drawing.
        Shape::Focus { .. } => {}
        Shape::Measure { from, to } => {
            draw_measure(pixmap, shift(*from), shift(*to), color, stroke, fonts)
        }
    }
}
