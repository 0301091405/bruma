//! A plain RGBA8 bitmap. Screens are always opaque.

use crate::geom::Rect;

#[derive(Clone, Debug, PartialEq)]
pub struct Image {
    pub width: u32,
    pub height: u32,
    /// Row-major RGBA, 4 bytes per pixel, `width * height * 4` long.
    pub rgba: Vec<u8>,
}

/// One screen pixel (blue, green, red, unused) as an RGBA pixel: red and blue
/// change places and the unused byte becomes full alpha.
#[inline(always)]
fn screen_to_rgba(v: u32) -> u32 {
    (v & 0x0000_FF00) | ((v & 0xFF) << 16) | ((v >> 16) & 0xFF) | 0xFF00_0000
}

impl Image {
    pub fn new(width: u32, height: u32, rgba: Vec<u8>) -> Self {
        assert_eq!(rgba.len(), width as usize * height as usize * 4);
        Self {
            width,
            height,
            rgba,
        }
    }

    pub fn filled(width: u32, height: u32, color: [u8; 4]) -> Self {
        let mut rgba = Vec::with_capacity(width as usize * height as usize * 4);
        for _ in 0..width as usize * height as usize {
            rgba.extend_from_slice(&color);
        }
        Self::new(width, height, rgba)
    }

    /// Builds an image from the BGRX bytes a screen capture hands over:
    /// red and blue change places and the unused byte becomes full alpha.
    pub fn from_bgrx(width: u32, height: u32, bgrx: &[u8]) -> Self {
        let mut rgba = vec![0u8; bgrx.len()];
        let row = (width as usize * 4).max(4);
        crate::par::bands(&mut rgba, row, |first, band| {
            let source = &bgrx[first * row..first * row + band.len()];
            // One word at a time, so the compiler can do sixteen bytes at once.
            for (out, px) in band.chunks_exact_mut(4).zip(source.chunks_exact(4)) {
                let v = u32::from_le_bytes([px[0], px[1], px[2], px[3]]);
                out.copy_from_slice(&screen_to_rgba(v).to_le_bytes());
            }
        });
        Self::new(width, height, rgba)
    }

    /// Whether this image shows exactly what the screen bytes `bgrx` show.
    /// Nothing is converted or allocated, so asking "has the screen changed?"
    /// costs one pass over memory, and less when the answer is yes.
    pub fn matches_bgrx(&self, width: u32, height: u32, bgrx: &[u8]) -> bool {
        if self.width != width || self.height != height || self.rgba.len() != bgrx.len() {
            return false;
        }
        // Block by block: a difference ends the comparison early, and inside
        // a block there is no branch, so sixteen bytes are compared at once.
        const BLOCK: usize = 1 << 14;
        self.rgba
            .chunks(BLOCK)
            .zip(bgrx.chunks(BLOCK))
            .all(|(ours, theirs)| {
                let mut differs = 0u32;
                for (a, b) in ours.chunks_exact(4).zip(theirs.chunks_exact(4)) {
                    let a = u32::from_le_bytes([a[0], a[1], a[2], a[3]]);
                    let b = u32::from_le_bytes([b[0], b[1], b[2], b[3]]);
                    differs |= a ^ screen_to_rgba(b);
                }
                differs == 0
            })
    }

    pub fn bounds(&self) -> Rect {
        Rect::new(0.0, 0.0, self.width as f32, self.height as f32)
    }

    /// Pixel at `(x, y)`, clamped to the image edge.
    pub fn pixel(&self, x: i32, y: i32) -> [u8; 4] {
        if self.width == 0 || self.height == 0 {
            return [0, 0, 0, 255];
        }
        let x = x.clamp(0, self.width as i32 - 1) as usize;
        let y = y.clamp(0, self.height as i32 - 1) as usize;
        let i = (y * self.width as usize + x) * 4;
        [
            self.rgba[i],
            self.rgba[i + 1],
            self.rgba[i + 2],
            self.rgba[i + 3],
        ]
    }

    pub fn set_pixel(&mut self, x: u32, y: u32, color: [u8; 4]) {
        if x < self.width && y < self.height {
            let i = (y as usize * self.width as usize + x as usize) * 4;
            self.rgba[i..i + 4].copy_from_slice(&color);
        }
    }

    pub fn fill_rect(&mut self, rect: Rect, color: [u8; 4]) {
        let (x0, y0, x1, y1) = self.pixel_span(rect);
        for y in y0..y1 {
            for x in x0..x1 {
                self.set_pixel(x, y, color);
            }
        }
    }

    /// Whole-pixel span of `rect` clipped to the image: `(x0, y0, x1, y1)`, end exclusive.
    pub fn pixel_span(&self, rect: Rect) -> (u32, u32, u32, u32) {
        let r = rect.round().intersect(&self.bounds());
        (r.x as u32, r.y as u32, r.right() as u32, r.bottom() as u32)
    }

