//! Scrolling capture: turn the wheel, take a picture, join it on, repeat,
//! until the page stops moving.
//!
//! This is the one place where Bruma moves your mouse and scrolls for you. It
//! stops at the end of the page, when the pictures no longer line up, after
//! `MAX_HEIGHT` pixels, or as soon as you press Esc.

use std::thread::sleep;
use std::time::Duration;

use bruma_core::stitch;
use bruma_core::{Image, Rect};

use crate::platform;

/// Stop here even if the page goes on.
const MAX_HEIGHT: u32 = 30_000;
const MAX_STEPS: usize = 120;

/// The area as it is now, once it has stopped moving: two pictures in a row
/// that are the same.
fn settled(area: Rect) -> Option<Image> {
    let grab = || platform::capture().ok().map(|c| c.image.crop(area));
    let mut last = grab()?;
    for _ in 0..6 {
        sleep(Duration::from_millis(70));
        let now = grab()?;
        if now == last {
            return Some(now);
        }
        last = now;
    }
    // Something keeps moving (a video, an animation); use what we have.
    Some(last)
}

/// Captures everything that scrolls through `area`, which is given in the
/// coordinates of a capture whose top-left pixel is at desktop `origin`.
pub fn capture(area: Rect, origin: (i32, i32)) -> Option<Image> {
    let centre = area.center();
    let came_from = platform::cursor_position();
    platform::set_cursor_position(origin.0 + centre.x as i32, origin.1 + centre.y as i32);
    // Give the overlay time to leave the screen.
    sleep(Duration::from_millis(250));

    let mut previous = settled(area)?;
    let mut total = previous.clone();
    for _ in 0..MAX_STEPS {
        if platform::escape_pressed() || total.height >= MAX_HEIGHT {
            break;
        }
        platform::scroll_wheel(-3);
        sleep(Duration::from_millis(160));
        let Some(next) = settled(area) else { break };
        match stitch::scroll_offset(&previous, &next) {
            // The end of the page, or pictures that cannot be joined.
            Some(0) | None => break,
            Some(moved) => {
                stitch::append(&mut total, &next, moved);
                previous = next;
            }
        }
    }
    if let Some((x, y)) = came_from {
        platform::set_cursor_position(x, y);
    }
    Some(total)
}
