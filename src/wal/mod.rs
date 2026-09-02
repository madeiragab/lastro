//! The write-ahead log: the layer that turns durable structures into a database.
//!
//! Writing a 4 KB page is not atomic. Between the `write` and the bytes
//! settling there is the operating system's cache, the controller's, and the
//! drive's own. A transaction that touches three pages cannot write all three
//! at once. So before any page changes, what is about to change is written to a
//! sequential log, and after a crash the log answers every question:
//!
//! - a `Commit` in the log but the page unwritten? **redo**
//! - no `Commit` but the page already written? **undo**
//! - a record whose checksum fails? the log ends there
//!
//! See `docs/en/05-wal-recovery.md`.

pub mod record;
pub mod recovery;
pub mod writer;

pub use record::{PageEdit, Record, RecordBody, RECORD_HEADER_SIZE};
pub use recovery::{recover, RecoveryReport};
pub use writer::{Wal, WalStats};

use crate::storage::BufferPool;
use crate::Result;

/// Makes every page the log describes durable, then starts the log over.
///
/// The order is the whole point and is not negotiable: pages first, log second.
/// Truncating the log before the pages it describes are on disk throws away the
/// only record of changes that exist nowhere else.
///
/// This is a *sharp* checkpoint — it does its work with nothing else running.
/// The specification calls for a fuzzy one, which matters under load and does
/// not matter yet; the deviation is recorded in `docs/en/05-wal-recovery.md`.
pub fn checkpoint(pool: &mut BufferPool) -> Result<()> {
    // Writes every dirty page and syncs the data file. Each write consults the
    // log first, through the WAL rule in the buffer pool's eviction path.
    pool.flush_all()?;
    let base = match pool.wal_mut() {
        Some(wal) => {
            wal.checkpoint_truncate()?;
            Some(wal.base_lsn())
        }
        None => None,
    };
    // The numbering continues across the truncation, and where it continues
    // from has to be durable: a log that restarted at zero would look older
    // than every page on disk, and redo would skip all of it in silence.
    if let Some(base) = base {
        pool.pager_mut().meta_mut().last_checkpoint_lsn = base;
    }
    // Only now, with the log empty, is it safe to write freelist headers over
    // the pages committed transactions gave up: nothing is left to replay on
    // top of them.
    pool.release_pending_frees()?;
    pool.pager_mut().sync()?;
    Ok(())
}
