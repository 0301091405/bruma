//! Rewind: a few seconds of the screen kept in memory, so a capture can reach
//! back to something that has already disappeared.
//!
//! A background thread samples the screen twice a second. An unchanged screen
//! costs one frame, however long it stays unchanged. Frames never leave
//! memory and are dropped as soon as they are older than the window.
//!
//! The recorder is meant not to be noticed. It runs below normal priority on
//! one core; it compares the screen with the newest frame before converting
//! anything; when neither the screen nor the user has moved for a while it
//! samples four times less often; and while a game or a film fills the screen,
//! or the machine is locked, it does nothing at all.
//!
//! It reads the screen through Desktop Duplication where that is to be had,
//! which tells it for a tenth of a millisecond that nothing has changed, and
//! hands over a changed screen in three. Because it already holds the screen,
//! it is also who a capture asks for the present moment ([`Rewind::shot`]).

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use bruma_core::Image;
use bruma_core::clip::{Area, Clip, Tape};
use bruma_core::session::Frame;

use crate::platform::{Attention, Duplicator, Grabber, Raw};
use crate::{note, platform};

const INTERVAL: Duration = Duration::from_millis(500);
/// How often the recorder wakes to see whether a sample is due.
const TICK: Duration = Duration::from_millis(100);
/// How often it looks while keeping the screen for GIFs: thirty times a
/// second.
const FAST_TICK: Duration = Duration::from_millis(33);
/// Upper bound on memory held for GIFs. Only a screen that changes all over,
/// all the time (a video, full screen) comes near it; it then keeps fewer
/// seconds.
const TAPE_BUDGET: usize = 320 * 1024 * 1024;
/// A screen that has not changed and a user who has touched nothing for this
/// long are sampled at the quiet pace.
const QUIET_AFTER: Duration = Duration::from_secs(2);
const QUIET_INTERVAL: Duration = Duration::from_secs(2);
/// Sampling waits this long after an overlay closes, so the overlay itself is
/// never recorded.
const SETTLE: Duration = Duration::from_millis(400);
/// Upper bound on memory held by recorded frames.
const BUDGET_BYTES: usize = 192 * 1024 * 1024;
/// How long a capture waits for the recorder before reading the screen itself.
const ANSWER_WITHIN: Duration = Duration::from_millis(80);
/// A desktop that declined duplication is asked again after this long.
const ASK_AGAIN: Duration = Duration::from_secs(30);

/// The screen as it is now, handed to a capture by the recorder.
pub struct Shot {
    pub image: Arc<Image>,
    /// Desktop coordinates of the image's top-left pixel.
    pub origin: (i32, i32),
}

/// A capture asking for the present moment; the answer goes back through it.
type Request = mpsc::Sender<Option<Shot>>;

struct Recorded {
    image: Arc<Image>,
    origin: (i32, i32),
    /// The last time the screen looked like this.
    last_seen: Instant,
}

struct Shared {
    frames: Mutex<VecDeque<Recorded>>,
    paused: AtomicBool,
    stopped: AtomicBool,
    resume_at: Mutex<Instant>,
    window: Duration,
    /// The screen for GIFs, and the desktop position of its top-left pixel.
    tape: Mutex<Option<(Tape, (i32, i32))>>,
    /// The tape's clock counts milliseconds from here.
    epoch: Instant,
    /// How much the tape keeps; zero for no tape.
    clip: Duration,
    /// When recording stopped for the overlay on screen: where a GIF ends.
    paused_at: Mutex<Option<Instant>>,
}

/// How long to wait between two samples. Things appear and vanish because
/// somebody did something, so input brings the full pace back at once.
fn pace(unchanged_for: Duration, idle_for: Duration, window: Duration) -> Duration {
    if unchanged_for >= QUIET_AFTER && idle_for >= QUIET_AFTER {
        QUIET_INTERVAL.min(window / 2).max(INTERVAL)
    } else {
        INTERVAL
    }
}

pub struct Rewind {
    shared: Arc<Shared>,
    requests: mpsc::Sender<Request>,
    thread: thread::JoinHandle<()>,
}

