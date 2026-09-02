//! ARIES recovery: analysis, redo, undo.
//!
//! Runs on every open where a non-empty log exists. Not optional and not
//! configurable: a database that serves queries before applying its log is a
//! database that returns wrong answers after a crash.
//!
//! See `docs/en/05-wal-recovery.md`.

use std::collections::{BinaryHeap, HashMap, HashSet};

use super::record::{PageEdit, Record, RecordBody};
use super::writer::Wal;
use crate::storage::BufferPool;
use crate::{Lsn, PageId, Result, TxId};

/// What recovery found and did. Reported so a crash leaves a trail rather than
/// a silent pause at startup.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RecoveryReport {
    /// Intact records read from the log.
    pub records_scanned: usize,
    /// Bytes discarded from the end of the log because they did not verify.
    /// Almost always the record that was being written when the power went.
    pub torn_tail_bytes: u64,
    /// Transactions that had committed and were kept.
    pub committed: usize,
    /// Transactions that had not committed and were rolled back.
    pub rolled_back: usize,
    /// Edits reapplied during redo.
    pub edits_redone: usize,
    /// Edits that were already on the page, so redo skipped them.
    pub edits_skipped: usize,
    /// Edits reversed during undo.
    pub edits_undone: usize,
}

impl RecoveryReport {
    /// True when the log had nothing to say, which is what a clean shutdown
    /// leaves behind.
    pub fn was_clean(&self) -> bool {
        self.records_scanned == 0 && self.torn_tail_bytes == 0
    }
}

/// Brings the database back to the last consistent state its log describes.
///
/// The pool must have a log attached. Afterwards the log is empty and every
/// page it described is on disk, so a second call finds nothing to do.
pub fn recover(pool: &mut BufferPool) -> Result<RecoveryReport> {
    let path = match pool.wal() {
        Some(wal) => wal.path().to_path_buf(),
        None => return Ok(RecoveryReport::default()),
    };

    let (records, intact_len) = Wal::read_all(&path)?;
    let file_len = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);

    let mut report = RecoveryReport {
        records_scanned: records.len(),
        torn_tail_bytes: file_len.saturating_sub(intact_len),
        ..RecoveryReport::default()
    };

    // The torn tail goes before anything else is written, so the compensation
    // records appended below land at the offsets their own LSNs claim.
    if report.torn_tail_bytes > 0 {
        pool.wal_mut()
            .expect("checked above")
            .truncate_to(intact_len)?;
    }

    if records.is_empty() {
        return Ok(report);
    }

    let analysis = analyse(&records);
    report.committed = analysis.committed.len();
    report.rolled_back = analysis.losers.len();

    redo(pool, &records, &mut report)?;
    undo(pool, &records, &analysis, &mut report)?;

    // Everything the log described is now on the pages. Push them out and start
    // the log over, so the next open has nothing to replay.
    super::checkpoint(pool)?;
    Ok(report)
}

/// What the analysis pass works out.
struct Analysis {
    /// Transactions with a `Commit` record. Their work stays.
    committed: HashSet<TxId>,
    /// Transactions that began and never ended. Their work is undone, each
    /// mapped to the last record it wrote.
    losers: HashMap<TxId, Lsn>,
}

/// Pass 1. Works out who won and who lost.
///
/// A transaction with a `Commit` is a winner even if none of its pages ever
/// reached disk — redo will put them there. A transaction without one is a
/// loser even if all of its pages did reach disk — undo will take them back.
fn analyse(records: &[Record]) -> Analysis {
    let mut committed = HashSet::new();
    let mut open: HashMap<TxId, Lsn> = HashMap::new();

    for record in records {
        match record.body {
            RecordBody::Begin => {
                open.insert(record.txid, record.lsn);
            }
            RecordBody::Commit => {
                committed.insert(record.txid);
                open.remove(&record.txid);
            }
            RecordBody::Abort => {
                open.remove(&record.txid);
            }
            _ => {
                if let Some(last) = open.get_mut(&record.txid) {
                    *last = record.lsn;
                }
            }
        }
    }

    Analysis {
        committed,
        losers: open,
    }
}

