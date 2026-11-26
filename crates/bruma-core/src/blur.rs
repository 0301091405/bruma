//! The mist. Fast blurs for the out-of-focus screen around a selection and
//! for the frosted glass behind every panel.
//!
//! Everything works on a reduced copy of the screen: blur hides the loss of
//! resolution, and a quarter of the pixels is a quarter of the work.

/// A small opaque RGBA buffer.
#[derive(Clone, Debug, PartialEq)]
pub struct Buf {
    pub w: usize,
    pub h: usize,
    pub px: Vec<u8>,
}

/// Shrinks an RGBA image by averaging `factor × factor` blocks.
pub fn downscale(src: &[u8], w: usize, h: usize, factor: usize) -> Buf {
    let factor = factor.max(1);
    let (ow, oh) = (w.div_ceil(factor).max(1), h.div_ceil(factor).max(1));
    let mut px = vec![255u8; ow * oh * 4];
    if w == 0 || h == 0 {
        return Buf { w: ow, h: oh, px };
    }
    crate::par::bands(&mut px, ow * 4, |first, band| {
        // Sums for one output row; source rows are added to it in order, so
        // memory is read straight through.
        let mut sums = vec![0u32; ow * 3];
        for (i, out) in band.chunks_exact_mut(ow * 4).enumerate() {
            let y0 = (first + i) * factor;
            let y1 = (y0 + factor).min(h);
            sums.fill(0);
            for y in y0..y1 {
                let row = &src[y * w * 4..(y + 1) * w * 4];
                for (block, pixels) in row.chunks(factor * 4).enumerate() {
                    let sum = &mut sums[block * 3..block * 3 + 3];
                    for p in pixels.chunks_exact(4) {
                        sum[0] += p[0] as u32;
                        sum[1] += p[1] as u32;
                        sum[2] += p[2] as u32;
                    }
                }
            }
            for (ox, o) in out.chunks_exact_mut(4).enumerate() {
                let wide = (w - ox * factor).min(factor);
                let n = (wide * (y1 - y0)) as u32;
                o[0] = (sums[ox * 3] / n) as u8;
                o[1] = (sums[ox * 3 + 1] / n) as u8;
                o[2] = (sums[ox * 3 + 2] / n) as u8;
            }
        }
    });
    Buf { w: ow, h: oh, px }
}

/// The widest blur: 31 pixels to each side, on the reduced copy.
const MAX_RADIUS: usize = 31;

/// Dividing every sum by the window size is the costliest step of a box
/// blur, so it is done as a multiplication and a shift: `(sum * r) >> 20`.
/// For windows of up to 63 pixels the result is the division's, exactly.
fn reciprocal(window: u32) -> u32 {
    (1u32 << 20).div_ceil(window)
}

/// One box-blur pass along rows, edges clamped.
fn pass_rows(src: &[u8], dst: &mut [u8], w: usize, r: usize) {
    let scale = reciprocal((2 * r + 1) as u32);
    // Pixels `near..far` have their whole window inside the row; only the
    // few outside that range need their neighbours clamped to the edge.
    let near = r.min(w);
    let far = w.saturating_sub(r + 1).max(near);
    for (from, to) in src.chunks_exact(w * 4).zip(dst.chunks_exact_mut(w * 4)) {
        // The three channels of pixel `i`, with the edges repeated.
        let at = |i: usize| -> [u32; 3] {
            let p = &from[i.min(w - 1) * 4..];
            [p[0] as u32, p[1] as u32, p[2] as u32]
        };
        // Writes the average and slides the window one pixel on: `enter`
        // comes in on the right, `leave` drops out on the left.
        let step = |sum: &mut [u32; 3], out: &mut [u8], enter: [u32; 3], leave: [u32; 3]| {
            for c in 0..3 {
                out[c] = ((sum[c] * scale) >> 20) as u8;
                sum[c] = sum[c] + enter[c] - leave[c];
            }
        };
        // Window centred on pixel 0, with the left edge repeated.
        let mut sum = at(0).map(|v| v * (r as u32 + 1));
        for i in 1..=r {
            let p = at(i);
            sum = [sum[0] + p[0], sum[1] + p[1], sum[2] + p[2]];
        }
        for i in 0..near {
            step(&mut sum, &mut to[i * 4..], at(i + r + 1), at(0));
        }
        if far > near {
            let entering = from[(near + r + 1) * 4..].chunks_exact(4);
            let leaving = from.chunks_exact(4);
            let middle = to[near * 4..far * 4].chunks_exact_mut(4);
            for ((out, a), b) in middle.zip(entering).zip(leaving) {
                let enter = [a[0] as u32, a[1] as u32, a[2] as u32];
                let leave = [b[0] as u32, b[1] as u32, b[2] as u32];
                step(&mut sum, out, enter, leave);
            }
        }
        for i in far..w {
            step(
                &mut sum,
                &mut to[i * 4..],
                at(w - 1),
                at(i.saturating_sub(r)),
            );
        }
    }
}

