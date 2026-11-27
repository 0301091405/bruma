//! Spreading a big pixel loop over the processor's cores.
//!
//! A full-screen pass touches eight megabytes at 1080p and thirty-three at
//! 4K. Cut into horizontal bands, one per core, it finishes several times
//! sooner. The bands run on a few threads started once and kept waiting, so
//! a job does not pay for starting threads, and so the application can put
//! them ahead of other work: on a machine kept busy by a game, threads
//! started at ordinary priority for each job may not get to run for seconds.

use std::cell::Cell;
use std::panic::{AssertUnwindSafe, catch_unwind, resume_unwind};
use std::sync::mpsc::{Sender, channel};
use std::sync::{Mutex, OnceLock};
use std::thread::JoinHandle;

thread_local! {
    static ALONE: Cell<bool> = const { Cell::new(false) };
}

/// Called with each band thread as it is started, before it is used: the
/// application sets its priority there. The core makes no system calls.
static ON_WORKER: OnceLock<fn(&JoinHandle<()>)> = OnceLock::new();

/// Has `prepare` called on every band thread when it is started. Set it
/// before the first job; threads already started are not seen again.
pub fn on_new_worker(prepare: fn(&JoinHandle<()>)) {
    let _ = ON_WORKER.set(prepare);
}

/// A job for a band thread, with its borrows made to look `'static`; see
/// [`bands`] for why that holds.
type Job = Box<dyn FnOnce() + Send + 'static>;

/// The band threads, one fewer than the bands (the caller does one), each
/// with its own queue.
struct Pool {
    queues: Vec<Mutex<Sender<Job>>>,
}

fn pool() -> &'static Pool {
    static POOL: OnceLock<Pool> = OnceLock::new();
    POOL.get_or_init(|| {
        let queues = (1..cores())
            .filter_map(|i| {
                let (queue, jobs) = channel::<Job>();
                let worker = std::thread::Builder::new()
                    .name(format!("bruma-band-{i}"))
                    .spawn(move || {
                        // A band that starts a job of its own does it
                        // itself: waiting for the other bands from one of
                        // them could wait for ever.
                        ALONE.with(|alone| alone.set(true));
                        for job in jobs {
                            job();
                        }
                    })
                    .ok()?;
                if let Some(prepare) = ON_WORKER.get() {
                    prepare(&worker);
                }
                Some(Mutex::new(queue))
            })
            .collect();
        Pool { queues }
    })
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

/// Keeps every job the calling thread starts from now on to itself, like
/// [`on_this_thread`]: for a thread whose work can wait.
pub fn keep_jobs_on_this_thread() {
    ALONE.with(|alone| alone.set(true));
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
    let work = &work;
    let pool = pool();
    let (done, finished) = channel::<std::thread::Result<()>>();
    let mut chunks = data.chunks_mut(per_band * row_len).enumerate();
    let first = chunks.next();
    let mut sent = 0;
    for (i, band) in chunks {
        let done = done.clone();
        let job: Box<dyn FnOnce() + Send + '_> = Box::new(move || {
            let result = catch_unwind(AssertUnwindSafe(|| work(i * per_band, band)));
            let _ = done.send(result);
        });
        // SAFETY: the job borrows `work` and a band of `data`, which live
        // until this function returns. It does not return before every job
        // handed to a band thread has finished or been dropped unrun (each
        // job owns a clone of `done`, so `finished` only reports
        // disconnection once all of them are gone), and it waits even when
        // a band panics. So no job outlives what it borrows.
        let job: Job = unsafe { std::mem::transmute(job) };
        let queue = pool
            .queues
            .get(i - 1)
            .and_then(|q| q.lock().ok().map(|q| q.clone()));
        // With no thread to take it, it is done here.
        match queue {
            Some(queue) => match queue.send(job) {
                Ok(()) => sent += 1,
                Err(unsent) => (unsent.0)(),
            },
            None => job(),
        }
    }
    drop(done);
    let mine = first.map(|(i, band)| catch_unwind(AssertUnwindSafe(|| work(i * per_band, band))));
    let mut panic = None;
    for _ in 0..sent {
        match finished.recv() {
            Ok(Err(payload)) => panic = panic.or(Some(payload)),
            Ok(Ok(())) => {}
            // Every remaining job was dropped unrun.
            Err(_) => break,
        }
    }
    if let Some(Err(payload)) = mine {
        resume_unwind(payload);
    }
    if let Some(payload) = panic {
        resume_unwind(payload);
    }
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
    fn a_band_that_panics_is_reported_and_the_threads_carry_on() {
        let mut data = vec![0u8; WORTH_IT * 4];
        let len = data.len();
        let caught = std::panic::catch_unwind(AssertUnwindSafe(|| {
            bands(&mut data, 1024, |first, _| {
                if first > 0 {
                    panic!("band at {first}");
                }
            })
        }));
        assert!(caught.is_err());
        // The same threads still do the next job, every row of it.
        let mut again = vec![0u8; len];
        bands(&mut again, 1024, |_, band| band.fill(1));
        assert!(again.iter().all(|&v| v == 1));
    }

    #[test]
    fn jobs_from_two_threads_at_once_and_jobs_inside_jobs_finish() {
        let job = || {
            let mut data = vec![0u32; WORTH_IT * 2];
            bands(&mut data, 512, |_, band| {
                let mut inner = vec![0u32; WORTH_IT * 2];
                bands(&mut inner, 512, |_, b| b.fill(2));
                band.fill(inner[0]);
            });
            assert!(data.iter().all(|&v| v == 2));
        };
        std::thread::scope(|scope| {
            for _ in 0..3 {
                scope.spawn(job);
            }
        });
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
