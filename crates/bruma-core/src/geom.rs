//! Points and rectangles in physical pixels.

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Point {
    pub x: f32,
    pub y: f32,
}

impl Point {
    pub const fn new(x: f32, y: f32) -> Self {
        Self { x, y }
    }

    pub fn distance(self, other: Point) -> f32 {
        (self.x - other.x).hypot(self.y - other.y)
    }

    pub fn offset(self, dx: f32, dy: f32) -> Point {
        Point::new(self.x + dx, self.y + dy)
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl Rect {
    pub const fn new(x: f32, y: f32, w: f32, h: f32) -> Self {
        Self { x, y, w, h }
    }

    /// Rectangle spanned by two opposite corners, in any order.
    pub fn from_points(a: Point, b: Point) -> Self {
        let (x0, x1) = if a.x <= b.x { (a.x, b.x) } else { (b.x, a.x) };
        let (y0, y1) = if a.y <= b.y { (a.y, b.y) } else { (b.y, a.y) };
        Self::new(x0, y0, x1 - x0, y1 - y0)
    }

    pub fn right(&self) -> f32 {
        self.x + self.w
    }

    pub fn bottom(&self) -> f32 {
        self.y + self.h
    }

    pub fn center(&self) -> Point {
        Point::new(self.x + self.w / 2.0, self.y + self.h / 2.0)
    }

    pub fn is_empty(&self) -> bool {
        self.w <= 0.0 || self.h <= 0.0
    }

    pub fn contains(&self, p: Point) -> bool {
        p.x >= self.x && p.x < self.right() && p.y >= self.y && p.y < self.bottom()
    }

    pub fn translate(&self, dx: f32, dy: f32) -> Rect {
        Rect::new(self.x + dx, self.y + dy, self.w, self.h)
    }

    /// Grows (or, with a negative amount, shrinks) on every side.
    pub fn inflate(&self, d: f32) -> Rect {
        Rect::new(
            self.x - d,
            self.y - d,
            (self.w + 2.0 * d).max(0.0),
            (self.h + 2.0 * d).max(0.0),
        )
    }

    /// Overlap of two rectangles; empty (zero-sized) when they do not touch.
    pub fn intersect(&self, other: &Rect) -> Rect {
        let x0 = self.x.max(other.x);
        let y0 = self.y.max(other.y);
        let x1 = self.right().min(other.right());
        let y1 = self.bottom().min(other.bottom());
        Rect::new(x0, y0, (x1 - x0).max(0.0), (y1 - y0).max(0.0))
    }

    pub fn union(&self, other: &Rect) -> Rect {
        let x0 = self.x.min(other.x);
        let y0 = self.y.min(other.y);
        let x1 = self.right().max(other.right());
        let y1 = self.bottom().max(other.bottom());
        Rect::new(x0, y0, x1 - x0, y1 - y0)
    }

    /// Moves the rectangle so it lies inside `bounds`, keeping its size.
    /// A rectangle larger than `bounds` is shrunk to fit.
    pub fn clamp_inside(&self, bounds: &Rect) -> Rect {
        let w = self.w.min(bounds.w);
        let h = self.h.min(bounds.h);
        let x = self.x.clamp(bounds.x, bounds.right() - w);
        let y = self.y.clamp(bounds.y, bounds.bottom() - h);
        Rect::new(x, y, w, h)
    }

    /// Snaps every edge to the nearest whole pixel.
    pub fn round(&self) -> Rect {
        let x0 = self.x.round();
        let y0 = self.y.round();
        let x1 = self.right().round();
        let y1 = self.bottom().round();
        Rect::new(x0, y0, x1 - x0, y1 - y0)
    }
}

/// Snaps `to` so the segment `from → to` lies on a multiple of 45°.
pub fn constrain_45(from: Point, to: Point) -> Point {
    let dx = to.x - from.x;
    let dy = to.y - from.y;
    let len = dx.hypot(dy);
    if len == 0.0 {
        return to;
    }
    let step = std::f32::consts::FRAC_PI_4;
    let angle = (dy.atan2(dx) / step).round() * step;
    Point::new(from.x + len * angle.cos(), from.y + len * angle.sin())
}

/// Snaps `to` so the box `from → to` is square, keeping the drag direction.
pub fn constrain_square(from: Point, to: Point) -> Point {
    let dx = to.x - from.x;
    let dy = to.y - from.y;
    let side = dx.abs().max(dy.abs());
    let sx = if dx < 0.0 { -1.0 } else { 1.0 };
    let sy = if dy < 0.0 { -1.0 } else { 1.0 };
    Point::new(from.x + side * sx, from.y + side * sy)
}

/// Distance from `p` to the segment `a → b`.
pub fn distance_to_segment(p: Point, a: Point, b: Point) -> f32 {
    let abx = b.x - a.x;
    let aby = b.y - a.y;
    let len2 = abx * abx + aby * aby;
    if len2 == 0.0 {
        return p.distance(a);
    }
    let t = (((p.x - a.x) * abx + (p.y - a.y) * aby) / len2).clamp(0.0, 1.0);
    p.distance(Point::new(a.x + t * abx, a.y + t * aby))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_points_normalises_reversed_corners() {
        let r = Rect::from_points(Point::new(30.0, 40.0), Point::new(10.0, 5.0));
        assert_eq!(r, Rect::new(10.0, 5.0, 20.0, 35.0));
    }

    #[test]
    fn clamp_inside_keeps_size_and_moves_inside() {
        let bounds = Rect::new(0.0, 0.0, 100.0, 100.0);
        let r = Rect::new(90.0, -20.0, 30.0, 30.0).clamp_inside(&bounds);
        assert_eq!(r, Rect::new(70.0, 0.0, 30.0, 30.0));
    }

    #[test]
    fn clamp_inside_shrinks_oversized() {
        let bounds = Rect::new(0.0, 0.0, 100.0, 100.0);
        let r = Rect::new(-10.0, 10.0, 300.0, 50.0).clamp_inside(&bounds);
        assert_eq!(r, Rect::new(0.0, 10.0, 100.0, 50.0));
    }

    #[test]
    fn intersect_of_disjoint_is_empty() {
        let a = Rect::new(0.0, 0.0, 10.0, 10.0);
        let b = Rect::new(20.0, 20.0, 5.0, 5.0);
        assert!(a.intersect(&b).is_empty());
    }

    #[test]
    fn constrain_45_snaps_to_axis_and_diagonal() {
        let o = Point::new(0.0, 0.0);
        let h = constrain_45(o, Point::new(10.0, 1.0));
        assert!((h.y).abs() < 1e-3 && h.x > 9.9);
        let d = constrain_45(o, Point::new(7.0, 9.0));
        assert!((d.x - d.y).abs() < 1e-3);
    }

    #[test]
    fn constrain_square_keeps_direction() {
        let p = constrain_square(Point::new(10.0, 10.0), Point::new(4.0, 30.0));
        assert_eq!(p, Point::new(-10.0, 30.0));
    }

    #[test]
    fn segment_distance() {
        let a = Point::new(0.0, 0.0);
        let b = Point::new(10.0, 0.0);
        assert_eq!(distance_to_segment(Point::new(5.0, 3.0), a, b), 3.0);
        assert_eq!(distance_to_segment(Point::new(13.0, 4.0), a, b), 5.0);
    }
}
