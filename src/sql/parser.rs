//! A recursive descent parser: one function per rule of the grammar.
//!
//! Operator precedence falls out of the rule hierarchy — `or_expr` calls
//! `and_expr`, which calls `cmp_expr`, and so on down. No precedence table, no
//! parser generator, and nothing to keep in sync with the grammar in
//! `docs/en/06-sql.md` beyond the shape of these functions.

use super::ast::*;
use super::lexer::{tokenize, Keyword, Token, TokenKind};
use crate::{Error, Result};

/// Parses exactly one statement, which must be the whole input.
pub fn parse(sql: &str) -> Result<Statement> {
    let mut parser = Parser::new(sql)?;
    let statement = parser.statement()?;
    parser.eat(TokenKind::Semicolon);
    parser.expect_end()?;
    Ok(statement)
}

/// Parses a sequence of statements separated by semicolons.
pub fn parse_many(sql: &str) -> Result<Vec<Statement>> {
    let mut parser = Parser::new(sql)?;
    let mut out = Vec::new();
    loop {
        while parser.eat(TokenKind::Semicolon) {}
        if parser.at_end() {
            return Ok(out);
        }
        out.push(parser.statement()?);
    }
}

struct Parser {
    tokens: Vec<Token>,
    at: usize,
}

impl Parser {
    fn new(sql: &str) -> Result<Parser> {
        Ok(Parser {
            tokens: tokenize(sql)?,
            at: 0,
        })
    }

    // -- token handling ----------------------------------------------------

    fn peek(&self) -> &TokenKind {
        &self.tokens[self.at].kind
    }

    fn peek_at(&self, ahead: usize) -> &TokenKind {
        let index = (self.at + ahead).min(self.tokens.len() - 1);
        &self.tokens[index].kind
    }

    fn position(&self) -> usize {
        self.tokens[self.at].at
    }

    fn at_end(&self) -> bool {
        matches!(self.peek(), TokenKind::End)
    }

    fn advance(&mut self) -> TokenKind {
        let kind = self.tokens[self.at].kind.clone();
        if self.at + 1 < self.tokens.len() {
            self.at += 1;
        }
        kind
    }

    fn eat(&mut self, kind: TokenKind) -> bool {
        if *self.peek() == kind {
            self.advance();
            return true;
        }
        false
    }

    fn eat_word(&mut self, word: Keyword) -> bool {
        self.eat(TokenKind::Word(word))
    }

    fn expect(&mut self, kind: TokenKind) -> Result<()> {
        if self.eat(kind.clone()) {
            return Ok(());
        }
        Err(self.unexpected(&format!("{kind}")))
    }

    fn expect_word(&mut self, word: Keyword) -> Result<()> {
        self.expect(TokenKind::Word(word))
    }

    fn expect_name(&mut self) -> Result<String> {
        match self.peek().clone() {
            TokenKind::Ident(name) => {
                self.advance();
                Ok(name)
            }
            _ => Err(self.unexpected("a name")),
        }
    }

    fn expect_end(&mut self) -> Result<()> {
        if self.at_end() {
            return Ok(());
        }
        Err(self.unexpected("the end of the statement"))
    }

    fn unexpected(&self, wanted: &str) -> Error {
        Error::Sql {
            message: format!("expected {wanted}, found {}", self.peek()),
            at: self.position(),
        }
    }

    // -- statements --------------------------------------------------------

    fn statement(&mut self) -> Result<Statement> {
        match self.peek().clone() {
            TokenKind::Word(Keyword::Select) => Ok(Statement::Select(self.select()?)),
            TokenKind::Word(Keyword::Insert) => Ok(Statement::Insert(self.insert()?)),
            TokenKind::Word(Keyword::Update) => Ok(Statement::Update(self.update()?)),
            TokenKind::Word(Keyword::Delete) => Ok(Statement::Delete(self.delete()?)),
            TokenKind::Word(Keyword::Create) => self.create(),
            TokenKind::Word(Keyword::Begin) => {
                self.advance();
                Ok(Statement::Begin)
            }
            TokenKind::Word(Keyword::Commit) => {
                self.advance();
                Ok(Statement::Commit)
            }
            TokenKind::Word(Keyword::Rollback) => {
                self.advance();
                Ok(Statement::Rollback)
            }
            TokenKind::Word(Keyword::Vacuum) => {
                self.advance();
                match self.peek().clone() {
                    TokenKind::Ident(name) => {
                        self.advance();
                        Ok(Statement::Vacuum(Some(name)))
                    }
                    _ => Ok(Statement::Vacuum(None)),
                }
            }
            TokenKind::Word(Keyword::Explain) => {
                self.advance();
                Ok(Statement::Explain(Box::new(self.statement()?)))
            }
            _ => Err(self.unexpected("a statement")),
        }
    }

