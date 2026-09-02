//! Binding names to positions, and choosing how to run a statement.
//!
//! The binder resolves every table and column against the catalog, so that from
//! here down nothing compares strings on the hot path — a column is an index
//! into a row. The planner then applies fixed rules, in order, to turn the bound
//! statement into a tree of operators.
//!
//! Rule based rather than cost based, on purpose: the goal of this project is
//! storage and durability, not query optimization, and fixed rules give
//! reasonable plans for a fraction of the effort. See `docs/en/adr.md`, ADR-007.

use std::fmt::Write as _;

use crate::sql::ast;
use crate::sql::ast::{BinaryOp, DataType, UnaryOp};
use crate::sql::catalog::{Catalog, ColumnSchema, IndexSchema, TableSchema};
use crate::storage::page::encoding::Value;
use crate::storage::BufferPool;
use crate::{Error, Result};

/// An expression with every name already resolved.
#[derive(Debug, Clone, PartialEq)]
pub enum PlanExpr {
    /// A value fixed when the statement was written.
    Const(Value),
    /// A column, by position in the row.
    Column(usize),
    /// A prefix operator.
    Unary {
        /// Which operator.
        op: UnaryOp,
        /// What it applies to.
        operand: Box<PlanExpr>,
    },
    /// An infix operator.
    Binary {
        /// The left operand.
        left: Box<PlanExpr>,
        /// Which operator.
        op: BinaryOp,
        /// The right operand.
        right: Box<PlanExpr>,
    },
    /// `IS NULL`, negated or not.
    IsNull {
        /// What is being tested.
        operand: Box<PlanExpr>,
        /// Whether `NOT` was written.
        negated: bool,
    },
    /// `LIKE`, negated or not.
    Like {
        /// What is being matched.
        left: Box<PlanExpr>,
        /// The pattern.
        pattern: Box<PlanExpr>,
        /// Whether `NOT` was written.
        negated: bool,
    },
    /// `BETWEEN`, negated or not.
    Between {
        /// What is being tested.
        operand: Box<PlanExpr>,
        /// The lower bound, inclusive.
        low: Box<PlanExpr>,
        /// The upper bound, inclusive.
        high: Box<PlanExpr>,
        /// Whether `NOT` was written.
        negated: bool,
    },
}

/// One end of a row id range.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Bound {
    /// The row id at the edge.
    pub value: i64,
    /// Whether the edge itself is included.
    pub inclusive: bool,
}

/// A node of the plan.
#[derive(Debug, Clone, PartialEq)]
pub enum Plan {
    /// Walks every row of a table in row id order.
    SeqScan {
        /// The table being read.
        table: TableSchema,
    },
    /// Walks the part of a table a row id range covers.
    ///
    /// Only possible on a table whose primary key is the row id; see
    /// [`TableSchema::rowid_column`].
    RowIdScan {
        /// The table being read.
        table: TableSchema,
        /// The lower edge of the range.
        lower: Option<Bound>,
        /// The upper edge of the range.
        upper: Option<Bound>,
    },
    /// Drops rows the predicate does not admit.
    Filter {
        /// Where the rows come from.
        input: Box<Plan>,
        /// The condition. Only `TRUE` admits a row.
        predicate: PlanExpr,
    },
    /// Computes the output columns.
    Project {
        /// Where the rows come from.
        input: Box<Plan>,
        /// One expression per output column.
        exprs: Vec<PlanExpr>,
        /// What to call each of them.
        names: Vec<String>,
    },
    /// Orders rows. Blocking: it has to see them all before yielding any.
    Sort {
        /// Where the rows come from.
        input: Box<Plan>,
        /// What to sort by, and whether each key descends.
        keys: Vec<(PlanExpr, bool)>,
        /// How many rows are actually wanted, when a limit sits above.
        top: Option<usize>,
    },
    /// Counts and stops.
    Limit {
        /// Where the rows come from.
        input: Box<Plan>,
        /// How many rows to yield, if bounded.
        limit: Option<u64>,
        /// How many to skip first.
        offset: u64,
    },
    /// Writes rows into a table.
    Insert {
        /// The table being written to.
        table: TableSchema,
        /// One vector of expressions per row.
        rows: Vec<Vec<PlanExpr>>,
        /// Where each supplied value goes in the table's column order.
        targets: Vec<usize>,
    },
    /// Finds rows through a secondary index rather than by walking the table.
    ///
    /// Two descents instead of a scan: one into the index to find the row ids,
    /// one into the table for each row.
    IndexScan {
        /// The table the rows come from.
        table: TableSchema,
        /// The index being read.
        index: IndexSchema,
        /// The value its leading column must equal.
        key: Value,
    },
    /// Pairs every row of one side with every row of the other, keeping the
    /// pairs the condition admits.
    NestedLoopJoin {
        /// The outer input.
        left: Box<Plan>,
        /// The inner input, walked once per outer row.
        right: Box<Plan>,
        /// The condition, over the two rows joined end to end.
        on: PlanExpr,
    },
    /// Builds a table from one side and probes it with the other.
    HashJoin {
        /// The side the table is built from.
        left: Box<Plan>,
        /// The side that probes it.
        right: Box<Plan>,
        /// The key on the build side, over a joined row.
        left_key: PlanExpr,
        /// The key on the probe side, over a row of the right input alone.
        right_key: PlanExpr,
        /// Whatever the equality did not cover, over the joined row.
        residual: Option<PlanExpr>,
    },
    /// Changes rows in place.
    Update {
        /// The table being changed.
        table: TableSchema,
        /// Which column to set, and to what.
        assignments: Vec<(usize, PlanExpr)>,
        /// Which rows to change. Absent means every one.
        filter: Option<PlanExpr>,
        /// The lower edge of the row id range to walk.
        lower: Option<Bound>,
        /// The upper edge of the row id range to walk.
        upper: Option<Bound>,
    },
    /// Removes rows.
    Delete {
        /// The table being emptied of them.
        table: TableSchema,
        /// Which rows to remove. Absent means every one.
        filter: Option<PlanExpr>,
        /// The lower edge of the row id range to walk.
        lower: Option<Bound>,
        /// The upper edge of the row id range to walk.
        upper: Option<Bound>,
    },
    /// Adds an index to a table.
    CreateIndex {
        /// The table to index.
        table: TableSchema,
        /// The index to build. Its root page is filled in when it runs.
        index: IndexSchema,
    },
    /// Adds a table to the catalog.
    CreateTable {
        /// The schema to record. Its root page is filled in when it runs.
        schema: TableSchema,
        /// Whether to do nothing when the table is already there.
        if_not_exists: bool,
    },
    /// Starts a transaction.
    Begin,
    /// Ends one, keeping its work.
    Commit,
    /// Ends one, discarding its work.
    Rollback,
}

