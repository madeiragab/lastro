//! The SQL front end: text in, an operator tree out.
//!
//! ```text
//! text -> lexer -> parser -> AST -> binder -> planner -> executor -> rows
//! ```
//!
//! Each arrow is a transformation between data structures, testable on its own.
//! The parser never touches disk. See `docs/en/06-sql.md`.
//!
//! Implemented so far: `CREATE TABLE`, `INSERT`, `SELECT` over one table with
//! `WHERE`, `ORDER BY` and `LIMIT`, and `EXPLAIN`. Joins, secondary indexes,
//! `UPDATE` and `DELETE` come next.
//!
//! ```
//! use lastro::sql::{parse, Statement};
//!
//! let statement = parse("SELECT brinco FROM gado WHERE peso > 400").unwrap();
//! assert!(matches!(statement, Statement::Select(_)));
//!
//! // Every tree renders back into SQL that parses to the same tree, which is
//! // the cheapest property test a parser can have.
//! let again = parse(&statement.to_string()).unwrap();
//! assert_eq!(statement, again);
//! ```

pub mod ast;
pub mod catalog;
pub mod engine;
pub mod exec;
pub mod lexer;
pub mod mvcc;
pub mod parser;
pub mod plan;

pub use ast::{
    BinaryOp, ColumnConstraint, ColumnDef, CreateIndex, CreateTable, DataType, Delete, Expr,
    Insert, Join, Literal, OrderItem, ProjItem, Projection, Select, Statement, TableRef, UnaryOp,
    Update,
};
pub use catalog::{Catalog, ColumnSchema, TableSchema};
pub use engine::{Database, Outcome};
pub use exec::Row;
pub use lexer::{tokenize, Keyword, Token, TokenKind};
pub use mvcc::{Snapshot, Version};
pub use parser::{parse, parse_many};
pub use plan::{Plan, PlanExpr};
