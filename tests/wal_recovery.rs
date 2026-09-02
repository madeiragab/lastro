//! Crash recovery.
//!
//! A crash is simulated by dropping the pool without flushing: dirty pages in
//! memory vanish, exactly as they would if the process were killed. It is not
//! the full story — a real power loss can also tear a page mid-write — but it
//! exercises redo, undo and compensation records, which is where the algorithm
//! lives. The `SIGKILL` fuzzer that covers torn writes comes next.

use std::path::{Path, PathBuf};

use lastro::storage::page::PageType;
use lastro::storage::{BufferPool, Pager};
use lastro::wal::{recover, RecordBody, Wal};
use lastro::{Lsn, PageId, TxId, PAGE_SIZE};

/// Opens the database and its log, running recovery the way a real open would.
fn open(path: &Path, capacity: usize) -> (BufferPool, lastro::wal::RecoveryReport) {
    let pager = Pager::open_or_create(path).unwrap();
    let mut pool = BufferPool::new(pager, capacity);
    pool.attach_wal(Wal::open(Wal::path_for(path)).unwrap());
    let report = recover(&mut pool).unwrap();
    (pool, report)
}

fn temp() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("crash.lastro");
    (dir, path)
}

/// Marks a page with a byte, through the log, as part of `txid`.
fn stamp(pool: &mut BufferPool, txid: TxId, prev: Lsn, page: PageId, byte: u8) -> Lsn {
    let pin = pool.fetch(page).unwrap();
    let lsn = pool
        .logged_write(txid, prev, &pin, 64, &[byte; 32])
        .unwrap();
    pool.unpin(pin);
    lsn
}

fn read_marker(pool: &mut BufferPool, page: PageId) -> [u8; 32] {
    let pin = pool.fetch(page).unwrap();
    let mut out = [0u8; 32];
    out.copy_from_slice(&pool.page(&pin).as_bytes()[64..96]);
    pool.unpin(pin);
    out
}

/// Allocates `count` pages up front so page numbers are stable across reopens.
fn reserve(pool: &mut BufferPool, count: usize) -> Vec<PageId> {
    let mut ids = Vec::new();
    for _ in 0..count {
        let pin = pool.new_page(PageType::Heap).unwrap();
        ids.push(pin.page_id);
        pool.unpin(pin);
    }
    lastro::wal::checkpoint(pool).unwrap();
    ids
}

#[test]
fn a_clean_log_gives_recovery_nothing_to_do() {
    let (_dir, path) = temp();
    let (mut pool, report) = open(&path, 8);
    assert!(report.was_clean());

    let pages = reserve(&mut pool, 2);
    let begin = pool
        .wal_mut()
        .unwrap()
        .append(1, 0, RecordBody::Begin)
        .unwrap();
    let write = stamp(&mut pool, 1, begin, pages[0], 0xAA);
    pool.wal_mut()
        .unwrap()
        .append(1, write, RecordBody::Commit)
        .unwrap();
    lastro::wal::checkpoint(&mut pool).unwrap();
    drop(pool);

    let (mut pool, report) = open(&path, 8);
    assert!(
        report.was_clean(),
        "a checkpoint should leave nothing: {report:?}"
    );
    assert_eq!(read_marker(&mut pool, pages[0]), [0xAA; 32]);
}

#[test]
fn a_committed_change_survives_a_crash_that_lost_its_page() {
    let (_dir, path) = temp();
    let (mut pool, _) = open(&path, 8);
    let pages = reserve(&mut pool, 2);

    let begin = pool
        .wal_mut()
        .unwrap()
        .append(1, 0, RecordBody::Begin)
        .unwrap();
    let write = stamp(&mut pool, 1, begin, pages[0], 0x5A);
    pool.wal_mut()
        .unwrap()
        .append(1, write, RecordBody::Commit)
        .unwrap();
    // Commit is durable; the page is not. This is the whole point of no-force.
    pool.wal_mut().unwrap().sync().unwrap();
    drop(pool);

    let (mut pool, report) = open(&path, 8);
    assert_eq!(report.committed, 1);
    assert_eq!(report.rolled_back, 0);
    assert!(
        report.edits_redone >= 1,
        "the page had to be redone: {report:?}"
    );
    assert_eq!(read_marker(&mut pool, pages[0]), [0x5A; 32]);
}