impl TableSchema {
    /// The column whose value is the row id, if the table has one.
    ///
    /// An `INTEGER PRIMARY KEY` becomes the key of the table's own tree rather
    /// than a separate counter, so looking a row up by that column is a descent
    /// rather than a scan. Same arrangement SQLite uses.
    pub fn rowid_column(&self) -> Option<usize> {
        self.columns
            .iter()
            .position(|column| column.primary_key && column.data_type == DataType::Integer)
    }
}

/// Binds a statement against the catalog and plans it.
pub fn plan(pool: &mut BufferPool, catalog: &Catalog, statement: &ast::Statement) -> Result<Plan> {
    match statement {
        ast::Statement::Begin => Ok(Plan::Begin),
        ast::Statement::Commit => Ok(Plan::Commit),
        ast::Statement::Rollback => Ok(Plan::Rollback),
        ast::Statement::Select(select) => plan_select(pool, catalog, select),
        ast::Statement::Insert(insert) => plan_insert(pool, catalog, insert),
        ast::Statement::Update(update) => plan_update(pool, catalog, update),
        ast::Statement::Delete(delete) => plan_delete(pool, catalog, delete),
        ast::Statement::CreateTable(create) => plan_create_table(create),
        ast::Statement::Explain(_) => Err(Error::Unsupported(
            "EXPLAIN cannot wrap another EXPLAIN".into(),
        )),
        ast::Statement::CreateIndex(create) => plan_create_index(pool, catalog, create),
    }
}

