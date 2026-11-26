//! Annotations and their undo history.

use crate::geom::{Point, Rect, distance_to_segment};

#[derive(Clone, Debug, PartialEq)]
pub enum Shape {
    Arrow {
        from: Point,
        to: Point,
    },
    Line {
        from: Point,
        to: Point,
    },
    Rect {
        rect: Rect,
    },
    Ellipse {
        rect: Rect,
    },
    Pen {
        points: Vec<Point>,
    },
    Marker {
        points: Vec<Point>,
    },
    /// `size` is the font size in pixels, `extent` the measured width and height.
    Text {
        at: Point,
        text: String,
        size: f32,
        extent: (f32, f32),
    },
    Step {
        at: Point,
        n: u32,
    },
    Blur {
        rect: Rect,
    },
    /// An area that stays sharp while the rest of the capture turns to mist.
    Focus {
        rect: Rect,
    },
    /// A ruler: a line labelled with its length in pixels.
    Measure {
        from: Point,
        to: Point,
    },
}

#[derive(Clone, Debug, PartialEq)]
pub struct Annotation {
    pub shape: Shape,
    /// Index into [`crate::theme::PALETTE`].
    pub color: u8,
    /// Stroke width in physical pixels.
    pub stroke: f32,
}

/// Radius of a step marker for a given stroke width.
pub fn step_radius(stroke: f32) -> f32 {
    stroke * 2.5 + 5.0
}

/// Width of a highlighter stroke for a given stroke width.
pub fn marker_width(stroke: f32) -> f32 {
    stroke * 5.0 + 6.0
}

fn points_bounds(points: &[Point]) -> Rect {
    let mut it = points.iter();
    let Some(first) = it.next() else {
        return Rect::default();
    };
    let (mut x0, mut y0, mut x1, mut y1) = (first.x, first.y, first.x, first.y);
    for p in it {
        x0 = x0.min(p.x);
        y0 = y0.min(p.y);
        x1 = x1.max(p.x);
        y1 = y1.max(p.y);
    }
    Rect::new(x0, y0, x1 - x0, y1 - y0)
}

fn polyline_distance(points: &[Point], p: Point) -> f32 {
    match points {
        [] => f32::INFINITY,
        [only] => only.distance(p),
        _ => points
            .windows(2)
            .map(|w| distance_to_segment(p, w[0], w[1]))
            .fold(f32::INFINITY, f32::min),
    }
}

impl Annotation {
    pub fn translate(&mut self, dx: f32, dy: f32) {
        match &mut self.shape {
            Shape::Arrow { from, to } | Shape::Line { from, to } | Shape::Measure { from, to } => {
                *from = from.offset(dx, dy);
                *to = to.offset(dx, dy);
            }
            Shape::Rect { rect }
            | Shape::Ellipse { rect }
            | Shape::Blur { rect }
            | Shape::Focus { rect } => {
                *rect = rect.translate(dx, dy);
            }
            Shape::Pen { points } | Shape::Marker { points } => {
                for p in points {
                    *p = p.offset(dx, dy);
                }
            }
            Shape::Text { at, .. } | Shape::Step { at, .. } => *at = at.offset(dx, dy),
        }
    }

    /// Bounding box including stroke width.
    pub fn bounds(&self) -> Rect {
        let half = self.stroke / 2.0;
        match &self.shape {
            Shape::Arrow { from, to } => {
                Rect::from_points(*from, *to).inflate(half + self.stroke * 2.0)
            }
            Shape::Line { from, to } => Rect::from_points(*from, *to).inflate(half),
            Shape::Measure { from, to } => Rect::from_points(*from, *to).inflate(half + 14.0),
            Shape::Rect { rect } | Shape::Ellipse { rect } => rect.inflate(half),
            Shape::Blur { rect } | Shape::Focus { rect } => *rect,
            Shape::Pen { points } => points_bounds(points).inflate(half),
            Shape::Marker { points } => {
                points_bounds(points).inflate(marker_width(self.stroke) / 2.0)
            }
            Shape::Text { at, extent, .. } => Rect::new(at.x, at.y, extent.0, extent.1),
            Shape::Step { at, .. } => {
                let r = step_radius(self.stroke);
                Rect::new(at.x - r, at.y - r, 2.0 * r, 2.0 * r)
            }
        }
    }

