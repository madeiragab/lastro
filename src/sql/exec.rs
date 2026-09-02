//! Evaluating expressions, and running a plan.
//!
//! The iterator model, also called Volcano: every operator is a `next` that
//! pulls one row from the operator below. A query with a filter over a huge
//! table streams through in constant memory; only `Sort` has to materialize,
//! and it is the only one that does.

use std::cmp::Ordering;
use std::collections::HashMap;
use std::fs::File;
use std::io::{BufReader, Read, Seek, SeekFrom, Write};

use crate::index::{BTree, Cursor};
use crate::sql::ast::{BinaryOp, DataType, UnaryOp};
use crate::sql::catalog::{IndexSchema, TableSchema};
use crate::sql::mvcc::{Snapshot, Version};
use crate::sql::plan::{Bound, Plan, PlanExpr};
use crate::storage::page::encoding::{
    decode_i64, decode_tuple, encode_i64, encode_key, encode_tuple, get_varint, put_varint, Value,
    ValueType,
};
use crate::storage::BufferPool;
use crate::{Error, Result, TxId};

/// One row: the table's columns, in the order they were declared.
pub type Row = Vec<Value>;

// -- evaluation ------------------------------------------------------------

/// Computes an expression against a row.
pub fn eval(expr: &PlanExpr, row: &[Value]) -> Result<Value> {
    Ok(match expr {
        PlanExpr::Const(value) => value.clone(),
        PlanExpr::Column(index) => row.get(*index).cloned().unwrap_or(Value::Null),
        PlanExpr::Unary { op, operand } => {
            let value = eval(operand, row)?;
            match op {
                UnaryOp::Not => match truth(&value) {
                    Some(flag) => Value::Bool(!flag),
                    None => Value::Null,
                },
                UnaryOp::Neg => match value {
                    Value::Int(number) => Value::Int(
                        number
                            .checked_neg()
                            .ok_or_else(|| overflow("negating an integer"))?,
                    ),
                    Value::Real(number) => Value::Real(-number),
                    _ => Value::Null,
                },
            }
        }
        PlanExpr::Binary { left, op, right } => binary(left, *op, right, row)?,
        PlanExpr::IsNull { operand, negated } => {
            let is_null = matches!(eval(operand, row)?, Value::Null);
            Value::Bool(is_null != *negated)
        }
        PlanExpr::Like {
            left,
            pattern,
            negated,
        } => {
            let subject = eval(left, row)?;
            let pattern = eval(pattern, row)?;
            match (&subject, &pattern) {
                (Value::Text(text), Value::Text(pattern)) => {
                    Value::Bool(like(text, pattern) != *negated)
                }
                _ => Value::Null,
            }
        }
        PlanExpr::Between {
            operand,
            low,
            high,
            negated,
        } => {
            let value = eval(operand, row)?;
            let low = eval(low, row)?;
            let high = eval(high, row)?;
            match (compare(&value, &low), compare(&value, &high)) {
                (Some(above), Some(below)) => {
                    let inside = above != Ordering::Less && below != Ordering::Greater;
                    Value::Bool(inside != *negated)
                }
                _ => Value::Null,
            }
        }
    })
}

fn binary(left: &PlanExpr, op: BinaryOp, right: &PlanExpr, row: &[Value]) -> Result<Value> {
    // Three-valued logic, and the part of it that is easy to get wrong: AND is
    // false as soon as either side is false, even when the other is unknown,
    // and OR is true as soon as either side is true. Only the remaining cases
    // are unknown.
    if op == BinaryOp::And {
        let left = truth(&eval(left, row)?);
        if left == Some(false) {
            return Ok(Value::Bool(false));
        }
        let right = truth(&eval(right, row)?);
        if right == Some(false) {
            return Ok(Value::Bool(false));
        }
        return Ok(match (left, right) {
            (Some(true), Some(true)) => Value::Bool(true),
            _ => Value::Null,
        });
    }
    if op == BinaryOp::Or {
        let left = truth(&eval(left, row)?);
        if left == Some(true) {
            return Ok(Value::Bool(true));
        }
        let right = truth(&eval(right, row)?);
        if right == Some(true) {
            return Ok(Value::Bool(true));
        }
        return Ok(match (left, right) {
            (Some(false), Some(false)) => Value::Bool(false),
            _ => Value::Null,
        });
    }

    let left = eval(left, row)?;
    let right = eval(right, row)?;

    if let Some(op) = comparison(op) {
        return Ok(match compare(&left, &right) {
            Some(ordering) => Value::Bool(op(ordering)),
            None => Value::Null,
        });
    }
    arithmetic(&left, op, &right)
}

fn comparison(op: BinaryOp) -> Option<fn(Ordering) -> bool> {
    Some(match op {
        BinaryOp::Eq => |o| o == Ordering::Equal,
        BinaryOp::NotEq => |o| o != Ordering::Equal,
        BinaryOp::Less => |o| o == Ordering::Less,
        BinaryOp::LessEq => |o| o != Ordering::Greater,
        BinaryOp::Greater => |o| o == Ordering::Greater,
        BinaryOp::GreaterEq => |o| o != Ordering::Less,
        _ => return None,
    })
}

fn arithmetic(left: &Value, op: BinaryOp, right: &Value) -> Result<Value> {
    match (left, right) {
        (Value::Int(a), Value::Int(b)) => {
            let (a, b) = (*a, *b);
            Ok(match op {
                BinaryOp::Add => Value::Int(a.checked_add(b).ok_or_else(|| overflow("+"))?),
                BinaryOp::Sub => Value::Int(a.checked_sub(b).ok_or_else(|| overflow("-"))?),
                BinaryOp::Mul => Value::Int(a.checked_mul(b).ok_or_else(|| overflow("*"))?),
                // Dividing by zero yields unknown rather than failing, which is
                // what SQL asks for and what SQLite does.
                BinaryOp::Div | BinaryOp::Mod if b == 0 => Value::Null,
                BinaryOp::Div => Value::Int(a.checked_div(b).ok_or_else(|| overflow("/"))?),
                BinaryOp::Mod => Value::Int(a.checked_rem(b).ok_or_else(|| overflow("%"))?),
                _ => Value::Null,
            })
        }
        _ => {
            let (Some(a), Some(b)) = (as_real(left), as_real(right)) else {
                return Ok(Value::Null);
            };
            Ok(match op {
                BinaryOp::Add => Value::Real(a + b),
                BinaryOp::Sub => Value::Real(a - b),
                BinaryOp::Mul => Value::Real(a * b),
                BinaryOp::Div if b == 0.0 => Value::Null,
                BinaryOp::Div => Value::Real(a / b),
                BinaryOp::Mod if b == 0.0 => Value::Null,
                BinaryOp::Mod => Value::Real(a % b),
                _ => Value::Null,
            })
        }
    }
}