fn plan_select(pool: &mut BufferPool, catalog: &Catalog, select: &ast::Select) -> Result<Plan> {
    let table = catalog.require(pool, &select.from.name)?;
    let mut scope = Scope::single(&table, select.from.alias.as_deref());

    // Every joined table joins the scope before anything is bound, so a
    // condition may name a column of either side.
    let mut joined = Vec::with_capacity(select.joins.len());
    for join in &select.joins {
        let right = catalog.require(pool, &join.table.name)?;
        let boundary = scope.width();
        scope.push(&right, join.table.alias.as_deref());
        joined.push((right, boundary));
    }

    let filter = select
        .filter
        .as_ref()
        .map(|expr| bind_expr(&scope, expr))
        .transpose()?;

    // Rule 1, access selection. A predicate that pins the row id down turns the
    // scan into a descent, which is the whole reason the primary key is the key
    // of the table's own tree. Only for a lone table: with a join the predicate
    // may name either side, and deciding which one it narrows is the job of a
    // cost based planner this one is not.
    let (lower, upper, residual) = match (&filter, table.rowid_column(), joined.is_empty()) {
        (Some(predicate), Some(rowid), true) => split_rowid_bounds(predicate, rowid),
        _ => (None, None, filter.clone()),
    };

    // Rule 1 again, one step further out: with no row id range to use, an
    // equality on the leading column of an index reaches the rows through it
    // instead of walking every one.
    let indexed = match (
        &residual,
        joined.is_empty(),
        lower.is_some() || upper.is_some(),
    ) {
        (Some(predicate), true, false) => index_lookup(predicate, &table),
        _ => None,
    };

    let mut plan = match (&indexed, lower.is_some() || upper.is_some()) {
        (Some((index, key, _)), _) => Plan::IndexScan {
            table: table.clone(),
            index: (*index).clone(),
            key: key.clone(),
        },
        (None, true) => Plan::RowIdScan {
            table: table.clone(),
            lower,
            upper,
        },
        (None, false) => Plan::SeqScan {
            table: table.clone(),
        },
    };
    // The predicate stays even when an index answers it. Index entries are
    // not removed when a version is superseded, so an entry may name a row
    // whose visible version no longer matches; re-checking above is what makes
    // a stale entry harmless rather than wrong.
    let _ = &indexed;

    // Rule 4, join selection. An equality with one side reading only the left
    // input and the other only the right is what a hash join needs; anything
    // else has to be checked pair by pair.
    for (join, (right, boundary)) in select.joins.iter().zip(joined) {
        let on = bind_expr(&scope, &join.on)?;
        let inner = Plan::SeqScan {
            table: right.clone(),
        };
        plan = match hash_keys(&on, boundary) {
            Some((left_key, right_key, residual)) => Plan::HashJoin {
                left: Box::new(plan),
                right: Box::new(inner),
                left_key,
                right_key: rebase(&right_key, boundary),
                residual,
            },
            None => Plan::NestedLoopJoin {
                left: Box::new(plan),
                right: Box::new(inner),
                on,
            },
        };
    }

    // Rule 2, predicate pushdown. With one table the filter already sits
    // directly above the scan; the rule earns its keep once joins exist.
    if let Some(predicate) = residual {
        plan = Plan::Filter {
            input: Box::new(plan),
            predicate,
        };
    }

    // Rule 5, sort elimination. A scan already yields rows in row id order, so
    // ordering by the primary key ascending is work that has already happened.
    let keys: Vec<(PlanExpr, bool)> = select
        .order_by
        .iter()
        .map(|item| Ok((bind_expr(&scope, &item.expr)?, item.descending)))
        .collect::<Result<_>>()?;

    let already_ordered = match (table.rowid_column(), select.joins.is_empty()) {
        (Some(rowid), true) => keys
            .iter()
            .all(|(expr, descending)| !descending && *expr == PlanExpr::Column(rowid)),
        _ => keys.is_empty(),
    };

    if !keys.is_empty() && !already_ordered {
        // Rule 6, limit pushdown. A limit above a sort becomes a bounded heap
        // rather than sorting everything and throwing most of it away.
        let top = select
            .limit
            .map(|limit| limit.saturating_add(select.offset.unwrap_or(0)) as usize);
        plan = Plan::Sort {
            input: Box::new(plan),
            keys,
            top,
        };
    }

    let (exprs, names) = bind_projection(&scope, &select.projection)?;
    plan = Plan::Project {
        input: Box::new(plan),
        exprs,
        names,
    };

    if select.limit.is_some() || select.offset.is_some() {
        plan = Plan::Limit {
            input: Box::new(plan),
            limit: select.limit,
            offset: select.offset.unwrap_or(0),
        };
    }
    Ok(plan)
}

fn plan_update(pool: &mut BufferPool, catalog: &Catalog, update: &ast::Update) -> Result<Plan> {
    let table = catalog.require(pool, &update.table)?;
    let scope = Scope::single(&table, None);

    let assignments = update
        .assignments
        .iter()
        .map(|(column, value)| {
            let index = table
                .column_index(column)
                .ok_or_else(|| Error::UnknownColumn(column.clone()))?;
            Ok((index, bind_expr(&scope, value)?))
        })
        .collect::<Result<Vec<_>>>()?;

    let (lower, upper, filter) = narrow(&scope, &table, update.filter.as_ref())?;
    Ok(Plan::Update {
        table,
        assignments,
        filter,
        lower,
        upper,
    })
}

fn plan_delete(pool: &mut BufferPool, catalog: &Catalog, delete: &ast::Delete) -> Result<Plan> {
    let table = catalog.require(pool, &delete.table)?;
    let scope = Scope::single(&table, None);
    let (lower, upper, filter) = narrow(&scope, &table, delete.filter.as_ref())?;
    Ok(Plan::Delete {
        table,
        filter,
        lower,
        upper,
    })
}

/// Finds an index an equality in the predicate can be answered through.
///
/// Equality on the leading column only. A range over an index is a natural next
/// step and is left out rather than half done: getting the edges of a composite
/// key right is exactly the kind of detail that is wrong in silence.
fn index_lookup<'a>(
    predicate: &PlanExpr,
    table: &'a TableSchema,
) -> Option<(&'a IndexSchema, Value, Option<PlanExpr>)> {
    let mut conjuncts = Vec::new();
    flatten_and(predicate, &mut conjuncts);

    let mut chosen = None;
    let mut residual = Vec::new();

    for conjunct in conjuncts {
        if chosen.is_none() {
            if let Some((column, value)) = equality_of(&conjunct) {
                if let Some(index) = table.index_leading_on(column) {
                    chosen = Some((index, value));
                    continue;
                }
            }
        }
        residual.push(conjunct);
    }

    let (index, value) = chosen?;
    let leftover = residual.into_iter().reduce(|left, right| PlanExpr::Binary {
        left: Box::new(left),
        op: BinaryOp::And,
        right: Box::new(right),
    });
    Some((index, value, leftover))
}

/// The column and constant an equality compares, written either way round.
fn equality_of(expr: &PlanExpr) -> Option<(usize, Value)> {
    let PlanExpr::Binary {
        left,
        op: BinaryOp::Eq,
        right,
    } = expr
    else {
        return None;
    };
    match (left.as_ref(), right.as_ref()) {
        (PlanExpr::Column(index), PlanExpr::Const(value))
        | (PlanExpr::Const(value), PlanExpr::Column(index)) => {
            // A null equals nothing, so it can never be an index lookup.
            if matches!(value, Value::Null) {
                None
            } else {
                Some((*index, value.clone()))
            }
        }
        _ => None,
    }
}

