//! Moving pictures. The recorder keeps the screen as a [`Tape`]: one picture
//! and then only the parts that changed, at up to thirty times a second. A
//! [`Clip`] is a stretch of the tape cut out for one area; it plays in the
//! overlay and becomes a GIF.
//!
//! Pixels are kept as 5-6-5 colour, two bytes each: a GIF has 256 colours,
//! so nothing that matters is lost, and the tape takes half the memory.

use std::collections::VecDeque;

use crate::image::Image;

/// A rectangle of whole pixels.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Area {
    pub x: usize,
    pub y: usize,
    pub w: usize,
    pub h: usize,
}

impl Area {
    pub fn new(x: usize, y: usize, w: usize, h: usize) -> Self {
        Self { x, y, w, h }
    }

    pub fn is_empty(&self) -> bool {
        self.w == 0 || self.h == 0
    }

    pub fn intersect(&self, other: &Area) -> Option<Area> {
        let x0 = self.x.max(other.x);
        let y0 = self.y.max(other.y);
        let x1 = (self.x + self.w).min(other.x + other.w);
        let y1 = (self.y + self.h).min(other.y + other.h);
        (x1 > x0 && y1 > y0).then(|| Area::new(x0, y0, x1 - x0, y1 - y0))
    }

    fn union(&self, other: &Area) -> Area {
        let x0 = self.x.min(other.x);
        let y0 = self.y.min(other.y);
        let x1 = (self.x + self.w).max(other.x + other.w);
        let y1 = (self.y + self.h).max(other.y + other.h);
        Area::new(x0, y0, x1 - x0, y1 - y0)
    }
}

#[inline]
fn pack(r: u8, g: u8, b: u8) -> u16 {
    ((r as u16 >> 3) << 11) | ((g as u16 >> 2) << 5) | (b as u16 >> 3)
}

#[inline]
fn unpack(v: u16) -> [u8; 3] {
    let r = ((v >> 11) & 31) as u8;
    let g = ((v >> 5) & 63) as u8;
    let b = (v & 31) as u8;
    [
        (r << 3) | (r >> 2),
        (g << 2) | (g >> 4),
        (b << 3) | (b >> 2),
    ]
}

/// The pixels of one area.
#[derive(Clone, Debug)]
struct Patch {
    area: Area,
    pixels: Vec<u16>,
}

impl Patch {
    /// Writes the patch into `dest`, a picture `width` wide whose top-left
    /// pixel is at `origin`, clipped to `bounds` (in the same coordinates as
    /// the patch).
    fn apply(&self, dest: &mut [u16], width: usize, origin: (usize, usize), bounds: &Area) {
        let Some(part) = self.area.intersect(bounds) else {
            return;
        };
        for y in part.y..part.y + part.h {
            let from = (y - self.area.y) * self.area.w + (part.x - self.area.x);
            let to = (y - origin.1) * width + (part.x - origin.0);
            dest[to..to + part.w].copy_from_slice(&self.pixels[from..from + part.w]);
        }
    }
}

#[derive(Debug)]
struct Change {
    at: u64,
    patches: Vec<Patch>,
    bytes: usize,
}

/// The screen over the last seconds, as a picture and the changes after it.
pub struct Tape {
    width: usize,
    height: usize,
    /// The screen just before the first change kept.
    base: Vec<u16>,
    base_at: u64,
    changes: VecDeque<Change>,
    bytes: usize,
    keep_ms: u64,
    budget: usize,
}

impl Tape {
    /// A tape that starts with the screen `bgrx` (blue, green, red, unused
    /// for each pixel) at time `at`, in milliseconds on any steady clock. It
    /// keeps `keep_ms` of changes, and never more than `budget` bytes of them.
    pub fn new(
        bgrx: &[u8],
        width: usize,
        height: usize,
        at: u64,
        keep_ms: u64,
        budget: usize,
    ) -> Self {
        let base = bgrx
            .chunks_exact(4)
            .take(width * height)
            .map(|p| pack(p[2], p[1], p[0]))
            .collect();
        Self {
            width,
            height,
            base,
            base_at: at,
            changes: VecDeque::new(),
            bytes: 0,
            keep_ms,
            budget,
        }
    }

    pub fn size(&self) -> (usize, usize) {
        (self.width, self.height)
    }

