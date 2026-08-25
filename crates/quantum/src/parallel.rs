//! A small ordered worker pool.
//!
//! Blocks are independent, so they compress and decompress in parallel; but
//! they must be written and consumed in their original order. This pipeline
//! does both, with a bounded job queue so a fast producer cannot pull the
//! whole input into memory.

use std::collections::BTreeMap;
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use crate::error::{Error, Result};

pub struct Pipeline<J: Send + 'static, R: Send + 'static> {
    jobs: Option<SyncSender<(usize, J)>>,
    results: Receiver<(usize, R)>,
    workers: Vec<JoinHandle<()>>,
    submitted: usize,
    delivered: usize,
    pending: BTreeMap<usize, R>,
}

impl<J: Send + 'static, R: Send + 'static> Pipeline<J, R> {
    pub fn new<F>(threads: usize, work: F) -> Self
    where
        F: Fn(J) -> R + Send + Sync + 'static,
    {
        let threads = threads.max(1);
        // One slot per worker plus one in hand keeps every worker busy while
        // capping how much unprocessed input is resident.
        let (job_tx, job_rx) = sync_channel::<(usize, J)>(threads);
        let (res_tx, res_rx) = std::sync::mpsc::channel::<(usize, R)>();
        let job_rx = Arc::new(Mutex::new(job_rx));
        let work = Arc::new(work);

        let workers = (0..threads)
            .map(|_| {
                let job_rx = Arc::clone(&job_rx);
                let res_tx = res_tx.clone();
                let work = Arc::clone(&work);
                std::thread::spawn(move || {
                    loop {
                        // Hold the lock only long enough to take a job.
                        let job = { job_rx.lock().unwrap().recv() };
                        match job {
                            Ok((index, job)) => {
                                if res_tx.send((index, work(job))).is_err() {
                                    return;
                                }
                            }
                            Err(_) => return,
                        }
                    }
                })
            })
            .collect();

        Pipeline {
            jobs: Some(job_tx),
            results: res_rx,
            workers,
            submitted: 0,
            delivered: 0,
            pending: BTreeMap::new(),
        }
    }

    /// Queue a job and return every result that is now ready, in order.
    ///
    /// Blocks while the queue is full, which is the backpressure that bounds
    /// memory use.
    pub fn submit(&mut self, job: J) -> Result<Vec<R>> {
        let index = self.submitted;
        self.submitted += 1;
        self.jobs
            .as_ref()
            .expect("submit after finish")
            .send((index, job))
            .map_err(|_| Error::Corrupt("worker thread stopped unexpectedly"))?;
        // Drain whatever has landed without waiting for more.
        while let Ok((i, r)) = self.results.try_recv() {
            self.pending.insert(i, r);
        }
        Ok(self.take_ready())
    }

    /// Wait for every outstanding job and return the remaining results in
    /// order.
    pub fn finish(mut self) -> Result<Vec<R>> {
        drop(self.jobs.take());
        // `take_ready` already advances `delivered`, so it alone says how much
        // is still outstanding.
        let mut out = self.take_ready();
        while self.delivered < self.submitted {
            match self.results.recv() {
                Ok((i, r)) => {
                    self.pending.insert(i, r);
                    out.extend(self.take_ready());
                }
                Err(_) => return Err(Error::Corrupt("worker thread stopped unexpectedly")),
            }
        }
        Ok(out)
    }

    /// Wait for the next result in submission order, or `None` when nothing
    /// is outstanding. Used by consumers that pull rather than push.
    pub fn drain_one(&mut self) -> Result<Option<R>> {
        loop {
            if let Some(r) = self.pending.remove(&self.delivered) {
                self.delivered += 1;
                return Ok(Some(r));
            }
            if self.delivered >= self.submitted {
                return Ok(None);
            }
            match self.results.recv() {
                Ok((i, r)) => {
                    self.pending.insert(i, r);
                }
                Err(_) => return Err(Error::Corrupt("worker thread stopped unexpectedly")),
            }
        }
    }

