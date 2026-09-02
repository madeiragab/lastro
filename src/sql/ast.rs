//! The abstract syntax tree, and how to write it back out as SQL.
//!
//! Every node knows how to render itself into SQL that parses to the same tree.
//! That is not decoration: the round trip is the cheapest property test there
//! is for a parser, and it catches whole classes of precedence and associativity
//! mistakes without anyone writing a case for them.

use std::fmt;

/// A complete statement.
#[derive(Debug, Clone, PartialEq)]
pub enum Statement {
    /// `SELECT`
    Select(Select),
    /// `INSERT`
    Insert(Insert),
    /// `UPDATE`
    Update(Update),
    /// `DELETE`
    Delete(Delete),
    /// `CREATE TABLE`
    CreateTable(CreateTable),
    /// `CREATE INDEX`
    CreateIndex(CreateIndex),
    /// `BEGIN`
    Begin,
    /// `COMMIT`
    Commit,
    /// `ROLLBACK`
    Rollback,
    /// `VACUUM`, over one table or over every one.
    Vacuum(Option<String>),
    /// `EXPLAIN`, wrapping the statement whose plan is wanted.
    Explain(Box<Statement>),
}

/// A query.
#[derive(Debug, Clone, PartialEq)]
pub struct Select {
    /// Whether duplicate output rows are collapsed into one.
    pub distinct: bool,
    /// What comes out.
    pub projection: Projection,
    /// The table it reads from.
    pub from: TableRef,
    /// Tables joined onto it.
    pub joins: Vec<Join>,
    /// The `WHERE` clause.
    pub filter: Option<Expr>,
    /// The `ORDER BY` clause.
    pub order_by: Vec<OrderItem>,
    /// The `LIMIT` clause.
    pub limit: Option<u64>,
    /// The `OFFSET` clause.
    pub offset: Option<u64>,
}

/// What a query returns.
#[derive(Debug, Clone, PartialEq)]
pub enum Projection {
    /// `*`, expanded by the binder once the schema is known.
    Star,
    /// An explicit list.
    Items(Vec<ProjItem>),
}

/// One output column.
#[derive(Debug, Clone, PartialEq)]
pub struct ProjItem {
    /// What to compute.
    pub expr: Expr,
    /// The name to give it, if one was written.
    pub alias: Option<String>,
}

/// A table, possibly renamed.
#[derive(Debug, Clone, PartialEq)]
pub struct TableRef {
    /// The table's name.
    pub name: String,
    /// The name it goes by in this statement.
    pub alias: Option<String>,
}

/// An inner join. Only inner joins exist here; see `docs/en/06-sql.md`.
#[derive(Debug, Clone, PartialEq)]
pub struct Join {
    /// The table being joined on.
    pub table: TableRef,
    /// The `ON` condition.
    pub on: Expr,
}

/// One `ORDER BY` term.
#[derive(Debug, Clone, PartialEq)]
pub struct OrderItem {
    /// What to sort by.
    pub expr: Expr,
    /// Whether `DESC` was written.
    pub descending: bool,
}

/// `INSERT INTO ... VALUES ...`
#[derive(Debug, Clone, PartialEq)]
pub struct Insert {
    /// The table to write into.
    pub table: String,
    /// The columns named, if any were.
    pub columns: Vec<String>,
    /// One vector per row.
    pub rows: Vec<Vec<Expr>>,
}

/// `UPDATE ... SET ...`
#[derive(Debug, Clone, PartialEq)]
pub struct Update {
    /// The table to change.
    pub table: String,
    /// Column and the value to put in it.
    pub assignments: Vec<(String, Expr)>,
    /// The `WHERE` clause. Absent means every row.
    pub filter: Option<Expr>,
}

/// `DELETE FROM ...`
#[derive(Debug, Clone, PartialEq)]
pub struct Delete {
    /// The table to delete from.
    pub table: String,
    /// The `WHERE` clause. Absent means every row.
    pub filter: Option<Expr>,
}

