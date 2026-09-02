//! The buffer pool: a bounded cache of pages with a clock replacement policy.
//!
//! The useful lie it tells the layers above is that every page is in memory.
//! See `docs/en/03-pager.md`.
//!
//! # A note on pins
//!
//! The specification describes a `PageGuard` whose `Drop` releases the pin. That
//! needs interior mutability in the pool, which arrives with the WAL layer.
//! Until then a pin is a [`PinnedPage`] token that must be handed back to
//! [`BufferPool::unpin`]. The token is not `Copy` and not `Clone`, so it cannot
//! be duplicated by accident, and [`BufferPool::check_invariants`] fails loudly
//! on any pin still outstanding at the end of an operation.

use std::collections::HashMap;

use crate::storage::crash::CrashHandle;
use crate::storage::page::{Page, PageType};
use crate::storage::pager::Pager;
use crate::wal::record::{PageEdit, RecordBody};
use crate::wal::Wal;
use crate::{Error, Lsn, PageId, Result, TxId};

/// Two differing stretches closer than this are logged as one record rather
/// than two. A record costs a 36 byte header plus an 8 byte edit header, so
/// splitting to save fewer bytes than that would lose.
const EDIT_COALESCE_GAP: usize = 48;

/// The transaction currently open on a pool. Single-writer, so there is at
/// most one.
#[derive(Debug)]
struct ActiveTxn {
    txid: TxId,
    /// The last record this transaction wrote, which the next one chains to.
    last_lsn: Lsn,
    /// Every edit it has made, newest last, so rollback can walk backwards
    /// without going back to the log for records that may still be buffered.
    edits: Vec<(Lsn, Lsn, PageEdit)>,
    /// Pages it gave up. Held back rather than freed on the spot; see
    /// [`BufferPool::free_page`].
    freed: Vec<PageId>,
}

/// A pin on a cached page. Hand it back to [`BufferPool::unpin`] when done.
///
/// Deliberately neither `Copy` nor `Clone`: every pin must be released exactly
/// once, and duplicating the token would make that impossible to audit.
#[derive(Debug)]
pub struct PinnedPage {
    /// Which page this pin refers to.
    pub page_id: PageId,
    frame: usize,
}

#[derive(Debug)]
struct Frame {
    page: Page,
    page_id: Option<PageId>,
    pin_count: u32,
    dirty: bool,
    ref_bit: bool,
}

impl Frame {
    fn empty() -> Frame {
        Frame {
            page: Page::zeroed(),
            page_id: None,
            pin_count: 0,
            dirty: false,
            ref_bit: false,
        }
    }
}

/// A fixed-size cache of pages in front of a [`Pager`].
#[derive(Debug)]
pub struct BufferPool {
    frames: Vec<Frame>,
    table: HashMap<PageId, usize>,
    clock_hand: usize,
    pager: Pager,
    wal: Option<Wal>,
    txn: Option<ActiveTxn>,
    /// Pages given up by committed transactions, waiting for a checkpoint to
    /// actually put them on the freelist. See [`BufferPool::free_page`].
    pending_frees: Vec<PageId>,
    /// Pages touched by the operation in flight, with their images from before
    /// it started. See [`BufferPool::begin_edit`].
    edit: Option<HashMap<PageId, (usize, Box<Page>)>>,
}

impl BufferPool {
    /// Builds a pool of `capacity` frames in front of `pager`.
    ///
    /// The frames are allocated once here and never grow. A pool that allocated
    /// on demand would defeat its own purpose, which is bounding memory use.
    pub fn new(pager: Pager, capacity: usize) -> BufferPool {
        let capacity = capacity.max(1);
        let frames: Vec<Frame> = (0..capacity).map(|_| Frame::empty()).collect();
        BufferPool {
            frames,
            table: HashMap::with_capacity(capacity),
            clock_hand: 0,
            pager,
            wal: None,
            txn: None,
            pending_frees: Vec::new(),
            edit: None,
        }
    }

    /// How many frames the pool has.
    pub fn capacity(&self) -> usize {
        self.frames.len()
    }