    fn select(&mut self) -> Result<Select> {
        self.expect_word(Keyword::Select)?;

        let projection = if self.eat(TokenKind::Star) {
            Projection::Star
        } else {
            let mut items = Vec::new();
            loop {
                let expr = self.expr()?;
                let alias = self.optional_alias()?;
                items.push(ProjItem { expr, alias });
                if !self.eat(TokenKind::Comma) {
                    break;
                }
            }
            Projection::Items(items)
        };

        self.expect_word(Keyword::From)?;
        let from = self.table_ref()?;

        let mut joins = Vec::new();
        loop {
            // INNER is noise: every join here is an inner join.
            let inner = self.eat_word(Keyword::Inner);
            if !self.eat_word(Keyword::Join) {
                if inner {
                    return Err(self.unexpected("JOIN"));
                }
                break;
            }
            let table = self.table_ref()?;
            self.expect_word(Keyword::On)?;
            let on = self.expr()?;
            joins.push(Join { table, on });
        }

        let filter = if self.eat_word(Keyword::Where) {
            Some(self.expr()?)
        } else {
            None
        };

        let mut order_by = Vec::new();
        if self.eat_word(Keyword::Order) {
            self.expect_word(Keyword::By)?;
            loop {
                let expr = self.expr()?;
                let descending = if self.eat_word(Keyword::Desc) {
                    true
                } else {
                    self.eat_word(Keyword::Asc);
                    false
                };
                order_by.push(OrderItem { expr, descending });
                if !self.eat(TokenKind::Comma) {
                    break;
                }
            }
        }

        let limit = if self.eat_word(Keyword::Limit) {
            Some(self.expect_count()?)
        } else {
            None
        };
        let offset = if self.eat_word(Keyword::Offset) {
            Some(self.expect_count()?)
        } else {
            None
        };

        Ok(Select {
            projection,
            from,
            joins,
            filter,
            order_by,
            limit,
            offset,
        })
    }

    fn expect_count(&mut self) -> Result<u64> {
        let at = self.position();
        match self.peek().clone() {
            TokenKind::Int(value) if value >= 0 => {
                self.advance();
                Ok(value as u64)
            }
            TokenKind::Int(_) => Err(Error::Sql {
                message: "a row count cannot be negative".into(),
                at,
            }),
            _ => Err(self.unexpected("a row count")),
        }
    }

    fn table_ref(&mut self) -> Result<TableRef> {
        let name = self.expect_name()?;
        let alias = self.optional_alias()?;
        Ok(TableRef { name, alias })
    }

    /// Reads `AS name`, or a bare name where one may follow.
    fn optional_alias(&mut self) -> Result<Option<String>> {
        if self.eat_word(Keyword::As) {
            return Ok(Some(self.expect_name()?));
        }
        if let TokenKind::Ident(name) = self.peek().clone() {
            self.advance();
            return Ok(Some(name));
        }
        Ok(None)
    }

    fn insert(&mut self) -> Result<Insert> {
        self.expect_word(Keyword::Insert)?;
        self.expect_word(Keyword::Into)?;
        let table = self.expect_name()?;

        let mut columns = Vec::new();
        // A parenthesis here begins the column list; VALUES begins the rows.
        if *self.peek() == TokenKind::LeftParen {
            self.advance();
            loop {
                columns.push(self.expect_name()?);
                if !self.eat(TokenKind::Comma) {
                    break;
                }
            }
            self.expect(TokenKind::RightParen)?;
        }

        self.expect_word(Keyword::Values)?;
        let mut rows = Vec::new();
        loop {
            self.expect(TokenKind::LeftParen)?;
            let mut row = Vec::new();
            loop {
                row.push(self.expr()?);
                if !self.eat(TokenKind::Comma) {
                    break;
                }
            }
            self.expect(TokenKind::RightParen)?;
            rows.push(row);
            if !self.eat(TokenKind::Comma) {
                break;
            }
        }

        Ok(Insert {
            table,
            columns,
            rows,
        })
    }

    fn update(&mut self) -> Result<Update> {
        self.expect_word(Keyword::Update)?;
        let table = self.expect_name()?;
        self.expect_word(Keyword::Set)?;

        let mut assignments = Vec::new();
        loop {
            let column = self.expect_name()?;
            self.expect(TokenKind::Eq)?;
            assignments.push((column, self.expr()?));
            if !self.eat(TokenKind::Comma) {
                break;
            }
        }

        let filter = if self.eat_word(Keyword::Where) {
            Some(self.expr()?)
        } else {
            None
        };
        Ok(Update {
            table,
            assignments,
            filter,
        })
    }

