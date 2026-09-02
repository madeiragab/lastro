//! The schema, stored in the database's own storage.
//!
//! The catalog is a B+Tree keyed by table name, and its root lives in the
//! metadata page's `catalog_root`. That is the only external pointer the
//! database has; everything else is reached from it.
//!
//! The payoff is that `CREATE TABLE` is an ordinary transaction. It gets the
//! write-ahead log, redo and undo for free, and a `CREATE TABLE` interrupted by
//! a crash is undone by exactly the same machinery that undoes an `INSERT`.
//! There is no special code to make schema changes atomic, which is a classic
//! source of bugs in databases that keep the schema outside the engine.
//!
//! # Deviation from the specification
//!
//! `docs/en/06-sql.md` describes three catalog tables — `lastro_tables`,
//! `lastro_columns`, `lastro_indexes`. This is one tree holding one record per
//! table instead. It keeps the property that matters, which is that the schema
//! lives in transactional storage, and costs three quarters of the code. The
//! split into three tables becomes worthwhile when `SELECT` can read the
//! catalog like any other table, and that is not yet true.

use crate::index::BTree;
use crate::sql::ast::DataType;
use crate::storage::page::encoding::{get_varint, put_varint, Value};
use crate::storage::BufferPool;
use crate::{Error, PageId, Result};

/// One column of a table.
#[derive(Debug, Clone, PartialEq)]
pub struct ColumnSchema {
    /// The column's name, as written.
    pub name: String,
    /// Its declared type.
    pub data_type: DataType,
    /// Whether it refuses nulls.
    pub not_null: bool,
    /// Whether it is the primary key.
    pub primary_key: bool,
    /// Whether the schema asked for it to be unique.
    pub unique: bool,
    /// What to store when a statement leaves it out.
    pub default: Option<Value>,
}

/// One secondary index.
///
/// A tree whose keys are the indexed columns followed by the row id, and whose
/// values are empty. The row id rides in the key so that two rows sharing an
/// index value stay distinct entries, and so that a match hands back the row to
/// fetch without a second structure.
#[derive(Debug, Clone, PartialEq)]
pub struct IndexSchema {
    /// The index's name.
    pub name: String,
    /// The root of its tree.
    pub root: PageId,
    /// Whether two rows may share a key.
    pub unique: bool,
    /// Which columns it covers, by position in the table.
    pub columns: Vec<usize>,
}

/// One table.
#[derive(Debug, Clone, PartialEq)]
pub struct TableSchema {
    /// The table's name.
    pub name: String,
    /// The root of the tree holding its rows, keyed by row id.
    pub root: PageId,
    /// Its columns, in the order they were declared.
    pub columns: Vec<ColumnSchema>,
    /// Its secondary indexes.
    pub indexes: Vec<IndexSchema>,
}

impl TableSchema {
    /// The position of a column, matched without regard to case.
    pub fn column_index(&self, name: &str) -> Option<usize> {
        self.columns
            .iter()
            .position(|column| column.name.eq_ignore_ascii_case(name))
    }

    /// The column at a position.
    pub fn column(&self, index: usize) -> &ColumnSchema {
        &self.columns[index]
    }

    /// The index whose leading column is `column`, if there is one.
    ///
    /// Leading, because an index on `(a, b)` orders by `a` first: a predicate
    /// on `b` alone reaches nothing the index can narrow.
    pub fn index_leading_on(&self, column: usize) -> Option<&IndexSchema> {
        self.indexes
            .iter()
            .find(|index| index.columns.first() == Some(&column))
    }
}

/// Reads and writes the schema.
///
/// Holds nothing but the root page: every lookup goes to the tree, so a schema
/// changed by one statement is visible to the next without anything to
/// invalidate.
#[derive(Debug, Clone, Copy)]
pub struct Catalog {
    tree: BTree,
}

impl Catalog {
    /// Opens the catalog, creating it if the database has none yet.
    ///
    /// Creating it writes the new root into the metadata page, so the caller
    /// must be inside a transaction for that to be durable.
    pub fn open(pool: &mut BufferPool) -> Result<Catalog> {
        let root = pool.pager().meta().catalog_root;
        if root != crate::NO_PAGE {
            return Ok(Catalog {
                tree: BTree::open(root),
            });
        }
        let tree = BTree::create(pool)?;
        pool.pager_mut().meta_mut().catalog_root = tree.root();
        Ok(Catalog { tree })
    }

    /// The catalog's root page.
    pub fn root(&self) -> PageId {
        self.tree.root()
    }

    /// Looks a table up by name, ignoring case.
    pub fn table(&self, pool: &mut BufferPool, name: &str) -> Result<Option<TableSchema>> {
        let key = name.to_ascii_lowercase();
        match self.tree.get(pool, key.as_bytes())? {
            Some(bytes) => Ok(Some(decode_table(&bytes)?)),
            None => Ok(None),
        }
    }