    /// The time of the oldest picture the tape can give.
    pub fn oldest(&self) -> u64 {
        self.base_at
    }

    /// Memory held by the changes, in bytes.
    pub fn bytes(&self) -> usize {
        self.bytes
    }

    /// Notes that at time `at` the screen `bgrx` changed inside `areas`.
    pub fn record(&mut self, bgrx: &[u8], areas: &[Area], at: u64) {
        let bounds = Area::new(0, 0, self.width, self.height);
        let mut patches = Vec::with_capacity(areas.len());
        for area in areas.iter().filter_map(|a| a.intersect(&bounds)) {
            let mut pixels = Vec::with_capacity(area.w * area.h);
            for y in area.y..area.y + area.h {
                let start = (y * self.width + area.x) * 4;
                let row = &bgrx[start..start + area.w * 4];
                pixels.extend(row.chunks_exact(4).map(|p| pack(p[2], p[1], p[0])));
            }
            patches.push(Patch { area, pixels });
        }
        if patches.is_empty() {
            return;
        }
        let bytes = patches.iter().map(|p| p.pixels.len() * 2).sum();
        self.bytes += bytes;
        self.changes.push_back(Change { at, patches, bytes });
        self.forget(at);
    }

    /// Folds changes older than the time kept, or over the budget, into the
    /// base picture.
    fn forget(&mut self, now: u64) {
        let bounds = Area::new(0, 0, self.width, self.height);
        while let Some(front) = self.changes.front() {
            let old = now.saturating_sub(front.at) > self.keep_ms;
            if !old && self.bytes <= self.budget {
                break;
            }
            let Some(change) = self.changes.pop_front() else {
                break;
            };
            for patch in &change.patches {
                patch.apply(&mut self.base, self.width, (0, 0), &bounds);
            }
            self.base_at = change.at;
            self.bytes -= change.bytes;
        }
    }

    /// The stretch from `from` to `to` (on the tape's clock) of `area`. The
    /// clip starts with the area as it was at `from`. `None` for an area off
    /// the tape.
    pub fn clip(&self, area: Area, from: u64, to: u64) -> Option<Clip> {
        let bounds = Area::new(0, 0, self.width, self.height);
        let area = area.intersect(&bounds)?;
        let from = from.max(self.base_at);
        let to = to.max(from);
        let mut first = vec![0u16; area.w * area.h];
        for y in 0..area.h {
            let start = (area.y + y) * self.width + area.x;
            first[y * area.w..(y + 1) * area.w].copy_from_slice(&self.base[start..start + area.w]);
        }
        let mut steps = Vec::new();
        for change in &self.changes {
            if change.at > to {
                break;
            }
            if change.at <= from {
                for patch in &change.patches {
                    patch.apply(&mut first, area.w, (area.x, area.y), &area);
                }
                continue;
            }
            let patches: Vec<Patch> = change
                .patches
                .iter()
                .filter_map(|patch| {
                    let part = patch.area.intersect(&area)?;
                    let mut pixels = vec![0u16; part.w * part.h];
                    patch.apply(&mut pixels, part.w, (part.x, part.y), &part);
                    Some(Patch {
                        area: Area::new(part.x - area.x, part.y - area.y, part.w, part.h),
                        pixels,
                    })
                })
                .collect();
            if !patches.is_empty() {
                steps.push(Step {
                    at: change.at - from,
                    patches,
                });
            }
        }
        Some(Clip::new(area.w, area.h, first, steps, to - from))
    }
}

#[derive(Debug)]
struct Step {
    /// Milliseconds after the start of the clip.
    at: u64,
    patches: Vec<Patch>,
}

/// After this many steps the clip keeps a whole picture, so that jumping to
/// a moment does not have to replay everything before it.
const KEY_EVERY: usize = 30;

/// A stretch of the screen for one area, ready to play and to save.
pub struct Clip {
    width: usize,
    height: usize,
    first: Vec<u16>,
    steps: Vec<Step>,
    /// The picture after step `i`, for every [`KEY_EVERY`]th step.
    keys: Vec<(usize, Vec<u16>)>,
    length: u64,
}

/// Where playback of a clip has got to.
#[derive(Clone, Debug, Default)]
pub struct Cursor {
    /// How many steps are applied to `state`.
    applied: usize,
    state: Vec<u16>,
}