fn as_real(value: &Value) -> Option<f64> {
    match value {
        Value::Int(number) => Some(*number as f64),
        Value::Real(number) => Some(*number),
        _ => None,
    }
}

/// A value's truth, or `None` for unknown.
pub fn truth(value: &Value) -> Option<bool> {
    match value {
        Value::Bool(flag) => Some(*flag),
        Value::Null => None,
        // A number stands in for a condition the way it does in most dialects.
        Value::Int(number) => Some(*number != 0),
        Value::Real(number) => Some(*number != 0.0),
        _ => None,
    }
}

/// Orders two values, or `None` when they cannot be compared.
///
/// `None` is what makes a comparison against `NULL` unknown rather than false,
/// which is the whole of three-valued logic in one return type.
pub fn compare(left: &Value, right: &Value) -> Option<Ordering> {
    match (left, right) {
        (Value::Null, _) | (_, Value::Null) => None,
        (Value::Int(a), Value::Int(b)) => Some(a.cmp(b)),
        (Value::Bool(a), Value::Bool(b)) => Some(a.cmp(b)),
        (Value::Text(a), Value::Text(b)) => Some(a.cmp(b)),
        (Value::Blob(a), Value::Blob(b)) => Some(a.cmp(b)),
        _ => {
            let (a, b) = (as_real(left)?, as_real(right)?);
            a.partial_cmp(&b)
        }
    }
}

/// SQL `LIKE`: `%` stands for any run of characters, `_` for exactly one.
///
/// Iterative with one backtrack point rather than recursive, so a pattern of
/// many wildcards cannot blow the stack.
pub fn like(text: &str, pattern: &str) -> bool {
    let text: Vec<char> = text.chars().collect();
    let pattern: Vec<char> = pattern.chars().collect();

    let (mut t, mut p) = (0usize, 0usize);
    let (mut star, mut resume) = (None, 0usize);

    while t < text.len() {
        if p < pattern.len() && (pattern[p] == '_' || pattern[p] == text[t]) {
            t += 1;
            p += 1;
        } else if p < pattern.len() && pattern[p] == '%' {
            star = Some(p);
            resume = t;
            p += 1;
        } else if let Some(at) = star {
            // Back up to the last `%` and let it swallow one more character.
            p = at + 1;
            resume += 1;
            t = resume;
        } else {
            return false;
        }
    }
    while p < pattern.len() && pattern[p] == '%' {
        p += 1;
    }
    p == pattern.len()
}

fn overflow(what: &str) -> Error {
    Error::Unsupported(format!("{what} overflowed a 64 bit integer"))
}

// -- spilling --------------------------------------------------------------

/// How many rows a sort keeps in memory before it starts writing runs out.
///
/// Everything else in the executor streams; a sort cannot, because it has to
/// see the last row before it knows which one comes first. What it can do is
/// bound what it holds, which is what this is for.
pub const DEFAULT_SORT_ROWS: usize = 8192;

/// A row written in a form that carries its own types.
///
/// The tuple encoding is flat and needs a schema to read back. A sort may sit
/// over a join, where the row is two schemas laid end to end, so the spill
/// format tags each value instead of asking anybody to remember.
fn spill_row(row: &[Value], out: &mut Vec<u8>) {
    let start = out.len();
    out.extend_from_slice(&[0u8; 4]);
    put_varint(out, row.len() as u64);
    for value in row {
        match value {
            Value::Null => out.push(0),
            Value::Int(number) => {
                out.push(1);
                out.extend_from_slice(&number.to_le_bytes());
            }
            Value::Real(number) => {
                out.push(2);
                out.extend_from_slice(&number.to_le_bytes());
            }
            Value::Text(text) => {
                out.push(3);
                put_varint(out, text.len() as u64);
                out.extend_from_slice(text.as_bytes());
            }
            Value::Blob(bytes) => {
                out.push(4);
                put_varint(out, bytes.len() as u64);
                out.extend_from_slice(bytes);
            }
            Value::Bool(flag) => {
                out.push(5);
                out.push(u8::from(*flag));
            }
        }
    }
    let length = (out.len() - start - 4) as u32;
    out[start..start + 4].copy_from_slice(&length.to_le_bytes());
}

fn unspill_row(input: &[u8]) -> Option<Row> {
    let (count, mut at) = get_varint(input)?;
    let mut row = Vec::with_capacity(count as usize);
    for _ in 0..count {
        let tag = *input.get(at)?;
        at += 1;
        let value = match tag {
            0 => Value::Null,
            1 => {
                let bytes = input.get(at..at + 8)?;
                at += 8;
                let mut buf = [0u8; 8];
                buf.copy_from_slice(bytes);
                Value::Int(i64::from_le_bytes(buf))
            }
            2 => {
                let bytes = input.get(at..at + 8)?;
                at += 8;
                let mut buf = [0u8; 8];
                buf.copy_from_slice(bytes);
                Value::Real(f64::from_le_bytes(buf))
            }
            3 | 4 => {
                let (length, read) = get_varint(input.get(at..)?)?;
                at += read;
                let bytes = input.get(at..at + length as usize)?;
                at += length as usize;
                if tag == 3 {
                    Value::Text(String::from_utf8(bytes.to_vec()).ok()?)
                } else {
                    Value::Blob(bytes.to_vec())
                }
            }
            5 => {
                let byte = *input.get(at)?;
                at += 1;
                Value::Bool(byte != 0)
            }
            _ => return None,
        };
        row.push(value);
    }
    Some(row)
}

/// A batch of sorted rows, written out so the sort can let go of them.
#[derive(Debug)]
pub struct SortedRun {
    file: BufReader<File>,
    next: Option<Row>,
}

impl SortedRun {
    /// Writes a sorted batch to a temporary file and reopens it for reading.
    fn spill(rows: &[Row]) -> Result<SortedRun> {
        let mut file = tempfile::tempfile()?;
        let mut buffer = Vec::new();
        for row in rows {
            spill_row(row, &mut buffer);
            if buffer.len() >= 1 << 16 {
                file.write_all(&buffer)?;
                buffer.clear();
            }
        }
        file.write_all(&buffer)?;
        file.seek(SeekFrom::Start(0))?;

        let mut run = SortedRun {
            file: BufReader::new(file),
            next: None,
        };
        run.advance()?;
        Ok(run)
    }