    fn delete(&mut self) -> Result<Delete> {
        self.expect_word(Keyword::Delete)?;
        self.expect_word(Keyword::From)?;
        let table = self.expect_name()?;
        let filter = if self.eat_word(Keyword::Where) {
            Some(self.expr()?)
        } else {
            None
        };
        Ok(Delete { table, filter })
    }

    fn create(&mut self) -> Result<Statement> {
        self.expect_word(Keyword::Create)?;

        let unique = self.eat_word(Keyword::Unique);
        if unique || *self.peek() == TokenKind::Word(Keyword::Index) {
            self.expect_word(Keyword::Index)?;
            let name = self.expect_name()?;
            self.expect_word(Keyword::On)?;
            let table = self.expect_name()?;
            self.expect(TokenKind::LeftParen)?;
            let mut columns = Vec::new();
            loop {
                columns.push(self.expect_name()?);
                if !self.eat(TokenKind::Comma) {
                    break;
                }
            }
            self.expect(TokenKind::RightParen)?;
            return Ok(Statement::CreateIndex(CreateIndex {
                name,
                table,
                columns,
                unique,
            }));
        }

        self.expect_word(Keyword::Table)?;
        let if_not_exists = if self.eat_word(Keyword::If) {
            self.expect_word(Keyword::Not)?;
            self.expect_word(Keyword::Exists)?;
            true
        } else {
            false
        };
        let name = self.expect_name()?;

        self.expect(TokenKind::LeftParen)?;
        let mut columns = Vec::new();
        loop {
            columns.push(self.column_def()?);
            if !self.eat(TokenKind::Comma) {
                break;
            }
        }
        self.expect(TokenKind::RightParen)?;

        Ok(Statement::CreateTable(CreateTable {
            name,
            if_not_exists,
            columns,
        }))
    }

    fn column_def(&mut self) -> Result<ColumnDef> {
        let name = self.expect_name()?;
        let data_type = match self.peek().clone() {
            TokenKind::Word(Keyword::Integer) => DataType::Integer,
            TokenKind::Word(Keyword::Real) => DataType::Real,
            TokenKind::Word(Keyword::Text) => DataType::Text,
            TokenKind::Word(Keyword::Blob) => DataType::Blob,
            TokenKind::Word(Keyword::Boolean) => DataType::Boolean,
            _ => return Err(self.unexpected("a column type")),
        };
        self.advance();

        let mut constraints = Vec::new();
        loop {
            if self.eat_word(Keyword::Primary) {
                self.expect_word(Keyword::Key)?;
                constraints.push(ColumnConstraint::PrimaryKey);
            } else if self.eat_word(Keyword::Not) {
                self.expect_word(Keyword::Null)?;
                constraints.push(ColumnConstraint::NotNull);
            } else if self.eat_word(Keyword::Unique) {
                constraints.push(ColumnConstraint::Unique);
            } else if self.eat_word(Keyword::Default) {
                constraints.push(ColumnConstraint::Default(self.literal()?));
            } else {
                break;
            }
        }
        Ok(ColumnDef {
            name,
            data_type,
            constraints,
        })
    }

    fn literal(&mut self) -> Result<Literal> {
        let value = match self.peek().clone() {
            TokenKind::Word(Keyword::Null) => Literal::Null,
            TokenKind::Word(Keyword::True) => Literal::Bool(true),
            TokenKind::Word(Keyword::False) => Literal::Bool(false),
            TokenKind::Int(value) => Literal::Int(value),
            TokenKind::Real(value) => Literal::Real(value),
            TokenKind::Text(value) => Literal::Text(value),
            _ => return Err(self.unexpected("a value")),
        };
        self.advance();
        Ok(value)
    }

    // -- expressions -------------------------------------------------------

    fn expr(&mut self) -> Result<Expr> {
        self.or_expr()
    }

    fn or_expr(&mut self) -> Result<Expr> {
        let mut left = self.and_expr()?;
        while self.eat_word(Keyword::Or) {
            let right = self.and_expr()?;
            left = binary(left, BinaryOp::Or, right);
        }
        Ok(left)
    }

    fn and_expr(&mut self) -> Result<Expr> {
        let mut left = self.not_expr()?;
        while self.eat_word(Keyword::And) {
            let right = self.not_expr()?;
            left = binary(left, BinaryOp::And, right);
        }
        Ok(left)
    }

