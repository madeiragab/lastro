//! The database as a caller sees it: text in, rows out.
//!
//! Ties the parser, the catalog, the planner and the executor to a pool with a
//! log attached, and runs recovery on open the way any database must.

use std::path::Path;

use crate::index::BTree;
use crate::sql::catalog::{Catalog, IndexSchema};
use crate::sql::exec::{self, Row};
use crate::sql::plan::{plan, Plan};
use crate::sql::{ast, parse_many};
use crate::storage::{BufferPool, Pager};
use crate::wal::{recover, RecoveryReport, Wal};
use crate::{Error, Result};

/// What running one statement produced.
#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    /// A query's result.
    Rows {
        /// The output column names.
        columns: Vec<String>,
        /// The rows, in order.
        rows: Vec<Row>,
    },
    /// How many rows a write touched.
    Affected(usize),
    /// An `EXPLAIN` result.
    Plan(String),
    /// A statement that produced nothing to report.
    Ack,
}

/// An open database.
#[derive(Debug)]
pub struct Database {
    pool: BufferPool,
    catalog: Catalog,
    /// Whether a `BEGIN` is outstanding. Without one, each write gets its own
    /// transaction.
    explicit: bool,
    /// How many rows a sort holds before it writes a run out.
    sort_budget: usize,
    report: RecoveryReport,
}

impl Database {
    /// Opens or creates a database, running recovery before anything else.
    pub fn open(path: impl AsRef<Path>) -> Result<Database> {
        Database::with_capacity(path, 256)
    }

    /// Opens with a buffer pool of a given size in pages.
    pub fn with_capacity(path: impl AsRef<Path>, capacity: usize) -> Result<Database> {
        let path = path.as_ref();
        let pager = Pager::open_or_create(path)?;
        // The log's numbering continues across checkpoints, and where it
        // continues from lives in the metadata page.
        let base = pager.meta().last_checkpoint_lsn;
        let mut pool = BufferPool::new(pager, capacity);
        pool.attach_wal(Wal::open(Wal::path_for(path), base)?);
        let report = recover(&mut pool)?;

        // Creating the catalog writes a new root into the metadata page, so it
        // happens inside a transaction like any other change.
        pool.begin_transaction()?;
        let catalog = match Catalog::open(&mut pool) {
            Ok(catalog) => catalog,
            Err(error) => {
                let _ = pool.rollback_transaction();
                return Err(error);
            }
        };
        pool.commit_transaction()?;
        crate::wal::checkpoint(&mut pool)?;

        Ok(Database {
            pool,
            catalog,
            explicit: false,
            sort_budget: exec::DEFAULT_SORT_ROWS,
            report,
        })
    }

    /// Sets how many rows a sort may hold before spilling to disk.
    ///
    /// Exists so that a test can force the spill path with a handful of rows
    /// rather than by generating enough to reach the real budget.
    pub fn set_sort_budget(&mut self, rows: usize) {
        self.sort_budget = rows.max(1);
    }

    /// What recovery found when the database was opened.
    pub fn recovery_report(&self) -> RecoveryReport {
        self.report
    }

    /// The buffer pool, for tests and for the inspection tool.
    pub fn pool_mut(&mut self) -> &mut BufferPool {
        &mut self.pool
    }

    /// Runs every statement in `sql`, stopping at the first failure.
    pub fn execute(&mut self, sql: &str) -> Result<Vec<Outcome>> {
        let statements = parse_many(sql)?;
        let mut out = Vec::with_capacity(statements.len());
        for statement in &statements {
            out.push(self.run(statement)?);
        }
        Ok(out)
    }

    /// Runs exactly one statement.
    pub fn query(&mut self, sql: &str) -> Result<Outcome> {
        let mut outcomes = self.execute(sql)?;
        match outcomes.len() {
            1 => Ok(outcomes.pop().expect("checked")),
            found => Err(Error::Unsupported(format!(
                "expected one statement, found {found}"
            ))),
        }
    }

    /// Forces everything to disk and empties the log.
    pub fn checkpoint(&mut self) -> Result<()> {
        crate::wal::checkpoint(&mut self.pool)
    }