    /// The row at the front of the run, without consuming it.
    fn peek(&self) -> Option<&Row> {
        self.next.as_ref()
    }

    /// Takes the front row and reads the one behind it.
    fn take(&mut self) -> Result<Option<Row>> {
        let row = self.next.take();
        self.advance()?;
        Ok(row)
    }

    fn advance(&mut self) -> Result<()> {
        let mut header = [0u8; 4];
        match self.file.read_exact(&mut header) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => {
                self.next = None;
                return Ok(());
            }
            Err(error) => return Err(error.into()),
        }
        let length = u32::from_le_bytes(header) as usize;
        let mut body = vec![0u8; length];
        self.file.read_exact(&mut body)?;
        self.next =
            Some(unspill_row(&body).ok_or_else(|| {
                Error::MalformedFile("a spilled row that does not decode".into())
            })?);
        Ok(())
    }
}

/// Orders two rows by a sort key list.
fn compare_rows(keys: &[(PlanExpr, bool)], left: &Row, right: &Row) -> Result<Ordering> {
    for (expr, descending) in keys {
        let a = eval(expr, left)?;
        let b = eval(expr, right)?;
        let ordering = match compare(&a, &b) {
            Some(ordering) => ordering,
            // Nulls sort before everything, the same way they do in an encoded
            // key.
            None => null_order(&a, &b),
        };
        if ordering != Ordering::Equal {
            return Ok(if *descending {
                ordering.reverse()
            } else {
                ordering
            });
        }
    }
    Ok(Ordering::Equal)
}

// -- versioned rows --------------------------------------------------------
//
// A row lives at key `rowid ++ xmin`, so every version of it is a separate
// entry and they sort together, oldest first. The value carries `xmax` ahead of
// the tuple, because that is the one field a later transaction has to change
// without touching the rest.

/// Bytes a row key takes: the row id, then the transaction that wrote it.
pub const ROW_KEY_LEN: usize = 16;

/// The key one version of a row lives at.
pub fn row_key(rowid: i64, xmin: TxId) -> Vec<u8> {
    let mut key = Vec::with_capacity(ROW_KEY_LEN);
    key.extend_from_slice(&encode_i64(rowid));
    key.extend_from_slice(&xmin.to_be_bytes());
    key
}

/// The smallest key any version of `rowid` can have.
fn row_key_floor(rowid: i64) -> Vec<u8> {
    row_key(rowid, 0)
}

/// The row id and creating transaction a key names.
fn split_row_key(key: &[u8]) -> Result<(i64, TxId)> {
    if key.len() != ROW_KEY_LEN {
        return Err(Error::MalformedFile(
            "a table key that is not a row id and a transaction".into(),
        ));
    }
    let mut rowid = [0u8; 8];
    rowid.copy_from_slice(&key[..8]);
    let mut xmin = [0u8; 8];
    xmin.copy_from_slice(&key[8..]);
    Ok((decode_i64(rowid), TxId::from_be_bytes(xmin)))
}

/// Wraps a row in its version header.
fn encode_version(xmax: TxId, row: &[Value]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&xmax.to_be_bytes());
    encode_tuple(row, &mut out);
    out
}

/// Splits a stored value into its removal stamp and the row itself.
fn decode_version(value: &[u8], types: &[ValueType]) -> Result<(TxId, Row)> {
    let head = value
        .get(..8)
        .ok_or_else(|| Error::MalformedFile("a version with no header".into()))?;
    let mut xmax = [0u8; 8];
    xmax.copy_from_slice(head);
    let row = decode_tuple(&value[8..], types)
        .ok_or_else(|| Error::MalformedFile("a row that does not decode".into()))?;
    Ok((TxId::from_be_bytes(xmax), row))
}

/// The version of one row a snapshot should read, if any.
fn fetch_visible(
    pool: &mut BufferPool,
    root: crate::PageId,
    rowid: i64,
    types: &[ValueType],
    snapshot: Snapshot,
) -> Result<Option<Row>> {
    let tree = BTree::open(root);
    let lower = row_key_floor(rowid);
    let upper = row_key_floor(rowid.saturating_add(1));
    let mut cursor = tree.cursor_range(pool, Some(&lower), Some(&upper))?;

    while let Some((key, value)) = cursor.next(pool)? {
        let (_, xmin) = split_row_key(&key)?;
        let (xmax, row) = decode_version(&value, types)?;
        if snapshot.sees(Version { xmin, xmax }) {
            return Ok(Some(row));
        }
    }
    Ok(None)
}

// -- operators -------------------------------------------------------------

/// A running operator.
#[derive(Debug)]
pub enum Op {
    /// Walks a table's tree, yielding the version of each row the snapshot
    /// should see and skipping the rest.
    Scan {
        /// The column types, for decoding.
        types: Vec<ValueType>,
        /// Where in the tree the scan is.
        cursor: Cursor,
        /// What this statement is allowed to see.
        snapshot: Snapshot,
        /// The last row already yielded, so its older versions are passed over.
        last: Option<i64>,
    },
    /// Walks an index and fetches the row each entry points at.
    ///
    /// The index is deliberately allowed to be stale: entries are not removed
    /// when a version is superseded, so an entry may name a row whose visible
    /// version no longer matches. The planner keeps the equality in the filter
    /// above for exactly that reason, and the fetch below checks visibility.
    IndexScan {
        /// The column types, for decoding a row.
        types: Vec<ValueType>,
        /// The tree holding the rows.
        table_root: crate::PageId,
        /// Where in the index the walk is.
        cursor: Cursor,
        /// What this statement is allowed to see.
        snapshot: Snapshot,
        /// Row ids already yielded, since an index may name one twice.
        seen: std::collections::HashSet<i64>,
    },
    /// Drops rows the predicate does not admit.
    Filter {
        /// Where rows come from.
        input: Box<Op>,
        /// The condition.
        predicate: PlanExpr,
    },
    /// Computes the output columns.
    Project {
        /// Where rows come from.
        input: Box<Op>,
        /// One expression per output column.
        exprs: Vec<PlanExpr>,
    },
    /// Orders rows. The only operator that has to see everything first.
    ///
    /// It holds at most `budget` rows: past that it sorts what it has, writes
    /// the batch out as a run, and starts again. At the end the runs are merged
    /// k ways. So a sort over more rows than fit still runs, in memory bounded
    /// by the budget rather than by the input.
    Sort {
        /// Where rows come from.
        input: Box<Op>,
        /// What to sort by, and whether each key descends.
        keys: Vec<(PlanExpr, bool)>,
        /// How many rows are wanted, when a limit sits above.
        top: Option<usize>,
        /// How many rows to hold before writing a run out.
        budget: usize,
        /// Runs written out, each already in order.
        runs: Vec<SortedRun>,
        /// The sorted rows, when everything fitted in memory.
        buffered: Option<std::vec::IntoIter<Row>>,
        /// How many rows the merge has yielded, for the top-n cutoff.
        yielded: usize,
        /// Whether the input has been read to the end.
        drained: bool,
    },
    /// Pairs every outer row with every inner row, keeping what the condition
    /// admits.
    ///
    /// The inner side is read once and kept, rather than walked again per outer
    /// row: a cursor cannot be rewound, and re-descending the tree for every
    /// outer row would cost more than the rows themselves.
    NestedLoopJoin {
        /// The outer input.
        left: Box<Op>,
        /// The inner input, already drained.
        right: Vec<Row>,
        /// The condition, over the two rows joined end to end.
        on: PlanExpr,
        /// The outer row being paired.
        current: Option<Row>,
        /// How far into the inner rows that pairing has got.
        at: usize,
    },
    /// Builds a table from one side and probes it with the other.
    HashJoin {
        /// The side the table is built from.
        left: Box<Op>,
        /// The side that probes it.
        right: Box<Op>,
        /// The key on the build side, over a joined row.
        left_key: PlanExpr,
        /// The key on the probe side, over a row of the right input alone.
        right_key: PlanExpr,
        /// Whatever the equality did not cover.
        residual: Option<PlanExpr>,
        /// The table, once the build side has been drained.
        built: Option<HashMap<Vec<u8>, Vec<Row>>>,
        /// Matches still to be yielded for the current probe row.
        pending: Vec<Row>,
    },
    /// Counts and stops.
    Limit {
        /// Where rows come from.
        input: Box<Op>,
        /// How many are left to yield.
        remaining: Option<u64>,
        /// How many are left to skip.
        skip: u64,
    },
}