/// One box-blur pass along columns. It keeps a running sum per column and
/// walks down row by row, so memory is read in order.
fn pass_columns(src: &[u8], dst: &mut [u8], w: usize, h: usize, r: usize) {
    let scale = reciprocal((2 * r + 1) as u32);
    let row = |y: usize| &src[y.min(h - 1) * w * 4..(y.min(h - 1) + 1) * w * 4];
    let mut sums = vec![0u32; w * 4];
    for (sum, v) in sums.iter_mut().zip(row(0)) {
        *sum = *v as u32 * (r as u32 + 1);
    }
    for y in 1..=r {
        for (sum, v) in sums.iter_mut().zip(row(y)) {
            *sum += *v as u32;
        }
    }
    for y in 0..h {
        let out = &mut dst[y * w * 4..(y + 1) * w * 4];
        for (o, sum) in out.iter_mut().zip(&sums) {
            *o = ((sum * scale) >> 20) as u8;
        }
        let (entering, leaving) = (row(y + r + 1), row(y.saturating_sub(r)));
        for ((sum, a), b) in sums.iter_mut().zip(entering).zip(leaving) {
            *sum = *sum + *a as u32 - *b as u32;
        }
    }
}

/// Blurs in place. Three box passes are a close stand-in for a Gaussian.
pub fn blur(buf: &mut Buf, radius: usize, passes: usize) {
    if radius == 0 || buf.w == 0 || buf.h == 0 {
        return;
    }
    let radius = radius.min(MAX_RADIUS);
    let mut tmp = buf.px.clone();
    for _ in 0..passes {
        pass_rows(&buf.px, &mut tmp, buf.w, radius);
        pass_columns(&tmp, &mut buf.px, buf.w, buf.h, radius);
    }
}

/// Stretches `buf` to `out_w × out_h` with bilinear filtering, multiplying
/// brightness by `keep` on the way.
pub fn upscale(buf: &Buf, out_w: usize, out_h: usize, keep: f32) -> Vec<u8> {
    // Zeroed memory comes from the system untouched; the bands below are the
    // first to write to it, each on its own core.
    let mut out = vec![0u8; out_w * out_h * 4];
    if buf.w == 0 || buf.h == 0 {
        out.fill(255);
        return out;
    }
    let keep = (keep.clamp(0.0, 1.0) * 256.0) as u32;
    // For each output coordinate: the two source indices and the blend weight.
    let taps = |out_n: usize, src_n: usize| -> Vec<(usize, usize, u32)> {
        (0..out_n)
            .map(|i| {
                let f = ((i as f32 + 0.5) * src_n as f32 / out_n as f32 - 0.5)
                    .clamp(0.0, (src_n - 1) as f32);
                let i0 = f.floor() as usize;
                (
                    i0,
                    (i0 + 1).min(src_n - 1),
                    ((f - i0 as f32) * 256.0) as u32,
                )
            })
            .collect()
    };
    let xs: Vec<(usize, usize, u32)> = taps(out_w, buf.w)
        .into_iter()
        .map(|(x0, x1, wx)| (x0 * 3, x1 * 3, wx))
        .collect();
    let ys = taps(out_h, buf.h);
    crate::par::bands(&mut out, out_w * 4, |first, band| {
        // The source row this output row lies on: the two rows around it,
        // blended and dimmed once, with eight more bits of precision. Every
        // pixel of the output row is then one blend along it.
        let mut source = vec![0u32; buf.w * 3];
        for (i, row) in band.chunks_exact_mut(out_w * 4).enumerate() {
            let (y0, y1, wy) = ys[first + i];
            let top = &buf.px[y0 * buf.w * 4..(y0 + 1) * buf.w * 4];
            let bottom = &buf.px[y1 * buf.w * 4..(y1 + 1) * buf.w * 4];
            for ((s, t), b) in source
                .chunks_exact_mut(3)
                .zip(top.chunks_exact(4))
                .zip(bottom.chunks_exact(4))
            {
                for c in 0..3 {
                    s[c] = ((t[c] as u32 * (256 - wy) + b[c] as u32 * wy) * keep) >> 8;
                }
            }
            for (o, &(x0, x1, wx)) in row.chunks_exact_mut(4).zip(&xs) {
                let (a, b) = (&source[x0..x0 + 3], &source[x1..x1 + 3]);
                o[0] = ((a[0] * (256 - wx) + b[0] * wx) >> 16) as u8;
                o[1] = ((a[1] * (256 - wx) + b[1] * wx) >> 16) as u8;
                o[2] = ((a[2] * (256 - wx) + b[2] * wx) >> 16) as u8;
                o[3] = 255;
            }
        }
    });
    out
}