    /// Attaches a log. From here on no dirty page reaches disk before the
    /// record describing it does.
    pub fn attach_wal(&mut self, wal: Wal) {
        self.wal = Some(wal);
    }

    /// Arms a simulated power loss across the pager and the log at once.
    ///
    /// They have to share one simulation: the whole question the crash fuzzer
    /// asks is whether the log reached the disk before the page it describes,
    /// and that is only meaningful if both are cut off at the same instant.
    pub fn arm_crash_sim(&mut self, sim: CrashHandle) {
        self.pager.arm_crash_sim(sim.clone());
        if let Some(wal) = self.wal.as_mut() {
            wal.arm_crash_sim(sim);
        }
    }

    /// Whether the simulated power has been cut.
    pub fn crashed(&self) -> bool {
        self.pager.crashed()
    }

    /// The attached log, if there is one.
    pub fn wal(&self) -> Option<&Wal> {
        self.wal.as_ref()
    }

    /// The attached log, mutable.
    pub fn wal_mut(&mut self) -> Option<&mut Wal> {
        self.wal.as_mut()
    }

    /// Logs a change to a range of bytes in a pinned page, then applies it.
    ///
    /// The before image is read off the page as it stands, which is what lets
    /// undo reverse the change later. The page is stamped with the record's LSN
    /// *after* the bytes go in, so an edit that happens to span the header's own
    /// LSN field cannot leave a stale value behind.
    pub fn logged_write(
        &mut self,
        txid: TxId,
        prev_lsn: Lsn,
        pin: &PinnedPage,
        offset: usize,
        after: &[u8],
    ) -> Result<Lsn> {
        let before = self.frames[pin.frame].page.as_bytes()[offset..offset + after.len()].to_vec();
        let edit = PageEdit::new(pin.page_id, offset as u16, before, after.to_vec());

        let wal = self.wal.as_mut().ok_or(Error::NoLogAttached)?;
        let lsn = wal.append(txid, prev_lsn, RecordBody::Update(edit))?;

        let page = self.page_mut(pin);
        page.as_bytes_mut()[offset..offset + after.len()].copy_from_slice(after);
        page.set_lsn(lsn);
        Ok(lsn)
    }

    /// Replaces a pinned page with `image`, logging only the bytes that moved.
    ///
    /// This is what keeps whole-page rewrites from costing whole-page log
    /// records, and so what keeps the logging physiological rather than
    /// physical. Returns `None` when the image is identical and nothing was
    /// logged.
    pub fn logged_replace(
        &mut self,
        txid: TxId,
        prev_lsn: Lsn,
        pin: &PinnedPage,
        image: &Page,
    ) -> Result<Option<Lsn>> {
        let edit = PageEdit::between(
            pin.page_id,
            self.frames[pin.frame].page.as_bytes(),
            image.as_bytes(),
        );
        let Some(edit) = edit else {
            return Ok(None);
        };

        let wal = self.wal.as_mut().ok_or(Error::NoLogAttached)?;
        let lsn = wal.append(txid, prev_lsn, RecordBody::Update(edit))?;

        let page = self.page_mut(pin);
        *page = image.clone();
        page.set_lsn(lsn);
        Ok(Some(lsn))
    }

    /// The pager underneath.
    pub fn pager(&self) -> &Pager {
        &self.pager
    }

    /// The pager underneath, mutable. Bypassing the pool for page reads and
    /// writes will desynchronize the cache; use it for metadata only.
    pub fn pager_mut(&mut self) -> &mut Pager {
        &mut self.pager
    }

    /// Pins page `id`, reading it from disk if it is not already cached.
    pub fn fetch(&mut self, id: PageId) -> Result<PinnedPage> {
        if let Some(&frame) = self.table.get(&id) {
            self.frames[frame].pin_count += 1;
            self.frames[frame].ref_bit = true;
            return Ok(PinnedPage { page_id: id, frame });
        }

        let frame = self.acquire_frame()?;
        self.pager.read_page(id, &mut self.frames[frame].page)?;

        let slot = &mut self.frames[frame];
        slot.page_id = Some(id);
        slot.pin_count = 1;
        slot.dirty = false;
        slot.ref_bit = true;
        self.table.insert(id, frame);
        Ok(PinnedPage { page_id: id, frame })
    }