impl Op {
    /// The next row, or `None` when the operator is done.
    pub fn next(&mut self, pool: &mut BufferPool) -> Result<Option<Row>> {
        match self {
            Op::Scan {
                types,
                cursor,
                snapshot,
                last,
            } => loop {
                let Some((key, value)) = cursor.next(pool)? else {
                    return Ok(None);
                };
                let (rowid, xmin) = split_row_key(&key)?;
                if *last == Some(rowid) {
                    // An older version of a row already yielded. At most one
                    // version of a row is ever visible, so there is nothing
                    // left to find here.
                    continue;
                }
                let (xmax, row) = decode_version(&value, types)?;
                if snapshot.sees(Version { xmin, xmax }) {
                    *last = Some(rowid);
                    return Ok(Some(row));
                }
            },

            Op::IndexScan {
                types,
                table_root,
                cursor,
                snapshot,
                seen,
            } => loop {
                let Some((key, _)) = cursor.next(pool)? else {
                    return Ok(None);
                };
                // The row id rides at the end of every index key, which is what
                // turns a match into a second descent rather than a scan.
                let rowid = rowid_of(&key)?;
                if !seen.insert(rowid) {
                    continue;
                }
                if let Some(row) = fetch_visible(pool, *table_root, rowid, types, *snapshot)? {
                    return Ok(Some(row));
                }
            },

            Op::Filter { input, predicate } => loop {
                let Some(row) = input.next(pool)? else {
                    return Ok(None);
                };
                // Only TRUE admits a row. Unknown does not, which is the rule
                // that follows from three-valued logic and the one people are
                // surprised by.
                if truth(&eval(predicate, &row)?) == Some(true) {
                    return Ok(Some(row));
                }
            },

            Op::Project { input, exprs } => {
                let Some(row) = input.next(pool)? else {
                    return Ok(None);
                };
                let mut out = Vec::with_capacity(exprs.len());
                for expr in exprs.iter() {
                    out.push(eval(expr, &row)?);
                }
                Ok(Some(out))
            }

            Op::Sort {
                input,
                keys,
                top,
                budget,
                runs,
                buffered,
                yielded,
                drained,
            } => {
                if !*drained {
                    let mut batch: Vec<Row> = Vec::new();
                    while let Some(row) = input.next(pool)? {
                        batch.push(row);
                        if batch.len() >= *budget {
                            sort_batch(keys, &mut batch)?;
                            runs.push(SortedRun::spill(&batch)?);
                            batch.clear();
                        }
                    }
                    sort_batch(keys, &mut batch)?;

                    if runs.is_empty() {
                        // Everything fitted, so there is nothing to merge.
                        if let Some(limit) = top {
                            batch.truncate(*limit);
                        }
                        *buffered = Some(batch.into_iter());
                    } else if !batch.is_empty() {
                        runs.push(SortedRun::spill(&batch)?);
                    }
                    *drained = true;
                }

                if let Some(rows) = buffered.as_mut() {
                    return Ok(rows.next());
                }
                if let Some(limit) = top {
                    if *yielded >= *limit {
                        return Ok(None);
                    }
                }

                // A k way merge over the runs: whichever run has the smallest
                // front row gives it up, and reads the one behind it.
                let mut best: Option<usize> = None;
                for index in 0..runs.len() {
                    let Some(candidate) = runs[index].peek() else {
                        continue;
                    };
                    match best {
                        None => best = Some(index),
                        Some(current) => {
                            let incumbent =
                                runs[current].peek().expect("chosen because it had one");
                            if compare_rows(keys, candidate, incumbent)? == Ordering::Less {
                                best = Some(index);
                            }
                        }
                    }
                }

                match best {
                    Some(index) => {
                        *yielded += 1;
                        runs[index].take()
                    }
                    None => Ok(None),
                }
            }

            Op::NestedLoopJoin {
                left,
                right,
                on,
                current,
                at,
            } => loop {
                if current.is_none() {
                    *current = left.next(pool)?;
                    *at = 0;
                    if current.is_none() {
                        return Ok(None);
                    }
                }
                let outer = current.as_ref().expect("filled above");
                if *at >= right.len() {
                    *current = None;
                    continue;
                }

                let mut joined = outer.clone();
                joined.extend_from_slice(&right[*at]);
                *at += 1;
                if truth(&eval(on, &joined)?) == Some(true) {
                    return Ok(Some(joined));
                }
            },

            Op::HashJoin {
                left,
                right,
                left_key,
                right_key,
                residual,
                built,
                pending,
            } => {
                if built.is_none() {
                    let mut table: HashMap<Vec<u8>, Vec<Row>> = HashMap::new();
                    while let Some(row) = left.next(pool)? {
                        // A null never equals anything, so a row keyed by one
                        // simply cannot match and is left out of the table.
                        if let Some(key) = hash_key(left_key, &row)? {
                            table.entry(key).or_default().push(row);
                        }
                    }
                    *built = Some(table);
                }
                let table = built.as_ref().expect("filled above");

                loop {
                    if let Some(row) = pending.pop() {
                        return Ok(Some(row));
                    }
                    let Some(probe) = right.next(pool)? else {
                        return Ok(None);
                    };
                    let Some(key) = hash_key(right_key, &probe)? else {
                        continue;
                    };
                    let Some(matches) = table.get(&key) else {
                        continue;
                    };
                    for build in matches {
                        let mut joined = build.clone();
                        joined.extend_from_slice(&probe);
                        let admitted = match residual {
                            Some(rest) => truth(&eval(rest, &joined)?) == Some(true),
                            None => true,
                        };
                        if admitted {
                            pending.push(joined);
                        }
                    }
                }
            }

            Op::Limit {
                input,
                remaining,
                skip,
            } => {
                while *skip > 0 {
                    if input.next(pool)?.is_none() {
                        return Ok(None);
                    }
                    *skip -= 1;
                }
                if *remaining == Some(0) {
                    return Ok(None);
                }
                let row = input.next(pool)?;
                if row.is_some() {
                    if let Some(left) = remaining {
                        *left -= 1;
                    }
                }
                Ok(row)
            }
        }
    }
}

