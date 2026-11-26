//! What changed between two moments of the same screen.
//!
//! This is what lets one key select the tooltip, menu or dialog that just
//! appeared, or the one that just vanished.

use crate::geom::Rect;
use crate::image::Image;

/// Side of the grid cells the comparison works in.
const CELL: usize = 8;
/// A channel must differ by more than this to count (ignores dithering noise).
const THRESHOLD: i32 = 24;

fn differs(a: &[u8], b: &[u8]) -> bool {
    (a[0] as i32 - b[0] as i32).abs() > THRESHOLD
        || (a[1] as i32 - b[1] as i32).abs() > THRESHOLD
        || (a[2] as i32 - b[2] as i32).abs() > THRESHOLD
}

/// Bounding boxes of the separate areas that differ between `a` and `b`,
/// largest first. Areas that almost touch are reported as one; single-cell
/// specks (a blinking caret) are ignored. Images of different sizes have no
/// comparable areas.
pub fn changed_regions(a: &Image, b: &Image) -> Vec<Rect> {
    if a.width != b.width || a.height != b.height || a.width == 0 || a.height == 0 {
        return Vec::new();
    }
    let (w, h) = (a.width as usize, a.height as usize);
    let (cols, rows) = (w.div_ceil(CELL), h.div_ceil(CELL));

    // Mark the cells that contain a changed pixel.
    let mut changed = vec![false; cols * rows];
    for cy in 0..rows {
        for cx in 0..cols {
            let (x0, x1) = (cx * CELL, ((cx + 1) * CELL).min(w));
            'cell: for y in cy * CELL..((cy + 1) * CELL).min(h) {
                let row = (y * w + x0) * 4..(y * w + x1) * 4;
                let (ra, rb) = (&a.rgba[row.clone()], &b.rgba[row]);
                if ra == rb {
                    continue;
                }
                for (pa, pb) in ra.chunks_exact(4).zip(rb.chunks_exact(4)) {
                    if differs(pa, pb) {
                        changed[cy * cols + cx] = true;
                        break 'cell;
                    }
                }
            }
        }
    }

    // Group changed cells that touch, or that sit one empty cell apart.
    let mut group = vec![usize::MAX; cols * rows];
    let mut regions = Vec::new();
    let mut stack = Vec::new();
    for start in 0..cols * rows {
        if !changed[start] || group[start] != usize::MAX {
            continue;
        }
        let id = regions.len();
        let (mut cells, mut c0, mut r0, mut c1, mut r1) = (0usize, cols, rows, 0usize, 0usize);
        group[start] = id;
        stack.push(start);
        while let Some(cell) = stack.pop() {
            let (cx, cy) = (cell % cols, cell / cols);
            cells += 1;
            (c0, r0, c1, r1) = (c0.min(cx), r0.min(cy), c1.max(cx), r1.max(cy));
            for ny in cy.saturating_sub(2)..=(cy + 2).min(rows - 1) {
                for nx in cx.saturating_sub(2)..=(cx + 2).min(cols - 1) {
                    let next = ny * cols + nx;
                    if changed[next] && group[next] == usize::MAX {
                        group[next] = id;
                        stack.push(next);
                    }
                }
            }
        }
        regions.push((cells, c0, r0, c1, r1));
    }

    // Tighten each group's cell box to the exact changed pixels.
    let mut out: Vec<Rect> = regions
        .into_iter()
        .filter(|&(cells, ..)| cells >= 2)
        .filter_map(|(_, c0, r0, c1, r1)| {
            let (bx0, by0) = (c0 * CELL, r0 * CELL);
            let (bx1, by1) = (((c1 + 1) * CELL).min(w), ((r1 + 1) * CELL).min(h));
            let (mut x0, mut y0, mut x1, mut y1) = (bx1, by1, bx0, by0);
            for y in by0..by1 {
                for x in bx0..bx1 {
                    let i = (y * w + x) * 4;
                    if differs(&a.rgba[i..i + 4], &b.rgba[i..i + 4]) {
                        (x0, y0, x1, y1) = (x0.min(x), y0.min(y), x1.max(x + 1), y1.max(y + 1));
                    }
                }
            }
            (x1 > x0 && y1 > y0)
                .then(|| Rect::new(x0 as f32, y0 as f32, (x1 - x0) as f32, (y1 - y0) as f32))
        })
        .collect();
    out.sort_by(|p, q| (q.w * q.h).total_cmp(&(p.w * p.h)));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn white() -> Image {
        Image::filled(400, 300, [255, 255, 255, 255])
    }

    #[test]
    fn identical_images_have_no_changes() {
        assert!(changed_regions(&white(), &white()).is_empty());
    }

    #[test]
    fn a_new_box_is_found_exactly() {
        let before = white();
        let mut after = white();
        after.fill_rect(Rect::new(103.0, 57.0, 90.0, 41.0), [20, 20, 20, 255]);
        assert_eq!(
            changed_regions(&after, &before),
            vec![Rect::new(103.0, 57.0, 90.0, 41.0)]
        );
        assert_eq!(
            changed_regions(&before, &after),
            vec![Rect::new(103.0, 57.0, 90.0, 41.0)]
        );
    }

    #[test]
    fn separate_changes_are_separate_and_sorted_by_size() {
        let before = white();
        let mut after = white();
        after.fill_rect(Rect::new(10.0, 10.0, 30.0, 30.0), [0, 0, 0, 255]);
        after.fill_rect(Rect::new(200.0, 150.0, 120.0, 80.0), [0, 0, 0, 255]);
        let regions = changed_regions(&after, &before);
        assert_eq!(
            regions,
            vec![
                Rect::new(200.0, 150.0, 120.0, 80.0),
                Rect::new(10.0, 10.0, 30.0, 30.0)
            ]
        );
    }

    #[test]
    fn nearby_fragments_merge_into_one_region() {
        let before = white();
        let mut after = white();
        // Lines of "text" 6 px apart belong to one block.
        for i in 0..5 {
            after.fill_rect(
                Rect::new(50.0, 50.0 + i as f32 * 14.0, 120.0, 8.0),
                [0, 0, 0, 255],
            );
        }
        assert_eq!(changed_regions(&after, &before).len(), 1);
    }

    #[test]
    fn specks_and_faint_noise_are_ignored() {
        let before = white();
        let mut after = white();
        after.fill_rect(Rect::new(97.0, 97.0, 2.0, 5.0), [0, 0, 0, 255]);
        after.fill_rect(Rect::new(200.0, 200.0, 60.0, 60.0), [240, 240, 240, 255]);
        assert!(changed_regions(&after, &before).is_empty());
    }

    #[test]
    fn different_sizes_compare_as_nothing() {
        let other = Image::filled(10, 10, [0, 0, 0, 255]);
        assert!(changed_regions(&white(), &other).is_empty());
    }
}