/// Splits a join condition into the equality a hash join can use and the rest.
///
/// The equality qualifies only when one side reads columns from the left input
/// alone and the other from the right alone. Anything mixed has to be evaluated
/// on the joined row, which is what a nested loop does.
fn hash_keys(on: &PlanExpr, boundary: usize) -> Option<(PlanExpr, PlanExpr, Option<PlanExpr>)> {
    let mut conjuncts = Vec::new();
    flatten_and(on, &mut conjuncts);

    let mut keys = None;
    let mut residual = Vec::new();

    for conjunct in conjuncts {
        if keys.is_none() {
            if let PlanExpr::Binary {
                left,
                op: BinaryOp::Eq,
                right,
            } = &conjunct
            {
                if let Some(pair) = sided(left, right, boundary) {
                    keys = Some(pair);
                    continue;
                }
            }
        }
        residual.push(conjunct);
    }

    let (left_key, right_key) = keys?;
    let leftover = residual.into_iter().reduce(|left, right| PlanExpr::Binary {
        left: Box::new(left),
        op: BinaryOp::And,
        right: Box::new(right),
    });
    Some((left_key, right_key, leftover))
}

/// Orders the two halves of an equality into build side and probe side, or
/// gives up when either half straddles the boundary.
fn sided(left: &PlanExpr, right: &PlanExpr, boundary: usize) -> Option<(PlanExpr, PlanExpr)> {
    let side = |expr: &PlanExpr| {
        let mut columns = Vec::new();
        columns_of(expr, &mut columns);
        if columns.is_empty() {
            return None;
        }
        let below = columns.iter().all(|index| *index < boundary);
        let above = columns.iter().all(|index| *index >= boundary);
        match (below, above) {
            (true, false) => Some(true),
            (false, true) => Some(false),
            _ => None,
        }
    };
    match (side(left)?, side(right)?) {
        (true, false) => Some((left.clone(), right.clone())),
        (false, true) => Some((right.clone(), left.clone())),
        _ => None,
    }
}

/// Binds a `WHERE` clause and pulls row id bounds out of it.
///
/// Rule 1 again, shared by everything that walks a table: a predicate that pins
/// the row id down turns the walk into a descent, and what it cannot express
/// stays behind as a filter.
fn narrow(
    scope: &Scope,
    table: &TableSchema,
    filter: Option<&ast::Expr>,
) -> Result<(Option<Bound>, Option<Bound>, Option<PlanExpr>)> {
    let bound = filter.map(|expr| bind_expr(scope, expr)).transpose()?;
    Ok(match (&bound, table.rowid_column()) {
        (Some(predicate), Some(rowid)) => split_rowid_bounds(predicate, rowid),
        _ => (None, None, bound),
    })
}

fn plan_insert(pool: &mut BufferPool, catalog: &Catalog, insert: &ast::Insert) -> Result<Plan> {
    let table = catalog.require(pool, &insert.table)?;

    let targets: Vec<usize> = if insert.columns.is_empty() {
        (0..table.columns.len()).collect()
    } else {
        insert
            .columns
            .iter()
            .map(|name| {
                table
                    .column_index(name)
                    .ok_or_else(|| Error::UnknownColumn(name.clone()))
            })
            .collect::<Result<_>>()?
    };

    // Values are constants and column references make no sense here, so the
    // scope is empty and any name in a VALUES list is an error.
    let scope = Scope::empty();
    let mut rows = Vec::with_capacity(insert.rows.len());
    for row in &insert.rows {
        if row.len() != targets.len() {
            return Err(Error::Unsupported(format!(
                "the table takes {} values but {} were given",
                targets.len(),
                row.len()
            )));
        }
        rows.push(
            row.iter()
                .map(|expr| bind_expr(&scope, expr))
                .collect::<Result<Vec<_>>>()?,
        );
    }

    Ok(Plan::Insert {
        table,
        rows,
        targets,
    })
}

fn plan_create_index(
    pool: &mut BufferPool,
    catalog: &Catalog,
    create: &ast::CreateIndex,
) -> Result<Plan> {
    let table = catalog.require(pool, &create.table)?;
    if table
        .indexes
        .iter()
        .any(|index| index.name.eq_ignore_ascii_case(&create.name))
    {
        return Err(Error::TableExists(create.name.clone()));
    }

    let columns = create
        .columns
        .iter()
        .map(|name| {
            table
                .column_index(name)
                .ok_or_else(|| Error::UnknownColumn(name.clone()))
        })
        .collect::<Result<Vec<_>>>()?;

    Ok(Plan::CreateIndex {
        table,
        index: IndexSchema {
            name: create.name.clone(),
            root: crate::NO_PAGE,
            unique: create.unique,
            columns,
        },
    })
}

