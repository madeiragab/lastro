//! The slotted page.
//!
//! ```text
//! 0                                                            4096
//! +--------+------------------+--------------------+--------------+
//! | header | slots            | free space         | cells        |
//! | 24 B   | 4 B each -->     |                    | <-- grow     |
//! +--------+------------------+--------------------+--------------+
//!          ^                  ^                    ^
//!          24            free_start            free_end
//! ```
//!
//! Slots grow forward from the header, cells grow backward from the end of the
//! page. The slot index is the stable address: a cell may move during
//! compaction, its slot index may not. See `docs/en/02-file-format.md`.

use std::fmt;

use crate::{Error, Lsn, PageId, Result, PAGE_SIZE};

/// Size of the fixed page header, in bytes.
pub const PAGE_HEADER_SIZE: usize = 24;

/// Size of one slot entry, in bytes: a `u16` offset and a `u16` length.
pub const SLOT_SIZE: usize = 4;

/// The largest payload that may be stored inline in a cell. Anything larger
/// belongs in an overflow chain. The limit is a quarter of a page, which
/// guarantees a minimum fanout of four in interior nodes.
///
/// This is a policy constant for the index layer, not something the page
/// enforces: a page only refuses a cell that cannot physically fit.
pub const MAX_INLINE_CELL: usize = PAGE_SIZE / 4;

/// Bytes a page can devote to slots and cells together.
pub const USABLE_SPACE: usize = PAGE_SIZE - PAGE_HEADER_SIZE;

/// The largest cell that can physically be stored, which is one occupying an
/// otherwise empty page.
pub const MAX_CELL: usize = USABLE_SPACE - SLOT_SIZE;

const OFF_TYPE: usize = 0;
const OFF_FLAGS: usize = 1;
const OFF_SLOT_COUNT: usize = 2;
const OFF_FREE_START: usize = 4;
const OFF_FREE_END: usize = 6;
const OFF_FRAGMENTED: usize = 8;
const OFF_LSN: usize = 12;
const OFF_EXTRA: usize = 20;

/// Set when the page is the root of a tree.
pub const FLAG_ROOT: u8 = 0b0000_0001;

/// What a page holds. Stored in the first header byte.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum PageType {
    /// Page 0: the database metadata.
    Meta = 1,
    /// A B+Tree interior node, holding separators and child pointers.
    Interior = 2,
    /// A B+Tree leaf, holding keys and values.
    Leaf = 3,
    /// A heap page, holding tuples addressed by RowId.
    Heap = 4,
    /// A page on the freelist, awaiting reuse.
    Freelist = 5,
    /// A continuation page for an oversized payload.
    Overflow = 6,
}

impl PageType {
    /// Reads a page type from its on-disk byte.
    pub fn from_u8(value: u8) -> Option<PageType> {
        match value {
            1 => Some(PageType::Meta),
            2 => Some(PageType::Interior),
            3 => Some(PageType::Leaf),
            4 => Some(PageType::Heap),
            5 => Some(PageType::Freelist),
            6 => Some(PageType::Overflow),
            _ => None,
        }
    }
}

/// One page of the database, exactly [`PAGE_SIZE`] bytes.
#[derive(Clone)]
#[repr(transparent)]
pub struct Page([u8; PAGE_SIZE]);

impl fmt::Debug for Page {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Page")
            .field("type", &PageType::from_u8(self.0[OFF_TYPE]))
            .field("slots", &self.slot_count())
            .field("free_start", &self.free_start())
            .field("free_end", &self.free_end())
            .field("fragmented", &self.fragmented())
            .field("lsn", &self.lsn())
            .field("extra", &self.extra())
            .finish()
    }
}

impl Default for Page {
    fn default() -> Self {
        Page::zeroed()
    }
}

impl Page {
    /// An all-zero page. Not valid until [`Page::init`] is called.
    pub fn zeroed() -> Page {
        Page([0u8; PAGE_SIZE])
    }

    /// Prepares an empty page of the given type: no slots, all space free.
    pub fn init(&mut self, page_type: PageType) {
        self.0 = [0u8; PAGE_SIZE];
        self.0[OFF_TYPE] = page_type as u8;
        self.set_slot_count(0);
        self.set_free_start(PAGE_HEADER_SIZE as u16);
        self.set_free_end(PAGE_SIZE as u16);
        self.set_fragmented(0);
        self.set_lsn(0);
        self.set_extra(crate::NO_PAGE);
    }