    /// Allocates a fresh page, initializes it to `page_type`, and pins it.
    pub fn new_page(&mut self, page_type: PageType) -> Result<PinnedPage> {
        let id = self.pager.allocate()?;

        // The page may still be cached from before it was freed, in which case
        // that frame must be reused rather than a second copy created.
        let frame = match self.table.get(&id) {
            Some(&existing) => existing,
            None => {
                let fresh = self.acquire_frame()?;
                self.table.insert(id, fresh);
                fresh
            }
        };

        // Recorded before the initialization, so that the initialization
        // itself is logged. Without this a page allocated inside a transaction
        // comes back blank after a crash, and the tree finds a hole where a
        // node should be.
        self.record_for_edit(id, frame);

        let slot = &mut self.frames[frame];
        slot.page.init(page_type);
        slot.page_id = Some(id);
        slot.pin_count += 1;
        slot.dirty = true;
        slot.ref_bit = true;
        Ok(PinnedPage { page_id: id, frame })
    }

    /// Reads a pinned page.
    pub fn page(&self, pin: &PinnedPage) -> &Page {
        &self.frames[pin.frame].page
    }

    /// Reads and writes a pinned page, marking it dirty.
    ///
    /// Inside an edit session the first call on a page also snapshots it and
    /// takes a pin, so that the change can be logged when the session closes
    /// and so that the page cannot be evicted before then.
    pub fn page_mut(&mut self, pin: &PinnedPage) -> &mut Page {
        self.record_for_edit(pin.page_id, pin.frame);
        let slot = &mut self.frames[pin.frame];
        slot.dirty = true;
        &mut slot.page
    }

    /// Snapshots a page the first time an operation touches it, and pins it
    /// until the session closes. See [`BufferPool::begin_edit`].
    fn record_for_edit(&mut self, page_id: PageId, frame: usize) {
        let Some(edit) = self.edit.as_mut() else {
            return;
        };
        if edit.contains_key(&page_id) {
            return;
        }
        let before = Box::new(self.frames[frame].page.clone());
        edit.insert(page_id, (frame, before));
        self.frames[frame].pin_count += 1;
    }

    /// Releases a pin.
    pub fn unpin(&mut self, pin: PinnedPage) {
        let slot = &mut self.frames[pin.frame];
        debug_assert!(slot.pin_count > 0, "unpinning a frame that is not pinned");
        slot.pin_count = slot.pin_count.saturating_sub(1);
    }

    /// Releases the last pin on a page and returns it to the freelist.
    pub fn free_page(&mut self, pin: PinnedPage) -> Result<()> {
        let (frame, id) = (pin.frame, pin.page_id);

        // A page being given back has no future worth logging. Drop it from the
        // edit session, along with the pin the session was holding on it.
        if let Some(edit) = self.edit.as_mut() {
            if edit.remove(&id).is_some() {
                self.frames[frame].pin_count = self.frames[frame].pin_count.saturating_sub(1);
            }
        }

        if self.frames[frame].pin_count > 1 {
            return Err(Error::PageStillPinned(id));
        }

        let slot = &mut self.frames[frame];
        slot.pin_count = 0;
        slot.dirty = false;
        slot.page_id = None;
        slot.ref_bit = false;
        self.table.remove(&id);

        // Inside a transaction the page is only set aside, not put on the
        // freelist. Writing the freelist header over it immediately would be an
        // unlogged write, and redo would later restore the page's old contents
        // on top of it and break the chain. The page waits for a checkpoint,
        // when the log is empty and nothing can be replayed over it.
        match self.txn.as_mut() {
            Some(txn) => {
                txn.freed.push(id);
                Ok(())
            }
            None => self.pager.free(id),
        }
    }