fn plan_create_table(create: &ast::CreateTable) -> Result<Plan> {
    let mut columns = Vec::with_capacity(create.columns.len());
    for column in &create.columns {
        let mut schema = ColumnSchema {
            name: column.name.clone(),
            data_type: column.data_type,
            not_null: false,
            primary_key: false,
            unique: false,
            default: None,
        };
        for constraint in &column.constraints {
            match constraint {
                ast::ColumnConstraint::PrimaryKey => {
                    schema.primary_key = true;
                    schema.not_null = true;
                }
                ast::ColumnConstraint::NotNull => schema.not_null = true,
                ast::ColumnConstraint::Unique => schema.unique = true,
                ast::ColumnConstraint::Default(literal) => {
                    schema.default = Some(literal_value(literal))
                }
            }
        }
        columns.push(schema);
    }

    if columns.is_empty() {
        return Err(Error::Unsupported(
            "a table needs at least one column".into(),
        ));
    }
    if columns.iter().filter(|column| column.primary_key).count() > 1 {
        return Err(Error::Unsupported(
            "a table may have at most one primary key".into(),
        ));
    }

    Ok(Plan::CreateTable {
        schema: TableSchema {
            name: create.name.clone(),
            root: crate::NO_PAGE,
            columns,
            indexes: Vec::new(),
        },
        if_not_exists: create.if_not_exists,
    })
}

// -- binding ---------------------------------------------------------------

/// One table visible to a statement, and where its columns sit in the row.
struct ScopeTable {
    /// The name it goes by: its alias if it has one, its own name otherwise.
    qualifier: String,
    /// Column names, lowercased, in declaration order.
    columns: Vec<String>,
    /// Where its first column sits in the joined row.
    offset: usize,
}

/// What names mean in one statement.
///
/// A joined row is the inputs laid end to end, so a column is one index into
/// that whole row. Resolving a name here is the last time a string is compared:
/// everything below works on positions.
struct Scope {
    tables: Vec<ScopeTable>,
}

impl Scope {
    fn empty() -> Scope {
        Scope { tables: Vec::new() }
    }

    fn single(table: &TableSchema, alias: Option<&str>) -> Scope {
        let mut scope = Scope::empty();
        scope.push(table, alias);
        scope
    }

    fn push(&mut self, table: &TableSchema, alias: Option<&str>) {
        let offset = self.width();
        self.tables.push(ScopeTable {
            qualifier: alias.unwrap_or(&table.name).to_ascii_lowercase(),
            columns: table
                .columns
                .iter()
                .map(|column| column.name.to_ascii_lowercase())
                .collect(),
            offset,
        });
    }

    /// How many columns a row in this scope has.
    fn width(&self) -> usize {
        self.tables
            .last()
            .map_or(0, |table| table.offset + table.columns.len())
    }

    /// Every column, in row order.
    fn column_names(&self) -> Vec<String> {
        self.tables
            .iter()
            .flat_map(|table| table.columns.iter().cloned())
            .collect()
    }

    fn resolve(&self, table: Option<&str>, name: &str) -> Result<usize> {
        if let Some(qualifier) = table {
            let found = self
                .tables
                .iter()
                .find(|entry| entry.qualifier.eq_ignore_ascii_case(qualifier))
                .ok_or_else(|| Error::UnknownTable(qualifier.to_string()))?;
            return found
                .columns
                .iter()
                .position(|column| column.eq_ignore_ascii_case(name))
                .map(|index| found.offset + index)
                .ok_or_else(|| Error::UnknownColumn(name.to_string()));
        }

        // Unqualified, so every table is a candidate. Two matches is an error
        // rather than a coin toss: the statement is genuinely ambiguous and
        // guessing would be worse than refusing.
        let mut found = None;
        for entry in &self.tables {
            if let Some(index) = entry
                .columns
                .iter()
                .position(|column| column.eq_ignore_ascii_case(name))
            {
                if found.is_some() {
                    return Err(Error::AmbiguousColumn(name.to_string()));
                }
                found = Some(entry.offset + index);
            }
        }
        found.ok_or_else(|| Error::UnknownColumn(name.to_string()))
    }
}

/// Every column an expression reads.
fn columns_of(expr: &PlanExpr, out: &mut Vec<usize>) {
    match expr {
        PlanExpr::Const(_) => {}
        PlanExpr::Column(index) => out.push(*index),
        PlanExpr::Unary { operand, .. } => columns_of(operand, out),
        PlanExpr::Binary { left, right, .. } => {
            columns_of(left, out);
            columns_of(right, out);
        }
        PlanExpr::IsNull { operand, .. } => columns_of(operand, out),
        PlanExpr::Like { left, pattern, .. } => {
            columns_of(left, out);
            columns_of(pattern, out);
        }
        PlanExpr::Between {
            operand, low, high, ..
        } => {
            columns_of(operand, out);
            columns_of(low, out);
            columns_of(high, out);
        }
    }
}

/// Shifts every column index down by `delta`.
///
/// The probe side of a hash join is evaluated against a row of that input
/// alone, not against the joined row, so its key has to be rewritten to match.
fn rebase(expr: &PlanExpr, delta: usize) -> PlanExpr {
    match expr {
        PlanExpr::Const(value) => PlanExpr::Const(value.clone()),
        PlanExpr::Column(index) => PlanExpr::Column(index.saturating_sub(delta)),
        PlanExpr::Unary { op, operand } => PlanExpr::Unary {
            op: *op,
            operand: Box::new(rebase(operand, delta)),
        },
        PlanExpr::Binary { left, op, right } => PlanExpr::Binary {
            left: Box::new(rebase(left, delta)),
            op: *op,
            right: Box::new(rebase(right, delta)),
        },
        PlanExpr::IsNull { operand, negated } => PlanExpr::IsNull {
            operand: Box::new(rebase(operand, delta)),
            negated: *negated,
        },
        PlanExpr::Like {
            left,
            pattern,
            negated,
        } => PlanExpr::Like {
            left: Box::new(rebase(left, delta)),
            pattern: Box::new(rebase(pattern, delta)),
            negated: *negated,
        },
        PlanExpr::Between {
            operand,
            low,
            high,
            negated,
        } => PlanExpr::Between {
            operand: Box::new(rebase(operand, delta)),
            low: Box::new(rebase(low, delta)),
            high: Box::new(rebase(high, delta)),
            negated: *negated,
        },
    }
}