    fn run(&mut self, statement: &ast::Statement) -> Result<Outcome> {
        match statement {
            ast::Statement::Begin => {
                self.pool.begin_transaction()?;
                self.explicit = true;
                Ok(Outcome::Ack)
            }
            ast::Statement::Commit => {
                self.pool.commit_transaction()?;
                self.explicit = false;
                Ok(Outcome::Ack)
            }
            ast::Statement::Rollback => {
                self.pool.rollback_transaction()?;
                self.explicit = false;
                Ok(Outcome::Ack)
            }
            ast::Statement::Explain(inner) => {
                let plan = plan(&mut self.pool, &self.catalog, inner)?;
                Ok(Outcome::Plan(plan.explain()))
            }
            // A query writes nothing, so it needs no transaction and produces
            // no log records.
            ast::Statement::Select(_) => {
                let plan = plan(&mut self.pool, &self.catalog, statement)?;
                let columns = plan.output_names();
                let mut op = exec::build_with(&plan, &mut self.pool, self.sort_budget)?;
                let mut rows = Vec::new();
                while let Some(row) = op.next(&mut self.pool)? {
                    rows.push(row);
                }
                Ok(Outcome::Rows { columns, rows })
            }
            _ => {
                // Anything that writes. Without an outstanding BEGIN it gets a
                // transaction of its own, so a statement is never half applied.
                let auto = !self.explicit;
                if auto {
                    self.pool.begin_transaction()?;
                }
                let outcome = self.run_write(statement);
                if auto {
                    match &outcome {
                        Ok(_) => self.pool.commit_transaction()?,
                        Err(_) => {
                            let _ = self.pool.rollback_transaction();
                        }
                    }
                }
                outcome
            }
        }
    }

    fn run_write(&mut self, statement: &ast::Statement) -> Result<Outcome> {
        let plan = plan(&mut self.pool, &self.catalog, statement)?;
        match plan {
            Plan::CreateTable {
                mut schema,
                if_not_exists,
            } => {
                if self.catalog.table(&mut self.pool, &schema.name)?.is_some() {
                    if if_not_exists {
                        return Ok(Outcome::Ack);
                    }
                    return Err(Error::TableExists(schema.name));
                }
                let tree = BTree::create(&mut self.pool)?;
                schema.root = tree.root();

                // A column declared UNIQUE is a request for an index, so one is
                // built rather than the constraint being recorded and forgotten.
                let unique: Vec<usize> = schema
                    .columns
                    .iter()
                    .enumerate()
                    .filter(|(_, column)| column.unique && !column.primary_key)
                    .map(|(index, _)| index)
                    .collect();
                for position in unique {
                    let index_tree = BTree::create(&mut self.pool)?;
                    schema.indexes.push(IndexSchema {
                        name: format!("{}_{}_unique", schema.name, schema.columns[position].name),
                        root: index_tree.root(),
                        unique: true,
                        columns: vec![position],
                    });
                }

                let mut catalog = self.catalog;
                catalog.put(&mut self.pool, &schema)?;
                self.catalog = catalog;
                Ok(Outcome::Ack)
            }
            Plan::CreateIndex { mut table, index } => {
                let tree = BTree::create(&mut self.pool)?;
                let mut index = index;
                index.root = tree.root();

                let built = exec::build_index(&mut self.pool, &table, &index)?;
                table.indexes.push(index);

                let mut catalog = self.catalog;
                catalog.put(&mut self.pool, &table)?;
                self.catalog = catalog;
                Ok(Outcome::Affected(built))
            }
            Plan::Insert {
                table,
                rows,
                targets,
            } => {
                let written = exec::insert(&mut self.pool, &table, &targets, &rows)?;
                Ok(Outcome::Affected(written))
            }
            Plan::Update {
                table,
                assignments,
                filter,
                lower,
                upper,
            } => {
                let changed = exec::update(
                    &mut self.pool,
                    &table,
                    &assignments,
                    filter.as_ref(),
                    lower,
                    upper,
                )?;
                Ok(Outcome::Affected(changed))
            }
            Plan::Delete {
                table,
                filter,
                lower,
                upper,
            } => {
                let removed = exec::delete(&mut self.pool, &table, filter.as_ref(), lower, upper)?;
                Ok(Outcome::Affected(removed))
            }
            other => Err(Error::Unsupported(format!(
                "{other:?} is not a statement that writes"
            ))),
        }
    }
}

impl Plan {
    /// The names of the columns the plan produces.
    pub fn output_names(&self) -> Vec<String> {
        match self {
            Plan::Project { names, .. } => names.clone(),
            Plan::Filter { input, .. } | Plan::Sort { input, .. } | Plan::Limit { input, .. } => {
                input.output_names()
            }
            _ => Vec::new(),
        }
    }
}