/// The bytes a join key hashes as, or `None` when it is null.
///
/// Reuses the order preserving key encoding rather than inventing a second one:
/// it already turns any value into bytes that compare, and bytes hash.
fn hash_key(expr: &PlanExpr, row: &[Value]) -> Result<Option<Vec<u8>>> {
    let value = eval(expr, row)?;
    if matches!(value, Value::Null) {
        return Ok(None);
    }
    let mut out = Vec::new();
    encode_key(&[value], &mut out)?;
    Ok(Some(out))
}

fn null_order(left: &Value, right: &Value) -> Ordering {
    match (matches!(left, Value::Null), matches!(right, Value::Null)) {
        (true, true) => Ordering::Equal,
        (true, false) => Ordering::Less,
        (false, true) => Ordering::Greater,
        // Neither is null but they still would not compare, which means their
        // types differ. Left alone rather than invented.
        (false, false) => Ordering::Equal,
    }
}

/// Sorts a batch, reporting the first evaluation failure rather than swallowing
/// it inside the comparator.
fn sort_batch(keys: &[(PlanExpr, bool)], rows: &mut [Row]) -> Result<()> {
    let mut failure = None;
    rows.sort_by(|left, right| match compare_rows(keys, left, right) {
        Ok(ordering) => ordering,
        Err(error) => {
            failure.get_or_insert(error);
            Ordering::Equal
        }
    });
    match failure {
        Some(error) => Err(error),
        None => Ok(()),
    }
}

/// Builds the running operator tree from a plan.
pub fn build(plan: &Plan, pool: &mut BufferPool, snapshot: Snapshot) -> Result<Op> {
    build_with(plan, pool, snapshot, DEFAULT_SORT_ROWS)
}

/// Builds the tree with a given sort budget, in rows.
pub fn build_with(
    plan: &Plan,
    pool: &mut BufferPool,
    snapshot: Snapshot,
    budget: usize,
) -> Result<Op> {
    Ok(match plan {
        Plan::SeqScan { table } => Op::Scan {
            types: column_types(table),
            cursor: BTree::open(table.root).cursor(pool, None)?,
            snapshot,
            last: None,
        },
        Plan::RowIdScan {
            table,
            lower,
            upper,
        } => {
            let (low, high) = encode_bounds(*lower, *upper);
            Op::Scan {
                types: column_types(table),
                cursor: BTree::open(table.root).cursor_range(
                    pool,
                    low.as_deref(),
                    high.as_deref(),
                )?,
                snapshot,
                last: None,
            }
        }
        Plan::NestedLoopJoin { left, right, on } => {
            let mut inner = build_with(right, pool, snapshot, budget)?;
            let mut rows = Vec::new();
            while let Some(row) = inner.next(pool)? {
                rows.push(row);
            }
            Op::NestedLoopJoin {
                left: Box::new(build_with(left, pool, snapshot, budget)?),
                right: rows,
                on: on.clone(),
                current: None,
                at: 0,
            }
        }
        Plan::HashJoin {
            left,
            right,
            left_key,
            right_key,
            residual,
        } => Op::HashJoin {
            left: Box::new(build_with(left, pool, snapshot, budget)?),
            right: Box::new(build_with(right, pool, snapshot, budget)?),
            left_key: left_key.clone(),
            right_key: right_key.clone(),
            residual: residual.clone(),
            built: None,
            pending: Vec::new(),
        },
        Plan::IndexScan { table, index, key } => {
            let prefix = index_prefix(std::slice::from_ref(key))?;
            let upper = successor(&prefix);
            Op::IndexScan {
                types: column_types(table),
                table_root: table.root,
                cursor: BTree::open(index.root).cursor_range(
                    pool,
                    Some(&prefix),
                    upper.as_deref(),
                )?,
                snapshot,
                seen: std::collections::HashSet::new(),
            }
        }
        Plan::Filter { input, predicate } => Op::Filter {
            input: Box::new(build_with(input, pool, snapshot, budget)?),
            predicate: predicate.clone(),
        },
        Plan::Project { input, exprs, .. } => Op::Project {
            input: Box::new(build_with(input, pool, snapshot, budget)?),
            exprs: exprs.clone(),
        },
        Plan::Sort { input, keys, top } => Op::Sort {
            input: Box::new(build_with(input, pool, snapshot, budget)?),
            keys: keys.clone(),
            top: *top,
            budget: budget.max(1),
            runs: Vec::new(),
            buffered: None,
            yielded: 0,
            drained: false,
        },
        Plan::Limit {
            input,
            limit,
            offset,
        } => Op::Limit {
            input: Box::new(build_with(input, pool, snapshot, budget)?),
            remaining: *limit,
            skip: *offset,
        },
        other => {
            return Err(Error::Unsupported(format!(
                "{other:?} is not something that yields rows"
            )))
        }
    })
}

fn column_types(table: &TableSchema) -> Vec<ValueType> {
    table
        .columns
        .iter()
        .map(|column| match column.data_type {
            DataType::Integer => ValueType::Int,
            DataType::Real => ValueType::Real,
            DataType::Text => ValueType::Text,
            DataType::Blob => ValueType::Blob,
            DataType::Boolean => ValueType::Bool,
        })
        .collect()
}