fn bind_projection(
    scope: &Scope,
    projection: &ast::Projection,
) -> Result<(Vec<PlanExpr>, Vec<String>)> {
    match projection {
        ast::Projection::Star => Ok((
            (0..scope.width()).map(PlanExpr::Column).collect(),
            scope.column_names(),
        )),
        ast::Projection::Items(items) => {
            let mut exprs = Vec::with_capacity(items.len());
            let mut names = Vec::with_capacity(items.len());
            for item in items {
                exprs.push(bind_expr(scope, &item.expr)?);
                names.push(match (&item.alias, &item.expr) {
                    (Some(alias), _) => alias.clone(),
                    (None, ast::Expr::Column { name, .. }) => name.clone(),
                    (None, expr) => expr.to_string(),
                });
            }
            Ok((exprs, names))
        }
    }
}

fn bind_expr(scope: &Scope, expr: &ast::Expr) -> Result<PlanExpr> {
    Ok(match expr {
        ast::Expr::Literal(literal) => PlanExpr::Const(literal_value(literal)),
        ast::Expr::Column { table, name } => {
            PlanExpr::Column(scope.resolve(table.as_deref(), name)?)
        }
        ast::Expr::Unary { op, operand } => PlanExpr::Unary {
            op: *op,
            operand: Box::new(bind_expr(scope, operand)?),
        },
        ast::Expr::Binary { left, op, right } => PlanExpr::Binary {
            left: Box::new(bind_expr(scope, left)?),
            op: *op,
            right: Box::new(bind_expr(scope, right)?),
        },
        ast::Expr::IsNull { operand, negated } => PlanExpr::IsNull {
            operand: Box::new(bind_expr(scope, operand)?),
            negated: *negated,
        },
        ast::Expr::Like {
            left,
            pattern,
            negated,
        } => PlanExpr::Like {
            left: Box::new(bind_expr(scope, left)?),
            pattern: Box::new(bind_expr(scope, pattern)?),
            negated: *negated,
        },
        ast::Expr::Between {
            operand,
            low,
            high,
            negated,
        } => PlanExpr::Between {
            operand: Box::new(bind_expr(scope, operand)?),
            low: Box::new(bind_expr(scope, low)?),
            high: Box::new(bind_expr(scope, high)?),
            negated: *negated,
        },
    })
}

fn literal_value(literal: &ast::Literal) -> Value {
    match literal {
        ast::Literal::Null => Value::Null,
        ast::Literal::Bool(flag) => Value::Bool(*flag),
        ast::Literal::Int(number) => Value::Int(*number),
        ast::Literal::Real(number) => Value::Real(*number),
        ast::Literal::Text(text) => Value::Text(text.clone()),
    }
}

// -- rule 1: turning a predicate into a row id range ------------------------

/// Pulls row id bounds out of a predicate, returning what is left over.
///
/// Only looks at conjuncts at the top level, because those are the ones that
/// must all hold. A bound found under an `OR` would not be a bound at all.
fn split_rowid_bounds(
    predicate: &PlanExpr,
    rowid: usize,
) -> (Option<Bound>, Option<Bound>, Option<PlanExpr>) {
    let mut conjuncts = Vec::new();
    flatten_and(predicate, &mut conjuncts);

    let mut lower: Option<Bound> = None;
    let mut upper: Option<Bound> = None;
    let mut residual = Vec::new();

    for conjunct in conjuncts {
        match rowid_bound_of(&conjunct, rowid) {
            Some((low, high)) => {
                lower = tighter(lower, low, true);
                upper = tighter(upper, high, false);
            }
            None => residual.push(conjunct),
        }
    }

    let leftover = residual.into_iter().reduce(|left, right| PlanExpr::Binary {
        left: Box::new(left),
        op: BinaryOp::And,
        right: Box::new(right),
    });
    (lower, upper, leftover)
}

fn flatten_and(expr: &PlanExpr, out: &mut Vec<PlanExpr>) {
    if let PlanExpr::Binary {
        left,
        op: BinaryOp::And,
        right,
    } = expr
    {
        flatten_and(left, out);
        flatten_and(right, out);
        return;
    }
    out.push(expr.clone());
}

