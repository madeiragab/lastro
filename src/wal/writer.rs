//! Appending to the log, and the durability rule that makes it worth writing.

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use super::record::{Record, RecordBody};
use crate::storage::crash::CrashHandle;
use crate::{Lsn, Result, TxId};

/// Counters for what the log has done. The crash fuzzer uses these to pick
/// where to kill the process.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct WalStats {
    /// Records appended.
    pub appended: u64,
    /// Writes issued to the log file.
    pub writes: u64,
    /// Syncs issued.
    pub syncs: u64,
}

/// The write-ahead log.
///
/// Append-only and strictly sequential, which is what makes a commit cost one
/// sequential `fsync` rather than a scattering of random writes.
#[derive(Debug)]
pub struct Wal {
    file: File,
    path: PathBuf,
    /// The LSN the file's first byte stands for.
    ///
    /// A checkpoint empties the file but must not restart the numbering: pages
    /// on disk carry the LSNs they were stamped with, and a redo pass compares
    /// against them. Restarting at zero makes every one of those pages look
    /// newer than the log and redo skips the lot, silently. So the file offset
    /// is `lsn - base`, and `base` is kept in the metadata page.
    base: Lsn,
    /// The next LSN to hand out. `base` plus the file's length.
    end: Lsn,
    /// Appended but not yet written to the file.
    pending: Vec<u8>,
    /// Every record below this offset is on the physical medium.
    durable: Lsn,
    stats: WalStats,
    /// Set only under the crash fuzzer.
    sim: Option<CrashHandle>,
}

impl Wal {
    /// The conventional log path beside a database file.
    pub fn path_for(database: impl AsRef<Path>) -> PathBuf {
        let mut path = database.as_ref().as_os_str().to_os_string();
        path.push(".wal");
        PathBuf::from(path)
    }