// -- writing ---------------------------------------------------------------

// -- indexes ---------------------------------------------------------------

/// The key one row takes in one index: the indexed values, then the row id.
fn index_key(index: &IndexSchema, row: &[Value], rowid: i64) -> Result<Vec<u8>> {
    let values: Vec<Value> = index
        .columns
        .iter()
        .map(|position| row[*position].clone())
        .collect();
    let mut key = index_prefix(&values)?;
    key.extend_from_slice(&encode_i64(rowid));
    Ok(key)
}

/// The part of an index key that comes before the row id.
fn index_prefix(values: &[Value]) -> Result<Vec<u8>> {
    let mut key = Vec::new();
    encode_key(values, &mut key)?;
    Ok(key)
}

/// The smallest key that sorts after everything starting with `prefix`.
///
/// `None` when there is none, which only happens for a prefix of all `0xFF`
/// bytes: the range then runs to the end of the index and needs no upper edge.
fn successor(prefix: &[u8]) -> Option<Vec<u8>> {
    let mut out = prefix.to_vec();
    for byte in out.iter_mut().rev() {
        if *byte < 0xFF {
            *byte += 1;
            return Some(out);
        }
        *byte = 0;
    }
    None
}

/// The row id an index key ends with.
fn rowid_of(key: &[u8]) -> Result<i64> {
    let tail = key
        .len()
        .checked_sub(8)
        .and_then(|at| key.get(at..))
        .ok_or_else(|| Error::MalformedFile("an index key with no row id".into()))?;
    let mut bytes = [0u8; 8];
    bytes.copy_from_slice(tail);
    Ok(decode_i64(bytes))
}

/// Refuses a row that would repeat a value a unique index admits once.
///
/// Asks the index for candidates and then reads each row, because an index
/// entry may point at a version nobody can see any more. A unique constraint is
/// about what is visible, not about what is still lying on disk.
fn check_unique(
    pool: &mut BufferPool,
    table: &TableSchema,
    row: &[Value],
    rowid: i64,
    snapshot: Snapshot,
) -> Result<()> {
    let types = column_types(table);
    for index in &table.indexes {
        if !index.unique {
            continue;
        }
        let values = index_values(index, row);
        // A null is not equal to anything, not even another null, so it cannot
        // collide and a unique index lets any number of them in.
        if values.iter().any(|value| matches!(value, Value::Null)) {
            continue;
        }

        let prefix = index_prefix(&values)?;
        let upper = successor(&prefix);
        let tree = BTree::open(index.root);
        let mut cursor = tree.cursor_range(pool, Some(&prefix), upper.as_deref())?;
        while let Some((key, _)) = cursor.next(pool)? {
            let other = rowid_of(&key)?;
            if other == rowid {
                continue;
            }
            let Some(existing) = fetch_visible(pool, table.root, other, &types, snapshot)? else {
                continue;
            };
            if index_values(index, &existing) == values {
                return Err(Error::NotUnique(index.name.clone()));
            }
        }
    }
    Ok(())
}

/// Adds one row to every index on its table.
fn index_row(pool: &mut BufferPool, table: &TableSchema, row: &[Value], rowid: i64) -> Result<()> {
    for index in &table.indexes {
        let mut tree = BTree::open(index.root);
        tree.insert(pool, &index_key(index, row, rowid)?, &[])?;
    }
    Ok(())
}

/// Builds an index over the rows a table already holds.
pub fn build_index(
    pool: &mut BufferPool,
    table: &TableSchema,
    index: &IndexSchema,
    snapshot: Snapshot,
) -> Result<usize> {
    let types = column_types(table);
    let mut entries = Vec::new();
    {
        let tree = BTree::open(table.root);
        let mut cursor = tree.cursor(pool, None)?;
        let mut last: Option<i64> = None;
        while let Some((key, value)) = cursor.next(pool)? {
            let (rowid, xmin) = split_row_key(&key)?;
            if last == Some(rowid) {
                continue;
            }
            let (xmax, row) = decode_version(&value, &types)?;
            if snapshot.sees(Version { xmin, xmax }) {
                last = Some(rowid);
                entries.push((rowid, row));
            }
        }
    }

    let mut tree = BTree::open(index.root);
    let mut seen: std::collections::HashMap<Vec<u8>, i64> = std::collections::HashMap::new();
    for (rowid, row) in &entries {
        if index.unique {
            let values = index_values(index, row);
            if !values.iter().any(|value| matches!(value, Value::Null)) {
                let prefix = index_prefix(&values)?;
                // The rows arrive in row id order, not index order, so a
                // duplicate need not be adjacent to its twin. Remembering what
                // has gone in catches it wherever it sits.
                if seen.insert(prefix, *rowid).is_some() {
                    return Err(Error::NotUnique(index.name.clone()));
                }
            }
        }
        tree.insert(pool, &index_key(index, row, *rowid)?, &[])?;
    }
    Ok(entries.len())
}

fn index_values(index: &IndexSchema, row: &[Value]) -> Vec<Value> {
    index
        .columns
        .iter()
        .map(|position| row[*position].clone())
        .collect()
}

// -- reclaiming ------------------------------------------------------------

/// What a sweep removed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct VacuumReport {
    /// Versions nobody could see any more, now gone.
    pub versions: usize,
    /// Index entries that pointed at them.
    pub entries: usize,
}

/// Removes the versions no transaction, present or future, can reach.
///
/// A version is dead when it was removed by a transaction that finished before
/// `horizon`. Everything below the horizon is settled, so nothing that could
/// still be reading is deprived of anything.
///
/// The long transaction problem, worth naming because it bites in practice: a
/// transaction left open holds the horizon back, and nothing newer than it can
/// be reclaimed however often this runs. In Postgres that is called bloat and
/// it causes half the production incidents with that database. The mitigation
/// here is the same one — the horizon is reported, so the cause is visible.
pub fn vacuum(pool: &mut BufferPool, table: &TableSchema, horizon: TxId) -> Result<VacuumReport> {
    let types = column_types(table);
    let mut report = VacuumReport::default();

    // Everything is read before anything is removed, for the same reason a
    // write gathers first: a cursor must not walk a tree being rewritten.
    let mut dead = Vec::new();
    let mut live: HashMap<i64, Row> = HashMap::new();
    {
        let tree = BTree::open(table.root);
        let mut cursor = tree.cursor(pool, None)?;
        while let Some((key, value)) = cursor.next(pool)? {
            let (rowid, _) = split_row_key(&key)?;
            let (xmax, row) = decode_version(&value, &types)?;
            if xmax != 0 && xmax < horizon {
                dead.push(key);
            } else if xmax == 0 {
                live.insert(rowid, row);
            }
        }
    }

    let mut tree = BTree::open(table.root);
    for key in &dead {
        tree.delete(pool, key)?;
        report.versions += 1;
    }

    // An index entry outlives the version it was written for, so the entries
    // left pointing at nothing go now too.
    for index in &table.indexes {
        let mut stale = Vec::new();
        {
            let tree = BTree::open(index.root);
            let mut cursor = tree.cursor(pool, None)?;
            while let Some((key, _)) = cursor.next(pool)? {
                let rowid = rowid_of(&key)?;
                let matches = match live.get(&rowid) {
                    Some(row) => index_key(index, row, rowid)? == key,
                    None => false,
                };
                if !matches {
                    stale.push(key);
                }
            }
        }
        let mut tree = BTree::open(index.root);
        for key in &stale {
            tree.delete(pool, key)?;
            report.entries += 1;
        }
    }

    Ok(report)
}