    /// Copy of the part of the image under `rect` (rounded, clipped to the image).
    pub fn crop(&self, rect: Rect) -> Image {
        let (x0, y0, x1, y1) = self.pixel_span(rect);
        let w = x1.saturating_sub(x0);
        let h = y1.saturating_sub(y0);
        let mut rgba = Vec::with_capacity(w as usize * h as usize * 4);
        for y in y0..y0 + h {
            let start = (y as usize * self.width as usize + x0 as usize) * 4;
            rgba.extend_from_slice(&self.rgba[start..start + w as usize * 4]);
        }
        Image::new(w, h, rgba)
    }

    /// Shrinks `rect` until no edge row or column is a single flat colour.
    ///
    /// The colour of the top-left pixel of `rect` is taken as the background.
    /// Returns `rect` unchanged when the whole area is uniform.
    pub fn tighten(&self, rect: Rect) -> Rect {
        let (x0, y0, x1, y1) = self.pixel_span(rect);
        if x1 <= x0 || y1 <= y0 {
            return rect;
        }
        let bg = self.pixel(x0 as i32, y0 as i32);
        let near = |c: [u8; 4]| {
            (c[0] as i32 - bg[0] as i32).abs() <= 6
                && (c[1] as i32 - bg[1] as i32).abs() <= 6
                && (c[2] as i32 - bg[2] as i32).abs() <= 6
        };
        let row_flat =
            |y: u32, xa: u32, xb: u32| (xa..xb).all(|x| near(self.pixel(x as i32, y as i32)));
        let col_flat =
            |x: u32, ya: u32, yb: u32| (ya..yb).all(|y| near(self.pixel(x as i32, y as i32)));

        let (mut l, mut t, mut r, mut b) = (x0, y0, x1, y1);
        while t < b && row_flat(t, l, r) {
            t += 1;
        }
        if t == b {
            return rect;
        }
        while b > t && row_flat(b - 1, l, r) {
            b -= 1;
        }
        while l < r && col_flat(l, t, b) {
            l += 1;
        }
        while r > l && col_flat(r - 1, t, b) {
            r -= 1;
        }
        Rect::new(l as f32, t as f32, (r - l) as f32, (b - t) as f32)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn screen_bytes_become_rgba() {
        // Two pixels as GDI gives them: blue, green, red, unused.
        let img = Image::from_bgrx(2, 1, &[10, 20, 30, 0, 200, 150, 100, 77]);
        assert_eq!(img.rgba, [30, 20, 10, 255, 100, 150, 200, 255]);
        // A big one goes through the multi-core path and must agree with the simple rule.
        let (w, h) = (700u32, 400u32);
        let bytes: Vec<u8> = (0..w * h * 4).map(|i| (i * 7 % 251) as u8).collect();
        let big = Image::from_bgrx(w, h, &bytes);
        for (out, px) in big.rgba.chunks_exact(4).zip(bytes.chunks_exact(4)) {
            assert_eq!(out, [px[2], px[1], px[0], 255]);
        }
    }

    #[test]
    fn screen_bytes_are_compared_without_converting() {
        let (w, h) = (300u32, 200u32);
        let mut bytes: Vec<u8> = (0..w * h * 4).map(|i| (i * 13 % 241) as u8).collect();
        let img = Image::from_bgrx(w, h, &bytes);
        assert!(img.matches_bgrx(w, h, &bytes));
        // The unused byte does not count.
        bytes[3] ^= 0xFF;
        assert!(img.matches_bgrx(w, h, &bytes));
        // One channel of one pixel, first, in the middle and last.
        for at in [0, (w * h * 2) as usize + 1, bytes.len() - 2] {
            bytes[at] ^= 1;
            assert!(!img.matches_bgrx(w, h, &bytes), "byte {at}");
            bytes[at] ^= 1;
        }
        // Another shape with as many pixels is another picture.
        assert!(!img.matches_bgrx(h, w, &bytes));
        assert!(!img.matches_bgrx(w, h, &bytes[4..]));
    }

    #[test]
    fn crop_clips_to_image() {
        let mut img = Image::filled(10, 10, [0, 0, 0, 255]);
        img.set_pixel(9, 9, [255, 0, 0, 255]);
        let c = img.crop(Rect::new(8.0, 8.0, 10.0, 10.0));
        assert_eq!((c.width, c.height), (2, 2));
        assert_eq!(c.pixel(1, 1), [255, 0, 0, 255]);
    }

    #[test]
    fn crop_outside_is_empty() {
        let img = Image::filled(10, 10, [0, 0, 0, 255]);
        let c = img.crop(Rect::new(50.0, 50.0, 5.0, 5.0));
        assert_eq!((c.width, c.height), (0, 0));
    }

    #[test]
    fn tighten_finds_content_block() {
        let mut img = Image::filled(100, 100, [255, 255, 255, 255]);
        img.fill_rect(Rect::new(30.0, 40.0, 10.0, 10.0), [0, 0, 0, 255]);
        let t = img.tighten(Rect::new(10.0, 10.0, 50.0, 50.0));
        assert_eq!(t, Rect::new(30.0, 40.0, 10.0, 10.0));
    }

    #[test]
    fn tighten_uniform_returns_input() {
        let img = Image::filled(100, 100, [255, 255, 255, 255]);
        let r = Rect::new(10.0, 10.0, 50.0, 50.0);
        assert_eq!(img.tighten(r), r);
    }
}
