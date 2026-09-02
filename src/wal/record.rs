//! The write-ahead log record format.
//!
//! ```text
//! offset  size  field
//!   0       8   lsn         this record's own offset in the log file
//!   8       8   txid
//!  16       8   prev_lsn    previous record of the SAME transaction
//!  24       1   rec_type
//!  25       1   flags
//!  26       2   reserved
//!  28       4   body_len
//!  32       4   checksum    CRC32C of the header, with this field zeroed, and the body
//!  36     var   body
//! ```
//!
//! Making the LSN the record's own offset is a deliberate simplification:
//! seeking to an LSN during undo is arithmetic, with no auxiliary index. See
//! `docs/en/05-wal-recovery.md`.

use crate::util::crc32c;
use crate::{Error, Lsn, PageId, Result, TxId};

/// Size of the fixed record header, in bytes.
pub const RECORD_HEADER_SIZE: usize = 36;

const OFF_LSN: usize = 0;
const OFF_TXID: usize = 8;
const OFF_PREV_LSN: usize = 16;
const OFF_TYPE: usize = 24;
const OFF_BODY_LEN: usize = 28;
const OFF_CHECKSUM: usize = 32;

/// Refuses a body larger than this rather than allocating whatever a corrupt
/// length field asks for.
const MAX_BODY_LEN: usize = 1 << 20;

/// What a record says happened.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecordBody {
    /// A transaction started.
    Begin,
    /// A transaction committed. Durable once this record is on disk.
    Commit,
    /// A transaction was rolled back, and its undo is complete.
    Abort,
    /// A range of bytes in a page changed.
    Update(PageEdit),
    /// A compensation record: an undo that already happened.
    ///
    /// Never undone, only redone. `undo_next_lsn` says where undo resumes if
    /// the process dies in the middle of rolling back.
    Clr {
        /// The next record to undo after this one, or zero at the beginning.
        undo_next_lsn: Lsn,
        /// The edit that was reversed.
        edit: PageEdit,
    },
    /// A page was handed out.
    PageAlloc(PageId),
    /// A page was returned to the freelist.
    PageFree(PageId),
}

/// A change to a contiguous range of bytes in one page.
///
/// Physiological logging: logical between pages, physical within a page. The
/// before image is what makes undo possible and the after image is what makes
/// redo idempotent. See `docs/en/adr.md`, ADR-005.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PageEdit {
    /// The page that changed.
    pub page: PageId,
    /// Where in the page the change starts.
    pub offset: u16,
    /// The bytes that were there.
    pub before: Vec<u8>,
    /// The bytes that are there now. Always the same length as `before`.
    pub after: Vec<u8>,
}

impl PageEdit {
    /// The edit that turns `before` into `after` over the same range.
    pub fn new(page: PageId, offset: u16, before: Vec<u8>, after: Vec<u8>) -> PageEdit {
        debug_assert_eq!(
            before.len(),
            after.len(),
            "an edit replaces a range with one of the same length"
        );
        PageEdit {
            page,
            offset,
            before,
            after,
        }
    }

    /// The smallest edit that turns `before` into `after`, or `None` when the
    /// two images are identical.
    ///
    /// This is what keeps whole-page rewrites from costing whole-page log
    /// records: only the bytes that actually moved are written.
    pub fn between(page: PageId, before: &[u8], after: &[u8]) -> Option<PageEdit> {
        debug_assert_eq!(before.len(), after.len());
        let first = before.iter().zip(after).position(|(a, b)| a != b)?;
        let last = before
            .iter()
            .zip(after)
            .rposition(|(a, b)| a != b)
            .expect("a first difference implies a last one");
        Some(PageEdit {
            page,
            offset: first as u16,
            before: before[first..=last].to_vec(),
            after: after[first..=last].to_vec(),
        })
    }

    /// How many bytes the edit covers.
    pub fn len(&self) -> usize {
        self.after.len()
    }

    /// Whether the edit covers nothing.
    pub fn is_empty(&self) -> bool {
        self.after.is_empty()
    }
}

/// One log record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Record {
    /// This record's offset in the log file.
    pub lsn: Lsn,
    /// The transaction that wrote it.
    pub txid: TxId,
    /// The previous record of the same transaction, or zero if it is the first.
    pub prev_lsn: Lsn,
    /// What happened.
    pub body: RecordBody,
}

impl Record {
    /// The record type byte, as stored.
    pub fn type_byte(&self) -> u8 {
        match self.body {
            RecordBody::Begin => 1,
            RecordBody::Update(_) => 2,
            RecordBody::Commit => 3,
            RecordBody::Abort => 4,
            RecordBody::Clr { .. } => 5,
            RecordBody::PageAlloc(_) => 8,
            RecordBody::PageFree(_) => 9,
        }
    }

    /// True when this record ends a transaction.
    pub fn ends_transaction(&self) -> bool {
        matches!(self.body, RecordBody::Commit | RecordBody::Abort)
    }

    /// The page this record touches, if any.
    pub fn page(&self) -> Option<PageId> {
        match &self.body {
            RecordBody::Update(edit) => Some(edit.page),
            RecordBody::Clr { edit, .. } => Some(edit.page),
            _ => None,
        }
    }