    /// Whether a pointer at `p` grabs this annotation, with `tol` pixels of slack.
    pub fn hit(&self, p: Point, tol: f32) -> bool {
        let reach = self.stroke / 2.0 + tol;
        match &self.shape {
            Shape::Arrow { from, to } | Shape::Line { from, to } | Shape::Measure { from, to } => {
                distance_to_segment(p, *from, *to) <= reach
            }
            Shape::Rect { rect } => {
                rect.inflate(reach).contains(p) && !rect.inflate(-reach).contains(p)
            }
            Shape::Ellipse { rect } => {
                if rect.is_empty() {
                    return false;
                }
                let c = rect.center();
                let (rx, ry) = (rect.w / 2.0, rect.h / 2.0);
                let norm = |grow: f32| {
                    let (ax, ay) = ((rx + grow).max(0.5), (ry + grow).max(0.5));
                    ((p.x - c.x) / ax).powi(2) + ((p.y - c.y) / ay).powi(2)
                };
                norm(reach) <= 1.0 && (rx <= reach || ry <= reach || norm(-reach) >= 1.0)
            }
            Shape::Pen { points } => polyline_distance(points, p) <= reach,
            Shape::Marker { points } => {
                polyline_distance(points, p) <= marker_width(self.stroke) / 2.0 + tol
            }
            Shape::Text { .. } | Shape::Blur { .. } | Shape::Focus { .. } => {
                self.bounds().inflate(tol).contains(p)
            }
            Shape::Step { at, .. } => at.distance(p) <= step_radius(self.stroke) + tol,
        }
    }
}