    /// Looks a table up, failing if it is not there.
    pub fn require(&self, pool: &mut BufferPool, name: &str) -> Result<TableSchema> {
        self.table(pool, name)?
            .ok_or_else(|| Error::UnknownTable(name.to_string()))
    }

    /// Writes a table's schema, replacing any entry under the same name.
    pub fn put(&mut self, pool: &mut BufferPool, schema: &TableSchema) -> Result<()> {
        let key = schema.name.to_ascii_lowercase();
        let mut value = Vec::new();
        encode_table(schema, &mut value);
        self.tree.insert(pool, key.as_bytes(), &value)
    }

    /// Every table, in name order.
    pub fn tables(&self, pool: &mut BufferPool) -> Result<Vec<TableSchema>> {
        self.tree
            .iter(pool)?
            .into_iter()
            .map(|(_, bytes)| decode_table(&bytes))
            .collect()
    }
}

// -- the record format -----------------------------------------------------
//
// Hand rolled rather than reusing the tuple encoding, because a schema is a
// nested thing and the tuple format is deliberately flat.

fn put_bytes(out: &mut Vec<u8>, bytes: &[u8]) {
    put_varint(out, bytes.len() as u64);
    out.extend_from_slice(bytes);
}

fn take_bytes<'a>(input: &mut &'a [u8]) -> Result<&'a [u8]> {
    let (length, read) = get_varint(input).ok_or_else(|| malformed("a truncated length"))?;
    let length = length as usize;
    let bytes = input
        .get(read..read + length)
        .ok_or_else(|| malformed("a length past the end of the record"))?;
    *input = &input[read + length..];
    Ok(bytes)
}

fn take_byte(input: &mut &[u8]) -> Result<u8> {
    let (&byte, rest) = input
        .split_first()
        .ok_or_else(|| malformed("a truncated record"))?;
    *input = rest;
    Ok(byte)
}

fn encode_table(schema: &TableSchema, out: &mut Vec<u8>) {
    put_bytes(out, schema.name.as_bytes());
    put_varint(out, schema.root as u64);
    put_varint(out, schema.columns.len() as u64);
    for column in &schema.columns {
        put_bytes(out, column.name.as_bytes());
        out.push(type_byte(column.data_type));
        let mut flags = 0u8;
        if column.not_null {
            flags |= 0b0000_0001;
        }
        if column.primary_key {
            flags |= 0b0000_0010;
        }
        if column.unique {
            flags |= 0b0000_0100;
        }
        out.push(flags);
        match &column.default {
            None => out.push(0),
            Some(value) => {
                out.push(1);
                encode_value(value, out);
            }
        }
    }

    put_varint(out, schema.indexes.len() as u64);
    for index in &schema.indexes {
        put_bytes(out, index.name.as_bytes());
        put_varint(out, index.root as u64);
        out.push(u8::from(index.unique));
        put_varint(out, index.columns.len() as u64);
        for column in &index.columns {
            put_varint(out, *column as u64);
        }
    }
}

fn decode_table(bytes: &[u8]) -> Result<TableSchema> {
    let mut input = bytes;
    let name = String::from_utf8(take_bytes(&mut input)?.to_vec())
        .map_err(|_| malformed("a table name that is not text"))?;
    let (root, read) = get_varint(input).ok_or_else(|| malformed("a truncated root page"))?;
    input = &input[read..];
    let (count, read) = get_varint(input).ok_or_else(|| malformed("a truncated column count"))?;
    input = &input[read..];

    let mut columns = Vec::with_capacity(count as usize);
    for _ in 0..count {
        let name = String::from_utf8(take_bytes(&mut input)?.to_vec())
            .map_err(|_| malformed("a column name that is not text"))?;
        let data_type = type_of(take_byte(&mut input)?)?;
        let flags = take_byte(&mut input)?;
        let default = match take_byte(&mut input)? {
            0 => None,
            _ => Some(decode_value(&mut input)?),
        };
        columns.push(ColumnSchema {
            name,
            data_type,
            not_null: flags & 0b0000_0001 != 0,
            primary_key: flags & 0b0000_0010 != 0,
            unique: flags & 0b0000_0100 != 0,
            default,
        });
    }

    let (index_count, read) =
        get_varint(input).ok_or_else(|| malformed("a truncated index count"))?;
    input = &input[read..];
    let mut indexes = Vec::with_capacity(index_count as usize);
    for _ in 0..index_count {
        let name = String::from_utf8(take_bytes(&mut input)?.to_vec())
            .map_err(|_| malformed("an index name that is not text"))?;
        let (root, read) = get_varint(input).ok_or_else(|| malformed("a truncated index root"))?;
        input = &input[read..];
        let unique = take_byte(&mut input)? != 0;
        let (count, read) =
            get_varint(input).ok_or_else(|| malformed("a truncated index column count"))?;
        input = &input[read..];
        let mut columns = Vec::with_capacity(count as usize);
        for _ in 0..count {
            let (column, read) =
                get_varint(input).ok_or_else(|| malformed("a truncated index column"))?;
            input = &input[read..];
            columns.push(column as usize);
        }
        indexes.push(IndexSchema {
            name,
            root: root as PageId,
            unique,
            columns,
        });
    }

    Ok(TableSchema {
        name,
        root: root as PageId,
        columns,
        indexes,
    })
}

