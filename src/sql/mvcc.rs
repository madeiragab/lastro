//! Multiversion concurrency control: who sees what.
//!
//! The whole idea in one sentence: **a write never overwrites, it creates a new
//! version.** A reader then never waits for a writer, because it reads the
//! version that was current when it started rather than whatever is current now.
//!
//! Every version of a row carries two transaction ids. `xmin` is the
//! transaction that created it; `xmax` is the one that removed or replaced it,
//! or zero while it is still live. `INSERT` writes a version, `DELETE` stamps
//! `xmax` on the visible one and leaves the bytes where they are, and `UPDATE`
//! is the two together.
//!
//! See `docs/en/07-mvcc.md`.
//!
//! # What "committed" means here
//!
//! The specification's visibility rule asks whether the transaction that wrote a
//! version had committed. Answering that in general needs a table of commit
//! statuses. It does not need one here, and the reason is worth stating: the
//! engine takes one writer at a time, and a transaction that rolls back has its
//! versions physically undone by the log rather than left behind as garbage. So
//! any version still on disk was written either by a transaction that committed
//! or by the one write transaction currently open — and the snapshot already
//! knows which that is.
//!
//! That is a real simplification bought by the single-writer choice in
//! `docs/en/adr.md`, ADR-003, and it stops being sound the moment a second
//! writer exists.

use crate::TxId;

/// A version's two transaction ids, as stored ahead of the row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Version {
    /// The transaction that created this version.
    pub xmin: TxId,
    /// The transaction that removed or replaced it, or zero while it is live.
    pub xmax: TxId,
}

impl Version {
    /// A freshly written version, not yet removed by anybody.
    pub fn created_by(xmin: TxId) -> Version {
        Version { xmin, xmax: 0 }
    }
}

/// A photograph of the concurrency state, taken when a transaction begins.
///
/// Immutable for as long as it is held, which is what gives repeatable read for
/// nothing: the same query run twice under one snapshot returns the same rows,
/// however much committed in between.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Snapshot {
    /// The reader's own transaction. It sees its own work before committing,
    /// which is what makes an `INSERT` followed by a `SELECT` return the row.
    pub own: Option<TxId>,
    /// Transactions numbered at or above this had not started yet, so nothing
    /// they did can be seen.
    pub xmax: TxId,
    /// The write transaction that was open when the photograph was taken. Its
    /// work is invisible to everybody but itself.
    pub active: Option<TxId>,
}

impl Snapshot {
    /// A snapshot that sees everything, for paths where versioning does not
    /// apply — building an index over a table, say, or vacuuming it.
    pub fn all() -> Snapshot {
        Snapshot {
            own: None,
            xmax: TxId::MAX,
            active: None,
        }
    }

    /// Whether a version is the one this snapshot should read.
    ///
    /// In plain terms: **visible if whoever created it had finished when I
    /// started, and whoever removed it had not.**
    pub fn sees(&self, version: Version) -> bool {
        self.finished(version.xmin) && !(version.xmax != 0 && self.finished(version.xmax))
    }

    /// Whether the effects of a transaction are in this snapshot.
    fn finished(&self, txid: TxId) -> bool {
        if Some(txid) == self.own {
            return true;
        }
        txid < self.xmax && Some(txid) != self.active
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A reader that began after transaction 10 finished and before 20 did.
    fn after_ten() -> Snapshot {
        Snapshot {
            own: None,
            xmax: 20,
            active: None,
        }
    }

    #[test]
    fn a_live_version_from_a_finished_transaction_is_visible() {
        assert!(after_ten().sees(Version::created_by(10)));
    }

    #[test]
    fn a_version_from_a_transaction_that_had_not_started_is_not() {
        assert!(!after_ten().sees(Version::created_by(20)));
        assert!(!after_ten().sees(Version::created_by(99)));
    }

    #[test]
    fn a_removed_version_is_visible_only_while_the_removal_is_not() {
        // Removed by 15, which had finished: gone.
        assert!(!after_ten().sees(Version { xmin: 10, xmax: 15 }));
        // Removed by 25, which had not started: still there.
        assert!(after_ten().sees(Version { xmin: 10, xmax: 25 }));
    }

    #[test]
    fn a_transaction_sees_its_own_work_before_committing() {
        let mine = Snapshot {
            own: Some(30),
            xmax: 30,
            active: Some(30),
        };
        assert!(mine.sees(Version::created_by(30)), "its own insert");
        assert!(
            !mine.sees(Version { xmin: 10, xmax: 30 }),
            "and its own delete"
        );
    }

    #[test]
    fn an_open_transaction_is_invisible_to_everybody_else() {
        let onlooker = Snapshot {
            own: None,
            xmax: 31,
            active: Some(30),
        };
        assert!(
            !onlooker.sees(Version::created_by(30)),
            "30 is still open, so its work has not happened yet"
        );
        assert!(
            onlooker.sees(Version { xmin: 10, xmax: 30 }),
            "and neither has its delete"
        );
    }

    #[test]
    fn the_whole_truth_table() {
        // Every combination of when a version was created and removed, against
        // a reader that started after 10 and before 20, with 15 open.
        let snapshot = Snapshot {
            own: None,
            xmax: 20,
            active: Some(15),
        };
        let cases = [
            //  xmin, xmax, visible
            (10, 0, true),   // created before, still live
            (10, 12, false), // created before, removed before
            (10, 15, true),  // removal is still open
            (10, 25, true),  // removal had not started
            (15, 0, false),  // creation is still open
            (25, 0, false),  // creation had not started
            (12, 18, false), // both before
        ];
        for (xmin, xmax, expected) in cases {
            assert_eq!(
                snapshot.sees(Version { xmin, xmax }),
                expected,
                "xmin {xmin}, xmax {xmax}"
            );
        }
    }

    #[test]
    fn a_snapshot_that_sees_everything_sees_live_versions() {
        assert!(Snapshot::all().sees(Version::created_by(1)));
        assert!(Snapshot::all().sees(Version::created_by(TxId::MAX - 1)));
        assert!(!Snapshot::all().sees(Version { xmin: 1, xmax: 2 }));
    }
}
