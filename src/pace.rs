//! How many threads read, decided and then decided again.
//!
//! The medium gives a first number. While the walk runs the pacer watches two
//! things it can measure without asking the kernel: how much the walk gets
//! through, and how long the reading threads wait for the consumer. A thread
//! is added on trial and kept only if it bought a fair share of what a thread
//! should; one is given back when the consumer is what everybody waits for.
//! It does not see other processes: standing aside for them is the I/O
//! priority's work, not this one's.

use std::path::PathBuf;
use std::sync::atomic::Ordering::Relaxed;
use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant};

use crate::probe::{Medium, medium_of};
use crate::walk::{Shared, ThreadUse, Threads};

/// How often the pacer looks.
const WINDOW: Duration = Duration::from_millis(100);

/// Of what one more thread would add if threads scaled perfectly, the share
/// it has to deliver to be kept. Under it the thread costs more elsewhere —
/// seeks, another reader's latency — than it returns here.
const WORTH: f64 = 0.4;

/// Above this share of the reading threads' time spent waiting for the
/// consumer, a thread is given back; below `CONSUMER_IDLE`, one may be tried.
const CONSUMER_BOUND: f64 = 0.25;
const CONSUMER_IDLE: f64 = 0.05;

/// Windows to sit still after a trial that failed: doubled each time, so a
/// walk that is at its number stops asking.
const FIRST_REST: u32 = 4;
const LONGEST_REST: u32 = 64;

/// The thread numbers a walk may use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Plan {
    pub min: usize,
    pub start: usize,
    pub max: usize,
    pub medium: Option<Medium>,
}

impl Plan {
    pub(crate) fn of(
        threads: Threads,
        known: Option<Medium>,
        roots: &[PathBuf],
        cores: usize,
    ) -> Plan {
        // Asked only when the answer is used: a statfs and a mount table a
        // walk is real money for a walk of forty files.
        let medium = || known.unwrap_or_else(|| slowest(roots));
        match threads {
            Threads::Fixed(n) => Plan {
                min: n.max(1),
                start: n.max(1),
                max: n.max(1),
                medium: known,
            },
            Threads::Static => {
                let medium = medium();
                let n = medium.walk_threads(cores);
                Plan {
                    min: n,
                    start: n,
                    max: n,
                    medium: Some(medium),
                }
            }
            Threads::Auto => {
                let medium = medium();
                let (min, start, max) = match medium {
                    // Measured: a second thread buys no wall time under a seek
                    // limit and takes the disk from whoever else wants it.
                    Medium::Spinning => (1, 1, 1),
                    // Round trips hide behind each other, up to a link nobody
                    // here can see the width of.
                    Medium::Network => (1, 4, 8),
                    Medium::Solid | Medium::Memory | Medium::Unknown => {
                        (1, 2, medium.walk_threads(cores))
                    }
                };
                Plan {
                    min,
                    start,
                    max,
                    medium: Some(medium),
                }
            }
            Threads::Range { min, max } => {
                let medium = medium();
                let min = min.max(1);
                let max = max.max(min);
                Plan {
                    min,
                    start: medium.walk_threads(cores).min(2).clamp(min, max),
                    max,
                    medium: Some(medium),
                }
            }
        }
    }

    pub(crate) fn adaptive(&self) -> bool {
        self.min < self.max
    }
}

/// The slowest medium under any root, by how many threads each is worth.
fn slowest(roots: &[PathBuf]) -> Medium {
    roots
        .iter()
        .map(|root| medium_of(root))
        .min_by_key(|medium| medium.walk_threads(64))
        .unwrap_or(Medium::Unknown)
}

/// What one window showed.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Sample {
    /// Entries a second.
    pub rate: f64,
    /// The share of the reading threads' time spent waiting for the consumer.
    pub waiting: f64,
}

