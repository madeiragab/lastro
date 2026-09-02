//! The pager: the lowest layer.
//!
//! It understands page numbers and [`crate::PAGE_SIZE`] bytes. It does not know
//! what a key, a tuple or a transaction is.
//!
//! Reads and writes are positional, so the pager holds no mutable cursor state.
//! See `docs/en/03-pager.md`.

use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io;
use std::path::Path;

use crate::storage::crash::CrashHandle;

use crate::storage::page::{Page, PageType};
use crate::util::crc32c;
use crate::{
    Error, Lsn, PageId, Result, TxId, FORMAT_VERSION, MAGIC, META_PAGE, NO_PAGE, PAGE_SIZE,
};

const META_OFF_VERSION: usize = 8;
const META_OFF_PAGE_SIZE: usize = 10;
const META_OFF_PAGE_COUNT: usize = 12;
const META_OFF_FREELIST_HEAD: usize = 16;
const META_OFF_FREELIST_COUNT: usize = 20;
const META_OFF_NEXT_TXID: usize = 24;
const META_OFF_CHECKPOINT_LSN: usize = 32;
const META_OFF_CATALOG_ROOT: usize = 40;
const META_OFF_SCHEMA_VERSION: usize = 44;
const META_OFF_CHECKSUM: usize = PAGE_SIZE - 4;

/// The contents of page 0.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Meta {
    /// On-disk format version.
    pub format_version: u16,
    /// Page size in bytes. Always [`crate::PAGE_SIZE`]; stored so that a future
    /// change can be rejected with a clear message instead of read as garbage.
    pub page_size: u16,
    /// Total pages allocated, page 0 included.
    pub page_count: u32,
    /// First page of the freelist, or [`crate::NO_PAGE`].
    pub freelist_head: PageId,
    /// How many pages are on the freelist.
    pub freelist_count: u32,
    /// Next transaction id to hand out.
    pub next_txid: TxId,
    /// Where recovery begins its analysis pass.
    pub last_checkpoint_lsn: Lsn,
    /// Root page of the catalog B+Tree. The only external pointer that exists.
    pub catalog_root: PageId,
    /// Bumped on every DDL statement.
    pub schema_version: u32,
}

impl Meta {
    fn new() -> Meta {
        Meta {
            format_version: FORMAT_VERSION,
            page_size: PAGE_SIZE as u16,
            page_count: 1,
            freelist_head: NO_PAGE,
            freelist_count: 0,
            next_txid: 1,
            last_checkpoint_lsn: 0,
            catalog_root: NO_PAGE,
            schema_version: 0,
        }
    }

    fn write_to(&self, page: &mut Page) {
        let bytes = page.as_bytes_mut();
        bytes.fill(0);
        bytes[..8].copy_from_slice(&MAGIC);
        put_u16(bytes, META_OFF_VERSION, self.format_version);
        put_u16(bytes, META_OFF_PAGE_SIZE, self.page_size);
        put_u32(bytes, META_OFF_PAGE_COUNT, self.page_count);
        put_u32(bytes, META_OFF_FREELIST_HEAD, self.freelist_head);
        put_u32(bytes, META_OFF_FREELIST_COUNT, self.freelist_count);
        put_u64(bytes, META_OFF_NEXT_TXID, self.next_txid);
        put_u64(bytes, META_OFF_CHECKPOINT_LSN, self.last_checkpoint_lsn);
        put_u32(bytes, META_OFF_CATALOG_ROOT, self.catalog_root);
        put_u32(bytes, META_OFF_SCHEMA_VERSION, self.schema_version);
        let checksum = crc32c(&bytes[..META_OFF_CHECKSUM]);
        put_u32(bytes, META_OFF_CHECKSUM, checksum);
    }