/// The bounds one comparison puts on the row id, if it puts any.
fn rowid_bound_of(expr: &PlanExpr, rowid: usize) -> Option<(Option<Bound>, Option<Bound>)> {
    match expr {
        PlanExpr::Binary { left, op, right } => {
            // The column may be written on either side, and flipping the
            // comparison is how `42 > id` becomes `id < 42`.
            let (constant, op) = match (left.as_ref(), right.as_ref()) {
                (PlanExpr::Column(index), PlanExpr::Const(Value::Int(value)))
                    if *index == rowid =>
                {
                    (*value, *op)
                }
                (PlanExpr::Const(Value::Int(value)), PlanExpr::Column(index))
                    if *index == rowid =>
                {
                    (*value, flip(*op))
                }
                _ => return None,
            };

            let inclusive = |value| {
                Some(Bound {
                    value,
                    inclusive: true,
                })
            };
            let exclusive = |value| {
                Some(Bound {
                    value,
                    inclusive: false,
                })
            };
            Some(match op {
                BinaryOp::Eq => (inclusive(constant), inclusive(constant)),
                BinaryOp::Greater => (exclusive(constant), None),
                BinaryOp::GreaterEq => (inclusive(constant), None),
                BinaryOp::Less => (None, exclusive(constant)),
                BinaryOp::LessEq => (None, inclusive(constant)),
                _ => return None,
            })
        }
        PlanExpr::Between {
            operand,
            low,
            high,
            negated: false,
        } => match (operand.as_ref(), low.as_ref(), high.as_ref()) {
            (
                PlanExpr::Column(index),
                PlanExpr::Const(Value::Int(low)),
                PlanExpr::Const(Value::Int(high)),
            ) if *index == rowid => Some((
                Some(Bound {
                    value: *low,
                    inclusive: true,
                }),
                Some(Bound {
                    value: *high,
                    inclusive: true,
                }),
            )),
            _ => None,
        },
        _ => None,
    }
}

fn flip(op: BinaryOp) -> BinaryOp {
    match op {
        BinaryOp::Less => BinaryOp::Greater,
        BinaryOp::LessEq => BinaryOp::GreaterEq,
        BinaryOp::Greater => BinaryOp::Less,
        BinaryOp::GreaterEq => BinaryOp::LessEq,
        other => other,
    }
}

/// Keeps whichever of two bounds constrains more.
fn tighter(current: Option<Bound>, candidate: Option<Bound>, is_lower: bool) -> Option<Bound> {
    match (current, candidate) {
        (None, other) => other,
        (some, None) => some,
        (Some(a), Some(b)) => {
            let a_wins = if is_lower {
                (a.value, !a.inclusive) > (b.value, !b.inclusive)
            } else {
                (a.value, a.inclusive) < (b.value, b.inclusive)
            };
            Some(if a_wins { a } else { b })
        }
    }
}

// -- EXPLAIN ---------------------------------------------------------------

impl Plan {
    /// Renders the plan the way `EXPLAIN` shows it.
    ///
    /// Written from the first day of this layer, because a planner whose choice
    /// cannot be inspected is a planner nobody can debug, and the printed plan
    /// is the cheapest test there is for the rules above.
    pub fn explain(&self) -> String {
        let mut out = String::new();
        self.write_explain(&mut out, 0);
        out
    }

    fn write_explain(&self, out: &mut String, depth: usize) {
        let pad = "  ".repeat(depth);
        match self {
            Plan::SeqScan { table } => {
                let _ = writeln!(out, "{pad}SeqScan {}", table.name);
            }
            Plan::RowIdScan {
                table,
                lower,
                upper,
            } => {
                let _ = writeln!(
                    out,
                    "{pad}RowIdScan {} ({})",
                    table.name,
                    describe_range(*lower, *upper)
                );
            }
            Plan::Filter { input, predicate } => {
                let _ = writeln!(out, "{pad}Filter {}", describe(predicate));
                input.write_explain(out, depth + 1);
            }
            Plan::Project {
                input,
                exprs,
                names,
            } => {
                let shown: Vec<String> = exprs
                    .iter()
                    .zip(names)
                    .map(|(expr, name)| match expr {
                        PlanExpr::Column(_) => name.clone(),
                        other => format!("{} AS {name}", describe(other)),
                    })
                    .collect();
                let _ = writeln!(out, "{pad}Project {}", shown.join(", "));
                input.write_explain(out, depth + 1);
            }
            Plan::Sort { input, keys, top } => {
                let shown: Vec<String> = keys
                    .iter()
                    .map(|(expr, descending)| {
                        let direction = if *descending { " DESC" } else { " ASC" };
                        format!("{}{direction}", describe(expr))
                    })
                    .collect();
                match top {
                    Some(n) => {
                        let _ = writeln!(out, "{pad}Sort {} (top-{n})", shown.join(", "));
                    }
                    None => {
                        let _ = writeln!(out, "{pad}Sort {}", shown.join(", "));
                    }
                }
                input.write_explain(out, depth + 1);
            }
            Plan::Limit {
                input,
                limit,
                offset,
            } => {
                let mut line = format!("{pad}Limit");
                if let Some(limit) = limit {
                    let _ = write!(line, " n={limit}");
                }
                if *offset > 0 {
                    let _ = write!(line, " offset={offset}");
                }
                let _ = writeln!(out, "{line}");
                input.write_explain(out, depth + 1);
            }
            Plan::Insert { table, rows, .. } => {
                let _ = writeln!(out, "{pad}Insert {} ({} rows)", table.name, rows.len());
            }
            Plan::IndexScan { table, index, key } => {
                let _ = writeln!(
                    out,
                    "{pad}IndexScan {} using {} (= {})",
                    table.name,
                    index.name,
                    describe_value(key)
                );
            }
            Plan::CreateIndex { table, index } => {
                let _ = writeln!(out, "{pad}CreateIndex {} on {}", index.name, table.name);
            }
            Plan::NestedLoopJoin { left, right, on } => {
                let _ = writeln!(out, "{pad}NestedLoopJoin {}", describe(on));
                left.write_explain(out, depth + 1);
                right.write_explain(out, depth + 1);
            }
            Plan::HashJoin {
                left,
                right,
                left_key,
                right_key,
                residual,
            } => {
                let mut line = format!(
                    "{pad}HashJoin {} = {}",
                    describe(left_key),
                    describe(right_key)
                );
                if let Some(rest) = residual {
                    let _ = write!(line, " and {}", describe(rest));
                }
                let _ = writeln!(out, "{line}");
                let _ = writeln!(out, "{pad}  build:");
                left.write_explain(out, depth + 2);
                let _ = writeln!(out, "{pad}  probe:");
                right.write_explain(out, depth + 2);
            }
            Plan::Update {
                table,
                assignments,
                filter,
                lower,
                upper,
            } => {
                let columns: Vec<String> = assignments
                    .iter()
                    .map(|(index, value)| {
                        format!("{} = {}", table.column(*index).name, describe(value))
                    })
                    .collect();
                let _ = writeln!(out, "{pad}Update {} SET {}", table.name, columns.join(", "));
                write_walk(out, depth + 1, table, *lower, *upper, filter);
            }
            Plan::Delete {
                table,
                filter,
                lower,
                upper,
            } => {
                let _ = writeln!(out, "{pad}Delete {}", table.name);
                write_walk(out, depth + 1, table, *lower, *upper, filter);
            }
            Plan::CreateTable { schema, .. } => {
                let _ = writeln!(out, "{pad}CreateTable {}", schema.name);
            }
            Plan::Begin => {
                let _ = writeln!(out, "{pad}Begin");
            }
            Plan::Commit => {
                let _ = writeln!(out, "{pad}Commit");
            }
            Plan::Rollback => {
                let _ = writeln!(out, "{pad}Rollback");
            }
        }
    }
}