#[test]
fn an_uncommitted_change_is_reversed_even_after_its_page_reached_disk() {
    let (_dir, path) = temp();
    // Two frames, so writing elsewhere forces the dirty page out. That is the
    // steal policy: a page belonging to a transaction that never committed can
    // and does reach the disk.
    let (mut pool, _) = open(&path, 2);
    let pages = reserve(&mut pool, 4);

    let committed = pool
        .wal_mut()
        .unwrap()
        .append(1, 0, RecordBody::Begin)
        .unwrap();
    let write = stamp(&mut pool, 1, committed, pages[0], 0x11);
    pool.wal_mut()
        .unwrap()
        .append(1, write, RecordBody::Commit)
        .unwrap();
    pool.wal_mut().unwrap().sync().unwrap();

    // A second transaction scribbles over the same page and never commits.
    let doomed = pool
        .wal_mut()
        .unwrap()
        .append(2, 0, RecordBody::Begin)
        .unwrap();
    stamp(&mut pool, 2, doomed, pages[0], 0x22);
    pool.wal_mut().unwrap().sync().unwrap();

    // Touch the other pages until the dirty one is evicted and written out.
    for page in &pages[1..] {
        let pin = pool.fetch(*page).unwrap();
        pool.unpin(pin);
    }
    drop(pool);

    let (mut pool, report) = open(&path, 8);
    assert_eq!(report.committed, 1);
    assert_eq!(report.rolled_back, 1);
    assert!(
        report.edits_undone >= 1,
        "the scribble had to be undone: {report:?}"
    );
    assert_eq!(
        read_marker(&mut pool, pages[0]),
        [0x11; 32],
        "the committed value must be what survives"
    );
}

#[test]
fn recovery_run_again_changes_nothing() {
    let (_dir, path) = temp();
    let (mut pool, _) = open(&path, 4);
    let pages = reserve(&mut pool, 3);

    let begin = pool
        .wal_mut()
        .unwrap()
        .append(1, 0, RecordBody::Begin)
        .unwrap();
    let mut prev = begin;
    for (index, page) in pages.iter().enumerate() {
        prev = stamp(&mut pool, 1, prev, *page, 0x70 + index as u8);
    }
    pool.wal_mut()
        .unwrap()
        .append(1, prev, RecordBody::Commit)
        .unwrap();
    pool.wal_mut().unwrap().sync().unwrap();

    let doomed = pool
        .wal_mut()
        .unwrap()
        .append(2, 0, RecordBody::Begin)
        .unwrap();
    stamp(&mut pool, 2, doomed, pages[0], 0xFF);
    pool.wal_mut().unwrap().sync().unwrap();
    drop(pool);

    let expected = {
        let (mut pool, report) = open(&path, 4);
        assert!(report.records_scanned > 0);
        let state: Vec<[u8; 32]> = pages.iter().map(|p| read_marker(&mut pool, *p)).collect();
        drop(pool);
        state
    };

    // Ten more opens. Redo is idempotent and the log is empty after the first,
    // so every one of them must land on exactly the same bytes.
    for round in 0..10 {
        let (mut pool, report) = open(&path, 4);
        assert!(
            report.was_clean(),
            "round {round} still had a log: {report:?}"
        );
        let state: Vec<[u8; 32]> = pages.iter().map(|p| read_marker(&mut pool, *p)).collect();
        assert_eq!(state, expected, "round {round} diverged");
    }
}

#[test]
fn a_crash_during_rollback_is_finished_by_the_next_recovery() {
    let (_dir, path) = temp();
    let (mut pool, _) = open(&path, 8);
    let pages = reserve(&mut pool, 3);

    // Three changes by a transaction that never commits.
    let begin = pool
        .wal_mut()
        .unwrap()
        .append(3, 0, RecordBody::Begin)
        .unwrap();
    let mut prev = begin;
    for page in &pages {
        prev = stamp(&mut pool, 3, prev, *page, 0xEE);
    }
    pool.wal_mut().unwrap().sync().unwrap();
    drop(pool);

    // Recovery rolls it back, and its compensation records land in the log.
    let (pool, report) = open(&path, 8);
    assert_eq!(report.rolled_back, 1);
    assert_eq!(report.edits_undone, 3);
    drop(pool);

    // Reopening again must find nothing left to do and leave the pages blank,
    // which is only true if every reversal actually stuck.
    let (mut pool, report) = open(&path, 8);
    assert!(report.was_clean(), "rollback did not finish: {report:?}");
    for page in &pages {
        assert_eq!(read_marker(&mut pool, *page), [0u8; 32]);
    }
}