    /// Opens the log at `path`, creating it if it does not exist.
    ///
    /// `base` is the LSN the file's first byte stands for, which the caller
    /// reads from the metadata page's `last_checkpoint_lsn`.
    ///
    /// New records append after whatever is already there. Recovery runs before
    /// this in the normal flow, so anything present is a prefix recovery has
    /// already accounted for.
    pub fn open(path: impl AsRef<Path>, base: Lsn) -> Result<Wal> {
        let path = path.as_ref().to_path_buf();
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)?;
        let end = base + file.seek(SeekFrom::End(0))?;
        Ok(Wal {
            file,
            path,
            base,
            end,
            pending: Vec::new(),
            durable: end,
            stats: WalStats::default(),
            sim: None,
        })
    }

    /// The LSN the file's first byte stands for. Belongs in the metadata page.
    pub fn base_lsn(&self) -> Lsn {
        self.base
    }

    /// Where the log lives.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The LSN the next record will be given.
    pub fn end_lsn(&self) -> Lsn {
        self.end
    }

    /// Every record below this offset is on the physical medium.
    pub fn durable_lsn(&self) -> Lsn {
        self.durable
    }

    /// What the log has done so far.
    pub fn stats(&self) -> WalStats {
        self.stats
    }

    /// Appends a record and returns its LSN. Not durable until [`Wal::sync`].
    pub fn append(&mut self, txid: TxId, prev_lsn: Lsn, body: RecordBody) -> Result<Lsn> {
        let lsn = self.end;
        let record = Record {
            lsn,
            txid,
            prev_lsn,
            body,
        };
        let written = record.encode(&mut self.pending);
        self.end += written as Lsn;
        self.stats.appended += 1;
        Ok(lsn)
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

    /// Writes the pending tail to the file without forcing it to the medium.
    pub fn flush(&mut self) -> Result<()> {
        // Under a power loss model a write that was never synced is lost, so
        // there is nothing to gain by moving the bytes to the file early. They
        // wait, and the sync decides how much of them survives.
        if self.sim.is_some() || self.pending.is_empty() {
            return Ok(());
        }
        self.file.seek(SeekFrom::End(0))?;
        self.file.write_all(&self.pending)?;
        self.pending.clear();
        self.stats.writes += 1;
        Ok(())
    }

    /// Writes the pending tail and forces it to the physical medium.
    ///
    /// This is the only `fsync` on the commit path, and it is sequential.
    pub fn sync(&mut self) -> Result<()> {
        if let Some(sim) = self.sim.clone() {
            let waiting = self.pending.len();
            let Some(landing) = sim.borrow_mut().admit(waiting) else {
                return Ok(());
            };

            // A partial landing is the torn tail: the record that was being
            // written when the power went. Its checksum will not verify, and
            // recovery is supposed to treat that as the end of the log.
            let offset = (self.end - self.base) - waiting as Lsn;
            self.file.seek(SeekFrom::Start(offset))?;
            self.file.write_all(&self.pending[..landing])?;
            self.file.sync_all()?;
            self.stats.writes += 1;
            self.stats.syncs += 1;

            if landing == waiting {
                self.pending.clear();
                self.durable = self.end;
            }
            return Ok(());
        }

        self.flush()?;
        self.file.sync_all()?;
        self.durable = self.end;
        self.stats.syncs += 1;
        Ok(())
    }

    /// Makes the log durable at least as far as the record that produced `lsn`.
    ///
    /// **This is the WAL rule.** The buffer pool calls it before writing any
    /// dirty page, so the record describing a change always reaches the medium
    /// before the page holding it. Skip this call and everything still passes in
    /// testing, and the database corrupts silently at the first power loss.
    pub fn sync_through(&mut self, lsn: Lsn) -> Result<()> {
        // `durable` is the end of the durable prefix, so a record at `lsn` is
        // safe once `durable` is strictly past it.
        if self.durable <= lsn {
            self.sync()?;
        }
        Ok(())
    }

    /// Reads every intact record from the front of the log.
    ///
    /// Stops at the first record that does not verify. The last record after a
    /// crash is almost always half written, and that is a normal thing to find:
    /// a commit is only reported to the caller after [`Wal::sync`], so a record
    /// that did not survive its checksum belongs to a transaction nobody was
    /// ever told had committed.
    ///
    /// Returns the records and the length of the intact prefix. Whatever lies
    /// past that in the file is the torn tail, and the caller truncates it
    /// before appending anything of its own.
    ///
    /// Reads the whole log into memory. Bounded by how often
    /// [`Wal::checkpoint_truncate`] runs, and the note in
    /// `docs/en/05-wal-recovery.md` explains why that is enough for now.
    pub fn read_all(path: impl AsRef<Path>, base: Lsn) -> Result<(Vec<Record>, Lsn)> {
        let mut file = match File::open(path.as_ref()) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok((Vec::new(), base))
            }
            Err(error) => return Err(error.into()),
        };

        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)?;

        let mut records = Vec::new();
        let mut offset = 0usize;
        while offset < bytes.len() {
            match Record::decode(&bytes[offset..])? {
                Some((record, read)) => {
                    // A record whose stored LSN disagrees with where it sits is
                    // not a torn tail, it is a log that has been rearranged.
                    if record.lsn != base + offset as Lsn {
                        return Err(crate::Error::MalformedFile(format!(
                            "record at offset {offset} of a log based at {base} claims lsn {}",
                            record.lsn
                        )));
                    }
                    records.push(record);
                    offset += read;
                }
                None => break,
            }
        }
        Ok((records, base + offset as Lsn))
    }

    /// Cuts the log back to `len`, discarding a torn tail.
    ///
    /// Called by recovery before it appends anything, so that the compensation
    /// records it writes land at the offsets their own LSNs claim.
    pub fn truncate_to(&mut self, lsn: Lsn) -> Result<()> {
        let offset = lsn.saturating_sub(self.base);
        self.pending.clear();
        self.file.set_len(offset)?;
        self.file.seek(SeekFrom::Start(offset))?;
        self.file.sync_all()?;
        self.end = self.base + offset;
        self.durable = self.end;
        self.stats.syncs += 1;
        Ok(())
    }

    /// Empties the log.
    ///
    /// Only safe once every page the log describes is on disk. The caller is
    /// [`crate::wal::checkpoint`], which flushes first.
    ///
    /// This is a *sharp* checkpoint: it stops the world for the duration. The
    /// specification calls for a fuzzy one that runs alongside normal work,
    /// which matters under load and does not matter yet. The deviation is
    /// recorded in `docs/en/05-wal-recovery.md`.
    pub fn checkpoint_truncate(&mut self) -> Result<()> {
        self.pending.clear();
        self.file.set_len(0)?;
        self.file.seek(SeekFrom::Start(0))?;
        self.file.sync_all()?;
        // The file restarts; the numbering does not. The caller must record the
        // new base in the metadata page before anything else happens.
        self.base = self.end;
        self.durable = self.end;
        self.stats.syncs += 1;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wal::record::PageEdit;
    use tempfile::tempdir;

    fn edit(page: u32, byte: u8) -> RecordBody {
        RecordBody::Update(PageEdit::new(page, 64, vec![0; 4], vec![byte; 4]))
    }

    #[test]
    fn lsns_are_offsets_and_records_read_back() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("t.wal");

        let mut wal = Wal::open(&path, 0).unwrap();
        let first = wal.append(1, 0, RecordBody::Begin).unwrap();
        let second = wal.append(1, first, edit(7, 9)).unwrap();
        let third = wal.append(1, second, RecordBody::Commit).unwrap();
        wal.sync().unwrap();

        assert_eq!(first, 0);
        assert!(second > first && third > second);
        assert_eq!(wal.end_lsn(), wal.durable_lsn());

        let (records, intact) = Wal::read_all(&path, 0).unwrap();
        assert_eq!(intact, wal.end_lsn(), "nothing was torn");
        assert_eq!(records.len(), 3);
        assert_eq!(records[0].lsn, first);
        assert_eq!(records[1].prev_lsn, first);
        assert_eq!(records[2].body, RecordBody::Commit);
    }

    #[test]
    fn appending_reopens_where_it_left_off() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("t.wal");

        let end = {
            let mut wal = Wal::open(&path, 0).unwrap();
            wal.append(1, 0, RecordBody::Begin).unwrap();
            wal.sync().unwrap();
            wal.end_lsn()
        };

        let mut wal = Wal::open(&path, 0).unwrap();
        assert_eq!(wal.end_lsn(), end);
        let next = wal.append(2, 0, RecordBody::Begin).unwrap();
        assert_eq!(next, end);
        wal.sync().unwrap();

        let (records, _) = Wal::read_all(&path, 0).unwrap();
        assert_eq!(records.len(), 2);
    }

    #[test]
    fn a_torn_tail_is_discarded_not_reported_as_corruption() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("t.wal");

        let mut wal = Wal::open(&path, 0).unwrap();
        wal.append(1, 0, RecordBody::Begin).unwrap();
        let second = wal.append(1, 0, edit(7, 9)).unwrap();
        wal.append(1, second, RecordBody::Commit).unwrap();
        wal.sync().unwrap();
        let full = wal.end_lsn();
        drop(wal);

        // Cut the file at every byte inside the last record. Each cut must read
        // as a shorter but intact log, never as an error.
        for cut in second..full {
            let bytes = std::fs::read(&path).unwrap();
            std::fs::write(&path, &bytes[..cut as usize]).unwrap();

            let (records, _) = Wal::read_all(&path, 0).unwrap();
            assert!(
                records.len() <= 3,
                "a truncated log must not gain records at cut {cut}"
            );
            for record in &records {
                assert!(record.lsn < cut, "a record must lie fully within the file");
            }
            std::fs::write(&path, &bytes).unwrap();
        }
    }

    #[test]
    fn sync_through_only_syncs_when_it_has_to() {
        let dir = tempdir().unwrap();
        let mut wal = Wal::open(dir.path().join("t.wal"), 0).unwrap();

        let lsn = wal.append(1, 0, edit(1, 1)).unwrap();
        assert_eq!(wal.stats().syncs, 0);

        wal.sync_through(lsn).unwrap();
        assert_eq!(wal.stats().syncs, 1, "an unsynced record forces a sync");
        assert!(wal.durable_lsn() > lsn);

        wal.sync_through(lsn).unwrap();
        assert_eq!(
            wal.stats().syncs,
            1,
            "a durable record needs no second sync"
        );
    }

    #[test]
    fn a_checkpoint_empties_the_log() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("t.wal");

        let mut wal = Wal::open(&path, 0).unwrap();
        wal.append(1, 0, edit(1, 1)).unwrap();
        wal.sync().unwrap();
        let before = wal.end_lsn();
        wal.checkpoint_truncate().unwrap();

        // The file is empty but the numbering carries on, so that pages already
        // stamped with an old LSN do not look newer than the log.
        assert_eq!(wal.base_lsn(), before);
        assert_eq!(wal.end_lsn(), before);
        let (records, intact) = Wal::read_all(&path, wal.base_lsn()).unwrap();
        assert!(records.is_empty());
        assert_eq!(intact, before);

        // And the log keeps working afterwards.
        assert_eq!(wal.append(2, 0, RecordBody::Begin).unwrap(), before);
        wal.sync().unwrap();
        assert_eq!(Wal::read_all(&path, before).unwrap().0.len(), 1);
    }
}