    /// Puts the pages that committed transactions gave up onto the freelist.
    ///
    /// Only safe once the log is empty, which is why the checkpoint calls it
    /// and nothing else does.
    pub fn release_pending_frees(&mut self) -> Result<()> {
        for id in std::mem::take(&mut self.pending_frees) {
            self.pager.free(id)?;
        }
        Ok(())
    }

    /// Writes every dirty page and syncs the file.
    pub fn flush_all(&mut self) -> Result<()> {
        for index in 0..self.frames.len() {
            self.flush_frame(index)?;
        }
        self.pager.sync()
    }

    /// How many pins are currently outstanding across the whole pool.
    pub fn pinned_frames(&self) -> usize {
        self.frames.iter().filter(|f| f.pin_count > 0).count()
    }

    /// Checks the invariants the pool is responsible for.
    ///
    /// Deliberately includes "nothing is pinned": every caller is expected to
    /// have handed its pins back before an operation is considered finished, so
    /// a leak surfaces at the operation that caused it.
    pub fn check_invariants(&self) -> Result<()> {
        let mapped = self.frames.iter().filter(|f| f.page_id.is_some()).count();
        if mapped != self.table.len() {
            return Err(Error::MalformedFile(format!(
                "{mapped} frames hold pages but the table has {} entries",
                self.table.len()
            )));
        }

        for (&id, &frame) in &self.table {
            if self.frames[frame].page_id != Some(id) {
                return Err(Error::MalformedFile(format!(
                    "table maps page {id} to a frame holding {:?}",
                    self.frames[frame].page_id
                )));
            }
        }

        let mut seen: Vec<PageId> = self.frames.iter().filter_map(|f| f.page_id).collect();
        let total = seen.len();
        seen.sort_unstable();
        seen.dedup();
        if seen.len() != total {
            return Err(Error::MalformedFile(
                "the same page occupies more than one frame".into(),
            ));
        }

        for frame in &self.frames {
            if frame.dirty && frame.page_id.is_none() {
                return Err(Error::MalformedFile(
                    "an empty frame is marked dirty".into(),
                ));
            }
        }

        if self.pinned_frames() != 0 {
            return Err(Error::MalformedFile(format!(
                "{} frames are still pinned",
                self.pinned_frames()
            )));
        }

        self.pager.check_invariants()
    }

    // -- transactions ------------------------------------------------------

    /// Starts a transaction. Single-writer, so only one may be open at a time.
    ///
    /// Without a log attached this is a no-op that hands back a transaction id
    /// nobody records, which is what keeps every non-logged caller working
    /// unchanged.
    pub fn begin_transaction(&mut self) -> Result<TxId> {
        if self.txn.is_some() {
            return Err(Error::TransactionAlreadyOpen);
        }
        let txid = self.pager.meta().next_txid;
        self.pager.meta_mut().next_txid += 1;

        let last_lsn = match self.wal.as_mut() {
            Some(wal) => wal.append(txid, 0, RecordBody::Begin)?,
            None => 0,
        };
        self.txn = Some(ActiveTxn {
            txid,
            last_lsn,
            edits: Vec::new(),
            freed: Vec::new(),
        });
        Ok(txid)
    }

    /// The open transaction's id, if there is one.
    pub fn transaction(&self) -> Option<TxId> {
        self.txn.as_ref().map(|txn| txn.txid)
    }

    /// Commits the open transaction and makes it durable.
    ///
    /// The `fsync` here is the only one on the critical path, and it is
    /// sequential. The pages the transaction changed may still be dirty in
    /// memory; the log is enough to bring them back.
    pub fn commit_transaction(&mut self) -> Result<()> {
        let Some(txn) = self.txn.take() else {
            return Err(Error::NoOpenTransaction);
        };

        // The metadata page goes down before the commit record does. It holds
        // the page count, and a page count that reverts after a crash hands the
        // same page numbers out twice — straight over data that was committed.
        // Erring early is the safe direction: if the crash lands between this
        // and the commit record, the transaction is undone and the metadata
        // merely over-counts, which leaks space rather than losing it.
        self.pager.sync()?;

        if let Some(wal) = self.wal.as_mut() {
            wal.append(txn.txid, txn.last_lsn, RecordBody::Commit)?;
            wal.sync()?;
        }
        self.pending_frees.extend(txn.freed);
        Ok(())
    }