/// Adds a sample: an unchanged screen refreshes the newest frame instead of
/// adding one; frames older than `window` or over the memory budget are dropped.
fn record(
    frames: &mut VecDeque<Recorded>,
    image: Image,
    origin: (i32, i32),
    now: Instant,
    window: Duration,
) {
    match frames.back_mut() {
        Some(last) if last.origin == origin && *last.image == image => last.last_seen = now,
        _ => frames.push_back(Recorded {
            image: Arc::new(image),
            origin,
            last_seen: now,
        }),
    }
    prune(frames, now, window);
}

/// The screen still looks like the newest frame.
fn seen_again(frames: &mut VecDeque<Recorded>, now: Instant, window: Duration) {
    if let Some(last) = frames.back_mut() {
        last.last_seen = now;
    }
    prune(frames, now, window);
}

/// Drops frames older than `window` or over the memory budget.
fn prune(frames: &mut VecDeque<Recorded>, now: Instant, window: Duration) {
    let bytes = |f: &VecDeque<Recorded>| f.iter().map(|r| r.image.rgba.len()).sum::<usize>();
    while frames.len() > 1
        && (now.duration_since(frames[0].last_seen) > window || bytes(frames) > BUDGET_BYTES)
    {
        frames.pop_front();
    }
}

/// The recorded frames that can precede a capture of `now`, oldest first.
/// Frames last seen longer ago than `window` (the recorder was resting) are
/// history of another kind and are left out.
fn before(
    frames: &VecDeque<Recorded>,
    now: &Image,
    origin: (i32, i32),
    at: Instant,
    window: Duration,
) -> Vec<Frame> {
    let usable = |r: &&Recorded| {
        !window.is_zero()
            && r.origin == origin
            && r.image.width == now.width
            && r.image.height == now.height
            && at.saturating_duration_since(r.last_seen) <= window + QUIET_INTERVAL
    };
    let mut out: Vec<&Recorded> = frames.iter().filter(usable).collect();
    // The newest sample usually is the present; showing it twice helps nobody.
    if out
        .last()
        .is_some_and(|r| std::ptr::eq(&*r.image, now) || *r.image == *now)
    {
        out.pop();
    }
    out.into_iter()
        .map(|r| Frame {
            image: r.image.clone(),
            age_ms: at
                .saturating_duration_since(r.last_seen)
                .as_millis()
                .min(u32::MAX as u128) as u32,
        })
        .collect()
}

/// Where two frames differ, in words: how many pixels, and the box around
/// them.
pub(crate) fn difference(before: &Image, now: &Image) -> String {
    if (before.width, before.height) != (now.width, now.height) {
        return "changed: another size".into();
    }
    let w = now.width.max(1) as usize;
    let (mut left, mut top, mut right, mut bottom, mut count) = (usize::MAX, usize::MAX, 0, 0, 0);
    let pixels = before.rgba.chunks_exact(4).zip(now.rgba.chunks_exact(4));
    for (i, _) in pixels.enumerate().filter(|(_, (a, b))| a != b) {
        let (x, y) = (i % w, i / w);
        (left, top) = (left.min(x), top.min(y));
        (right, bottom) = (right.max(x), bottom.max(y));
        count += 1;
    }
    if count == 0 {
        return "changed: nothing visible".into();
    }
    format!(
        "changed: {count} px in {}x{} at {left},{top}",
        right - left + 1,
        bottom - top + 1
    )
}

/// The recorder's eyes: Desktop Duplication where it is to be had, GDI
/// everywhere else.
struct Eyes {
    /// Whether duplication may be used at all (the `fast_capture` setting).
    fast: bool,
    duplicator: Option<Duplicator>,
    grabber: Option<Grabber>,
    /// Duplication is not asked for again before this moment.
    ask_at: Instant,
    /// The duplicated picture has changed since the last look.
    unseen: bool,
}

impl Eyes {
    fn new(fast: bool) -> Self {
        Self {
            fast,
            duplicator: None,
            grabber: None,
            ask_at: Instant::now(),
            unseen: false,
        }
    }