    /// The raw bytes, for the pager to write.
    pub fn as_bytes(&self) -> &[u8; PAGE_SIZE] {
        &self.0
    }

    /// The raw bytes, for the pager to read into.
    pub fn as_bytes_mut(&mut self) -> &mut [u8; PAGE_SIZE] {
        &mut self.0
    }

    // -- header accessors --------------------------------------------------

    /// The page's declared type, or `None` if the byte is not a known type.
    pub fn page_type(&self) -> Option<PageType> {
        PageType::from_u8(self.0[OFF_TYPE])
    }

    /// Overwrites the page type without touching anything else.
    pub fn set_page_type(&mut self, page_type: PageType) {
        self.0[OFF_TYPE] = page_type as u8;
    }

    /// True when the page is the root of a tree.
    pub fn is_root(&self) -> bool {
        self.0[OFF_FLAGS] & FLAG_ROOT != 0
    }

    /// Marks or unmarks the page as a tree root.
    pub fn set_root(&mut self, root: bool) {
        if root {
            self.0[OFF_FLAGS] |= FLAG_ROOT;
        } else {
            self.0[OFF_FLAGS] &= !FLAG_ROOT;
        }
    }

    /// How many slots the page has, dead ones included.
    pub fn slot_count(&self) -> u16 {
        self.u16_at(OFF_SLOT_COUNT)
    }

    /// Offset of the first free byte after the slot array.
    pub fn free_start(&self) -> u16 {
        self.u16_at(OFF_FREE_START)
    }

    /// Offset of the lowest cell, or [`PAGE_SIZE`] when there are none.
    pub fn free_end(&self) -> u16 {
        self.u16_at(OFF_FREE_END)
    }

    /// Bytes lost to holes left by deleted cells, reclaimable by compaction.
    pub fn fragmented(&self) -> u16 {
        self.u16_at(OFF_FRAGMENTED)
    }

    /// LSN of the last modification to this page. The redo pass compares
    /// against it to stay idempotent.
    pub fn lsn(&self) -> Lsn {
        self.u64_at(OFF_LSN)
    }

    /// Records the LSN of the change just applied.
    pub fn set_lsn(&mut self, lsn: Lsn) {
        self.set_u64(OFF_LSN, lsn);
    }

    /// The type-dependent trailing header word: rightmost child for interior
    /// pages, right sibling for leaves, next link for freelist and overflow.
    pub fn extra(&self) -> PageId {
        self.u32_at(OFF_EXTRA)
    }

    /// Sets the type-dependent trailing header word. See [`Page::extra`].
    pub fn set_extra(&mut self, value: PageId) {
        self.set_u32(OFF_EXTRA, value);
    }

    // -- space accounting --------------------------------------------------

    /// Contiguous free bytes between the slot array and the lowest cell.
    pub fn free_space(&self) -> usize {
        (self.free_end() as usize).saturating_sub(self.free_start() as usize)
    }

    /// Free bytes including those recoverable by compaction.
    pub fn total_free(&self) -> usize {
        self.free_space() + self.fragmented() as usize
    }

    /// Bytes actually occupied by slots and live cells.
    pub fn used_space(&self) -> usize {
        USABLE_SPACE - self.total_free()
    }

    /// How full the page is, in percent of [`USABLE_SPACE`].
    ///
    /// The B+Tree uses this to decide when a node has underflowed. Integer
    /// arithmetic on purpose: a threshold that depends on float rounding is a
    /// threshold that behaves differently on different machines.
    pub fn occupancy_percent(&self) -> usize {
        self.used_space() * 100 / USABLE_SPACE
    }

    // -- cells -------------------------------------------------------------

    /// The bytes of a live cell, or `None` if the slot is dead or absent.
    pub fn cell(&self, slot: u16) -> Option<&[u8]> {
        if slot >= self.slot_count() {
            return None;
        }
        let (offset, length) = self.slot(slot);
        if offset == 0 {
            return None;
        }
        let start = offset as usize;
        Some(&self.0[start..start + length as usize])
    }

    /// Appends a cell after the last slot, returning its slot index.
    pub fn push_cell(&mut self, bytes: &[u8]) -> Result<u16> {
        let index = self.slot_count();
        self.insert_cell_at(index, bytes)?;
        Ok(index)
    }

