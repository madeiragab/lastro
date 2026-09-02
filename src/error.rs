//! The crate's error type.
//!
//! Hand-written rather than derived, so the library keeps zero dependencies.

use std::fmt;
use std::io;

use crate::PageId;

/// The result type used throughout the crate.
pub type Result<T> = std::result::Result<T, Error>;

/// Everything that can go wrong below the SQL layer.
#[derive(Debug)]
pub enum Error {
    /// An underlying I/O failure.
    Io(io::Error),

    /// The file does not start with the `lastro` signature.
    BadMagic,

    /// The file was written by a format version this build does not understand.
    UnsupportedVersion(u16),

    /// The file declares a page size other than [`crate::PAGE_SIZE`].
    UnsupportedPageSize(u16),

    /// A page's checksum does not match its contents.
    ChecksumMismatch { page: PageId },

    /// The file length is not a whole number of pages, or disagrees with the
    /// page count recorded in the metadata page.
    MalformedFile(String),

    /// A read or write named a page beyond the end of the file.
    PageOutOfRange(PageId),

    /// A cell does not fit in the page, even after compaction.
    PageFull,

    /// A cell is larger than a page can ever hold inline. Oversized payloads
    /// belong in an overflow chain.
    CellTooLarge(usize),

    /// A slot index does not exist in this page.
    NoSuchSlot(u16),

    /// Every frame in the buffer pool is pinned, so nothing can be evicted.
    /// Always a forgotten unpin, never a normal condition.
    AllFramesPinned,

    /// A page was handed back while another pin on it was still outstanding.
    PageStillPinned(PageId),

    /// Page 0 holds the database metadata and can never be freed.
    CannotFreeMetaPage,

    /// A key could not be encoded; currently only `NaN`.
    InvalidKey(&'static str),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Io(e) => write!(f, "io error: {e}"),
            Error::BadMagic => write!(f, "not a lastro database: bad magic"),
            Error::UnsupportedVersion(v) => {
                let known = crate::FORMAT_VERSION;
                write!(
                    f,
                    "unsupported format version {v}, this build understands {known}"
                )
            }
            Error::UnsupportedPageSize(s) => {
                let want = crate::PAGE_SIZE;
                write!(f, "unsupported page size {s}, this build requires {want}")
            }
            Error::ChecksumMismatch { page } => write!(f, "checksum mismatch on page {page}"),
            Error::MalformedFile(why) => write!(f, "malformed database file: {why}"),
            Error::PageOutOfRange(p) => write!(f, "page {p} is beyond the end of the file"),
            Error::PageFull => write!(f, "page is full"),
            Error::CellTooLarge(n) => write!(f, "cell of {n} bytes exceeds the inline maximum"),
            Error::NoSuchSlot(s) => write!(f, "slot {s} does not exist in this page"),
            Error::AllFramesPinned => {
                write!(
                    f,
                    "every buffer pool frame is pinned; this is a missing unpin"
                )
            }
            Error::PageStillPinned(p) => write!(f, "page {p} is still pinned elsewhere"),
            Error::CannotFreeMetaPage => write!(f, "the metadata page cannot be freed"),
            Error::InvalidKey(why) => write!(f, "invalid key: {why}"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<io::Error> for Error {
    fn from(e: io::Error) -> Self {
        Error::Io(e)
    }
}