    /// Sets duplication up if it may be used and is not running. `plain`:
    /// the ordinary desktop, without the mouse cursor in the picture.
    fn get_ready(&mut self, plain: bool) {
        if !(self.fast && plain) {
            self.duplicator = None;
            return;
        }
        let now = Instant::now();
        if self.duplicator.is_none() && now >= self.ask_at {
            self.duplicator = Duplicator::new();
            let started = self.duplicator.is_some();
            if !started {
                self.ask_at = now + ASK_AGAIN;
            }
            note(|| {
                format!(
                    "rewind: desktop duplication {} in {:.1} ms",
                    if started { "started" } else { "declined" },
                    now.elapsed().as_secs_f64() * 1000.0
                )
            });
        }
    }

    /// Brings the duplicated picture up to date. `Some(true)` when something
    /// changed; `None` without duplication.
    fn poll(&mut self) -> Option<bool> {
        match self.duplicator.as_mut()?.refresh() {
            Ok(changed) => {
                self.unseen |= changed;
                Some(changed)
            }
            Err(()) => {
                // Lost: the desktop changed, or something took the monitor.
                self.duplicator = None;
                self.ask_at = Instant::now() + QUIET_INTERVAL;
                note(|| "rewind: desktop duplication lost".into());
                None
            }
        }
    }

    /// Reads the screen. The flag says that the screen is known not to have
    /// changed since the last look. With duplication the picture is the one
    /// [`Eyes::poll`] brought up to date.
    fn look(&mut self) -> Option<(Raw<'_>, bool)> {
        if let Some(duplicator) = &self.duplicator {
            self.grabber = None;
            let same = !std::mem::take(&mut self.unseen);
            return Some((duplicator.raw(), same));
        }
        if self.grabber.is_none() {
            self.grabber = Grabber::new().ok();
        }
        let raw = self.grabber.as_mut()?.grab().ok()?;
        Some((raw, false))
    }

    /// Lets go of everything; `true` if there was anything to let go of.
    fn rest(&mut self) -> bool {
        self.duplicator.take().is_some() | self.grabber.take().is_some()
    }
}

/// The recorder: its eyes, and what it knows about what they last saw.
struct Recorder<'a> {
    shared: &'a Shared,
    eyes: Eyes,
    /// Whether the newest frame shows what duplication last handed over, so
    /// that "nothing changed since" means "the newest frame still holds".
    in_step: bool,
    changed_at: Instant,
}