    fn take_ready(&mut self) -> Vec<R> {
        let mut out = Vec::new();
        while let Some(r) = self.pending.remove(&self.delivered) {
            out.push(r);
            self.delivered += 1;
        }
        out
    }
}

impl<J: Send + 'static, R: Send + 'static> Drop for Pipeline<J, R> {
    fn drop(&mut self) {
        // Closing the job queue is what tells the workers to stop; without it
        // an abandoned pipeline would leave threads parked forever.
        drop(self.jobs.take());
        for w in self.workers.drain(..) {
            let _ = w.join();
        }
    }
}

/// Number of worker threads to use by default.
pub fn default_threads() -> usize {
    std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn results_come_back_in_submission_order() {
        for threads in [1usize, 2, 4, 8] {
            let mut pipe = Pipeline::new(threads, |x: u64| {
                // Uneven work, so completion order differs from submission.
                let mut acc = x;
                for _ in 0..(x % 7) * 2000 {
                    acc = acc.wrapping_mul(6364136223846793005).wrapping_add(1);
                }
                (x, acc)
            });
            let mut got = Vec::new();
            for i in 0..200u64 {
                got.extend(pipe.submit(i).unwrap());
            }
            got.extend(pipe.finish().unwrap());
            let order: Vec<u64> = got.iter().map(|(x, _)| *x).collect();
            assert_eq!(order, (0..200).collect::<Vec<_>>(), "threads = {threads}");
        }
    }

    #[test]
    fn jobs_really_do_run_concurrently() {
        // Asserting a wall-clock speedup would be flaky wherever CPU is
        // scarce. Instead, each job announces itself and then waits for all
        // the others: that can only complete if the pool genuinely runs
        // `threads` jobs at the same time, however few cores there are.
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::{Arc, Condvar, Mutex as StdMutex};

        const THREADS: usize = 4;
        let arrived = Arc::new(AtomicUsize::new(0));
        let gate = Arc::new((StdMutex::new(false), Condvar::new()));
        let (a, g) = (Arc::clone(&arrived), Arc::clone(&gate));

        let mut pipe = Pipeline::new(THREADS, move |x: usize| {
            if a.fetch_add(1, Ordering::SeqCst) + 1 == THREADS {
                let (lock, cv) = &*g;
                *lock.lock().unwrap() = true;
                cv.notify_all();
            }
            let (lock, cv) = &*g;
            let mut open = lock.lock().unwrap();
            while !*open {
                let (guard, timeout) =
                    cv.wait_timeout(open, std::time::Duration::from_secs(20)).unwrap();
                open = guard;
                if timeout.timed_out() {
                    break;
                }
            }
            (x, *open)
        });

        for i in 0..THREADS {
            pipe.submit(i).unwrap();
        }
        let results = pipe.finish().unwrap();
        assert_eq!(results.len(), THREADS);
        for (i, (x, all_arrived)) in results.iter().enumerate() {
            assert_eq!(*x, i);
            assert!(all_arrived, "job {i} gave up waiting: the pool runs jobs one at a time");
        }
    }

    #[test]
    fn empty_pipeline_finishes() {
        let pipe: Pipeline<u32, u32> = Pipeline::new(4, |x| x);
        assert!(pipe.finish().unwrap().is_empty());
    }

    #[test]
    fn backpressure_bounds_in_flight_work() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static LIVE: AtomicUsize = AtomicUsize::new(0);
        static PEAK: AtomicUsize = AtomicUsize::new(0);
        let mut pipe = Pipeline::new(2, |x: usize| {
            let live = LIVE.fetch_add(1, Ordering::SeqCst) + 1;
            PEAK.fetch_max(live, Ordering::SeqCst);
            std::thread::yield_now();
            LIVE.fetch_sub(1, Ordering::SeqCst);
            x
        });
        for i in 0..500 {
            pipe.submit(i).unwrap();
        }
        pipe.finish().unwrap();
        assert!(PEAK.load(Ordering::SeqCst) <= 2, "more workers ran than exist");
    }
}