/// Renders the scan a write walks over, so `EXPLAIN` shows the same shape for
/// a change as it does for a query.
fn write_walk(
    out: &mut String,
    depth: usize,
    table: &TableSchema,
    lower: Option<Bound>,
    upper: Option<Bound>,
    filter: &Option<PlanExpr>,
) {
    let pad = "  ".repeat(depth);
    if let Some(predicate) = filter {
        let _ = writeln!(out, "{pad}Filter {}", describe(predicate));
    }
    let inner = if filter.is_some() { depth + 1 } else { depth };
    let pad = "  ".repeat(inner);
    if lower.is_some() || upper.is_some() {
        let _ = writeln!(
            out,
            "{pad}RowIdScan {} ({})",
            table.name,
            describe_range(lower, upper)
        );
    } else {
        let _ = writeln!(out, "{pad}SeqScan {}", table.name);
    }
}

fn describe_range(lower: Option<Bound>, upper: Option<Bound>) -> String {
    match (lower, upper) {
        (Some(low), Some(high)) if low == high && low.inclusive => format!("= {}", low.value),
        (low, high) => {
            let left = match low {
                Some(bound) if bound.inclusive => format!(">= {}", bound.value),
                Some(bound) => format!("> {}", bound.value),
                None => "unbounded".into(),
            };
            let right = match high {
                Some(bound) if bound.inclusive => format!("<= {}", bound.value),
                Some(bound) => format!("< {}", bound.value),
                None => "unbounded".into(),
            };
            format!("{left}, {right}")
        }
    }
}

fn describe(expr: &PlanExpr) -> String {
    match expr {
        PlanExpr::Const(value) => describe_value(value),
        PlanExpr::Column(index) => format!("#{index}"),
        PlanExpr::Unary { op, operand } => match op {
            UnaryOp::Neg => format!("(-{})", describe(operand)),
            UnaryOp::Not => format!("(NOT {})", describe(operand)),
        },
        PlanExpr::Binary { left, op, right } => {
            format!("({} {} {})", describe(left), op.as_str(), describe(right))
        }
        PlanExpr::IsNull { operand, negated } => {
            let not = if *negated { "NOT " } else { "" };
            format!("({} IS {not}NULL)", describe(operand))
        }
        PlanExpr::Like {
            left,
            pattern,
            negated,
        } => {
            let not = if *negated { "NOT " } else { "" };
            format!("({} {not}LIKE {})", describe(left), describe(pattern))
        }
        PlanExpr::Between {
            operand,
            low,
            high,
            negated,
        } => {
            let not = if *negated { "NOT " } else { "" };
            format!(
                "({} {not}BETWEEN {} AND {})",
                describe(operand),
                describe(low),
                describe(high)
            )
        }
    }
}

fn describe_value(value: &Value) -> String {
    match value {
        Value::Null => "NULL".into(),
        Value::Bool(true) => "TRUE".into(),
        Value::Bool(false) => "FALSE".into(),
        Value::Int(number) => number.to_string(),
        Value::Real(number) => format!("{number:?}"),
        Value::Text(text) => format!("'{text}'"),
        Value::Blob(bytes) => format!("<{} bytes>", bytes.len()),
    }
}