    /// Rolls the open transaction back, newest change first.
    ///
    /// Every reversal is logged as a compensation record before it is applied,
    /// so a crash in the middle of this leaves recovery able to finish the job.
    pub fn rollback_transaction(&mut self) -> Result<()> {
        // An operation that failed halfway may have left a session open. Its
        // changes are about to be reversed anyway, so drop it rather than log
        // it.
        self.abort_edit();

        let Some(mut txn) = self.txn.take() else {
            return Err(Error::NoOpenTransaction);
        };
        if self.wal.is_none() {
            return Ok(());
        }

        while let Some((lsn, prev_lsn, edit)) = txn.edits.pop() {
            let compensation = RecordBody::Clr {
                undo_next_lsn: prev_lsn,
                edit: PageEdit::new(
                    edit.page,
                    edit.offset,
                    edit.after.clone(),
                    edit.before.clone(),
                ),
            };
            let wal = self.wal.as_mut().expect("checked above");
            let clr_lsn = wal.append(txn.txid, lsn, compensation)?;

            let pin = self.fetch(edit.page)?;
            let start = edit.offset as usize;
            let page = self.page_mut(&pin);
            page.as_bytes_mut()[start..start + edit.before.len()].copy_from_slice(&edit.before);
            page.set_lsn(clr_lsn);
            self.unpin(pin);

            txn.last_lsn = clr_lsn;
        }

        let wal = self.wal.as_mut().expect("checked above");
        wal.append(txn.txid, txn.last_lsn, RecordBody::Abort)?;
        wal.sync()?;

        // Pages the transaction gave up are simply dropped. They are no longer
        // referenced by anything and they are not on the freelist either, so
        // they leak until the file is rebuilt. Space, not correctness; recorded
        // as a known limitation rather than pretended away.
        Ok(())
    }

    // -- edit sessions -----------------------------------------------------

    /// Starts recording the pages an operation is about to change.
    ///
    /// While a session is open, the first [`BufferPool::page_mut`] on a page
    /// snapshots it and takes an extra pin. The pin is what makes this correct:
    /// a page with changes that are not in the log yet must not be evicted,
    /// because the WAL rule could not be honoured for changes that have not
    /// been written down. It is released by [`BufferPool::end_edit`].
    ///
    /// The pool therefore needs a frame for every page a single operation
    /// touches. For a B+Tree that is the descent path plus the pages a split
    /// creates; too few frames surfaces loudly as [`Error::AllFramesPinned`]
    /// rather than as a slow leak.
    ///
    /// A no-op when there is no log or no open transaction, which is what lets
    /// every caller run the same code whether or not it is being logged.
    /// Returns whether it opened one, so a caller that finds a session already
    /// running leaves it to whoever started it. Closing somebody else's session
    /// early would log half an operation and call it whole.
    pub fn begin_edit(&mut self) -> bool {
        if self.wal.is_some() && self.txn.is_some() && self.edit.is_none() {
            self.edit = Some(HashMap::new());
            return true;
        }
        false
    }

    /// Closes the session, logging the smallest edit that describes each page.
    ///
    /// Diffing whole images and logging only what moved is what keeps this
    /// physiological rather than physical: rewriting a node to change one cell
    /// costs a record the size of that cell, not the size of a page.
    pub fn end_edit(&mut self) -> Result<()> {
        let Some(edit) = self.edit.take() else {
            return Ok(());
        };
        let Some(txn) = self.txn.as_mut() else {
            return Ok(());
        };

        // Sorted so that the same operation produces the same log twice, which
        // matters when a failing crash-fuzzer seed has to be replayed.
        let mut touched: Vec<(PageId, usize, Box<Page>)> = edit
            .into_iter()
            .map(|(page, (frame, before))| (page, frame, before))
            .collect();
        touched.sort_by_key(|(page, _, _)| *page);

        for (page_id, frame, before) in touched {
            let runs = PageEdit::runs(
                page_id,
                before.as_bytes(),
                self.frames[frame].page.as_bytes(),
                EDIT_COALESCE_GAP,
            );
            let mut last_lsn = txn.last_lsn;
            for diff in runs {
                let wal = self.wal.as_mut().ok_or(Error::NoLogAttached)?;
                let lsn = wal.append(txn.txid, last_lsn, RecordBody::Update(diff.clone()))?;
                txn.edits.push((lsn, last_lsn, diff));
                last_lsn = lsn;
            }
            // Stamped after the diffs are taken, so an edit that happens to span
            // the header's own LSN field cannot leave a stale value behind.
            if last_lsn != txn.last_lsn {
                self.frames[frame].page.set_lsn(last_lsn);
                txn.last_lsn = last_lsn;
            }
            self.frames[frame].pin_count = self.frames[frame].pin_count.saturating_sub(1);
        }
        Ok(())
    }