impl Recorder<'_> {
    /// Brings the duplicated picture up to date and puts what changed on
    /// the tape.
    fn refresh(&mut self, now: Instant) {
        let Some(changed) = self.eyes.poll() else {
            return;
        };
        if self.shared.clip.is_zero() {
            return;
        }
        let Some(duplicator) = self.eyes.duplicator.as_ref() else {
            return;
        };
        let raw = duplicator.raw();
        let (w, h) = (raw.width as usize, raw.height as usize);
        let ms = now.duration_since(self.shared.epoch).as_millis() as u64;
        let Ok(mut tape) = self.shared.tape.lock() else {
            return;
        };
        match tape.as_mut() {
            Some((tape, origin)) if tape.size() == (w, h) && *origin == raw.origin => {
                if changed {
                    tape.record(raw.bytes, duplicator.dirty(), ms);
                }
            }
            _ => {
                let keep = self.shared.clip.as_millis() as u64 + 1000;
                *tape = Some((
                    Tape::new(raw.bytes, w, h, ms, keep, TAPE_BUDGET),
                    raw.origin,
                ));
            }
        }
    }

    /// Looks at the screen and files what it sees: a new frame, or the
    /// newest one seen again. Returns that frame, the present moment.
    /// `urgent`: a capture is waiting, so be quick rather than cheap.
    fn sample(&mut self, attention: Attention, urgent: bool) -> Option<Shot> {
        let started = Instant::now();
        let window = self.shared.window;
        let plain = attention == Attention::Desktop && !platform::captures_cursor();
        // Setting duplication up takes a moment a waiting capture does not have.
        if !urgent || !(self.eyes.fast && plain) {
            self.eyes.get_ready(plain);
        }
        self.refresh(started);
        let Some((raw, same)) = self.eyes.look() else {
            self.in_step = false;
            return None;
        };
        // An overlay may have opened while we were looking.
        if !urgent && self.shared.paused.load(Ordering::Relaxed) {
            self.in_step = false;
            return None;
        }
        let newest = self
            .shared
            .frames
            .lock()
            .ok()
            .and_then(|frames| frames.back().map(|r| (r.image.clone(), r.origin)));
        let unchanged = newest.as_ref().is_some_and(|(image, origin)| {
            *origin == raw.origin
                && ((same && self.in_step) || image.matches_bgrx(raw.width, raw.height, raw.bytes))
        });
        let convert = || Image::from_bgrx(raw.width, raw.height, raw.bytes);
        let fresh = (!unchanged).then(|| {
            if urgent {
                convert()
            } else {
                bruma_core::par::on_this_thread(convert)
            }
        });
        // For the trace only: what moved on a screen that will not hold still.
        let what = match (&newest, &fresh) {
            // (Not for a capture: describing the change would keep it waiting.)
            (Some((before, _)), Some(now)) if crate::tracing() && !urgent => {
                difference(before, now)
            }
            (_, Some(_)) => "changed".into(),
            _ => "unchanged".into(),
        };
        let origin = raw.origin;
        let mut frames = self.shared.frames.lock().ok()?;
        let at = Instant::now();
        match fresh {
            Some(image) => {
                self.changed_at = started;
                record(&mut frames, image, origin, at, window);
            }
            None => seen_again(&mut frames, at, window),
        }
        self.in_step = true;
        note(|| {
            format!(
                "rewind: {} in {:.1} ms, {what}, {} frames held",
                if urgent { "shot" } else { "sampled" },
                started.elapsed().as_secs_f64() * 1000.0,
                frames.len()
            )
        });
        frames.back().map(|r| Shot {
            image: r.image.clone(),
            origin: r.origin,
        })
    }
}

