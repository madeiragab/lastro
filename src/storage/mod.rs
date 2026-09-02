//! The storage layer: pages, the pager, and the buffer pool.
//!
//! Nothing here knows what a key, a tuple or a transaction is. See
//! `docs/en/01-architecture.md` for why that separation is enforced.

pub mod buffer;
pub mod crash;
pub mod page;
pub mod pager;

pub use buffer::{BufferPool, PinnedPage};
pub use crash::{CrashHandle, CrashSim};
pub use page::{Page, PageType};
pub use pager::{Meta, Pager};