    /// Abandons a session without logging anything, releasing its pins.
    ///
    /// For the error path: an operation that failed halfway has left the pages
    /// in whatever state it got to, and the transaction it belongs to is going
    /// to be rolled back.
    pub fn abort_edit(&mut self) {
        let Some(edit) = self.edit.take() else {
            return;
        };
        for (_, (frame, _)) in edit {
            self.frames[frame].pin_count = self.frames[frame].pin_count.saturating_sub(1);
        }
    }

    // -- private -----------------------------------------------------------

    fn flush_frame(&mut self, index: usize) -> Result<()> {
        if !self.frames[index].dirty {
            return Ok(());
        }
        let Some(id) = self.frames[index].page_id else {
            return Ok(());
        };

        // The WAL rule, and the single most important line in the storage
        // layer: the record describing this page reaches the medium before the
        // page does. Delete it and every test still passes, and the database
        // corrupts silently at the first power loss.
        let lsn = self.frames[index].page.lsn();
        if let Some(wal) = self.wal.as_mut() {
            wal.sync_through(lsn)?;
        }

        self.pager.write_page(id, &self.frames[index].page)?;
        self.frames[index].dirty = false;
        Ok(())
    }

    /// Finds a frame to load a page into: a free one, or a victim chosen by the
    /// clock policy.
    fn acquire_frame(&mut self) -> Result<usize> {
        if let Some(index) = self.frames.iter().position(|f| f.page_id.is_none()) {
            return Ok(index);
        }

        let count = self.frames.len();
        for _ in 0..(2 * count) {
            let index = self.clock_hand;
            self.clock_hand = (self.clock_hand + 1) % count;

            if self.frames[index].pin_count > 0 {
                continue;
            }
            if self.frames[index].ref_bit {
                self.frames[index].ref_bit = false;
                continue;
            }

            self.flush_frame(index)?;
            if let Some(old) = self.frames[index].page_id.take() {
                self.table.remove(&old);
            }
            return Ok(index);
        }

        // Two full revolutions without a victim means every frame is pinned.
        // Always a missing unpin, never a normal condition.
        Err(Error::AllFramesPinned)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn pool(capacity: usize) -> (tempfile::TempDir, BufferPool) {
        let dir = tempdir().unwrap();
        let pager = Pager::create(dir.path().join("t.lastro")).unwrap();
        (dir, BufferPool::new(pager, capacity))
    }

    #[test]
    fn writes_are_visible_after_a_flush_and_reload() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("t.lastro");

        let id = {
            let mut pool = BufferPool::new(Pager::create(&path).unwrap(), 4);
            let pin = pool.new_page(PageType::Heap).unwrap();
            let id = pin.page_id;
            pool.page_mut(&pin).push_cell(b"persisted").unwrap();
            pool.unpin(pin);
            pool.flush_all().unwrap();
            pool.check_invariants().unwrap();
            id
        };

        let mut pool = BufferPool::new(Pager::open(&path).unwrap(), 4);
        let pin = pool.fetch(id).unwrap();
        assert_eq!(pool.page(&pin).cell(0), Some(&b"persisted"[..]));
        pool.unpin(pin);
        pool.check_invariants().unwrap();
    }