/// The mist version of an RGBA image: blurred, dimmed to `keep`, same size.
pub fn mist(rgba: &[u8], w: usize, h: usize, scale: f32, keep: f32) -> Vec<u8> {
    let mut small = downscale(rgba, w, h, 4);
    blur(&mut small, (2.0 * scale).round().max(1.0) as usize, 3);
    upscale(&small, w, h, keep)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn flat(w: usize, h: usize, v: u8) -> Vec<u8> {
        [v, v, v, 255].repeat(w * h)
    }

    #[test]
    fn downscale_averages_blocks_and_rounds_size_up() {
        let mut src = flat(5, 2, 0);
        for p in src.chunks_exact_mut(4).take(2) {
            p[0] = 200;
        }
        let small = downscale(&src, 5, 2, 2);
        assert_eq!((small.w, small.h), (3, 1));
        assert_eq!(
            small.px[0], 100,
            "two of the four pixels in the block are 200"
        );
        assert_eq!(small.px[4], 0);
    }

    #[test]
    fn big_images_downscale_the_same_on_many_cores_as_on_one() {
        // Large enough to be cut into bands; compare with the plain definition.
        let (w, h) = (613usize, 407usize);
        let src: Vec<u8> = (0..w * h * 4).map(|i| (i * 31 % 255) as u8).collect();
        let small = downscale(&src, w, h, 4);
        assert_eq!((small.w, small.h), (154, 102));
        for (oy, ox) in [(0usize, 0usize), (50, 77), (101, 153), (101, 0), (0, 153)] {
            let mut sum = [0u32; 3];
            let mut n = 0;
            for y in oy * 4..(oy * 4 + 4).min(h) {
                for x in ox * 4..(ox * 4 + 4).min(w) {
                    for c in 0..3 {
                        sum[c] += src[(y * w + x) * 4 + c] as u32;
                    }
                    n += 1;
                }
            }
            let got = &small.px[(oy * small.w + ox) * 4..][..3];
            assert_eq!(
                got,
                [(sum[0] / n) as u8, (sum[1] / n) as u8, (sum[2] / n) as u8],
                "block {ox},{oy}"
            );
        }
    }

    #[test]
    fn blur_is_the_same_along_rows_and_columns() {
        // A picture and its transpose must blur to transposes of each other.
        let n = 23usize;
        let mut a = Buf {
            w: n,
            h: n,
            px: flat(n, n, 0),
        };
        let mut b = a.clone();
        for (x, y, v) in [
            (3usize, 5usize, 255u8),
            (17, 9, 180),
            (11, 20, 90),
            (0, 0, 200),
            (22, 22, 140),
        ] {
            a.px[(y * n + x) * 4] = v;
            b.px[(x * n + y) * 4] = v;
        }
        blur(&mut a, 3, 2);
        blur(&mut b, 3, 2);
        for y in 0..n {
            for x in 0..n {
                let (p, q) = (a.px[(y * n + x) * 4] as i32, b.px[(x * n + y) * 4] as i32);
                assert!((p - q).abs() <= 2, "({x},{y}): {p} vs {q}");
            }
        }
    }

    #[test]
    fn the_multiplication_divides_exactly() {
        for radius in 0..=MAX_RADIUS {
            let window = (2 * radius + 1) as u32;
            let scale = reciprocal(window);
            for sum in 0..=255 * window {
                assert_eq!((sum * scale) >> 20, sum / window, "{sum} / {window}");
            }
        }
    }

    #[test]
    fn blurring_a_flat_image_changes_nothing() {
        let mut buf = Buf {
            w: 16,
            h: 9,
            px: flat(16, 9, 77),
        };
        let before = buf.clone();
        blur(&mut buf, 4, 3);
        assert_eq!(buf, before);
    }

    #[test]
    fn blur_spreads_a_bright_pixel_and_keeps_alpha() {
        let mut buf = Buf {
            w: 21,
            h: 21,
            px: flat(21, 21, 0),
        };
        let centre = (10 * 21 + 10) * 4;
        buf.px[centre] = 255;
        blur(&mut buf, 2, 3);
        assert!(buf.px[centre] < 60, "the peak is flattened");
        assert!(
            buf.px[centre - 3 * 4] > 0,
            "its light reaches the neighbours"
        );
        assert!(buf.px.chunks_exact(4).all(|p| p[3] == 255));
    }

    #[test]
    fn blur_handles_a_radius_larger_than_the_image() {
        let mut buf = Buf {
            w: 3,
            h: 2,
            px: flat(3, 2, 40),
        };
        blur(&mut buf, 50, 2);
        assert!(buf.px.chunks_exact(4).all(|p| p[0] == 40));
    }

    #[test]
    fn upscale_keeps_flat_colour_and_applies_dimming() {
        let buf = Buf {
            w: 4,
            h: 3,
            px: flat(4, 3, 200),
        };
        let out = upscale(&buf, 9, 7, 0.5);
        assert_eq!(out.len(), 9 * 7 * 4);
        assert!(
            out.chunks_exact(4).all(|p| p[0] == 100 && p[3] == 255),
            "{:?}",
            &out[..8]
        );
    }
}