/// `CREATE TABLE`
#[derive(Debug, Clone, PartialEq)]
pub struct CreateTable {
    /// The table's name.
    pub name: String,
    /// Whether `IF NOT EXISTS` was written.
    pub if_not_exists: bool,
    /// Its columns, in order.
    pub columns: Vec<ColumnDef>,
}

/// One column in a `CREATE TABLE`.
#[derive(Debug, Clone, PartialEq)]
pub struct ColumnDef {
    /// The column's name.
    pub name: String,
    /// Its type.
    pub data_type: DataType,
    /// Anything written after the type.
    pub constraints: Vec<ColumnConstraint>,
}

/// The types a column may have.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DataType {
    /// A signed 64 bit integer.
    Integer,
    /// A 64 bit float.
    Real,
    /// UTF-8 text.
    Text,
    /// An opaque byte string.
    Blob,
    /// True or false.
    Boolean,
}

impl DataType {
    /// The type as written in a statement.
    pub fn as_str(&self) -> &'static str {
        match self {
            DataType::Integer => "INTEGER",
            DataType::Real => "REAL",
            DataType::Text => "TEXT",
            DataType::Blob => "BLOB",
            DataType::Boolean => "BOOLEAN",
        }
    }
}

/// What may follow a column's type.
#[derive(Debug, Clone, PartialEq)]
pub enum ColumnConstraint {
    /// `PRIMARY KEY`
    PrimaryKey,
    /// `NOT NULL`
    NotNull,
    /// `UNIQUE`
    Unique,
    /// `DEFAULT <literal>`
    Default(Literal),
}

/// `CREATE INDEX`
#[derive(Debug, Clone, PartialEq)]
pub struct CreateIndex {
    /// The index's name.
    pub name: String,
    /// The table it indexes.
    pub table: String,
    /// The columns it covers, in order.
    pub columns: Vec<String>,
    /// Whether `UNIQUE` was written.
    pub unique: bool,
}

/// A value written directly in a statement.
#[derive(Debug, Clone, PartialEq)]
pub enum Literal {
    /// `NULL`
    Null,
    /// `TRUE` or `FALSE`
    Bool(bool),
    /// A whole number.
    Int(i64),
    /// A number with a fractional part.
    Real(f64),
    /// A quoted string.
    Text(String),
}

/// A prefix operator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnaryOp {
    /// Arithmetic negation.
    Neg,
    /// Logical negation.
    Not,
}

/// An infix operator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinaryOp {
    /// `OR`
    Or,
    /// `AND`
    And,
    /// `=`
    Eq,
    /// `<>`
    NotEq,
    /// `<`
    Less,
    /// `<=`
    LessEq,
    /// `>`
    Greater,
    /// `>=`
    GreaterEq,
    /// `+`
    Add,
    /// `-`
    Sub,
    /// `*`
    Mul,
    /// `/`
    Div,
    /// `%`
    Mod,
}

impl BinaryOp {
    /// The operator as written in a statement.
    pub fn as_str(&self) -> &'static str {
        match self {
            BinaryOp::Or => "OR",
            BinaryOp::And => "AND",
            BinaryOp::Eq => "=",
            BinaryOp::NotEq => "<>",
            BinaryOp::Less => "<",
            BinaryOp::LessEq => "<=",
            BinaryOp::Greater => ">",
            BinaryOp::GreaterEq => ">=",
            BinaryOp::Add => "+",
            BinaryOp::Sub => "-",
            BinaryOp::Mul => "*",
            BinaryOp::Div => "/",
            BinaryOp::Mod => "%",
        }
    }
}