    fn read_from(page: &Page) -> Result<Meta> {
        let bytes = page.as_bytes();
        if bytes[..8] != MAGIC {
            return Err(Error::BadMagic);
        }

        let stored = get_u32(bytes, META_OFF_CHECKSUM);
        let computed = crc32c(&bytes[..META_OFF_CHECKSUM]);
        if stored != computed {
            return Err(Error::ChecksumMismatch { page: META_PAGE });
        }

        let format_version = get_u16(bytes, META_OFF_VERSION);
        if format_version != FORMAT_VERSION {
            return Err(Error::UnsupportedVersion(format_version));
        }
        let page_size = get_u16(bytes, META_OFF_PAGE_SIZE);
        if page_size as usize != PAGE_SIZE {
            return Err(Error::UnsupportedPageSize(page_size));
        }

        Ok(Meta {
            format_version,
            page_size,
            page_count: get_u32(bytes, META_OFF_PAGE_COUNT),
            freelist_head: get_u32(bytes, META_OFF_FREELIST_HEAD),
            freelist_count: get_u32(bytes, META_OFF_FREELIST_COUNT),
            next_txid: get_u64(bytes, META_OFF_NEXT_TXID),
            last_checkpoint_lsn: get_u64(bytes, META_OFF_CHECKPOINT_LSN),
            catalog_root: get_u32(bytes, META_OFF_CATALOG_ROOT),
            schema_version: get_u32(bytes, META_OFF_SCHEMA_VERSION),
        })
    }
}

/// Counters for how much I/O has happened. The crash fuzzer will use these to
/// decide where to kill the process.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct IoStats {
    /// Page reads issued.
    pub reads: u64,
    /// Page writes issued.
    pub writes: u64,
    /// Syncs issued.
    pub syncs: u64,
}

/// Reads and writes pages, and owns the freelist.
#[derive(Debug)]
pub struct Pager {
    file: File,
    meta: Meta,
    stats: IoStats,
    /// Set only under the crash fuzzer. Pages written but not yet made durable,
    /// which a simulated power loss discards. Ordered so that a partial flush
    /// is reproducible from a seed.
    pending: BTreeMap<PageId, Box<Page>>,
    sim: Option<CrashHandle>,
    truncated_pages: u64,
}