    fn not_expr(&mut self) -> Result<Expr> {
        if self.eat_word(Keyword::Not) {
            let operand = self.not_expr()?;
            return Ok(Expr::Unary {
                op: UnaryOp::Not,
                operand: Box::new(operand),
            });
        }
        self.cmp_expr()
    }

    fn cmp_expr(&mut self) -> Result<Expr> {
        let left = self.add_expr()?;

        if let Some(op) = comparison_of(self.peek()) {
            self.advance();
            let right = self.add_expr()?;
            return Ok(binary(left, op, right));
        }

        if self.eat_word(Keyword::Is) {
            let negated = self.eat_word(Keyword::Not);
            self.expect_word(Keyword::Null)?;
            return Ok(Expr::IsNull {
                operand: Box::new(left),
                negated,
            });
        }

        // A bare NOT here only makes sense before LIKE or BETWEEN. Looking one
        // token ahead keeps `a NOT LIKE 'x'` from being mistaken for the start
        // of a new negated expression.
        let negated = matches!(self.peek(), TokenKind::Word(Keyword::Not))
            && matches!(
                self.peek_at(1),
                TokenKind::Word(Keyword::Like) | TokenKind::Word(Keyword::Between)
            );
        if negated {
            self.advance();
        }

        if self.eat_word(Keyword::Like) {
            let pattern = self.add_expr()?;
            return Ok(Expr::Like {
                left: Box::new(left),
                pattern: Box::new(pattern),
                negated,
            });
        }
        if self.eat_word(Keyword::Between) {
            let low = self.add_expr()?;
            self.expect_word(Keyword::And)?;
            let high = self.add_expr()?;
            return Ok(Expr::Between {
                operand: Box::new(left),
                low: Box::new(low),
                high: Box::new(high),
                negated,
            });
        }

        Ok(left)
    }

    fn add_expr(&mut self) -> Result<Expr> {
        let mut left = self.mul_expr()?;
        loop {
            let op = match self.peek() {
                TokenKind::Plus => BinaryOp::Add,
                TokenKind::Minus => BinaryOp::Sub,
                _ => return Ok(left),
            };
            self.advance();
            let right = self.mul_expr()?;
            left = binary(left, op, right);
        }
    }

    fn mul_expr(&mut self) -> Result<Expr> {
        let mut left = self.primary()?;
        loop {
            let op = match self.peek() {
                TokenKind::Star => BinaryOp::Mul,
                TokenKind::Slash => BinaryOp::Div,
                TokenKind::Percent => BinaryOp::Mod,
                _ => return Ok(left),
            };
            self.advance();
            let right = self.primary()?;
            left = binary(left, op, right);
        }
    }

    fn primary(&mut self) -> Result<Expr> {
        match self.peek().clone() {
            TokenKind::Minus => {
                self.advance();
                let operand = self.primary()?;
                Ok(Expr::Unary {
                    op: UnaryOp::Neg,
                    operand: Box::new(operand),
                })
            }
            TokenKind::LeftParen => {
                self.advance();
                let inner = self.expr()?;
                self.expect(TokenKind::RightParen)?;
                Ok(inner)
            }
            TokenKind::Ident(name) => {
                self.advance();
                if self.eat(TokenKind::Dot) {
                    let column = self.expect_name()?;
                    Ok(Expr::Column {
                        table: Some(name),
                        name: column,
                    })
                } else {
                    Ok(Expr::Column { table: None, name })
                }
            }
            TokenKind::Word(Keyword::Null)
            | TokenKind::Word(Keyword::True)
            | TokenKind::Word(Keyword::False)
            | TokenKind::Int(_)
            | TokenKind::Real(_)
            | TokenKind::Text(_) => Ok(Expr::Literal(self.literal()?)),
            _ => Err(self.unexpected("a value, a column or '('")),
        }
    }
}

fn binary(left: Expr, op: BinaryOp, right: Expr) -> Expr {
    Expr::Binary {
        left: Box::new(left),
        op,
        right: Box::new(right),
    }
}

fn comparison_of(kind: &TokenKind) -> Option<BinaryOp> {
    Some(match kind {
        TokenKind::Eq => BinaryOp::Eq,
        TokenKind::NotEq => BinaryOp::NotEq,
        TokenKind::Less => BinaryOp::Less,
        TokenKind::LessEq => BinaryOp::LessEq,
        TokenKind::Greater => BinaryOp::Greater,
        TokenKind::GreaterEq => BinaryOp::GreaterEq,
        _ => return None,
    })
}