/// The recorder's loop: runs on its own thread until the [`Rewind`] is gone.
fn run(shared: &Shared, requests: mpsc::Receiver<Request>, in_fullscreen: bool, fast: bool) {
    platform::run_this_thread_in_the_background();
    // Without a window of history there is nothing to record; the thread
    // then only keeps its eyes ready for captures.
    let records = !shared.window.is_zero();
    let mut recorder = Recorder {
        shared,
        eyes: Eyes::new(fast),
        in_step: false,
        changed_at: Instant::now(),
    };
    let mut sampled_at = Instant::now();
    // For GIFs the screen is looked at on a steady beat of thirty a second.
    // Waiting on the channel would wake on the system's coarse clock (every
    // 15.6 ms), turning 33 ms into 47; a plain sleep is precise.
    let steady = !shared.clip.is_zero();
    let mut beat = Instant::now();
    loop {
        let request = if steady {
            let now = Instant::now();
            if beat > now {
                thread::sleep(beat - now);
            }
            beat += FAST_TICK;
            let now = Instant::now();
            if beat < now {
                // Fell behind (the machine was busy): start the beat again.
                beat = now + FAST_TICK;
            }
            match requests.try_recv() {
                Ok(reply) => Ok(reply),
                Err(mpsc::TryRecvError::Empty) => Err(mpsc::RecvTimeoutError::Timeout),
                Err(mpsc::TryRecvError::Disconnected) => Err(mpsc::RecvTimeoutError::Disconnected),
            }
        } else {
            requests.recv_timeout(TICK)
        };
        match request {
            Ok(reply) => {
                let shot = recorder.sample(platform::attention(), true);
                let _ = reply.send(shot);
                if !records {
                    if let Ok(mut frames) = shared.frames.lock() {
                        frames.clear();
                    }
                    recorder.in_step = false;
                }
                // The capture hurried this thread up; back to the background.
                platform::run_this_thread_in_the_background();
                continue;
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
        if shared.stopped.load(Ordering::Relaxed) {
            break;
        }
        let now = Instant::now();
        let waiting = shared.resume_at.lock().is_ok_and(|t| now < *t);
        if shared.paused.load(Ordering::Relaxed) || waiting {
            continue;
        }
        // The tape, between the samples.
        if !shared.clip.is_zero() && recorder.eyes.duplicator.is_some() {
            recorder.refresh(now);
        }
        let since = now.duration_since(sampled_at);
        if since < INTERVAL {
            continue;
        }
        let due = if records {
            let unchanged_for = now.duration_since(recorder.changed_at);
            pace(unchanged_for, platform::idle_for(), shared.window)
        } else {
            QUIET_INTERVAL
        };
        if since < due {
            continue;
        }
        sampled_at = now;
        let attention = platform::attention();
        let resting = match attention {
            Attention::Desktop => false,
            Attention::FullScreen => !in_fullscreen,
            Attention::Away => true,
        };
        if resting {
            // Nothing is of use while resting; give the memory back, and
            // leave the monitor to whatever fills it.
            if recorder.eyes.rest() {
                note(|| format!("rewind: resting ({attention:?})"));
            }
            recorder.in_step = false;
        } else if records {
            recorder.sample(attention, false);
        } else {
            let plain = attention == Attention::Desktop && !platform::captures_cursor();
            recorder.eyes.get_ready(plain);
        }
    }
}

impl Rewind {
    /// Starts recording the last `seconds` of the screen; with `in_fullscreen`
    /// also while a game or a film fills it. With `fast`, the screen is read
    /// through Desktop Duplication where possible, and the recorder then
    /// serves captures even when `seconds` is zero and nothing is recorded.
    /// `None` when there is nothing for it to do.
    pub fn start(seconds: f32, in_fullscreen: bool, fast: bool, clip: f32) -> Option<Self> {
        let sane = |v: f32, most: f32| if v.is_nan() { 0.0 } else { v.clamp(0.0, most) };
        let seconds = sane(seconds, 30.0);
        // GIFs need the quick way of reading the screen: it is what says
        // which parts changed.
        let clip = if fast { sane(clip, 10.0) } else { 0.0 };
        if seconds <= 0.0 && !fast {
            return None;
        }
        let shared = Arc::new(Shared {
            frames: Mutex::new(VecDeque::new()),
            paused: AtomicBool::new(false),
            stopped: AtomicBool::new(false),
            resume_at: Mutex::new(Instant::now()),
            window: Duration::from_secs_f32(seconds),
            tape: Mutex::new(None),
            epoch: Instant::now(),
            clip: Duration::from_secs_f32(clip),
            paused_at: Mutex::new(None),
        });
        let worker = shared.clone();
        let (requests, inbox) = mpsc::channel();
        let thread = thread::Builder::new()
            .name("bruma-rewind".into())
            .spawn(move || run(&worker, inbox, in_fullscreen, fast))
            .ok()?;
        Some(Self {
            shared,
            requests,
            thread,
        })
    }

    /// The screen as it is now, from the recorder, which has it sooner than
    /// a capture made from scratch. `None` when it cannot say in time; the
    /// caller then reads the screen itself.
    pub fn shot(&self) -> Option<Shot> {
        let (reply, answer) = mpsc::channel();
        // The recorder idles below normal priority; a capture must not wait
        // behind whatever else is busy.
        platform::hurry(&self.thread);
        self.requests.send(reply).ok()?;
        answer.recv_timeout(ANSWER_WITHIN).ok().flatten()
    }

    /// Stops sampling while an overlay is on screen.
    pub fn pause(&self) {
        if !self.shared.paused.swap(true, Ordering::Relaxed) {
            if let Ok(mut at) = self.shared.paused_at.lock() {
                *at = Some(Instant::now());
            }
        }
    }

    pub fn resume(&self) {
        if let Ok(mut at) = self.shared.resume_at.lock() {
            *at = Instant::now() + SETTLE;
        }
        if let Ok(mut at) = self.shared.paused_at.lock() {
            *at = None;
        }
        self.shared.paused.store(false, Ordering::Relaxed);
    }

    /// The last seconds of `area` of the desktop picture whose top-left
    /// pixel is at `origin`, up to when recording stopped for the overlay,
    /// at the pace they were recorded. `None` when nothing was kept for it.
    pub fn clip(&self, area: Area, origin: (i32, i32)) -> Option<Clip> {
        let guard = self.shared.tape.lock().ok()?;
        let (tape, tape_origin) = guard.as_ref()?;
        if *tape_origin != origin {
            return None;
        }
        let end = self
            .shared
            .paused_at
            .lock()
            .ok()
            .and_then(|at| *at)
            .unwrap_or_else(Instant::now);
        let end = end.duration_since(self.shared.epoch).as_millis() as u64;
        let from = end.saturating_sub(self.shared.clip.as_millis() as u64);
        let clip = tape.clip(area, from, end)?;
        (clip.length() > 0).then_some(clip)
    }

    /// Recorded frames that differ from the capture `now`, oldest first.
    pub fn frames_before(&self, now: &Image, origin: (i32, i32), at: Instant) -> Vec<Frame> {
        match self.shared.frames.lock() {
            Ok(frames) => before(&frames, now, origin, at, self.shared.window),
            Err(_) => Vec::new(),
        }
    }
}

impl Drop for Rewind {
    fn drop(&mut self) {
        self.shared.stopped.store(true, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn image(v: u8) -> Image {
        Image::filled(4, 4, [v, v, v, 255])
    }

    const WINDOW: Duration = Duration::from_secs(4);

    #[test]
    fn an_unchanged_screen_stays_one_frame() {
        let t = Instant::now();
        let mut frames = VecDeque::new();
        for i in 0..10 {
            record(&mut frames, image(7), (0, 0), t + INTERVAL * i, WINDOW);
        }
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].last_seen, t + INTERVAL * 9);
    }

    #[test]
    fn changes_add_frames_and_old_ones_expire() {
        let t = Instant::now();
        let mut frames = VecDeque::new();
        record(&mut frames, image(1), (0, 0), t, WINDOW);
        record(
            &mut frames,
            image(2),
            (0, 0),
            t + Duration::from_secs(1),
            WINDOW,
        );
        record(
            &mut frames,
            image(3),
            (0, 0),
            t + Duration::from_secs(2),
            WINDOW,
        );
        assert_eq!(frames.len(), 3);
        record(
            &mut frames,
            image(4),
            (0, 0),
            t + Duration::from_secs(5),
            WINDOW,
        );
        let left: Vec<u8> = frames.iter().map(|r| r.image.rgba[0]).collect();
        assert_eq!(left, [2, 3, 4], "the frame last seen 5 s ago is gone");
    }

    #[test]
    fn the_newest_frame_always_survives() {
        let t = Instant::now();
        let mut frames = VecDeque::new();
        record(&mut frames, image(1), (0, 0), t, WINDOW);
        record(
            &mut frames,
            image(1),
            (0, 0),
            t + Duration::from_secs(60),
            WINDOW,
        );
        assert_eq!(frames.len(), 1);
    }

    #[test]
    fn history_skips_the_present_and_reports_ages() {
        let t = Instant::now();
        let mut frames = VecDeque::new();
        record(&mut frames, image(1), (0, 0), t, WINDOW);
        record(
            &mut frames,
            image(2),
            (0, 0),
            t + Duration::from_millis(500),
            WINDOW,
        );
        record(
            &mut frames,
            image(3),
            (0, 0),
            t + Duration::from_millis(1000),
            WINDOW,
        );
        let at = t + Duration::from_millis(1200);
        let history = before(&frames, &image(3), (0, 0), at, WINDOW);
        let ages: Vec<u32> = history.iter().map(|f| f.age_ms).collect();
        assert_eq!(
            ages,
            [1200, 700],
            "the frame equal to the present is left out"
        );
        // A present that differs from the newest sample keeps all three.
        assert_eq!(before(&frames, &image(9), (0, 0), at, WINDOW).len(), 3);
    }

    #[test]
    fn history_ignores_another_desktop_layout() {
        let t = Instant::now();
        let mut frames = VecDeque::new();
        record(&mut frames, image(1), (0, 0), t, WINDOW);
        assert!(before(&frames, &image(2), (-1920, 0), t, WINDOW).is_empty());
        let bigger = Image::filled(8, 8, [2, 2, 2, 255]);
        assert!(before(&frames, &bigger, (0, 0), t, WINDOW).is_empty());
    }

    #[test]
    fn a_difference_is_described_by_its_size_and_place() {
        let a = Image::filled(20, 10, [0, 0, 0, 255]);
        let mut b = a.clone();
        b.set_pixel(3, 2, [9, 9, 9, 255]);
        b.set_pixel(7, 5, [9, 9, 9, 255]);
        assert_eq!(difference(&a, &b), "changed: 2 px in 5x4 at 3,2");
        assert_eq!(difference(&a, &a), "changed: nothing visible");
        assert_eq!(difference(&a, &image(1)), "changed: another size");
    }

    #[test]
    fn zero_seconds_switches_rewind_off() {
        assert!(Rewind::start(0.0, false, false, 0.0).is_none());
        assert!(Rewind::start(-1.0, false, false, 5.0).is_none());
        assert!(Rewind::start(f32::NAN, false, false, f32::NAN).is_none());
    }

    #[test]
    fn a_still_screen_and_an_idle_user_are_sampled_less_often() {
        let (long, short) = (Duration::from_secs(10), Duration::from_millis(300));
        assert_eq!(pace(long, long, WINDOW), QUIET_INTERVAL);
        // Either one moving brings the full pace back.
        assert_eq!(pace(short, long, WINDOW), INTERVAL);
        assert_eq!(pace(long, short, WINDOW), INTERVAL);
        // The quiet pace never outruns a short window, nor the normal pace.
        assert_eq!(
            pace(long, long, Duration::from_secs(2)),
            Duration::from_secs(1)
        );
        assert_eq!(pace(long, long, Duration::from_millis(600)), INTERVAL);
    }

    #[test]
    fn seeing_the_same_screen_again_keeps_its_frame_alive() {
        let t = Instant::now();
        let mut frames = VecDeque::new();
        record(&mut frames, image(1), (0, 0), t, WINDOW);
        record(
            &mut frames,
            image(2),
            (0, 0),
            t + Duration::from_secs(1),
            WINDOW,
        );
        seen_again(&mut frames, t + Duration::from_secs(6), WINDOW);
        assert_eq!(frames.len(), 1, "the older frame expired");
        assert_eq!(frames[0].last_seen, t + Duration::from_secs(6));
        seen_again(&mut VecDeque::new(), t, WINDOW);
    }

    #[test]
    fn without_a_window_nothing_is_history() {
        let t = Instant::now();
        let mut frames = VecDeque::new();
        record(&mut frames, image(1), (0, 0), t, Duration::ZERO);
        record(&mut frames, image(2), (0, 0), t, Duration::ZERO);
        assert!(before(&frames, &image(3), (0, 0), t, Duration::ZERO).is_empty());
    }

    #[test]
    fn the_shot_itself_is_not_its_own_past() {
        let t = Instant::now();
        let mut frames = VecDeque::new();
        record(&mut frames, image(1), (0, 0), t, WINDOW);
        record(&mut frames, image(2), (0, 0), t, WINDOW);
        // The capture holds the very frame the recorder filed last.
        let present = frames.back().unwrap().image.clone();
        let history = before(&frames, &present, (0, 0), t, WINDOW);
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].image.rgba[0], 1);
    }

    #[test]
    fn frames_from_before_a_rest_are_not_history() {
        let t = Instant::now();
        let mut frames = VecDeque::new();
        record(&mut frames, image(1), (0, 0), t, WINDOW);
        // The recorder rested (a game was in front) for ten minutes.
        let later = t + Duration::from_secs(600);
        assert!(before(&frames, &image(2), (0, 0), later, WINDOW).is_empty());
        // Within the window, plus the slack of the quiet pace, it still counts.
        let soon = t + WINDOW + QUIET_INTERVAL;
        assert_eq!(before(&frames, &image(2), (0, 0), soon, WINDOW).len(), 1);
    }
}