impl Pager {
    /// Creates a new database file. Fails if the path already exists.
    pub fn create(path: impl AsRef<Path>) -> Result<Pager> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(path)?;
        let mut pager = Pager {
            file,
            meta: Meta::new(),
            stats: IoStats::default(),
            pending: BTreeMap::new(),
            sim: None,
            truncated_pages: 0,
        };
        pager.flush_meta()?;
        pager.sync()?;
        Ok(pager)
    }

    /// Opens an existing database file, validating it before returning.
    ///
    /// The checks run in order and fail immediately: signature, checksum,
    /// format version, page size, then file length against the page count.
    pub fn open(path: impl AsRef<Path>) -> Result<Pager> {
        let file = OpenOptions::new().read(true).write(true).open(path)?;

        let length = file.metadata()?.len();
        if length < PAGE_SIZE as u64 {
            return Err(Error::MalformedFile(format!(
                "file is {length} bytes, shorter than one page"
            )));
        }
        if length % PAGE_SIZE as u64 != 0 {
            return Err(Error::MalformedFile(format!(
                "file is {length} bytes, not a whole number of pages"
            )));
        }

        let mut page = Page::zeroed();
        read_exact_at(&file, page.as_bytes_mut(), 0)?;
        let meta = Meta::read_from(&page)?;

        // A file shorter than the metadata claims is a normal thing to find
        // after a crash: the page count reached the disk and some of the pages
        // it counts did not. The missing pages are blank here and recovery
        // fills in whatever the log has to say about them.
        let pages_on_disk = length / PAGE_SIZE as u64;
        let short_by = (meta.page_count as u64).saturating_sub(pages_on_disk);

        let mut pager = Pager {
            file,
            meta,
            stats: IoStats {
                reads: 1,
                ..IoStats::default()
            },
            truncated_pages: 0,
            pending: BTreeMap::new(),
            sim: None,
        };
        if short_by > 0 {
            let blank = Page::zeroed();
            for id in pages_on_disk as u32..pager.meta.page_count {
                write_all_at(&pager.file, blank.as_bytes(), offset_of(id))?;
            }
            pager.file.sync_all()?;
            pager.truncated_pages = short_by;
        }
        Ok(pager)
    }

    /// How many pages the file was missing when it was opened, relative to what
    /// the metadata claimed. Non-zero only after a crash.
    pub fn truncated_pages(&self) -> u64 {
        self.truncated_pages
    }

    /// Opens the file if it exists, creating it otherwise.
    pub fn open_or_create(path: impl AsRef<Path>) -> Result<Pager> {
        let path = path.as_ref();
        if path.exists() {
            Pager::open(path)
        } else {
            Pager::create(path)
        }
    }

    /// The current metadata.
    pub fn meta(&self) -> &Meta {
        &self.meta
    }

    /// The metadata, mutable. Changes are not durable until [`Pager::sync`].
    pub fn meta_mut(&mut self) -> &mut Meta {
        &mut self.meta
    }

    /// How many pages the database has allocated, page 0 included.
    pub fn page_count(&self) -> u32 {
        self.meta.page_count
    }

    /// I/O counters since the pager was opened.
    pub fn stats(&self) -> IoStats {
        self.stats
    }

    /// Arms a simulated power loss. Only the crash fuzzer does this.
    pub fn arm_crash_sim(&mut self, sim: CrashHandle) {
        self.sim = Some(sim);
    }

    /// Whether the simulated power has been cut.
    pub fn crashed(&self) -> bool {
        self.sim
            .as_ref()
            .map(|sim| sim.borrow().crashed())
            .unwrap_or(false)
    }

    /// Reads page `id` into `page`.
    pub fn read_page(&mut self, id: PageId, page: &mut Page) -> Result<()> {
        if id >= self.meta.page_count {
            return Err(Error::PageOutOfRange(id));
        }
        // A process sees its own writes even before they are durable, so the
        // simulation has to serve them back.
        if let Some(held) = self.pending.get(&id) {
            page.as_bytes_mut().copy_from_slice(held.as_bytes());
            self.stats.reads += 1;
            return Ok(());
        }
        read_exact_at(&self.file, page.as_bytes_mut(), offset_of(id))?;
        self.stats.reads += 1;
        Ok(())
    }

    /// Writes `page` to page `id`.
    pub fn write_page(&mut self, id: PageId, page: &Page) -> Result<()> {
        if id >= self.meta.page_count {
            return Err(Error::PageOutOfRange(id));
        }
        self.put_page(id, page)
    }

    fn put_page(&mut self, id: PageId, page: &Page) -> Result<()> {
        if self.sim.is_some() {
            // Held back. Under a power loss model a write means nothing until a
            // sync makes it durable.
            self.pending.insert(id, Box::new(page.clone()));
            self.stats.writes += 1;
            return Ok(());
        }
        write_all_at(&self.file, page.as_bytes(), offset_of(id))?;
        self.stats.writes += 1;
        Ok(())
    }

    /// Hands out a page, reusing one from the freelist when possible.
    ///
    /// The returned page is not initialized; the caller decides its type.
    pub fn allocate(&mut self) -> Result<PageId> {
        if self.meta.freelist_head != NO_PAGE {
            let id = self.meta.freelist_head;
            let mut page = Page::zeroed();
            self.read_page(id, &mut page)?;
            self.meta.freelist_head = page.extra();
            self.meta.freelist_count -= 1;
            return Ok(id);
        }

        let id = self.meta.page_count;
        self.meta.page_count += 1;
        let page = Page::zeroed();
        self.write_page(id, &page)?;
        Ok(id)
    }

    /// Extends the file until it holds at least `count` pages.
    ///
    /// Recovery needs this: the metadata page is only made durable at a
    /// checkpoint, so after a crash it can name fewer pages than the log
    /// refers to. Unlike [`Pager::allocate`] this never touches the freelist,
    /// because the caller has a specific page number in mind.
    pub fn ensure_page_count(&mut self, count: u32) -> Result<()> {
        if self.meta.page_count >= count {
            return Ok(());
        }
        // Only ever extends. A page that is already in the file keeps whatever
        // it holds: after a crash its content may be the only copy of something
        // the log no longer describes, and blanking it would destroy that.
        let on_disk = (self.file.metadata()?.len() / PAGE_SIZE as u64) as u32;
        let blank = Page::zeroed();
        while self.meta.page_count < count {
            let id = self.meta.page_count;
            self.meta.page_count += 1;
            if id >= on_disk {
                self.put_page(id, &blank)?;
            }
        }
        Ok(())
    }

    /// Returns a page to the freelist. The file never shrinks; see
    /// `docs/en/03-pager.md` on why compaction is out of scope.
    pub fn free(&mut self, id: PageId) -> Result<()> {
        if id == META_PAGE {
            return Err(Error::CannotFreeMetaPage);
        }
        if id >= self.meta.page_count {
            return Err(Error::PageOutOfRange(id));
        }

        let mut page = Page::zeroed();
        page.init(PageType::Freelist);
        page.set_extra(self.meta.freelist_head);
        self.write_page(id, &page)?;

        self.meta.freelist_head = id;
        self.meta.freelist_count += 1;
        Ok(())
    }

    /// Writes the metadata page. Not durable until [`Pager::sync`].
    pub fn flush_meta(&mut self) -> Result<()> {
        let mut page = Page::zeroed();
        self.meta.write_to(&mut page);
        self.put_page(META_PAGE, &page)
    }

    /// Flushes the metadata page and forces everything to the physical medium.
    pub fn sync(&mut self) -> Result<()> {
        self.flush_meta()?;

        if let Some(sim) = self.sim.clone() {
            let mut held = std::mem::take(&mut self.pending);

            // The metadata page counts the others, so it goes down last and
            // only if all of them made it. A page count that reaches the disk
            // ahead of the pages it counts describes a file that does not
            // exist — which is exactly what the fuzzer caught the first time it
            // ran.
            let meta_page = held.remove(&META_PAGE);
            let Some(landing) = sim.borrow_mut().admit(held.len()) else {
                return Ok(());
            };

            let complete = landing == held.len();
            for (id, page) in held.into_iter().take(landing) {
                write_all_at(&self.file, page.as_bytes(), offset_of(id))?;
            }
            if complete {
                if let Some(page) = meta_page {
                    write_all_at(&self.file, page.as_bytes(), offset_of(META_PAGE))?;
                }
            }
            self.file.sync_all()?;
            self.stats.syncs += 1;
            return Ok(());
        }

        self.file.sync_all()?;
        self.stats.syncs += 1;
        Ok(())
    }

    /// Checks the invariants the pager is responsible for.
    pub fn check_invariants(&self) -> Result<()> {
        let on_disk = self.file.metadata()?.len() / PAGE_SIZE as u64;
        if (self.meta.page_count as u64) > on_disk {
            return Err(Error::MalformedFile(format!(
                "page_count {} exceeds the {on_disk} pages on disk",
                self.meta.page_count
            )));
        }
        if self.meta.freelist_count >= self.meta.page_count {
            return Err(Error::MalformedFile(format!(
                "freelist holds {} of {} pages",
                self.meta.freelist_count, self.meta.page_count
            )));
        }
        Ok(())
    }

    /// Walks the freelist, returning its pages in order.
    ///
    /// Used by tests to confirm the list is well formed and the right length.
    pub fn freelist(&mut self) -> Result<Vec<PageId>> {
        let mut out = Vec::new();
        let mut current = self.meta.freelist_head;
        let mut page = Page::zeroed();
        while current != NO_PAGE {
            out.push(current);
            if out.len() > self.meta.page_count as usize {
                return Err(Error::MalformedFile("freelist contains a cycle".into()));
            }
            self.read_page(current, &mut page)?;
            current = page.extra();
        }
        Ok(out)
    }
}

