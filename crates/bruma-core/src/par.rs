//! Spreading a big pixel loop over the processor's cores.
//!
//! A full-screen pass touches eight megabytes at 1080p and thirty-three at
//! 4K. Cut into horizontal bands, one per core, it finishes several times
//! sooner; no dependency needed, scoped threads do it.

use std::cell::Cell;
use std::sync::OnceLock;

thread_local! {
    static ALONE: Cell<bool> = const { Cell::new(false) };
}

/// Runs `work` with every job it starts kept on the calling thread. For work
/// in the background that should be cheap rather than fast: it must not take
/// every core for a moment while the user is doing something else.
pub fn on_this_thread<R>(work: impl FnOnce() -> R) -> R {
    let before = ALONE.with(|alone| alone.replace(true));
    let result = work();
    ALONE.with(|alone| alone.set(before));
    result
}

/// How many bands to cut a job into.
fn cores() -> usize {
    static CORES: OnceLock<usize> = OnceLock::new();
    *CORES.get_or_init(|| {
        std::thread::available_parallelism()
            .map_or(1, |n| n.get())
            .min(8)
    })
}

/// Jobs smaller than this many elements are done on the spot: starting
/// threads would cost more than it saves.
const WORTH_IT: usize = 1 << 17;

/// Calls `work(first_row, band)` for bands of whole rows that together cover
/// `data`, each band on its own core. `row_len` is the number of elements in
/// one row.
pub fn bands<T: Send>(data: &mut [T], row_len: usize, work: impl Fn(usize, &mut [T]) + Sync) {
    let row_len = row_len.max(1);
    let rows = data.len() / row_len;
    let threads = cores().min(rows.max(1));
    if threads < 2 || data.len() < WORTH_IT || ALONE.with(Cell::get) {
        work(0, data);
        return;
    }
    let per_band = rows.div_ceil(threads);
    std::thread::scope(|scope| {
        for (i, band) in data.chunks_mut(per_band * row_len).enumerate() {
            let work = &work;
            scope.spawn(move || work(i * per_band, band));
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_row_is_visited_once_with_its_own_index() {
        for rows in [1usize, 2, 7, 64, 1000, 4097] {
            let row_len = 300;
            let mut data = vec![0u32; rows * row_len];
            bands(&mut data, row_len, |first, band| {
                for (i, row) in band.chunks_mut(row_len).enumerate() {
                    for v in row {
                        *v += (first + i) as u32 + 1;
                    }
                }
            });
            for (y, row) in data.chunks(row_len).enumerate() {
                assert!(
                    row.iter().all(|v| *v == y as u32 + 1),
                    "{rows} rows, row {y}"
                );
            }
        }
    }

    #[test]
    fn work_can_keep_its_jobs_to_one_thread() {
        let me = std::thread::current().id();
        let calls = std::sync::atomic::AtomicUsize::new(0);
        let mut data = vec![0u8; WORTH_IT * 4];
        let len = data.len();
        on_this_thread(|| {
            bands(&mut data, 1024, |first, band| {
                calls.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                assert_eq!((first, band.len()), (0, len));
                assert_eq!(std::thread::current().id(), me);
            });
        });
        assert_eq!(calls.into_inner(), 1);
        // Afterwards jobs spread out again (where there are cores to spread to).
        let threads = std::sync::Mutex::new(std::collections::HashSet::new());
        bands(&mut data, 1024, |_, _| {
            threads.lock().unwrap().insert(std::thread::current().id());
        });
        let spread = threads.into_inner().unwrap().len();
        assert_eq!(spread > 1, cores() > 1);
    }

    #[test]
    fn empty_and_tiny_jobs_are_fine() {
        let mut none: Vec<u8> = Vec::new();
        bands(&mut none, 4, |_, band| assert!(band.is_empty()));
        let mut one = vec![1u8; 3];
        bands(&mut one, 0, |first, band| {
            assert_eq!((first, band.len()), (0, 3));
        });
    }
}