fn numbers(values: &[f32]) -> String {
    values
        .iter()
        .map(|v| format!("{v}"))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Writes annotations as text, one per line, so a capture can carry its own
/// drawings and be edited again later.
pub fn serialize(items: &[Annotation]) -> String {
    let mut out = String::new();
    for item in items {
        let (kind, data, tail) = match &item.shape {
            Shape::Arrow { from, to } => ("arrow", numbers(&[from.x, from.y, to.x, to.y]), None),
            Shape::Line { from, to } => ("line", numbers(&[from.x, from.y, to.x, to.y]), None),
            Shape::Measure { from, to } => {
                ("measure", numbers(&[from.x, from.y, to.x, to.y]), None)
            }
            Shape::Rect { rect } => ("rect", numbers(&[rect.x, rect.y, rect.w, rect.h]), None),
            Shape::Ellipse { rect } => {
                ("ellipse", numbers(&[rect.x, rect.y, rect.w, rect.h]), None)
            }
            Shape::Blur { rect } => ("blur", numbers(&[rect.x, rect.y, rect.w, rect.h]), None),
            Shape::Focus { rect } => ("focus", numbers(&[rect.x, rect.y, rect.w, rect.h]), None),
            Shape::Pen { points } | Shape::Marker { points } => {
                let kind = if matches!(item.shape, Shape::Pen { .. }) {
                    "pen"
                } else {
                    "marker"
                };
                let flat: Vec<f32> = points.iter().flat_map(|p| [p.x, p.y]).collect();
                (kind, numbers(&flat), None)
            }
            Shape::Text {
                at,
                text,
                size,
                extent,
            } => {
                let escaped = text.replace('\\', "\\\\").replace('\n', "\\n");
                (
                    "text",
                    numbers(&[at.x, at.y, *size, extent.0, extent.1]),
                    Some(escaped),
                )
            }
            Shape::Step { at, n } => ("step", numbers(&[at.x, at.y, *n as f32]), None),
        };
        out.push_str(&format!("{kind} {} {} {data}", item.color, item.stroke));
        if let Some(text) = tail {
            out.push_str(" |");
            out.push_str(&text);
        }
        out.push('\n');
    }
    out
}

/// Reads what [`serialize`] wrote. Lines that cannot be read are skipped, so
/// a file from a newer version still opens with what this one understands.
pub fn parse(text: &str) -> Vec<Annotation> {
    text.lines().filter_map(parse_line).collect()
}

fn parse_line(line: &str) -> Option<Annotation> {
    let (head, tail) = match line.split_once(" |") {
        Some((head, tail)) => (head, Some(tail)),
        None => (line, None),
    };
    let mut words = head.split_whitespace();
    let kind = words.next()?;
    let color: u8 = words.next()?.parse().ok()?;
    let stroke: f32 = words.next()?.parse().ok()?;
    let v: Vec<f32> = words.map(|w| w.parse().ok()).collect::<Option<_>>()?;
    let point = |i: usize| Some(Point::new(*v.get(i)?, *v.get(i + 1)?));
    let rect = || Some(Rect::new(*v.first()?, *v.get(1)?, *v.get(2)?, *v.get(3)?));
    let shape = match kind {
        "arrow" => Shape::Arrow {
            from: point(0)?,
            to: point(2)?,
        },
        "line" => Shape::Line {
            from: point(0)?,
            to: point(2)?,
        },
        "measure" => Shape::Measure {
            from: point(0)?,
            to: point(2)?,
        },
        "rect" => Shape::Rect { rect: rect()? },
        "ellipse" => Shape::Ellipse { rect: rect()? },
        "blur" => Shape::Blur { rect: rect()? },
        "focus" => Shape::Focus { rect: rect()? },
        "pen" | "marker" => {
            let points: Vec<Point> = v.chunks_exact(2).map(|c| Point::new(c[0], c[1])).collect();
            if points.is_empty() {
                return None;
            }
            if kind == "pen" {
                Shape::Pen { points }
            } else {
                Shape::Marker { points }
            }
        }
        "text" => {
            let mut unescaped = String::new();
            let mut chars = tail?.chars();
            while let Some(c) = chars.next() {
                match (c, c == '\\') {
                    (_, true) => match chars.next() {
                        Some('n') => unescaped.push('\n'),
                        Some(other) => unescaped.push(other),
                        None => {}
                    },
                    (c, false) => unescaped.push(c),
                }
            }
            Shape::Text {
                at: point(0)?,
                text: unescaped,
                size: *v.get(2)?,
                extent: (*v.get(3)?, *v.get(4)?),
            }
        }
        "step" => Shape::Step {
            at: point(0)?,
            n: *v.get(2)? as u32,
        },
        _ => return None,
    };
    Some(Annotation {
        shape,
        color,
        stroke,
    })
}

/// The list of annotations plus snapshot-based undo and redo.
#[derive(Clone, Debug, Default)]
pub struct Doc {
    items: Vec<Annotation>,
    past: Vec<Vec<Annotation>>,
    future: Vec<Vec<Annotation>>,
}

impl Doc {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn items(&self) -> &[Annotation] {
        &self.items
    }

    /// Applies a change as one undoable step. A change that leaves the list
    /// identical records nothing.
    pub fn edit(&mut self, change: impl FnOnce(&mut Vec<Annotation>)) {
        let before = self.items.clone();
        change(&mut self.items);
        if self.items != before {
            self.past.push(before);
            self.future.clear();
        }
    }

    /// Opens an undo step for a gesture that mutates through [`Doc::items_mut`]
    /// many times, such as dragging an annotation. Close it with
    /// [`Doc::end_gesture`].
    pub fn begin_gesture(&mut self) {
        self.past.push(self.items.clone());
    }

    /// Direct access for use between `begin_gesture` and `end_gesture`.
    pub fn items_mut(&mut self) -> &mut Vec<Annotation> {
        &mut self.items
    }

    /// Closes a gesture, dropping the undo step if nothing actually changed.
    pub fn end_gesture(&mut self) {
        if self.past.last() == Some(&self.items) {
            self.past.pop();
        } else {
            self.future.clear();
        }
    }

    pub fn can_undo(&self) -> bool {
        !self.past.is_empty()
    }

    pub fn can_redo(&self) -> bool {
        !self.future.is_empty()
    }

    pub fn undo(&mut self) -> bool {
        match self.past.pop() {
            Some(prev) => {
                self.future.push(std::mem::replace(&mut self.items, prev));
                true
            }
            None => false,
        }
    }

    pub fn redo(&mut self) -> bool {
        match self.future.pop() {
            Some(next) => {
                self.past.push(std::mem::replace(&mut self.items, next));
                true
            }
            None => false,
        }
    }

    /// Topmost annotation under `p`, if any.
    pub fn hit(&self, p: Point, tol: f32) -> Option<usize> {
        self.items.iter().rposition(|a| a.hit(p, tol))
    }

    /// Number the next step marker should carry.
    pub fn next_step(&self) -> u32 {
        self.items
            .iter()
            .filter_map(|a| match a.shape {
                Shape::Step { n, .. } => Some(n),
                _ => None,
            })
            .max()
            .unwrap_or(0)
            + 1
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(x0: f32, y0: f32, x1: f32, y1: f32) -> Annotation {
        Annotation {
            shape: Shape::Line {
                from: Point::new(x0, y0),
                to: Point::new(x1, y1),
            },
            color: 0,
            stroke: 2.0,
        }
    }

    #[test]
    fn edit_undo_redo_round_trip() {
        let mut doc = Doc::new();
        doc.edit(|v| v.push(line(0.0, 0.0, 10.0, 0.0)));
        doc.edit(|v| v.push(line(0.0, 5.0, 10.0, 5.0)));
        assert_eq!(doc.items().len(), 2);
        assert!(doc.undo());
        assert_eq!(doc.items().len(), 1);
        assert!(doc.redo());
        assert_eq!(doc.items().len(), 2);
    }

    #[test]
    fn undo_and_redo_on_empty_history_are_noops() {
        let mut doc = Doc::new();
        assert!(!doc.undo());
        assert!(!doc.redo());
        assert!(doc.items().is_empty());
    }

    #[test]
    fn edit_after_undo_clears_redo() {
        let mut doc = Doc::new();
        doc.edit(|v| v.push(line(0.0, 0.0, 10.0, 0.0)));
        doc.undo();
        doc.edit(|v| v.push(line(0.0, 9.0, 10.0, 9.0)));
        assert!(!doc.redo());
        assert_eq!(doc.items().len(), 1);
    }

    #[test]
    fn unchanged_edit_records_nothing() {
        let mut doc = Doc::new();
        doc.edit(|_| {});
        assert!(!doc.undo());
    }

    #[test]
    fn gesture_is_one_undo_step_and_vanishes_when_unchanged() {
        let mut doc = Doc::new();
        doc.edit(|v| v.push(line(0.0, 0.0, 10.0, 0.0)));
        doc.begin_gesture();
        doc.end_gesture();
        doc.begin_gesture();
        doc.items_mut()[0].translate(1.0, 0.0);
        doc.items_mut()[0].translate(1.0, 0.0);
        doc.end_gesture();
        assert!(doc.undo());
        assert_eq!(doc.items()[0], line(0.0, 0.0, 10.0, 0.0));
        assert!(doc.undo());
        assert!(doc.items().is_empty());
        assert!(!doc.undo());
    }

    #[test]
    fn line_hit_within_tolerance_only() {
        let a = line(0.0, 0.0, 100.0, 0.0);
        assert!(a.hit(Point::new(50.0, 4.0), 4.0));
        assert!(!a.hit(Point::new(50.0, 9.0), 4.0));
    }

    #[test]
    fn rect_hits_on_border_not_in_middle() {
        let a = Annotation {
            shape: Shape::Rect {
                rect: Rect::new(0.0, 0.0, 100.0, 100.0),
            },
            color: 0,
            stroke: 2.0,
        };
        assert!(a.hit(Point::new(1.0, 50.0), 3.0));
        assert!(!a.hit(Point::new(50.0, 50.0), 3.0));
    }

    #[test]
    fn hit_returns_topmost() {
        let mut doc = Doc::new();
        doc.edit(|v| v.push(line(0.0, 0.0, 10.0, 0.0)));
        doc.edit(|v| v.push(line(0.0, 0.0, 10.0, 0.0)));
        assert_eq!(doc.hit(Point::new(5.0, 0.0), 2.0), Some(1));
        assert_eq!(doc.hit(Point::new(5.0, 50.0), 2.0), None);
    }

    #[test]
    fn steps_count_up_from_highest() {
        let mut doc = Doc::new();
        assert_eq!(doc.next_step(), 1);
        doc.edit(|v| {
            v.push(Annotation {
                shape: Shape::Step {
                    at: Point::new(0.0, 0.0),
                    n: 1,
                },
                color: 0,
                stroke: 3.0,
            })
        });
        assert_eq!(doc.next_step(), 2);
    }

    #[test]
    fn every_kind_of_drawing_survives_being_written_and_read() {
        let p = Point::new;
        let r = Rect::new(1.5, 2.0, 30.0, 40.25);
        let shapes = vec![
            Shape::Arrow {
                from: p(1.0, 2.0),
                to: p(3.5, -4.0),
            },
            Shape::Line {
                from: p(0.0, 0.0),
                to: p(9.0, 9.0),
            },
            Shape::Measure {
                from: p(5.0, 5.0),
                to: p(105.0, 5.0),
            },
            Shape::Rect { rect: r },
            Shape::Ellipse { rect: r },
            Shape::Blur { rect: r },
            Shape::Focus { rect: r },
            Shape::Pen {
                points: vec![p(1.0, 1.0), p(2.0, 3.0), p(4.0, 8.0)],
            },
            Shape::Marker {
                points: vec![p(7.0, 7.0), p(70.0, 7.5)],
            },
            Shape::Text {
                at: p(10.0, 20.0),
                text: "two | lines\nback\\slash".into(),
                size: 18.0,
                extent: (120.0, 46.0),
            },
            Shape::Step {
                at: p(50.0, 60.0),
                n: 7,
            },
        ];
        let items: Vec<Annotation> = shapes
            .into_iter()
            .enumerate()
            .map(|(i, shape)| Annotation {
                shape,
                color: (i % 6) as u8,
                stroke: 1.5 + i as f32,
            })
            .collect();
        assert_eq!(parse(&serialize(&items)), items);
    }

    #[test]
    fn unreadable_lines_are_skipped() {
        let text = "line 0 2 0 0 10 10\nhologram 1 2 3\nrect 0 2 1 2\n\nstep x 3 1 1 1\n";
        assert_eq!(parse(text).len(), 1);
        assert!(parse("").is_empty());
    }

    #[test]
    fn translate_moves_every_point() {
        let mut a = Annotation {
            shape: Shape::Pen {
                points: vec![Point::new(0.0, 0.0), Point::new(5.0, 5.0)],
            },
            color: 0,
            stroke: 1.0,
        };
        a.translate(10.0, -2.0);
        assert_eq!(
            a.shape,
            Shape::Pen {
                points: vec![Point::new(10.0, -2.0), Point::new(15.0, 3.0)]
            }
        );
    }
}