impl Clip {
    fn new(width: usize, height: usize, first: Vec<u16>, steps: Vec<Step>, length: u64) -> Self {
        let mut keys = Vec::new();
        let mut state = first.clone();
        let whole = Area::new(0, 0, width, height);
        for (i, step) in steps.iter().enumerate() {
            for patch in &step.patches {
                patch.apply(&mut state, width, (0, 0), &whole);
            }
            if (i + 1) % KEY_EVERY == 0 {
                keys.push((i + 1, state.clone()));
            }
        }
        Self {
            width,
            height,
            first,
            steps,
            keys,
            length,
        }
    }

    /// A clip made of whole pictures, each with the time it appeared (in
    /// milliseconds from the first), all the same size. For when only a few
    /// pictures a second were recorded.
    pub fn from_pictures(pictures: &[(&Image, u64)], length: u64) -> Option<Self> {
        let (first_image, first_at) = pictures.first()?;
        let (w, h) = (first_image.width as usize, first_image.height as usize);
        let packed = |img: &Image| -> Vec<u16> {
            img.rgba
                .chunks_exact(4)
                .map(|p| pack(p[0], p[1], p[2]))
                .collect()
        };
        let first = packed(first_image);
        let steps = pictures[1..]
            .iter()
            .filter(|(img, _)| (img.width as usize, img.height as usize) == (w, h))
            .map(|(img, at)| Step {
                at: at.saturating_sub(*first_at),
                patches: vec![Patch {
                    area: Area::new(0, 0, w, h),
                    pixels: packed(img),
                }],
            })
            .collect();
        Some(Self::new(w, h, first, steps, length))
    }

    pub fn width(&self) -> u32 {
        self.width as u32
    }

    pub fn height(&self) -> u32 {
        self.height as u32
    }

    /// How long the clip is, in milliseconds.
    pub fn length(&self) -> u64 {
        self.length
    }