    /// The edit this record carries, if any.
    pub fn edit(&self) -> Option<&PageEdit> {
        match &self.body {
            RecordBody::Update(edit) => Some(edit),
            RecordBody::Clr { edit, .. } => Some(edit),
            _ => None,
        }
    }

    /// Appends the encoded record to `out`, and returns how many bytes it took.
    pub fn encode(&self, out: &mut Vec<u8>) -> usize {
        let start = out.len();
        out.resize(start + RECORD_HEADER_SIZE, 0);
        encode_body(&self.body, out);
        let body_len = out.len() - start - RECORD_HEADER_SIZE;

        let header = &mut out[start..start + RECORD_HEADER_SIZE];
        header[OFF_LSN..OFF_LSN + 8].copy_from_slice(&self.lsn.to_le_bytes());
        header[OFF_TXID..OFF_TXID + 8].copy_from_slice(&self.txid.to_le_bytes());
        header[OFF_PREV_LSN..OFF_PREV_LSN + 8].copy_from_slice(&self.prev_lsn.to_le_bytes());
        header[OFF_TYPE] = self.type_byte();
        header[OFF_BODY_LEN..OFF_BODY_LEN + 4].copy_from_slice(&(body_len as u32).to_le_bytes());

        // The checksum covers the header with its own field left zeroed, plus
        // the body. A record whose checksum does not verify is a torn tail.
        let checksum = crc32c(&out[start..]);
        out[start + OFF_CHECKSUM..start + OFF_CHECKSUM + 4]
            .copy_from_slice(&checksum.to_le_bytes());
        out.len() - start
    }

    /// Reads one record from the front of `buf`.
    ///
    /// Returns `Ok(None)` for a torn tail: a buffer too short to hold the
    /// record, or a checksum that does not verify. Both mean the log ends here,
    /// which is a normal thing to find after a crash.
    ///
    /// Returns an error only when the bytes verify but say something impossible,
    /// which would mean corruption rather than truncation.
    pub fn decode(buf: &[u8]) -> Result<Option<(Record, usize)>> {
        if buf.len() < RECORD_HEADER_SIZE {
            return Ok(None);
        }
        let body_len = u32::from_le_bytes([
            buf[OFF_BODY_LEN],
            buf[OFF_BODY_LEN + 1],
            buf[OFF_BODY_LEN + 2],
            buf[OFF_BODY_LEN + 3],
        ]) as usize;
        if body_len > MAX_BODY_LEN {
            return Ok(None);
        }
        let total = RECORD_HEADER_SIZE + body_len;
        if buf.len() < total {
            return Ok(None);
        }

        let stored = u32::from_le_bytes([
            buf[OFF_CHECKSUM],
            buf[OFF_CHECKSUM + 1],
            buf[OFF_CHECKSUM + 2],
            buf[OFF_CHECKSUM + 3],
        ]);
        let mut scratch = buf[..total].to_vec();
        scratch[OFF_CHECKSUM..OFF_CHECKSUM + 4].fill(0);
        if crc32c(&scratch) != stored {
            return Ok(None);
        }

        let lsn = read_u64(buf, OFF_LSN);
        let txid = read_u64(buf, OFF_TXID);
        let prev_lsn = read_u64(buf, OFF_PREV_LSN);
        let body = decode_body(buf[OFF_TYPE], &buf[RECORD_HEADER_SIZE..total])?;

        Ok(Some((
            Record {
                lsn,
                txid,
                prev_lsn,
                body,
            },
            total,
        )))
    }
}

fn encode_body(body: &RecordBody, out: &mut Vec<u8>) {
    match body {
        RecordBody::Begin | RecordBody::Commit | RecordBody::Abort => {}
        RecordBody::Update(edit) => encode_edit(edit, out),
        RecordBody::Clr {
            undo_next_lsn,
            edit,
        } => {
            out.extend_from_slice(&undo_next_lsn.to_le_bytes());
            encode_edit(edit, out);
        }
        RecordBody::PageAlloc(page) | RecordBody::PageFree(page) => {
            out.extend_from_slice(&page.to_le_bytes());
        }
    }
}

fn encode_edit(edit: &PageEdit, out: &mut Vec<u8>) {
    out.extend_from_slice(&edit.page.to_le_bytes());
    out.extend_from_slice(&edit.offset.to_le_bytes());
    out.extend_from_slice(&(edit.before.len() as u16).to_le_bytes());
    out.extend_from_slice(&edit.before);
    out.extend_from_slice(&edit.after);
}

fn decode_body(kind: u8, body: &[u8]) -> Result<RecordBody> {
    match kind {
        1 => Ok(RecordBody::Begin),
        2 => Ok(RecordBody::Update(decode_edit(body)?)),
        3 => Ok(RecordBody::Commit),
        4 => Ok(RecordBody::Abort),
        5 => {
            if body.len() < 8 {
                return Err(corrupt("a CLR body is shorter than its undo_next_lsn"));
            }
            Ok(RecordBody::Clr {
                undo_next_lsn: read_u64(body, 0),
                edit: decode_edit(&body[8..])?,
            })
        }
        8 | 9 => {
            if body.len() < 4 {
                return Err(corrupt("a page record body is shorter than a page id"));
            }
            let page = u32::from_le_bytes([body[0], body[1], body[2], body[3]]);
            Ok(if kind == 8 {
                RecordBody::PageAlloc(page)
            } else {
                RecordBody::PageFree(page)
            })
        }
        other => Err(corrupt(format!("unknown log record type {other}"))),
    }
}