/// Anything that computes a value.
#[derive(Debug, Clone, PartialEq)]
pub enum Expr {
    /// A value written directly.
    Literal(Literal),
    /// A column, optionally qualified by a table or alias.
    Column {
        /// The table or alias it was qualified with.
        table: Option<String>,
        /// The column's name.
        name: String,
    },
    /// A prefix operator applied to something.
    Unary {
        /// Which operator.
        op: UnaryOp,
        /// What it applies to.
        operand: Box<Expr>,
    },
    /// Two operands joined by an infix operator.
    Binary {
        /// The left operand.
        left: Box<Expr>,
        /// Which operator.
        op: BinaryOp,
        /// The right operand.
        right: Box<Expr>,
    },
    /// `IS NULL` or `IS NOT NULL`.
    IsNull {
        /// What is being tested.
        operand: Box<Expr>,
        /// Whether `NOT` was written.
        negated: bool,
    },
    /// `LIKE` or `NOT LIKE`.
    Like {
        /// What is being matched.
        left: Box<Expr>,
        /// The pattern.
        pattern: Box<Expr>,
        /// Whether `NOT` was written.
        negated: bool,
    },
    /// `BETWEEN` or `NOT BETWEEN`.
    Between {
        /// What is being tested.
        operand: Box<Expr>,
        /// The lower bound, inclusive.
        low: Box<Expr>,
        /// The upper bound, inclusive.
        high: Box<Expr>,
        /// Whether `NOT` was written.
        negated: bool,
    },
}

// -- writing it back out ---------------------------------------------------

impl fmt::Display for Literal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Literal::Null => write!(f, "NULL"),
            Literal::Bool(true) => write!(f, "TRUE"),
            Literal::Bool(false) => write!(f, "FALSE"),
            Literal::Int(value) => write!(f, "{value}"),
            // The debug form of a float keeps the decimal point, so a whole
            // number written as a real parses back as one rather than as an
            // integer.
            Literal::Real(value) => write!(f, "{value:?}"),
            Literal::Text(value) => write!(f, "'{}'", value.replace('\'', "''")),
        }
    }
}

impl fmt::Display for Expr {
    /// Renders the expression fully parenthesized.
    ///
    /// Ugly to read and impossible to get wrong: no rendering can silently
    /// change the shape of the tree, which is the only property the round trip
    /// needs.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Expr::Literal(value) => write!(f, "{value}"),
            Expr::Column { table: None, name } => write!(f, "{name}"),
            Expr::Column {
                table: Some(table),
                name,
            } => write!(f, "{table}.{name}"),
            Expr::Unary { op, operand } => match op {
                UnaryOp::Neg => write!(f, "(-{operand})"),
                UnaryOp::Not => write!(f, "(NOT {operand})"),
            },
            Expr::Binary { left, op, right } => {
                write!(f, "({left} {} {right})", op.as_str())
            }
            Expr::IsNull { operand, negated } => {
                let not = if *negated { "NOT " } else { "" };
                write!(f, "({operand} IS {not}NULL)")
            }
            Expr::Like {
                left,
                pattern,
                negated,
            } => {
                let not = if *negated { "NOT " } else { "" };
                write!(f, "({left} {not}LIKE {pattern})")
            }
            Expr::Between {
                operand,
                low,
                high,
                negated,
            } => {
                let not = if *negated { "NOT " } else { "" };
                write!(f, "({operand} {not}BETWEEN {low} AND {high})")
            }
        }
    }
}

impl fmt::Display for TableRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.alias {
            Some(alias) => write!(f, "{} AS {alias}", self.name),
            None => write!(f, "{}", self.name),
        }
    }
}

fn join_with(items: &[String], separator: &str) -> String {
    items.join(separator)
}

impl fmt::Display for Statement {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Statement::Begin => write!(f, "BEGIN"),
            Statement::Commit => write!(f, "COMMIT"),
            Statement::Rollback => write!(f, "ROLLBACK"),
            Statement::Vacuum(None) => write!(f, "VACUUM"),
            Statement::Vacuum(Some(table)) => write!(f, "VACUUM {table}"),
            Statement::Explain(inner) => write!(f, "EXPLAIN {inner}"),
            Statement::Select(select) => write!(f, "{select}"),
            Statement::Insert(insert) => write!(f, "{insert}"),
            Statement::Update(update) => write!(f, "{update}"),
            Statement::Delete(delete) => write!(f, "{delete}"),
            Statement::CreateTable(create) => write!(f, "{create}"),
            Statement::CreateIndex(create) => write!(f, "{create}"),
        }
    }
}