/// Pass 2. Repeats history.
///
/// Reapplies every edit, including those of transactions that never committed.
/// That looks wrong and is exactly what makes the algorithm simple: afterwards
/// the pages are in the state they were in at the instant of the crash, and undo
/// then has no special cases to handle.
///
/// The `page.lsn < record.lsn` test is what makes this idempotent. A page that
/// already reached disk carrying the change has the higher LSN and is skipped,
/// so recovery can run ten times over the same log without changing anything.
fn redo(pool: &mut BufferPool, records: &[Record], report: &mut RecoveryReport) -> Result<()> {
    for record in records {
        let Some(edit) = record.edit() else {
            continue;
        };
        if apply(pool, edit, &edit.after, record.lsn)? {
            report.edits_redone += 1;
        } else {
            report.edits_skipped += 1;
        }
    }
    Ok(())
}

/// Pass 3. Rolls the losers back, newest change first.
///
/// Every reversal is itself logged, as a compensation record. If the process
/// dies here and recovery starts over, redo replays those compensations and undo
/// picks up exactly where it stopped, guided by `undo_next_lsn`. Compensation
/// records are never undone, only redone.
fn undo(
    pool: &mut BufferPool,
    records: &[Record],
    analysis: &Analysis,
    report: &mut RecoveryReport,
) -> Result<()> {
    if analysis.losers.is_empty() {
        return Ok(());
    }

    let by_lsn: HashMap<Lsn, &Record> = records.iter().map(|r| (r.lsn, r)).collect();

    // A max-heap gives the records back in descending LSN order across all
    // losing transactions at once, which is what makes undo a single pass.
    let mut pending: BinaryHeap<Lsn> = analysis.losers.values().copied().collect();
    let mut seen: HashSet<Lsn> = HashSet::new();

    while let Some(lsn) = pending.pop() {
        if !seen.insert(lsn) {
            continue;
        }
        let Some(record) = by_lsn.get(&lsn) else {
            continue;
        };

        match &record.body {
            RecordBody::Begin => {
                let wal = pool.wal_mut().expect("recovery runs with a log");
                wal.append(record.txid, record.lsn, RecordBody::Abort)?;
            }
            RecordBody::Update(edit) => {
                let compensation = RecordBody::Clr {
                    undo_next_lsn: record.prev_lsn,
                    edit: PageEdit::new(
                        edit.page,
                        edit.offset,
                        edit.after.clone(),
                        edit.before.clone(),
                    ),
                };
                let wal = pool.wal_mut().expect("recovery runs with a log");
                let clr_lsn = wal.append(record.txid, record.lsn, compensation)?;

                apply(pool, edit, &edit.before, clr_lsn)?;
                report.edits_undone += 1;
                pending.push(record.prev_lsn);
            }
            RecordBody::Clr { undo_next_lsn, .. } => {
                // Already compensated. Carry on from where it points.
                pending.push(*undo_next_lsn);
            }
            _ => {
                pending.push(record.prev_lsn);
            }
        }
    }
    Ok(())
}

/// Writes `image` over the edit's range and stamps the page with `lsn`.
///
/// Returns whether the page needed it. During redo a page whose LSN already
/// covers the record is left alone.
fn apply(pool: &mut BufferPool, edit: &PageEdit, image: &[u8], lsn: Lsn) -> Result<bool> {
    grow_to_fit(pool, edit.page)?;

    let pin = pool.fetch(edit.page)?;
    if pool.page(&pin).lsn() >= lsn {
        pool.unpin(pin);
        return Ok(false);
    }

    let start = edit.offset as usize;
    let page = pool.page_mut(&pin);
    page.as_bytes_mut()[start..start + image.len()].copy_from_slice(image);
    // The LSN is stamped after the image, so an edit that happens to span the
    // header's LSN field cannot leave a stale value behind.
    page.set_lsn(lsn);
    pool.unpin(pin);
    Ok(true)
}

/// Makes sure the file is long enough to hold the page a record names.
///
/// The metadata page records how many pages exist, and it is only made durable
/// at a checkpoint. After a crash it can name fewer pages than the log does, so
/// redo would otherwise be reading past the end of the file.
fn grow_to_fit(pool: &mut BufferPool, page: PageId) -> Result<()> {
    pool.pager_mut().ensure_page_count(page + 1)
}