fn offset_of(id: PageId) -> u64 {
    id as u64 * PAGE_SIZE as u64
}

fn put_u16(bytes: &mut [u8], offset: usize, value: u16) {
    bytes[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
}

fn put_u32(bytes: &mut [u8], offset: usize, value: u32) {
    bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}

fn put_u64(bytes: &mut [u8], offset: usize, value: u64) {
    bytes[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
}

fn get_u16(bytes: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes([bytes[offset], bytes[offset + 1]])
}

fn get_u32(bytes: &[u8], offset: usize) -> u32 {
    let mut buf = [0u8; 4];
    buf.copy_from_slice(&bytes[offset..offset + 4]);
    u32::from_le_bytes(buf)
}

fn get_u64(bytes: &[u8], offset: usize) -> u64 {
    let mut buf = [0u8; 8];
    buf.copy_from_slice(&bytes[offset..offset + 8]);
    u64::from_le_bytes(buf)
}

// -- positional I/O --------------------------------------------------------
//
// `pread`/`pwrite` on Unix, `seek_read`/`seek_write` on Windows, and the same
// pair again under WASI, which is how the engine runs in a browser. All of them
// take the offset as an argument, so the pager never owns a file cursor.

#[cfg(unix)]
fn read_exact_at(file: &File, buf: &mut [u8], offset: u64) -> io::Result<()> {
    use std::os::unix::fs::FileExt;
    file.read_exact_at(buf, offset)
}

#[cfg(unix)]
fn write_all_at(file: &File, buf: &[u8], offset: u64) -> io::Result<()> {
    use std::os::unix::fs::FileExt;
    file.write_all_at(buf, offset)
}

#[cfg(target_os = "wasi")]
fn read_exact_at(file: &File, buf: &mut [u8], offset: u64) -> io::Result<()> {
    use std::os::wasi::fs::FileExt;
    file.read_exact_at(buf, offset)
}

#[cfg(target_os = "wasi")]
fn write_all_at(file: &File, buf: &[u8], offset: u64) -> io::Result<()> {
    use std::os::wasi::fs::FileExt;
    file.write_all_at(buf, offset)
}

#[cfg(windows)]
fn read_exact_at(file: &File, buf: &mut [u8], offset: u64) -> io::Result<()> {
    use std::os::windows::fs::FileExt;
    let mut done = 0usize;
    while done < buf.len() {
        let read = file.seek_read(&mut buf[done..], offset + done as u64)?;
        if read == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "short read from database file",
            ));
        }
        done += read;
    }
    Ok(())
}