impl fmt::Display for Select {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "SELECT ")?;
        if self.distinct {
            write!(f, "DISTINCT ")?;
        }
        match &self.projection {
            Projection::Star => write!(f, "*")?,
            Projection::Items(items) => {
                let rendered: Vec<String> = items
                    .iter()
                    .map(|item| match &item.alias {
                        Some(alias) => format!("{} AS {alias}", item.expr),
                        None => item.expr.to_string(),
                    })
                    .collect();
                write!(f, "{}", join_with(&rendered, ", "))?;
            }
        }

        write!(f, " FROM {}", self.from)?;
        for join in &self.joins {
            write!(f, " JOIN {} ON {}", join.table, join.on)?;
        }
        if let Some(filter) = &self.filter {
            write!(f, " WHERE {filter}")?;
        }
        if !self.order_by.is_empty() {
            let rendered: Vec<String> = self
                .order_by
                .iter()
                .map(|item| {
                    let direction = if item.descending { " DESC" } else { " ASC" };
                    format!("{}{direction}", item.expr)
                })
                .collect();
            write!(f, " ORDER BY {}", join_with(&rendered, ", "))?;
        }
        if let Some(limit) = self.limit {
            write!(f, " LIMIT {limit}")?;
        }
        if let Some(offset) = self.offset {
            write!(f, " OFFSET {offset}")?;
        }
        Ok(())
    }
}

impl fmt::Display for Insert {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "INSERT INTO {}", self.table)?;
        if !self.columns.is_empty() {
            write!(f, " ({})", join_with(&self.columns, ", "))?;
        }
        let rows: Vec<String> = self
            .rows
            .iter()
            .map(|row| {
                let values: Vec<String> = row.iter().map(Expr::to_string).collect();
                format!("({})", join_with(&values, ", "))
            })
            .collect();
        write!(f, " VALUES {}", join_with(&rows, ", "))
    }
}

impl fmt::Display for Update {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let sets: Vec<String> = self
            .assignments
            .iter()
            .map(|(column, value)| format!("{column} = {value}"))
            .collect();
        write!(f, "UPDATE {} SET {}", self.table, join_with(&sets, ", "))?;
        if let Some(filter) = &self.filter {
            write!(f, " WHERE {filter}")?;
        }
        Ok(())
    }
}

impl fmt::Display for Delete {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "DELETE FROM {}", self.table)?;
        if let Some(filter) = &self.filter {
            write!(f, " WHERE {filter}")?;
        }
        Ok(())
    }
}

impl fmt::Display for CreateTable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "CREATE TABLE ")?;
        if self.if_not_exists {
            write!(f, "IF NOT EXISTS ")?;
        }
        let columns: Vec<String> = self
            .columns
            .iter()
            .map(|column| {
                let mut rendered = format!("{} {}", column.name, column.data_type.as_str());
                for constraint in &column.constraints {
                    match constraint {
                        ColumnConstraint::PrimaryKey => rendered.push_str(" PRIMARY KEY"),
                        ColumnConstraint::NotNull => rendered.push_str(" NOT NULL"),
                        ColumnConstraint::Unique => rendered.push_str(" UNIQUE"),
                        ColumnConstraint::Default(value) => {
                            rendered.push_str(&format!(" DEFAULT {value}"))
                        }
                    }
                }
                rendered
            })
            .collect();
        write!(f, "{} ({})", self.name, join_with(&columns, ", "))
    }
}

impl fmt::Display for CreateIndex {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "CREATE ")?;
        if self.unique {
            write!(f, "UNIQUE ")?;
        }
        write!(
            f,
            "INDEX {} ON {} ({})",
            self.name,
            self.table,
            join_with(&self.columns, ", ")
        )
    }
}