#[test]
fn a_torn_tail_is_discarded_wherever_it_is_cut() {
    let (_dir, path) = temp();
    let log_path = Wal::path_for(&path);

    let (mut pool, _) = open(&path, 8);
    let pages = reserve(&mut pool, 2);
    let begin = pool
        .wal_mut()
        .unwrap()
        .append(1, 0, RecordBody::Begin)
        .unwrap();
    let write = stamp(&mut pool, 1, begin, pages[0], 0x3C);
    pool.wal_mut()
        .unwrap()
        .append(1, write, RecordBody::Commit)
        .unwrap();
    pool.wal_mut().unwrap().sync().unwrap();
    drop(pool);

    let whole = std::fs::read(&log_path).unwrap();
    let pristine = std::fs::read(&path).unwrap();

    // Cut the log at every byte. Each cut must open cleanly: either the commit
    // survived and the page shows the value, or it did not and the page is
    // blank. Never an error, and never anything in between.
    for cut in 0..whole.len() {
        std::fs::write(&path, &pristine).unwrap();
        std::fs::write(&log_path, &whole[..cut]).unwrap();

        let (mut pool, _) = open(&path, 8);
        let marker = read_marker(&mut pool, pages[0]);
        assert!(
            marker == [0x3C; 32] || marker == [0u8; 32],
            "cut at {cut} left a half-applied page: {marker:?}"
        );
        drop(pool);
    }
}

#[test]
fn the_wal_rule_holds_on_every_eviction() {
    let (_dir, path) = temp();
    // One frame: every write evicts the previous page, so the rule is exercised
    // on every single operation rather than occasionally.
    let (mut pool, _) = open(&path, 1);

    let mut ids = Vec::new();
    for _ in 0..12 {
        let pin = pool.new_page(PageType::Heap).unwrap();
        ids.push(pin.page_id);
        pool.unpin(pin);
    }

    let begin = pool
        .wal_mut()
        .unwrap()
        .append(1, 0, RecordBody::Begin)
        .unwrap();
    let mut prev = begin;
    for (index, page) in ids.iter().enumerate() {
        prev = stamp(&mut pool, 1, prev, *page, index as u8);

        // Whatever has been written to the data file so far, the log covering
        // it is already on the medium.
        let pin = pool.fetch(*page).unwrap();
        let page_lsn = pool.page(&pin).lsn();
        pool.unpin(pin);
        assert!(
            pool.wal().unwrap().durable_lsn() > page_lsn
                || pool.wal().unwrap().end_lsn() > page_lsn,
            "a page was stamped with an lsn the log has not reached"
        );
    }

    assert!(
        pool.wal().unwrap().stats().syncs > 0,
        "evicting dirty pages through a one-frame pool must have forced syncs"
    );
}

#[test]
fn a_logged_replace_records_only_what_moved() {
    let (_dir, path) = temp();
    let (mut pool, _) = open(&path, 8);
    let page = reserve(&mut pool, 1)[0];

    let pin = pool.fetch(page).unwrap();
    let mut image = pool.page(&pin).clone();
    image.as_bytes_mut()[1000] = 0xAB;
    image.as_bytes_mut()[1003] = 0xCD;

    let before = pool.wal().unwrap().end_lsn();
    pool.logged_replace(1, 0, &pin, &image).unwrap().unwrap();
    let after = pool.wal().unwrap().end_lsn();
    pool.unpin(pin);

    // Four changed bytes, logged twice for the two images, plus the record
    // header and the edit header. Nothing close to a whole page, which is the
    // difference between physiological logging and physical.
    let cost = after - before;
    assert!(
        cost < 100,
        "a four byte change cost {cost} bytes of log, against a {PAGE_SIZE} byte page"
    );
}