/// The rows a write is about to touch, gathered before any of them change.
///
/// Collected first on purpose. A cursor holds no pin between calls, which is
/// what keeps it cheap, and the price of that is it must not walk a tree that
/// is being rewritten underneath it. Reading the whole match set first costs
/// memory proportional to what the statement touches and removes the hazard
/// entirely.
///
/// Only the version each row shows to `snapshot` is returned, so a write sees
/// exactly what a read would.
fn gather(
    pool: &mut BufferPool,
    table: &TableSchema,
    filter: Option<&PlanExpr>,
    lower: Option<Bound>,
    upper: Option<Bound>,
    snapshot: Snapshot,
) -> Result<Vec<(i64, TxId, Row)>> {
    let types = column_types(table);
    let tree = BTree::open(table.root);
    let (low, high) = encode_bounds(lower, upper);
    let mut cursor = tree.cursor_range(pool, low.as_deref(), high.as_deref())?;

    let mut matched = Vec::new();
    let mut last: Option<i64> = None;
    while let Some((key, value)) = cursor.next(pool)? {
        let (rowid, xmin) = split_row_key(&key)?;
        if last == Some(rowid) {
            continue;
        }
        let (xmax, row) = decode_version(&value, &types)?;
        if !snapshot.sees(Version { xmin, xmax }) {
            continue;
        }
        last = Some(rowid);

        let admitted = match filter {
            Some(predicate) => truth(&eval(predicate, &row)?) == Some(true),
            None => true,
        };
        if admitted {
            matched.push((rowid, xmin, row));
        }
    }
    Ok(matched)
}

fn encode_bounds(lower: Option<Bound>, upper: Option<Bound>) -> (Option<Vec<u8>>, Option<Vec<u8>>) {
    let low = lower.map(|bound| {
        let value = if bound.inclusive {
            bound.value
        } else {
            bound.value.saturating_add(1)
        };
        row_key_floor(value)
    });
    // The upper edge of a cursor is exclusive, so an inclusive bound is the
    // floor of the row id one past the one it names.
    let high = upper.map(|bound| {
        let value = if bound.inclusive {
            bound.value.saturating_add(1)
        } else {
            bound.value
        };
        row_key_floor(value)
    });
    (low, high)
}

/// What a write reads through, and what it stamps versions with.
///
/// Carried as one thing because the two always travel together: a write reads
/// the versions its own transaction can see, and writes versions marked with
/// that same transaction.
#[derive(Debug, Clone, Copy)]
pub struct Writer {
    /// The versions this write is allowed to see.
    pub snapshot: Snapshot,
    /// The transaction doing the writing.
    pub txid: TxId,
}

/// Marks every row the filter admits as removed, returning how many went.
///
/// Nothing is erased. The version is stamped with the transaction that removed
/// it and stays where it is, so a reader that started earlier still finds it.
pub fn delete(
    pool: &mut BufferPool,
    table: &TableSchema,
    filter: Option<&PlanExpr>,
    lower: Option<Bound>,
    upper: Option<Bound>,
    writer: Writer,
) -> Result<usize> {
    let matched = gather(pool, table, filter, lower, upper, writer.snapshot)?;
    let mut tree = BTree::open(table.root);
    for (rowid, xmin, row) in &matched {
        tree.insert(
            pool,
            &row_key(*rowid, *xmin),
            &encode_version(writer.txid, row),
        )?;
    }
    Ok(matched.len())
}

/// Applies the assignments to every row the filter admits.
///
/// An update is a removal and an insertion: the version that was there is
/// stamped as removed and a new one is written beside it. Both stay, and which
/// one a reader finds depends on when the reader started.
pub fn update(
    pool: &mut BufferPool,
    table: &TableSchema,
    assignments: &[(usize, PlanExpr)],
    filter: Option<&PlanExpr>,
    lower: Option<Bound>,
    upper: Option<Bound>,
    writer: Writer,
) -> Result<usize> {
    let matched = gather(pool, table, filter, lower, upper, writer.snapshot)?;
    let rowid_column = table.rowid_column();
    let mut tree = BTree::open(table.root);

    for (rowid, xmin, row) in &matched {
        let mut updated = row.clone();
        for (index, value) in assignments {
            let column = table.column(*index);
            let computed = eval(value, row)?;
            updated[*index] = coerce(&computed, column.data_type, &column.name)?;
        }
        for (index, column) in table.columns.iter().enumerate() {
            if column.not_null && matches!(updated[index], Value::Null) {
                return Err(Error::NotNull(column.name.clone()));
            }
        }

        let new_rowid = match rowid_column {
            Some(index) => match updated[index] {
                Value::Int(number) => number,
                // Anything else was already refused by the coercion above,
                // unless it was null, which the check above catches.
                _ => return Err(Error::NotNull(table.column(index).name.clone())),
            },
            None => *rowid,
        };

        check_unique(pool, table, &updated, new_rowid, writer.snapshot)?;
        tree.insert(
            pool,
            &row_key(*rowid, *xmin),
            &encode_version(writer.txid, row),
        )?;
        tree.insert(
            pool,
            &row_key(new_rowid, writer.txid),
            &encode_version(0, &updated),
        )?;
        index_row(pool, table, &updated, new_rowid)?;
    }
    Ok(matched.len())
}

