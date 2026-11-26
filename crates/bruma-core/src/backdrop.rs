//! The screen behind a full-screen panel: as it is, in mist, and as frosted
//! glass for whatever sits on top.

use tiny_skia::Pixmap;

use crate::image::Image;
use crate::{blur, render, theme};

pub struct Backdrop {
    width: u32,
    height: u32,
    sharp: Vec<u8>,
    mist: Vec<u8>,
    glass: Pixmap,
}

impl Backdrop {
    /// Prepares the mist and the glass of `screen`. `None` for an empty
    /// picture.
    pub fn new(screen: &Image, scale: f32) -> Option<Self> {
        let (w, h) = (screen.width as usize, screen.height as usize);
        if w == 0 || h == 0 {
            return None;
        }
        let quarter = blur::downscale(&screen.rgba, w, h, 4);
        let glass = render::build_glass(&quarter, scale)?;
        let mut small = quarter;
        blur::blur(&mut small, (2.0 * scale).round().max(1.0) as usize, 3);
        let mist = blur::upscale(&small, w, h, theme::SOFT_KEEP);
        Some(Self {
            width: screen.width,
            height: screen.height,
            sharp: screen.rgba.clone(),
            mist,
            glass,
        })
    }

    pub fn width(&self) -> u32 {
        self.width
    }

    pub fn height(&self) -> u32 {
        self.height
    }

    /// Fills `pixmap`, which must be the size of the screen, with the screen
    /// gone `amount` of the way into mist: 0 is the screen as it is, 1 is all
    /// mist.
    pub fn paint(&self, pixmap: &mut Pixmap, amount: f32) {
        assert_eq!((pixmap.width(), pixmap.height()), (self.width, self.height));
        let a = (amount.clamp(0.0, 1.0) * 256.0).round() as u32;
        let out = pixmap.data_mut();
        if a == 0 {
            out.copy_from_slice(&self.sharp);
            return;
        }
        if a >= 256 {
            out.copy_from_slice(&self.mist);
            return;
        }
        let row = (self.width as usize * 4).max(4);
        crate::par::bands(out, row, |first, band| {
            let at = first * row..first * row + band.len();
            let pairs = self.sharp[at.clone()].iter().zip(&self.mist[at]);
            for (o, (s, m)) in band.iter_mut().zip(pairs) {
                *o = ((*s as u32 * (256 - a) + *m as u32 * a) >> 8) as u8;
            }
        });
    }

    /// The blurred screen at a quarter of its size, for frosted panels.
    pub(crate) fn glass(&self) -> &Pixmap {
        &self.glass
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geom::Rect;

    fn screen() -> Image {
        let mut img = Image::filled(64, 48, [200, 200, 200, 255]);
        img.fill_rect(Rect::new(10.0, 10.0, 20.0, 20.0), [20, 40, 60, 255]);
        img
    }

    #[test]
    fn nothing_is_the_screen_and_everything_is_the_mist() {
        let img = screen();
        let backdrop = Backdrop::new(&img, 1.0).unwrap();
        let mut pixmap = Pixmap::new(64, 48).unwrap();
        backdrop.paint(&mut pixmap, 0.0);
        assert_eq!(pixmap.data(), &img.rgba[..]);
        backdrop.paint(&mut pixmap, 1.0);
        assert_eq!(pixmap.data(), &backdrop.mist[..]);
        // The mist is darker than the screen.
        let sum = |d: &[u8]| d.iter().map(|&v| v as u64).sum::<u64>();
        assert!(sum(&backdrop.mist) < sum(&img.rgba));
    }

    #[test]
    fn halfway_lies_between_and_stays_opaque() {
        let img = screen();
        let backdrop = Backdrop::new(&img, 1.0).unwrap();
        let mut pixmap = Pixmap::new(64, 48).unwrap();
        backdrop.paint(&mut pixmap, 0.5);
        for ((o, s), m) in pixmap.data().iter().zip(&img.rgba).zip(&backdrop.mist) {
            let (lo, hi) = ((*s).min(*m), (*s).max(*m));
            assert!((lo..=hi).contains(o));
        }
        assert!(pixmap.data().chunks_exact(4).all(|p| p[3] == 255));
    }

    #[test]
    fn an_empty_screen_has_no_backdrop() {
        assert!(Backdrop::new(&Image::filled(0, 0, [0, 0, 0, 255]), 1.0).is_none());
    }
}
