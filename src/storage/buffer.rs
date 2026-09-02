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

use crate::storage::page::{Page, PageType};
use crate::storage::pager::Pager;
use crate::wal::record::{PageEdit, RecordBody};
use crate::wal::Wal;
use crate::{Error, Lsn, PageId, Result, TxId};

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
    pub fn page_mut(&mut self, pin: &PinnedPage) -> &mut Page {
        let slot = &mut self.frames[pin.frame];
        slot.dirty = true;
        &mut slot.page
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
        if self.frames[frame].pin_count > 1 {
            return Err(Error::PageStillPinned(id));
        }

        let slot = &mut self.frames[frame];
        slot.pin_count = 0;
        slot.dirty = false;
        slot.page_id = None;
        slot.ref_bit = false;
        self.table.remove(&id);

        self.pager.free(id)
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