    /// Inserts a cell at `index`, shifting later slots up by one.
    ///
    /// Compacts first if the contiguous free space is short but the page holds
    /// enough reclaimable bytes overall.
    pub fn insert_cell_at(&mut self, index: u16, bytes: &[u8]) -> Result<()> {
        if bytes.len() > MAX_CELL {
            return Err(Error::CellTooLarge(bytes.len()));
        }
        let count = self.slot_count();
        if index > count {
            return Err(Error::NoSuchSlot(index));
        }

        let needed = bytes.len() + SLOT_SIZE;
        if self.free_space() < needed {
            if self.total_free() < needed {
                return Err(Error::PageFull);
            }
            self.compact();
            if self.free_space() < needed {
                return Err(Error::PageFull);
            }
        }

        let from = PAGE_HEADER_SIZE + index as usize * SLOT_SIZE;
        let to = PAGE_HEADER_SIZE + count as usize * SLOT_SIZE;
        if from < to {
            self.0.copy_within(from..to, from + SLOT_SIZE);
        }

        let cell_start = self.free_end() as usize - bytes.len();
        self.0[cell_start..cell_start + bytes.len()].copy_from_slice(bytes);

        self.set_free_end(cell_start as u16);
        self.set_slot(index, cell_start as u16, bytes.len() as u16);
        self.set_slot_count(count + 1);
        self.set_free_start((PAGE_HEADER_SIZE + (count as usize + 1) * SLOT_SIZE) as u16);
        Ok(())
    }

    /// Marks a cell dead. The slot index survives so that RowIds stay valid;
    /// its bytes are reclaimed by the next compaction. Idempotent.
    pub fn delete_cell(&mut self, slot: u16) -> Result<()> {
        if slot >= self.slot_count() {
            return Err(Error::NoSuchSlot(slot));
        }
        let (offset, length) = self.slot(slot);
        if offset == 0 {
            return Ok(());
        }
        self.set_slot(slot, 0, 0);
        self.set_fragmented(self.fragmented() + length);
        Ok(())
    }

    /// Removes a slot entirely, shifting later slots down by one.
    ///
    /// Only safe where slot indices are not used as stable addresses, which
    /// means B+Tree pages but never the heap.
    pub fn remove_slot(&mut self, slot: u16) -> Result<()> {
        let count = self.slot_count();
        if slot >= count {
            return Err(Error::NoSuchSlot(slot));
        }
        self.delete_cell(slot)?;

        let from = PAGE_HEADER_SIZE + (slot as usize + 1) * SLOT_SIZE;
        let to = PAGE_HEADER_SIZE + count as usize * SLOT_SIZE;
        if from < to {
            self.0.copy_within(from..to, from - SLOT_SIZE);
        }
        self.set_slot_count(count - 1);
        self.set_free_start((PAGE_HEADER_SIZE + (count as usize - 1) * SLOT_SIZE) as u16);
        Ok(())
    }

    /// Rewrites the live cells packed against the end of the page, reclaiming
    /// every fragmented byte. Slot indices do not change.
    pub fn compact(&mut self) {
        let count = self.slot_count();
        let mut live: Vec<(u16, Vec<u8>)> = Vec::new();
        for slot in 0..count {
            if let Some(bytes) = self.cell(slot) {
                live.push((slot, bytes.to_vec()));
            }
        }

        let mut end = PAGE_SIZE;
        for (slot, bytes) in &live {
            end -= bytes.len();
            self.0[end..end + bytes.len()].copy_from_slice(bytes);
            self.set_slot(*slot, end as u16, bytes.len() as u16);
        }

        self.set_free_end(end as u16);
        self.set_fragmented(0);
    }

