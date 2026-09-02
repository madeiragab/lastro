//! Evaluating expressions, and running a plan.
//!
//! The iterator model, also called Volcano: every operator is a `next` that
//! pulls one row from the operator below. A query with a filter over a huge
//! table streams through in constant memory; only `Sort` has to materialize,
//! and it is the only one that does.

use std::cmp::Ordering;

use crate::index::{BTree, Cursor};
use crate::sql::ast::{BinaryOp, DataType, UnaryOp};
use crate::sql::catalog::TableSchema;
use crate::sql::plan::{Bound, Plan, PlanExpr};
use crate::storage::page::encoding::{decode_tuple, encode_i64, encode_tuple, Value, ValueType};
use crate::storage::BufferPool;
use crate::{Error, Result};

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

// -- operators -------------------------------------------------------------

/// A running operator.
#[derive(Debug)]
pub enum Op {
    /// Walks a table's tree, decoding each row.
    Scan {
        /// The column types, for decoding.
        types: Vec<ValueType>,
        /// Where in the tree the scan is.
        cursor: Cursor,
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
    Sort {
        /// Where rows come from.
        input: Box<Op>,
        /// What to sort by, and whether each key descends.
        keys: Vec<(PlanExpr, bool)>,
        /// How many rows are wanted, when a limit sits above.
        top: Option<usize>,
        /// The sorted rows, once the input has been drained.
        buffered: Option<std::vec::IntoIter<Row>>,
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
            Op::Scan { types, cursor } => match cursor.next(pool)? {
                Some((_, value)) => decode_tuple(&value, types)
                    .map(Some)
                    .ok_or_else(|| Error::MalformedFile("a row that does not decode".into())),
                None => Ok(None),
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
                buffered,
            } => {
                if buffered.is_none() {
                    let mut rows = Vec::new();
                    while let Some(row) = input.next(pool)? {
                        rows.push(row);
                    }

                    let mut failure = None;
                    rows.sort_by(|left, right| {
                        for (expr, descending) in keys.iter() {
                            let ordering = match (eval(expr, left), eval(expr, right)) {
                                (Ok(a), Ok(b)) => match compare(&a, &b) {
                                    Some(ordering) => ordering,
                                    // Nulls sort before everything, the same
                                    // way they do in an encoded key.
                                    None => null_order(&a, &b),
                                },
                                (Err(error), _) | (_, Err(error)) => {
                                    failure.get_or_insert(error);
                                    Ordering::Equal
                                }
                            };
                            if ordering != Ordering::Equal {
                                return if *descending {
                                    ordering.reverse()
                                } else {
                                    ordering
                                };
                            }
                        }
                        Ordering::Equal
                    });
                    if let Some(error) = failure {
                        return Err(error);
                    }

                    if let Some(limit) = top {
                        rows.truncate(*limit);
                    }
                    *buffered = Some(rows.into_iter());
                }
                Ok(buffered.as_mut().expect("filled above").next())
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

/// Builds the running operator tree from a plan.
pub fn build(plan: &Plan, pool: &mut BufferPool) -> Result<Op> {
    Ok(match plan {
        Plan::SeqScan { table } => Op::Scan {
            types: column_types(table),
            cursor: BTree::open(table.root).cursor(pool, None)?,
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
            }
        }
        Plan::Filter { input, predicate } => Op::Filter {
            input: Box::new(build(input, pool)?),
            predicate: predicate.clone(),
        },
        Plan::Project { input, exprs, .. } => Op::Project {
            input: Box::new(build(input, pool)?),
            exprs: exprs.clone(),
        },
        Plan::Sort { input, keys, top } => Op::Sort {
            input: Box::new(build(input, pool)?),
            keys: keys.clone(),
            top: *top,
            buffered: None,
        },
        Plan::Limit {
            input,
            limit,
            offset,
        } => Op::Limit {
            input: Box::new(build(input, pool)?),
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

/// The rows a write is about to touch, gathered before any of them change.
///
/// Collected first on purpose. A cursor holds no pin between calls, which is
/// what keeps it cheap, and the price of that is it must not walk a tree that
/// is being rewritten underneath it. Reading the whole match set first costs
/// memory proportional to what the statement touches and removes the hazard
/// entirely.
fn gather(
    pool: &mut BufferPool,
    table: &TableSchema,
    filter: Option<&PlanExpr>,
    lower: Option<Bound>,
    upper: Option<Bound>,
) -> Result<Vec<(Vec<u8>, Row)>> {
    let types = column_types(table);
    let tree = BTree::open(table.root);
    let (low, high) = encode_bounds(lower, upper);
    let mut cursor = tree.cursor_range(pool, low.as_deref(), high.as_deref())?;

    let mut matched = Vec::new();
    while let Some((key, encoded)) = cursor.next(pool)? {
        let row = decode_tuple(&encoded, &types)
            .ok_or_else(|| Error::MalformedFile("a row that does not decode".into()))?;
        let admitted = match filter {
            Some(predicate) => truth(&eval(predicate, &row)?) == Some(true),
            None => true,
        };
        if admitted {
            matched.push((key, row));
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
        encode_i64(value).to_vec()
    });
    // The upper edge of a cursor is exclusive, so an inclusive bound is one
    // past the value it names.
    let high = upper.map(|bound| {
        let value = if bound.inclusive {
            bound.value.saturating_add(1)
        } else {
            bound.value
        };
        encode_i64(value).to_vec()
    });
    (low, high)
}

/// Removes every row the filter admits, returning how many went.
pub fn delete(
    pool: &mut BufferPool,
    table: &TableSchema,
    filter: Option<&PlanExpr>,
    lower: Option<Bound>,
    upper: Option<Bound>,
) -> Result<usize> {
    let matched = gather(pool, table, filter, lower, upper)?;
    let mut tree = BTree::open(table.root);
    for (key, _) in &matched {
        tree.delete(pool, key)?;
    }
    Ok(matched.len())
}

/// Applies the assignments to every row the filter admits.
///
/// A change to the primary key moves the row, because the primary key is the
/// key of the table own tree. That is a removal and a fresh write rather than
/// an edit in place, and the order matters: writing first and removing after
/// would delete the row that had just been written.
pub fn update(
    pool: &mut BufferPool,
    table: &TableSchema,
    assignments: &[(usize, PlanExpr)],
    filter: Option<&PlanExpr>,
    lower: Option<Bound>,
    upper: Option<Bound>,
) -> Result<usize> {
    let matched = gather(pool, table, filter, lower, upper)?;
    let rowid_column = table.rowid_column();
    let mut tree = BTree::open(table.root);

    for (key, row) in &matched {
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

        let new_key = match rowid_column {
            Some(index) => match updated[index] {
                Value::Int(number) => encode_i64(number).to_vec(),
                // Anything else was already refused by the coercion above,
                // unless it was null, which the check above catches.
                _ => return Err(Error::NotNull(table.column(index).name.clone())),
            },
            None => key.clone(),
        };

        let mut encoded = Vec::new();
        encode_tuple(&updated, &mut encoded);
        if new_key != *key {
            tree.delete(pool, key)?;
        }
        tree.insert(pool, &new_key, &encoded)?;
    }
    Ok(matched.len())
}

/// Writes the rows of an `INSERT`, returning how many landed.
pub fn insert(
    pool: &mut BufferPool,
    table: &TableSchema,
    targets: &[usize],
    rows: &[Vec<PlanExpr>],
) -> Result<usize> {
    let mut tree = BTree::open(table.root);
    let rowid_column = table.rowid_column();

    // Where an auto-assigned row id starts. Read once, from the rightmost edge
    // of the tree, rather than kept as a counter the catalog has to rewrite on
    // every insert.
    let mut next_auto = match tree.last_key(pool)? {
        Some(key) if key.len() == 8 => {
            let mut bytes = [0u8; 8];
            bytes.copy_from_slice(&key);
            crate::storage::page::encoding::decode_i64(bytes).saturating_add(1)
        }
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

        let mut encoded = Vec::new();
        encode_tuple(&values, &mut encoded);
        tree.insert(pool, &encode_i64(rowid), &encoded)?;
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
