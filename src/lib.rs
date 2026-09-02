//! `lastro` — an embedded relational database written from scratch.
//!
//! The layers, bottom-up:
//!
//! - [`storage::pager`] — reads and writes fixed-size pages, owns the freelist
//! - [`storage::buffer`] — a bounded cache of pages with a clock replacement policy
//! - [`storage::page`] — the slotted page layout and the on-disk encodings
//! - [`index::btree`] — an ordered map of bytes to bytes, over those pages
//! - [`wal`] — the write-ahead log and ARIES recovery
//! - [`sql`] — the lexer, the syntax tree and the parser
//!
//! What is left is the rest of the SQL layer and MVCC, both specified in
//! `docs/` and not yet implemented. The specification is the contract; see `docs/en/02-file-format.md`
//! for the binary layouts these modules implement.
//!
//! ```
//! use lastro::storage::{BufferPool, Pager};
//! use lastro::storage::page::PageType;
//!
//! let dir = tempfile::tempdir().unwrap();
//! let path = dir.path().join("example.lastro");
//!
//! let pager = Pager::create(&path).unwrap();
//! let mut pool = BufferPool::new(pager, 16);
//!
//! let pin = pool.new_page(PageType::Heap).unwrap();
//! let slot = pool.page_mut(&pin).push_cell(b"hello").unwrap();
//! assert_eq!(pool.page(&pin).cell(slot), Some(&b"hello"[..]));
//! pool.unpin(pin);
//!
//! pool.flush_all().unwrap();
//! ```
//!
//! And the ordered map on top of it:
//!
//! ```
//! use lastro::index::BTree;
//! use lastro::storage::{BufferPool, Pager};
//!
//! let dir = tempfile::tempdir().unwrap();
//! let pager = Pager::create(dir.path().join("cattle.lastro")).unwrap();
//! let mut pool = BufferPool::new(pager, 64);
//!
//! let mut tree = BTree::create(&mut pool).unwrap();
//! for id in 0u32..5_000 {
//!     tree.insert(&mut pool, &id.to_be_bytes(), b"BR-0042").unwrap();
//! }
//!
//! // A range scan descends once and then follows leaf siblings.
//! let window = tree
//!     .range(&mut pool, Some(&100u32.to_be_bytes()), Some(&200u32.to_be_bytes()))
//!     .unwrap();
//! assert_eq!(window.len(), 100);
//!
//! tree.check_tree(&mut pool).unwrap();
//! ```

pub mod error;
pub mod index;
pub mod sql;
pub mod storage;
pub mod util;
pub mod wal;

pub use error::{Error, Result};

/// Size of a page, in bytes. Fixed; see `docs/en/adr.md`, ADR-002.
pub const PAGE_SIZE: usize = 4096;

/// A page number. Page 0 is always the metadata page, which is why 0 doubles as
/// the "no page" sentinel everywhere else.
pub type PageId = u32;

/// A log sequence number. Equal to the record's own offset in the log file.
pub type Lsn = u64;

/// A transaction id.
pub type TxId = u64;

/// The metadata page.
pub const META_PAGE: PageId = 0;

/// The sentinel meaning "no page". Safe because page 0 is never a child, a
/// freelist link or an overflow link.
pub const NO_PAGE: PageId = 0;

/// File signature, stored in the first 8 bytes of page 0.
pub const MAGIC: [u8; 8] = *b"LASTRO\x00\x00";

/// On-disk format version understood by this build.
pub const FORMAT_VERSION: u16 = 1;