/// The decision, apart from the clock and the threads so it can be tested
/// with numbers.
#[derive(Debug)]
pub(crate) struct Pace {
    plan: Plan,
    limit: usize,
    /// The smoothed rate while nothing is being tried.
    steady: Option<f64>,
    /// A thread is on trial: the rate before it, and whether the window that
    /// holds the change itself has gone by.
    trial: Option<(f64, bool)>,
    rest: u32,
    next_rest: u32,
    used: ThreadUse,
}

impl Pace {
    pub(crate) fn new(plan: Plan) -> Pace {
        Pace {
            plan,
            limit: plan.start,
            steady: None,
            trial: None,
            rest: 1,
            next_rest: FIRST_REST,
            used: ThreadUse {
                first: plan.start,
                least: plan.start,
                most: plan.start,
                last: plan.start,
                changes: 0,
            },
        }
    }

    /// One window's numbers in, the number of threads to read on out.
    pub(crate) fn step(&mut self, sample: Sample) -> usize {
        let before = self.limit;
        if sample.waiting > CONSUMER_BOUND && self.limit > self.plan.min {
            // Whatever was being tried, the consumer has answered it.
            self.limit -= 1;
            self.trial = None;
            self.steady = None;
            self.rest = self.next_rest;
        } else if let Some((was, settled)) = self.trial.take() {
            if !settled {
                // The first window after a change holds the change itself.
                self.trial = Some((was, true));
            } else {
                let ideal = 1.0 / (self.limit - 1) as f64;
                if sample.rate >= was * (1.0 + WORTH * ideal) {
                    self.next_rest = FIRST_REST;
                    self.steady = Some(sample.rate);
                    // It scaled, so ask again at once: a walk of two seconds
                    // has no time to sit between questions.
                    if self.limit < self.plan.max && sample.waiting < CONSUMER_IDLE {
                        self.trial = Some((sample.rate, false));
                        self.limit += 1;
                    }
                } else {
                    self.limit -= 1;
                    self.rest = self.next_rest;
                    self.next_rest = (self.next_rest * 2).min(LONGEST_REST);
                    self.steady = None;
                }
            }
        } else {
            self.steady = Some(match self.steady {
                Some(old) => (old + sample.rate) / 2.0,
                None => sample.rate,
            });
            if self.rest > 0 {
                self.rest -= 1;
            } else if self.limit < self.plan.max
                && sample.waiting < CONSUMER_IDLE
                && sample.rate > 0.0
            {
                self.trial = Some((self.steady.unwrap_or(sample.rate), false));
                self.limit += 1;
            }
        }
        if self.limit != before {
            self.used.changes += 1;
            self.used.least = self.used.least.min(self.limit);
            self.used.most = self.used.most.max(self.limit);
            self.used.last = self.limit;
        }
        self.limit
    }