    #[test]
    fn a_second_fetch_hits_the_same_frame() {
        let (_dir, mut pool) = pool(4);
        let first = pool.new_page(PageType::Heap).unwrap();
        let id = first.page_id;
        pool.unpin(first);

        let a = pool.fetch(id).unwrap();
        let b = pool.fetch(id).unwrap();
        assert_eq!(a.frame, b.frame, "a cached page must not be loaded twice");
        pool.unpin(a);
        pool.unpin(b);
        pool.check_invariants().unwrap();
    }

    #[test]
    fn eviction_writes_dirty_pages_out() {
        let (_dir, mut pool) = pool(2);

        let mut ids = Vec::new();
        for marker in 0u8..6 {
            let pin = pool.new_page(PageType::Heap).unwrap();
            ids.push(pin.page_id);
            pool.page_mut(&pin).push_cell(&[marker; 16]).unwrap();
            pool.unpin(pin);
        }
        // Six pages through two frames: everything was evicted at least once.
        assert_eq!(pool.capacity(), 2);

        for (marker, id) in ids.iter().enumerate() {
            let pin = pool.fetch(*id).unwrap();
            assert_eq!(pool.page(&pin).cell(0), Some(&[marker as u8; 16][..]));
            pool.unpin(pin);
        }
        pool.check_invariants().unwrap();
    }

    #[test]
    fn every_frame_pinned_is_an_error_not_a_hang() {
        let (_dir, mut pool) = pool(2);
        let a = pool.new_page(PageType::Heap).unwrap();
        let b = pool.new_page(PageType::Heap).unwrap();

        assert!(matches!(
            pool.new_page(PageType::Heap),
            Err(Error::AllFramesPinned)
        ));

        pool.unpin(a);
        pool.unpin(b);
    }

    #[test]
    fn freeing_a_page_recycles_it() {
        let (_dir, mut pool) = pool(4);
        let pin = pool.new_page(PageType::Heap).unwrap();
        let id = pin.page_id;
        pool.page_mut(&pin).push_cell(b"doomed").unwrap();
        pool.free_page(pin).unwrap();
        pool.check_invariants().unwrap();

        let reused = pool.new_page(PageType::Leaf).unwrap();
        assert_eq!(reused.page_id, id, "the freelist must hand the page back");
        assert_eq!(pool.page(&reused).slot_count(), 0, "and it must be clean");
        assert_eq!(pool.page(&reused).page_type(), Some(PageType::Leaf));
        pool.unpin(reused);
        pool.check_invariants().unwrap();
    }

    #[test]
    fn freeing_a_page_pinned_twice_is_refused() {
        let (_dir, mut pool) = pool(4);
        let first = pool.new_page(PageType::Heap).unwrap();
        let id = first.page_id;
        let second = pool.fetch(id).unwrap();

        assert!(matches!(
            pool.free_page(second),
            Err(Error::PageStillPinned(_))
        ));
        pool.unpin(first);
    }

    #[test]
    fn invariants_catch_a_leaked_pin() {
        let (_dir, mut pool) = pool(4);
        let pin = pool.new_page(PageType::Heap).unwrap();
        assert!(
            pool.check_invariants().is_err(),
            "a leaked pin must be seen"
        );
        pool.unpin(pin);
        pool.check_invariants().unwrap();
    }

    #[test]
    fn many_pages_through_a_small_pool_keep_their_contents() {
        let (_dir, mut pool) = pool(3);

        let mut ids = Vec::new();
        for index in 0..64u64 {
            let pin = pool.new_page(PageType::Heap).unwrap();
            ids.push(pin.page_id);
            pool.page_mut(&pin).push_cell(&index.to_le_bytes()).unwrap();
            pool.unpin(pin);
        }

        for (index, id) in ids.iter().enumerate() {
            let pin = pool.fetch(*id).unwrap();
            let expected = (index as u64).to_le_bytes();
            assert_eq!(pool.page(&pin).cell(0), Some(&expected[..]));
            pool.page(&pin).check_invariants().unwrap();
            pool.unpin(pin);
        }
        pool.check_invariants().unwrap();
    }
}