fn type_byte(data_type: DataType) -> u8 {
    match data_type {
        DataType::Integer => 1,
        DataType::Real => 2,
        DataType::Text => 3,
        DataType::Blob => 4,
        DataType::Boolean => 5,
    }
}

fn type_of(byte: u8) -> Result<DataType> {
    Ok(match byte {
        1 => DataType::Integer,
        2 => DataType::Real,
        3 => DataType::Text,
        4 => DataType::Blob,
        5 => DataType::Boolean,
        other => return Err(malformed(format!("an unknown column type {other}"))),
    })
}

fn encode_value(value: &Value, out: &mut Vec<u8>) {
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
            put_bytes(out, text.as_bytes());
        }
        Value::Blob(bytes) => {
            out.push(4);
            put_bytes(out, bytes);
        }
        Value::Bool(flag) => {
            out.push(5);
            out.push(u8::from(*flag));
        }
    }
}

fn decode_value(input: &mut &[u8]) -> Result<Value> {
    Ok(match take_byte(input)? {
        0 => Value::Null,
        1 => Value::Int(i64::from_le_bytes(take_eight(input)?)),
        2 => Value::Real(f64::from_le_bytes(take_eight(input)?)),
        3 => Value::Text(
            String::from_utf8(take_bytes(input)?.to_vec())
                .map_err(|_| malformed("a default that is not text"))?,
        ),
        4 => Value::Blob(take_bytes(input)?.to_vec()),
        5 => Value::Bool(take_byte(input)? != 0),
        other => return Err(malformed(format!("an unknown value tag {other}"))),
    })
}

fn take_eight(input: &mut &[u8]) -> Result<[u8; 8]> {
    let slice = input
        .get(..8)
        .ok_or_else(|| malformed("a truncated number"))?;
    let mut out = [0u8; 8];
    out.copy_from_slice(slice);
    *input = &input[8..];
    Ok(out)
}

fn malformed(why: impl Into<String>) -> Error {
    Error::MalformedFile(format!("the catalog holds {}", why.into()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn schema() -> TableSchema {
        TableSchema {
            name: "gado".into(),
            root: 42,
            indexes: vec![IndexSchema {
                name: "idx_brinco".into(),
                root: 77,
                unique: true,
                columns: vec![1],
            }],
            columns: vec![
                ColumnSchema {
                    name: "id".into(),
                    data_type: DataType::Integer,
                    not_null: true,
                    primary_key: true,
                    unique: false,
                    default: None,
                },
                ColumnSchema {
                    name: "brinco".into(),
                    data_type: DataType::Text,
                    not_null: true,
                    primary_key: false,
                    unique: false,
                    default: Some(Value::Text("sem brinco".into())),
                },
                ColumnSchema {
                    name: "peso".into(),
                    data_type: DataType::Real,
                    not_null: false,
                    primary_key: false,
                    unique: false,
                    default: Some(Value::Real(0.0)),
                },
                ColumnSchema {
                    name: "ativo".into(),
                    data_type: DataType::Boolean,
                    not_null: false,
                    primary_key: false,
                    unique: false,
                    default: Some(Value::Bool(true)),
                },
            ],
        }
    }

    #[test]
    fn a_schema_round_trips() {
        let mut bytes = Vec::new();
        encode_table(&schema(), &mut bytes);
        assert_eq!(decode_table(&bytes).unwrap(), schema());
    }

    #[test]
    fn a_truncated_record_is_reported_not_read() {
        let mut bytes = Vec::new();
        encode_table(&schema(), &mut bytes);
        for cut in 0..bytes.len() {
            assert!(
                decode_table(&bytes[..cut]).is_err(),
                "a record cut at {cut} decoded"
            );
        }
    }

    #[test]
    fn columns_are_found_without_regard_to_case() {
        let schema = schema();
        assert_eq!(schema.column_index("BRINCO"), Some(1));
        assert_eq!(schema.column_index("brinco"), Some(1));
        assert_eq!(schema.column_index("ausente"), None);
    }
}