    /// When the picture changes, in milliseconds from the start.
    pub fn changes(&self) -> impl Iterator<Item = u64> + '_ {
        self.steps.iter().map(|s| s.at)
    }

    /// How many pictures a second the clip has, at most: the shortest gap
    /// between two changes, as a rate.
    pub fn rate(&self) -> u32 {
        let gaps = self
            .steps
            .windows(2)
            .map(|w| w[1].at.saturating_sub(w[0].at))
            .filter(|&g| g > 0);
        match gaps.min() {
            Some(gap) => (1000 / gap).min(60) as u32,
            None => 0,
        }
    }

    /// Brings `cursor` to time `t`: the picture as it was then.
    pub fn seek(&self, cursor: &mut Cursor, t: u64) {
        let target = self.steps.partition_point(|s| s.at <= t);
        if cursor.state.len() != self.first.len() || target < cursor.applied {
            // Back to the last whole picture before `t`.
            match self.keys.iter().rev().find(|(i, _)| *i <= target) {
                Some((i, state)) => {
                    cursor.applied = *i;
                    cursor.state.clone_from(state);
                }
                None => {
                    cursor.applied = 0;
                    cursor.state.clone_from(&self.first);
                }
            }
        }
        let whole = Area::new(0, 0, self.width, self.height);
        for step in &self.steps[cursor.applied..target] {
            for patch in &step.patches {
                patch.apply(&mut cursor.state, self.width, (0, 0), &whole);
            }
        }
        cursor.applied = target;
    }

    /// The picture `cursor` is at.
    pub fn picture(&self, cursor: &Cursor) -> Image {
        let rgba = cursor
            .state
            .iter()
            .flat_map(|&v| {
                let [r, g, b] = unpack(v);
                [r, g, b, 255]
            })
            .collect();
        Image::new(self.width as u32, self.height as u32, rgba)
    }

    /// The part from `from` to `to` (milliseconds into the clip) as a
    /// looping GIF. `dress` puts the drawings on each picture before it is
    /// written.
    pub fn gif(
        &self,
        from: u64,
        to: u64,
        mut dress: impl FnMut(Image) -> Image,
    ) -> Result<Vec<u8>, String> {
        let (w, h) = (
            u16::try_from(self.width).map_err(|_| "the area is too wide for a GIF")?,
            u16::try_from(self.height).map_err(|_| "the area is too tall for a GIF")?,
        );
        if w == 0 || h == 0 {
            return Err("the area is empty".into());
        }
        let to = to.min(self.length).max(from);
        // The moments a new picture is shown: the start, then each change.
        let mut times = vec![from];
        times.extend(self.changes().filter(|&t| t > from && t <= to));

        let mut cursor = Cursor::default();
        self.seek(&mut cursor, from);
        let first = dress(self.picture(&cursor));

        // One palette for the whole GIF, from the first picture and a sample
        // of everything that changes after it.
        let mut samples = first.rgba.clone();
        let budget = 600_000usize;
        let moving: usize = self
            .steps
            .iter()
            .flat_map(|s| &s.patches)
            .map(|p| p.pixels.len())
            .sum();
        let every = (moving / budget.max(1)).max(1);
        for (i, &v) in self
            .steps
            .iter()
            .filter(|s| s.at > from && s.at <= to)
            .flat_map(|s| &s.patches)
            .flat_map(|p| &p.pixels)
            .enumerate()
        {
            if i % every == 0 {
                let [r, g, b] = unpack(v);
                samples.extend_from_slice(&[r, g, b, 255]);
            }
        }
        let quant = color_quant::NeuQuant::new(10, 256, &samples);
        let palette = quant.color_map_rgb();
        // Colours are looked up once for each 5-6-5 value.
        let mut table = vec![0u8; 1 << 16];
        for (v, slot) in table.iter_mut().enumerate() {
            let [r, g, b] = unpack(v as u16);
            *slot = quant.index_of(&[r, g, b, 255]) as u8;
        }
        let index = |img: &Image| -> Vec<u8> {
            img.rgba
                .chunks_exact(4)
                .map(|p| table[pack(p[0], p[1], p[2]) as usize])
                .collect()
        };

        let mut out = Vec::new();
        {
            let mut encoder =
                gif::Encoder::new(&mut out, w, h, &palette).map_err(|e| e.to_string())?;
            encoder
                .set_repeat(gif::Repeat::Infinite)
                .map_err(|e| e.to_string())?;
            let mut shown = index(&first);
            // Each picture stays until the next one; the delays are counted
            // from the start so that rounding does not add up.
            let centi = |ms: u64| ((ms - from) as f64 / 10.0).round() as u64;
            let mut pending: Option<(Area, Vec<u8>, u64)> = Some((
                Area::new(0, 0, self.width, self.height),
                shown.clone(),
                from,
            ));
            let mut write = |frame: (Area, Vec<u8>, u64), until: u64| -> Result<(), String> {
                let (area, buffer, start) = frame;
                let delay = centi(until).saturating_sub(centi(start)).max(2);
                let gif_frame = gif::Frame {
                    left: area.x as u16,
                    top: area.y as u16,
                    width: area.w as u16,
                    height: area.h as u16,
                    delay: delay.min(u16::MAX as u64) as u16,
                    buffer: std::borrow::Cow::Owned(buffer),
                    ..gif::Frame::default()
                };
                encoder.write_frame(&gif_frame).map_err(|e| e.to_string())
            };
            for &t in &times[1..] {
                self.seek(&mut cursor, t);
                let next = index(&dress(self.picture(&cursor)));
                // Only the part that differs from the picture before.
                let mut changed: Option<Area> = None;
                for y in 0..self.height {
                    let row = y * self.width;
                    let (a, b) = (&shown[row..row + self.width], &next[row..row + self.width]);
                    if a == b {
                        continue;
                    }
                    let x0 = a.iter().zip(b).position(|(p, q)| p != q).unwrap_or(0);
                    let x1 =
                        self.width - a.iter().zip(b).rev().position(|(p, q)| p != q).unwrap_or(0);
                    let line = Area::new(x0, y, x1 - x0, 1);
                    changed = Some(changed.map_or(line, |c| c.union(&line)));
                }
                let Some(area) = changed else {
                    continue;
                };
                if let Some(frame) = pending.take() {
                    write(frame, t)?;
                }
                let mut buffer = Vec::with_capacity(area.w * area.h);
                for y in area.y..area.y + area.h {
                    let row = y * self.width;
                    buffer.extend_from_slice(&next[row + area.x..row + area.x + area.w]);
                }
                pending = Some((area, buffer, t));
                shown = next;
            }
            if let Some(frame) = pending.take() {
                write(frame, to.max(from + 20))?;
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A `w` × `h` screen of one grey, as the screen hands it over.
    fn screen(w: usize, h: usize, v: u8) -> Vec<u8> {
        [v, v, v, 0].repeat(w * h)
    }

    fn paint(bgrx: &mut [u8], w: usize, area: Area, bgr: [u8; 3]) {
        for y in area.y..area.y + area.h {
            for x in area.x..area.x + area.w {
                let i = (y * w + x) * 4;
                bgrx[i..i + 3].copy_from_slice(&bgr);
            }
        }
    }

    #[test]
    fn colours_survive_the_trip_through_two_bytes() {
        for c in [[0u8, 0, 0], [255, 255, 255], [255, 79, 31], [12, 200, 99]] {
            let back = unpack(pack(c[0], c[1], c[2]));
            for (a, b) in c.iter().zip(back) {
                assert!(
                    (*a as i32 - b as i32).abs() <= 8,
                    "{c:?} came back as {back:?}"
                );
            }
        }
    }

    #[test]
    fn a_clip_plays_the_changes_of_its_area_and_nothing_else() {
        let (w, h) = (40, 30);
        let mut s = screen(w, h, 100);
        let mut tape = Tape::new(&s, w, h, 0, 10_000, usize::MAX);
        // A box turns red at 100 ms, far from the clip's area another at 200.
        paint(&mut s, w, Area::new(5, 5, 4, 4), [0, 0, 255]);
        tape.record(&s, &[Area::new(5, 5, 4, 4)], 100);
        paint(&mut s, w, Area::new(30, 20, 4, 4), [255, 0, 0]);
        tape.record(&s, &[Area::new(30, 20, 4, 4)], 200);

        let clip = tape.clip(Area::new(0, 0, 20, 20), 0, 300).unwrap();
        assert_eq!((clip.width(), clip.height(), clip.length()), (20, 20, 300));
        assert_eq!(
            clip.changes().collect::<Vec<_>>(),
            [100],
            "only the change inside the area"
        );
        let mut cursor = Cursor::default();
        clip.seek(&mut cursor, 50);
        assert_eq!(
            clip.picture(&cursor).pixel(6, 6)[0],
            unpack(pack(100, 100, 100))[0]
        );
        clip.seek(&mut cursor, 150);
        assert!(clip.picture(&cursor).pixel(6, 6)[0] > 240, "red by now");
        // And back again.
        clip.seek(&mut cursor, 10);
        assert!(clip.picture(&cursor).pixel(6, 6)[0] < 120);
    }

    #[test]
    fn a_clip_that_starts_late_starts_from_the_picture_at_that_moment() {
        let (w, h) = (20, 20);
        let mut s = screen(w, h, 0);
        let mut tape = Tape::new(&s, w, h, 0, 10_000, usize::MAX);
        paint(&mut s, w, Area::new(0, 0, 20, 20), [255, 255, 255]);
        tape.record(&s, &[Area::new(0, 0, 20, 20)], 100);
        let clip = tape.clip(Area::new(0, 0, 20, 20), 150, 400).unwrap();
        assert_eq!(clip.changes().count(), 0);
        let mut cursor = Cursor::default();
        clip.seek(&mut cursor, 0);
        assert_eq!(clip.picture(&cursor).pixel(3, 3)[0], 255);
    }

    #[test]
    fn old_changes_and_changes_over_budget_fold_into_the_start() {
        let (w, h) = (10, 10);
        let mut s = screen(w, h, 0);
        let mut tape = Tape::new(&s, w, h, 0, 1000, usize::MAX);
        for i in 1..=5u64 {
            paint(&mut s, w, Area::new(0, 0, 10, 10), [i as u8 * 40; 3]);
            tape.record(&s, &[Area::new(0, 0, 10, 10)], i * 500);
        }
        // Kept: changes from the last second.
        assert_eq!(tape.oldest(), 1000);
        assert_eq!(tape.changes.len(), 3);
        // A budget for one change keeps one.
        let mut small = Tape::new(&screen(w, h, 0), w, h, 0, 60_000, 200);
        for i in 1..=4u64 {
            small.record(&s, &[Area::new(0, 0, 10, 10)], i * 10);
        }
        assert_eq!(small.changes.len(), 1);
        assert!(small.bytes() <= 200);
    }

    #[test]
    fn jumping_around_a_long_clip_gives_the_same_pictures_as_playing_it() {
        let (w, h) = (16, 8);
        let mut s = screen(w, h, 0);
        let mut tape = Tape::new(&s, w, h, 0, 100_000, usize::MAX);
        for i in 1..=100u64 {
            let x = (i as usize) % 16;
            paint(&mut s, w, Area::new(x, 0, 1, 8), [(i * 2) as u8; 3]);
            tape.record(&s, &[Area::new(x, 0, 1, 8)], i * 33);
        }
        let clip = tape.clip(Area::new(0, 0, 16, 8), 0, 3300).unwrap();
        let mut played = Cursor::default();
        let mut jumped = Cursor::default();
        for t in (0..3300).step_by(97) {
            clip.seek(&mut played, t);
            clip.seek(&mut jumped, 3300);
            clip.seek(&mut jumped, t);
            assert_eq!(played.state, jumped.state, "at {t} ms");
        }
        assert_eq!(clip.rate(), 30);
    }

    #[test]
    fn the_gif_has_a_frame_per_change_at_the_recorded_pace() {
        let (w, h) = (24, 24);
        let mut s = screen(w, h, 30);
        let mut tape = Tape::new(&s, w, h, 0, 10_000, usize::MAX);
        for i in 1..=30u64 {
            let y = (i as usize) % 24;
            paint(&mut s, w, Area::new(0, y, 24, 1), [(i * 8) as u8, 100, 200]);
            tape.record(&s, &[Area::new(0, y, 24, 1)], i * 33);
        }
        let clip = tape.clip(Area::new(0, 0, 24, 24), 0, 1000).unwrap();
        let bytes = clip.gif(0, 1000, |img| img).unwrap();
        let mut decoder = gif::DecodeOptions::new().read_info(&bytes[..]).unwrap();
        assert_eq!((decoder.width(), decoder.height()), (24, 24));
        let mut delays = Vec::new();
        let mut first = true;
        while let Some(frame) = decoder.read_next_frame().unwrap() {
            if !first {
                // Only the changed row is written.
                assert_eq!((frame.width, frame.height), (24, 1));
            }
            first = false;
            delays.push(frame.delay as u64);
        }
        assert_eq!(delays.len(), 31, "the start and thirty changes");
        let total: u64 = delays.iter().sum();
        // A second in hundredths; a frame shorter than two hundredths is
        // shown for two, as browsers slow anything quicker down.
        assert!((100..=102).contains(&total), "{total}");
        assert!(
            delays[1..30].iter().all(|&d| d == 3 || d == 4),
            "30 a second: {delays:?}"
        );
    }

    #[test]
    fn drawings_go_on_every_frame() {
        let (w, h) = (10, 10);
        let mut s = screen(w, h, 0);
        let mut tape = Tape::new(&s, w, h, 0, 10_000, usize::MAX);
        paint(&mut s, w, Area::new(0, 0, 2, 2), [255, 255, 255]);
        tape.record(&s, &[Area::new(0, 0, 2, 2)], 100);
        let clip = tape.clip(Area::new(0, 0, 10, 10), 0, 300).unwrap();
        let mut dressed = 0;
        clip.gif(0, 300, |mut img| {
            dressed += 1;
            img.set_pixel(9, 9, [255, 0, 0, 255]);
            img
        })
        .unwrap();
        assert_eq!(dressed, 2);
    }

    #[test]
    fn pictures_from_a_slow_recording_make_a_clip_too() {
        let a = Image::filled(8, 8, [0, 0, 0, 255]);
        let b = Image::filled(8, 8, [255, 255, 255, 255]);
        let clip = Clip::from_pictures(&[(&a, 0), (&b, 500)], 1000).unwrap();
        assert_eq!(clip.changes().collect::<Vec<_>>(), [500]);
        let mut cursor = Cursor::default();
        clip.seek(&mut cursor, 600);
        assert_eq!(clip.picture(&cursor).pixel(1, 1)[0], 255);
    }
}
