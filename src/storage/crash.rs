//! Simulated power loss, for the crash fuzzer.
//!
//! # Why not just kill the process
//!
//! `SIGKILL` is the obvious way to simulate a crash and it does not test what
//! needs testing. A killed process loses its own buffers, but every `write`
//! that already reached the operating system is still in the page cache, and
//! the kernel writes it out afterwards regardless. So the data survives, the
//! WAL rule is never put under any pressure, and the test passes whether or not
//! the rule is even there.
//!
//! Power loss is different: **only what was `fsync`ed survives.** That is the
//! condition the whole write-ahead log exists to handle, so that is what this
//! models. Writes are held back until a sync makes them durable, and a crash
//! discards whatever had not reached that point.
//!
//! The crash lands on the *n*-th sync, and at that sync only a prefix of what
//! was pending actually lands — which also produces the torn log tail that the
//! per-record checksums are there to catch.
//!
//! What this does **not** model is a page write torn in the middle. Data pages
//! carry no checksum, so a half-written page would be undetectable; real
//! databases handle it by writing full page images after a checkpoint. That is
//! recorded as future work in `docs/en/05-wal-recovery.md` rather than quietly
//! left out.

use std::cell::RefCell;
use std::rc::Rc;

/// Shared between the pager and the log, which have to agree on when the crash
/// happened.
pub type CrashHandle = Rc<RefCell<CrashSim>>;

/// Counts sync points and cuts the power at one of them.
#[derive(Debug)]
pub struct CrashSim {
    seed: u64,
    syncs: u64,
    crash_at: u64,
    crashed: bool,
}

impl CrashSim {
    /// Arms a simulation that cuts power at the `crash_at`-th sync.
    ///
    /// A `crash_at` beyond the number of syncs a workload performs simply never
    /// fires, which is how the harness counts sync points before sweeping them.
    pub fn arm(crash_at: u64, seed: u64) -> CrashHandle {
        Rc::new(RefCell::new(CrashSim {
            seed,
            syncs: 0,
            crash_at,
            crashed: false,
        }))
    }

    /// Whether the power has been cut.
    pub fn crashed(&self) -> bool {
        self.crashed
    }

    /// How many sync points have been reached.
    pub fn syncs(&self) -> u64 {
        self.syncs
    }

    /// Called at every sync point, with how much is waiting to be made durable.
    ///
    /// Returns how much of it actually lands. `None` once the power is gone:
    /// from then on nothing more reaches the device, whatever the process
    /// believes it is doing.
    pub fn admit(&mut self, pending: usize) -> Option<usize> {
        if self.crashed {
            return None;
        }
        self.syncs += 1;
        if self.syncs != self.crash_at {
            return Some(pending);
        }

        self.crashed = true;
        if pending == 0 {
            return Some(0);
        }
        // A deterministic fraction, so a failing seed replays exactly.
        let roll = scramble(self.seed ^ self.syncs) % (pending as u64 + 1);
        Some(roll as usize)
    }
}

fn scramble(mut value: u64) -> u64 {
    value ^= value << 13;
    value ^= value >> 7;
    value ^= value << 17;
    value
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn everything_lands_until_the_crash_point() {
        let sim = CrashSim::arm(3, 7);
        assert_eq!(sim.borrow_mut().admit(100), Some(100));
        assert_eq!(sim.borrow_mut().admit(100), Some(100));
        assert!(!sim.borrow().crashed());

        let partial = sim.borrow_mut().admit(100).unwrap();
        assert!(partial <= 100, "the crash sync lands at most everything");
        assert!(sim.borrow().crashed());

        assert_eq!(sim.borrow_mut().admit(100), None, "the power is gone");
        assert_eq!(sim.borrow_mut().admit(100), None);
    }

    #[test]
    fn a_crash_point_beyond_the_workload_never_fires() {
        let sim = CrashSim::arm(u64::MAX, 1);
        for _ in 0..50 {
            assert_eq!(sim.borrow_mut().admit(10), Some(10));
        }
        assert!(!sim.borrow().crashed());
        assert_eq!(sim.borrow().syncs(), 50);
    }

    #[test]
    fn the_same_seed_tears_at_the_same_place() {
        let tear = || {
            let sim = CrashSim::arm(2, 12345);
            sim.borrow_mut().admit(500);
            let torn = sim.borrow_mut().admit(500).unwrap();
            torn
        };
        let (first, second) = (tear(), tear());
        assert_eq!(first, second, "a failing seed has to replay exactly");
    }
}