#[cfg(windows)]
fn write_all_at(file: &File, buf: &[u8], offset: u64) -> io::Result<()> {
    use std::os::windows::fs::FileExt;
    let mut done = 0usize;
    while done < buf.len() {
        let written = file.seek_write(&buf[done..], offset + done as u64)?;
        if written == 0 {
            return Err(io::Error::new(
                io::ErrorKind::WriteZero,
                "short write to database file",
            ));
        }
        done += written;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn fresh() -> (tempfile::TempDir, Pager) {
        let dir = tempdir().unwrap();
        let pager = Pager::create(dir.path().join("t.lastro")).unwrap();
        (dir, pager)
    }

    #[test]
    fn a_new_database_has_only_the_meta_page() {
        let (_dir, pager) = fresh();
        assert_eq!(pager.page_count(), 1);
        assert_eq!(pager.meta().freelist_head, NO_PAGE);
        assert_eq!(pager.meta().format_version, FORMAT_VERSION);
        pager.check_invariants().unwrap();
    }

    #[test]
    fn creating_twice_fails() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("t.lastro");
        Pager::create(&path).unwrap();
        assert!(Pager::create(&path).is_err());
    }

    #[test]
    fn pages_round_trip_through_the_file() {
        let (_dir, mut pager) = fresh();
        let id = pager.allocate().unwrap();

        let mut written = Page::zeroed();
        written.init(PageType::Heap);
        written.push_cell(b"durable enough for now").unwrap();
        written.set_lsn(99);
        pager.write_page(id, &written).unwrap();

        let mut read = Page::zeroed();
        pager.read_page(id, &mut read).unwrap();
        assert_eq!(read.cell(0), Some(&b"durable enough for now"[..]));
        assert_eq!(read.lsn(), 99);
        assert_eq!(read.page_type(), Some(PageType::Heap));
    }

    #[test]
    fn metadata_survives_reopening() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("t.lastro");

        let mut pager = Pager::create(&path).unwrap();
        let id = pager.allocate().unwrap();
        pager.meta_mut().catalog_root = id;
        pager.meta_mut().next_txid = 4242;
        pager.sync().unwrap();
        drop(pager);

        let pager = Pager::open(&path).unwrap();
        assert_eq!(pager.meta().catalog_root, id);
        assert_eq!(pager.meta().next_txid, 4242);
        assert_eq!(pager.page_count(), 2);
    }

    #[test]
    fn freed_pages_come_back_in_reverse_order() {
        let (_dir, mut pager) = fresh();
        let a = pager.allocate().unwrap();
        let b = pager.allocate().unwrap();
        let c = pager.allocate().unwrap();
        assert_eq!((a, b, c), (1, 2, 3));

        pager.free(a).unwrap();
        pager.free(b).unwrap();
        assert_eq!(pager.meta().freelist_count, 2);
        assert_eq!(pager.freelist().unwrap(), vec![b, a]);

        assert_eq!(pager.allocate().unwrap(), b);
        assert_eq!(pager.allocate().unwrap(), a);
        assert_eq!(pager.meta().freelist_count, 0);
        assert_eq!(pager.page_count(), 4, "the file must not have grown");
        pager.check_invariants().unwrap();
    }

    #[test]
    fn the_meta_page_cannot_be_freed() {
        let (_dir, mut pager) = fresh();
        assert!(matches!(
            pager.free(META_PAGE),
            Err(Error::CannotFreeMetaPage)
        ));
    }

    #[test]
    fn reading_past_the_end_is_an_error() {
        let (_dir, mut pager) = fresh();
        let mut page = Page::zeroed();
        assert!(matches!(
            pager.read_page(99, &mut page),
            Err(Error::PageOutOfRange(99))
        ));
    }

    #[test]
    fn a_corrupted_meta_page_is_detected() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("t.lastro");
        let mut pager = Pager::create(&path).unwrap();
        pager.meta_mut().next_txid = 7;
        pager.sync().unwrap();
        drop(pager);

        // Flip one byte inside the checksummed region.
        let mut bytes = std::fs::read(&path).unwrap();
        bytes[META_OFF_NEXT_TXID] ^= 0xFF;
        std::fs::write(&path, &bytes).unwrap();

        assert!(matches!(
            Pager::open(&path),
            Err(Error::ChecksumMismatch { page: 0 })
        ));
    }

    #[test]
    fn a_foreign_file_is_rejected() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("not-a-database");
        std::fs::write(&path, vec![0u8; PAGE_SIZE]).unwrap();
        assert!(matches!(Pager::open(&path), Err(Error::BadMagic)));
    }

    #[test]
    fn a_truncated_file_is_rejected() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("t.lastro");
        Pager::create(&path).unwrap();
        std::fs::write(&path, vec![0u8; 100]).unwrap();
        assert!(matches!(Pager::open(&path), Err(Error::MalformedFile(_))));
    }
}