fn decode_edit(body: &[u8]) -> Result<PageEdit> {
    if body.len() < 8 {
        return Err(corrupt("an edit body is shorter than its header"));
    }
    let page = u32::from_le_bytes([body[0], body[1], body[2], body[3]]);
    let offset = u16::from_le_bytes([body[4], body[5]]);
    let length = u16::from_le_bytes([body[6], body[7]]) as usize;

    let images = &body[8..];
    if images.len() != length * 2 {
        return Err(corrupt(format!(
            "an edit claims {length} bytes per image but carries {}",
            images.len()
        )));
    }
    Ok(PageEdit {
        page,
        offset,
        before: images[..length].to_vec(),
        after: images[length..].to_vec(),
    })
}

fn read_u64(buf: &[u8], offset: usize) -> u64 {
    let mut bytes = [0u8; 8];
    bytes.copy_from_slice(&buf[offset..offset + 8]);
    u64::from_le_bytes(bytes)
}

fn corrupt(why: impl Into<String>) -> Error {
    Error::MalformedFile(why.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Vec<Record> {
        let edit = PageEdit::new(7, 128, vec![1, 2, 3, 4], vec![9, 9, 9, 9]);
        vec![
            Record {
                lsn: 0,
                txid: 1,
                prev_lsn: 0,
                body: RecordBody::Begin,
            },
            Record {
                lsn: 36,
                txid: 1,
                prev_lsn: 0,
                body: RecordBody::Update(edit.clone()),
            },
            Record {
                lsn: 200,
                txid: 1,
                prev_lsn: 36,
                body: RecordBody::Clr {
                    undo_next_lsn: 36,
                    edit,
                },
            },
            Record {
                lsn: 400,
                txid: 1,
                prev_lsn: 200,
                body: RecordBody::Commit,
            },
            Record {
                lsn: 440,
                txid: 2,
                prev_lsn: 0,
                body: RecordBody::PageAlloc(12),
            },
            Record {
                lsn: 480,
                txid: 2,
                prev_lsn: 440,
                body: RecordBody::PageFree(12),
            },
            Record {
                lsn: 520,
                txid: 2,
                prev_lsn: 480,
                body: RecordBody::Abort,
            },
        ]
    }

    #[test]
    fn records_round_trip() {
        for record in sample() {
            let mut buf = Vec::new();
            let written = record.encode(&mut buf);
            assert_eq!(written, buf.len());
            let (decoded, read) = Record::decode(&buf).unwrap().unwrap();
            assert_eq!(read, buf.len());
            assert_eq!(decoded, record);
        }
    }

    #[test]
    fn records_decode_back_to_back() {
        let records = sample();
        let mut buf = Vec::new();
        for record in &records {
            record.encode(&mut buf);
        }

        let mut rest = &buf[..];
        for expected in &records {
            let (decoded, read) = Record::decode(rest).unwrap().unwrap();
            assert_eq!(&decoded, expected);
            rest = &rest[read..];
        }
        assert!(rest.is_empty());
    }

    #[test]
    fn a_truncated_record_reads_as_the_end_of_the_log() {
        let mut buf = Vec::new();
        sample()[1].encode(&mut buf);
        for cut in 0..buf.len() {
            assert_eq!(
                Record::decode(&buf[..cut]).unwrap(),
                None,
                "a record cut at {cut} must read as a torn tail"
            );
        }
    }

    #[test]
    fn a_flipped_bit_reads_as_the_end_of_the_log() {
        let mut buf = Vec::new();
        sample()[1].encode(&mut buf);
        let bits = buf.len() * 8;
        for index in 0..bits {
            let (byte, bit) = (index / 8, index % 8);
            buf[byte] ^= 1u8 << bit;
            // A flip either fails the checksum or, in the length field, makes
            // the record look short. Either way it must not decode as valid.
            let decoded = Record::decode(&buf).unwrap();
            assert!(decoded.is_none(), "flip at byte {byte} bit {bit} decoded");
            buf[byte] ^= 1u8 << bit;
        }
    }

    #[test]
    fn the_minimal_edit_covers_only_what_moved() {
        let before = [0u8; 64];
        let mut after = before;
        after[10] = 1;
        after[12] = 1;

        let edit = PageEdit::between(3, &before, &after).unwrap();
        assert_eq!(edit.offset, 10);
        assert_eq!(edit.len(), 3, "10 through 12 inclusive");
        assert_eq!(edit.after, vec![1, 0, 1]);
        assert_eq!(edit.before, vec![0, 0, 0]);
    }

    #[test]
    fn identical_images_produce_no_edit() {
        let page = [7u8; 64];
        assert_eq!(PageEdit::between(3, &page, &page), None);
    }
}