/// Writes the rows of an `INSERT`, returning how many landed.
pub fn insert(
    pool: &mut BufferPool,
    table: &TableSchema,
    targets: &[usize],
    rows: &[Vec<PlanExpr>],
    writer: Writer,
) -> Result<usize> {
    let mut tree = BTree::open(table.root);
    let rowid_column = table.rowid_column();

    // Where an auto-assigned row id starts. Read once, from the rightmost edge
    // of the tree, rather than kept as a counter the catalog has to rewrite on
    // every insert.
    let mut next_auto = match tree.last_key(pool)? {
        Some(key) if key.len() == ROW_KEY_LEN => split_row_key(&key)?.0.saturating_add(1),
        _ => 1,
    };

    for supplied in rows {
        let mut values = vec![Value::Null; table.columns.len()];
        let mut given = vec![false; table.columns.len()];

        for (slot, expr) in targets.iter().zip(supplied) {
            values[*slot] = eval(expr, &[])?;
            given[*slot] = true;
        }
        for (index, column) in table.columns.iter().enumerate() {
            if !given[index] {
                values[index] = column.default.clone().unwrap_or(Value::Null);
            }
            values[index] = coerce(&values[index], column.data_type, &column.name)?;
        }

        // The row id is settled before anything is checked. A primary key
        // implies NOT NULL, and a statement that leaves it out is asking for
        // one to be handed out rather than breaking the rule.
        let rowid = match rowid_column {
            Some(index) => match values[index] {
                Value::Int(number) => number,
                _ => {
                    let assigned = next_auto;
                    values[index] = Value::Int(assigned);
                    assigned
                }
            },
            None => next_auto,
        };
        next_auto = next_auto.max(rowid.saturating_add(1));

        for (index, column) in table.columns.iter().enumerate() {
            if column.not_null && matches!(values[index], Value::Null) {
                return Err(Error::NotNull(column.name.clone()));
            }
        }

        check_unique(pool, table, &values, rowid, writer.snapshot)?;
        tree.insert(
            pool,
            &row_key(rowid, writer.txid),
            &encode_version(0, &values),
        )?;
        index_row(pool, table, &values, rowid)?;
    }
    Ok(rows.len())
}

/// Converts a value to what a column declares, or explains why it cannot.
///
/// Only widening happens here: an integer written into a real column becomes a
/// real. Anything else is refused rather than silently reinterpreted, which is
/// the part of SQLite's behaviour worth not copying.
fn coerce(value: &Value, data_type: DataType, column: &str) -> Result<Value> {
    Ok(match (value, data_type) {
        (Value::Null, _) => Value::Null,
        (Value::Int(_), DataType::Integer) => value.clone(),
        (Value::Real(_), DataType::Real) => value.clone(),
        (Value::Text(_), DataType::Text) => value.clone(),
        (Value::Blob(_), DataType::Blob) => value.clone(),
        (Value::Bool(_), DataType::Boolean) => value.clone(),
        (Value::Int(number), DataType::Real) => Value::Real(*number as f64),
        (Value::Text(text), DataType::Blob) => Value::Blob(text.as_bytes().to_vec()),
        (found, wanted) => {
            return Err(Error::TypeMismatch {
                column: column.to_string(),
                wanted: wanted.as_str(),
                found: kind_of(found),
            })
        }
    })
}

fn kind_of(value: &Value) -> &'static str {
    match value {
        Value::Null => "NULL",
        Value::Int(_) => "INTEGER",
        Value::Real(_) => "REAL",
        Value::Text(_) => "TEXT",
        Value::Blob(_) => "BLOB",
        Value::Bool(_) => "BOOLEAN",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn like_handles_the_wildcards() {
        assert!(like("BR-0042", "BR-%"));
        assert!(like("BR-0042", "%0042"));
        assert!(like("BR-0042", "BR-____"));
        assert!(like("BR-0042", "%"));
        assert!(like("", "%"));
        assert!(like("abc", "a%c"));
        assert!(like("aXXXbXXXc", "a%b%c"));

        assert!(!like("BR-0042", "BR-___"));
        assert!(!like("BR-0042", "br-%"), "matching is case sensitive");
        assert!(!like("abc", "a%d"));
        assert!(!like("", "_"));
    }

    #[test]
    fn a_pattern_of_wildcards_does_not_blow_the_stack() {
        let text = "a".repeat(400);
        let pattern = "%".repeat(200) + "b";
        assert!(!like(&text, &pattern));
    }

    #[test]
    fn three_valued_logic_matches_the_truth_table() {
        let t = PlanExpr::Const(Value::Bool(true));
        let f = PlanExpr::Const(Value::Bool(false));
        let n = PlanExpr::Const(Value::Null);

        let and = |left: &PlanExpr, right: &PlanExpr| {
            eval(
                &PlanExpr::Binary {
                    left: Box::new(left.clone()),
                    op: BinaryOp::And,
                    right: Box::new(right.clone()),
                },
                &[],
            )
            .unwrap()
        };
        let or = |left: &PlanExpr, right: &PlanExpr| {
            eval(
                &PlanExpr::Binary {
                    left: Box::new(left.clone()),
                    op: BinaryOp::Or,
                    right: Box::new(right.clone()),
                },
                &[],
            )
            .unwrap()
        };

        assert_eq!(and(&t, &n), Value::Null);
        assert_eq!(and(&f, &n), Value::Bool(false), "false wins over unknown");
        assert_eq!(and(&n, &n), Value::Null);
        assert_eq!(or(&t, &n), Value::Bool(true), "true wins over unknown");
        assert_eq!(or(&f, &n), Value::Null);
        assert_eq!(or(&n, &n), Value::Null);
    }

    #[test]
    fn comparing_against_null_is_unknown_not_false() {
        assert_eq!(compare(&Value::Null, &Value::Null), None);
        assert_eq!(compare(&Value::Int(1), &Value::Null), None);
        assert_eq!(
            compare(&Value::Int(1), &Value::Real(1.5)),
            Some(Ordering::Less),
            "integers and reals compare as numbers"
        );
        assert_eq!(
            compare(&Value::Int(1), &Value::Text("1".into())),
            None,
            "different kinds do not compare"
        );
    }

    #[test]
    fn dividing_by_zero_is_unknown_rather_than_a_failure() {
        let divide =
            |a: i64, b: i64| arithmetic(&Value::Int(a), BinaryOp::Div, &Value::Int(b)).unwrap();
        assert_eq!(divide(6, 3), Value::Int(2));
        assert_eq!(divide(1, 0), Value::Null);
    }

    #[test]
    fn integer_overflow_is_refused_rather_than_wrapped() {
        let result = arithmetic(&Value::Int(i64::MAX), BinaryOp::Add, &Value::Int(1));
        assert!(result.is_err());
    }
}