    /// Watch the walk until `over` says it has ended.
    pub(crate) fn run(mut self, shared: &Shared, over: &(Mutex<bool>, Condvar)) -> ThreadUse {
        let mut at = Instant::now();
        let mut seen = shared.seen.load(Relaxed);
        let mut waited = shared.waited_ns.load(Relaxed);
        let mut done = over.0.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            let (guard, _) = over
                .1
                .wait_timeout(done, WINDOW)
                .unwrap_or_else(|e| e.into_inner());
            done = guard;
            if *done {
                return self.used;
            }
            let (now, seen_now, waited_now) = (
                Instant::now(),
                shared.seen.load(Relaxed),
                shared.waited_ns.load(Relaxed),
            );
            let span = now.duration_since(at).as_secs_f64();
            if span < WINDOW.as_secs_f64() / 2.0 {
                // Woken early for nothing; a sliver of a window is noise.
                continue;
            }
            let sample = Sample {
                rate: (seen_now - seen) as f64 / span,
                waiting: (waited_now - waited) as f64 / 1e9 / (span * self.limit as f64),
            };
            (at, seen, waited) = (now, seen_now, waited_now);
            let limit = self.limit;
            if self.step(sample) != limit {
                shared.set_limit(self.limit);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plan(min: usize, start: usize, max: usize) -> Plan {
        Plan {
            min,
            start,
            max,
            medium: None,
        }
    }

    fn free(rate: f64) -> Sample {
        Sample { rate, waiting: 0.0 }
    }

    /// Feed a pacer a world where `rate_at(threads)` is what the walk gets.
    fn settle(pace: &mut Pace, windows: usize, rate_at: impl Fn(usize) -> f64) -> usize {
        let mut limit = pace.limit;
        for _ in 0..windows {
            limit = pace.step(free(rate_at(limit)));
        }
        limit
    }

    #[test]
    fn threads_that_scale_are_kept_up_to_the_ceiling() {
        let mut pace = Pace::new(plan(1, 2, 8));
        let limit = settle(&mut pace, 60, |n| 100_000.0 * n as f64);
        assert_eq!(limit, 8);
        assert_eq!(pace.used.most, 8);
    }

    #[test]
    fn a_thread_that_buys_nothing_is_given_back_and_asked_for_less_often() {
        // A seek limit: the same throughput however many read.
        let mut pace = Pace::new(plan(1, 1, 8));
        let mut most = 0;
        let mut trials = 0;
        let mut limit = 1;
        for _ in 0..200 {
            let next = pace.step(free(50_000.0));
            trials += usize::from(next > limit);
            limit = next;
            most = most.max(limit);
        }
        assert_eq!(limit, 1);
        assert_eq!(most, 2, "never more than the one thread on trial");
        // Rests of 4, 8, 16, 32, 64 windows with a trial of two between them:
        // five trials fit in two hundred windows, and a pacer that never
        // backed off would have run thirty.
        assert!(trials <= 6, "{trials} trials");
    }

    #[test]
    fn scaling_that_stops_is_where_the_walk_stays() {
        // Four threads' worth of device, then nothing.
        let mut pace = Pace::new(plan(1, 2, 8));
        let limit = settle(&mut pace, 120, |n| 100_000.0 * n.min(4) as f64);
        assert_eq!(limit, 4);
    }

    #[test]
    fn a_consumer_that_cannot_keep_up_takes_threads_away_down_to_the_floor() {
        let mut pace = Pace::new(plan(1, 4, 8));
        let mut limit = 4;
        for _ in 0..40 {
            limit = pace.step(Sample {
                rate: 80_000.0,
                waiting: 0.6,
            });
        }
        assert_eq!(limit, 1);
    }

    #[test]
    fn a_busy_consumer_is_not_offered_another_thread() {
        let mut pace = Pace::new(plan(1, 2, 8));
        for _ in 0..40 {
            // Between the two bounds: not bad enough to give one back.
            let limit = pace.step(Sample {
                rate: 80_000.0,
                waiting: 0.15,
            });
            assert_eq!(limit, 2);
        }
    }

    #[test]
    fn a_fixed_number_asks_nothing_and_does_not_adapt() {
        let plan = Plan::of(Threads::Fixed(3), None, &[], 16);
        assert_eq!((plan.min, plan.start, plan.max), (3, 3, 3));
        assert_eq!(plan.medium, None);
        assert!(!plan.adaptive());
        assert_eq!(Plan::of(Threads::Fixed(0), None, &[], 16).start, 1);
    }

    #[test]
    fn a_spinning_disk_is_one_thread_whatever_is_asked_of_auto() {
        let plan = Plan::of(Threads::Auto, Some(Medium::Spinning), &[], 32);
        assert_eq!((plan.min, plan.start, plan.max), (1, 1, 1));
        let solid = Plan::of(Threads::Auto, Some(Medium::Solid), &[], 32);
        assert_eq!((solid.min, solid.start, solid.max), (1, 2, 8));
        let net = Plan::of(Threads::Auto, Some(Medium::Network), &[], 32);
        assert_eq!((net.min, net.start, net.max), (1, 4, 8));
    }
}