    /// Iterates over the live cells, yielding each with its slot index.
    pub fn iter_cells(&self) -> impl Iterator<Item = (u16, &[u8])> + '_ {
        (0..self.slot_count()).filter_map(move |slot| self.cell(slot).map(|bytes| (slot, bytes)))
    }

    /// How many slots are live.
    pub fn live_count(&self) -> u16 {
        (0..self.slot_count())
            .filter(|&slot| self.cell(slot).is_some())
            .count() as u16
    }

    // -- invariants --------------------------------------------------------

    /// Checks every structural invariant of a slotted page.
    ///
    /// Called at the end of every test, and from `debug_assert!` on the hot
    /// path. A violation is reported at the operation that caused it rather
    /// than ten thousand operations later.
    pub fn check_invariants(&self) -> Result<()> {
        let count = self.slot_count() as usize;
        let expected_free_start = PAGE_HEADER_SIZE + count * SLOT_SIZE;
        if self.free_start() as usize != expected_free_start {
            return Err(Error::MalformedFile(format!(
                "free_start is {} but {count} slots imply {expected_free_start}",
                self.free_start()
            )));
        }
        if self.free_start() > self.free_end() {
            return Err(Error::MalformedFile(format!(
                "free_start {} is above free_end {}",
                self.free_start(),
                self.free_end()
            )));
        }
        if self.free_end() as usize > PAGE_SIZE {
            return Err(Error::MalformedFile(format!(
                "free_end {} is beyond the page",
                self.free_end()
            )));
        }

        let mut occupied = vec![false; PAGE_SIZE];
        let mut live_bytes = 0usize;
        for slot in 0..self.slot_count() {
            let (offset, length) = self.slot(slot);
            if offset == 0 {
                continue;
            }
            let start = offset as usize;
            let end = start + length as usize;
            if start < self.free_end() as usize || end > PAGE_SIZE {
                return Err(Error::MalformedFile(format!(
                    "slot {slot} spans {start}..{end}, outside the cell area"
                )));
            }
            for byte in occupied.iter_mut().take(end).skip(start) {
                if *byte {
                    return Err(Error::MalformedFile(format!(
                        "slot {slot} overlaps another"
                    )));
                }
                *byte = true;
            }
            live_bytes += length as usize;
        }

        let accounted = live_bytes + self.fragmented() as usize;
        let allocated = PAGE_SIZE - self.free_end() as usize;
        if accounted != allocated {
            return Err(Error::MalformedFile(format!(
                "live {live_bytes} plus fragmented {} is {accounted}, but {allocated} bytes are allocated",
                self.fragmented()
            )));
        }
        Ok(())
    }

    // -- private -----------------------------------------------------------

    fn slot(&self, slot: u16) -> (u16, u16) {
        let base = PAGE_HEADER_SIZE + slot as usize * SLOT_SIZE;
        (self.u16_at(base), self.u16_at(base + 2))
    }

    fn set_slot(&mut self, slot: u16, offset: u16, length: u16) {
        let base = PAGE_HEADER_SIZE + slot as usize * SLOT_SIZE;
        self.set_u16(base, offset);
        self.set_u16(base + 2, length);
    }

    fn set_slot_count(&mut self, value: u16) {
        self.set_u16(OFF_SLOT_COUNT, value);
    }

    fn set_free_start(&mut self, value: u16) {
        self.set_u16(OFF_FREE_START, value);
    }

    fn set_free_end(&mut self, value: u16) {
        self.set_u16(OFF_FREE_END, value);
    }

    fn set_fragmented(&mut self, value: u16) {
        self.set_u16(OFF_FRAGMENTED, value);
    }

    fn u16_at(&self, offset: usize) -> u16 {
        u16::from_le_bytes([self.0[offset], self.0[offset + 1]])
    }

    fn set_u16(&mut self, offset: usize, value: u16) {
        self.0[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
    }

    fn u32_at(&self, offset: usize) -> u32 {
        let mut buf = [0u8; 4];
        buf.copy_from_slice(&self.0[offset..offset + 4]);
        u32::from_le_bytes(buf)
    }

    fn set_u32(&mut self, offset: usize, value: u32) {
        self.0[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
    }

    fn u64_at(&self, offset: usize) -> u64 {
        let mut buf = [0u8; 8];
        buf.copy_from_slice(&self.0[offset..offset + 8]);
        u64::from_le_bytes(buf)
    }

    fn set_u64(&mut self, offset: usize, value: u64) {
        self.0[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn leaf() -> Page {
        let mut page = Page::zeroed();
        page.init(PageType::Leaf);
        page
    }

    #[test]
    fn empty_page_is_consistent() {
        let page = leaf();
        assert_eq!(page.page_type(), Some(PageType::Leaf));
        assert_eq!(page.slot_count(), 0);
        assert_eq!(page.free_end() as usize, PAGE_SIZE);
        assert_eq!(page.free_space(), PAGE_SIZE - PAGE_HEADER_SIZE);
        page.check_invariants().unwrap();
    }

    #[test]
    fn round_trips_cells() {
        let mut page = leaf();
        let a = page.push_cell(b"first").unwrap();
        let b = page.push_cell(b"second").unwrap();
        let c = page.push_cell(&[]).unwrap();

        assert_eq!(page.cell(a), Some(&b"first"[..]));
        assert_eq!(page.cell(b), Some(&b"second"[..]));
        assert_eq!(page.cell(c), Some(&[][..]));
        assert_eq!(page.slot_count(), 3);
        page.check_invariants().unwrap();
    }

    #[test]
    fn insert_in_the_middle_shifts_slots() {
        let mut page = leaf();
        page.push_cell(b"a").unwrap();
        page.push_cell(b"c").unwrap();
        page.insert_cell_at(1, b"b").unwrap();

        let cells: Vec<&[u8]> = page.iter_cells().map(|(_, bytes)| bytes).collect();
        assert_eq!(cells, vec![&b"a"[..], &b"b"[..], &b"c"[..]]);
        page.check_invariants().unwrap();
    }

    #[test]
    fn deleting_frees_space_only_after_compaction() {
        let mut page = leaf();
        let slot = page.push_cell(&[7u8; 100]).unwrap();
        page.push_cell(&[8u8; 100]).unwrap();

        let free_before = page.free_space();
        page.delete_cell(slot).unwrap();
        assert_eq!(page.cell(slot), None);
        assert_eq!(page.fragmented(), 100);
        assert_eq!(page.free_space(), free_before);
        assert_eq!(page.total_free(), free_before + 100);
        page.check_invariants().unwrap();

        page.compact();
        assert_eq!(page.fragmented(), 0);
        assert_eq!(page.free_space(), free_before + 100);
        assert_eq!(page.cell(slot), None);
        assert_eq!(page.cell(1), Some(&[8u8; 100][..]));
        page.check_invariants().unwrap();
    }

    #[test]
    fn deleting_is_idempotent() {
        let mut page = leaf();
        let slot = page.push_cell(b"gone").unwrap();
        page.delete_cell(slot).unwrap();
        page.delete_cell(slot).unwrap();
        assert_eq!(page.fragmented(), 4);
        page.check_invariants().unwrap();
    }

    #[test]
    fn compaction_happens_automatically_when_needed() {
        let mut page = leaf();
        // Fill the page with cells, then delete every other one. The remaining
        // free space is fragmented, so the next insert must compact.
        let mut slots = Vec::new();
        while let Ok(slot) = page.push_cell(&[1u8; 200]) {
            slots.push(slot);
        }
        for slot in slots.iter().step_by(2) {
            page.delete_cell(*slot).unwrap();
        }
        assert!(page.free_space() < 200);
        assert!(page.total_free() >= 204);

        page.push_cell(&[2u8; 200]).unwrap();
        assert_eq!(page.fragmented(), 0);
        page.check_invariants().unwrap();
    }

    #[test]
    fn rejects_oversized_and_overfull() {
        let mut page = leaf();
        assert!(matches!(
            page.push_cell(&vec![0u8; MAX_CELL + 1]),
            Err(Error::CellTooLarge(_))
        ));

        // A cell that exactly fills an empty page is accepted, and nothing
        // fits beside it.
        page.push_cell(&vec![0u8; MAX_CELL]).unwrap();
        assert_eq!(page.free_space(), 0);
        assert!(matches!(page.push_cell(b"x"), Err(Error::PageFull)));
        page.check_invariants().unwrap();
    }

    #[test]
    fn occupancy_tracks_used_space() {
        let mut page = leaf();
        assert_eq!(page.used_space(), 0);
        assert_eq!(page.occupancy_percent(), 0);

        page.push_cell(&[0u8; 1000]).unwrap();
        assert_eq!(page.used_space(), 1004);
        assert_eq!(page.occupancy_percent(), 1004 * 100 / USABLE_SPACE);

        page.delete_cell(0).unwrap();
        assert_eq!(page.used_space(), 4, "the dead slot still costs its slot");
    }

    #[test]
    fn remove_slot_shifts_indices_down() {
        let mut page = leaf();
        page.push_cell(b"a").unwrap();
        page.push_cell(b"b").unwrap();
        page.push_cell(b"c").unwrap();

        page.remove_slot(1).unwrap();
        assert_eq!(page.slot_count(), 2);
        assert_eq!(page.cell(0), Some(&b"a"[..]));
        assert_eq!(page.cell(1), Some(&b"c"[..]));
        page.check_invariants().unwrap();
    }

    #[test]
    fn header_fields_survive_cell_operations() {
        let mut page = leaf();
        page.set_lsn(0xDEAD_BEEF_CAFE);
        page.set_extra(42);
        page.set_root(true);

        page.push_cell(b"payload").unwrap();
        page.compact();

        assert_eq!(page.lsn(), 0xDEAD_BEEF_CAFE);
        assert_eq!(page.extra(), 42);
        assert!(page.is_root());
        assert_eq!(page.page_type(), Some(PageType::Leaf));
    }
}
