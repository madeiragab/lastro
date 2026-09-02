//! Turning SQL text into tokens.
//!
//! Every token carries the byte offset it started at, so an error can point at
//! the place that caused it rather than saying "syntax error" and leaving the
//! reader to find it.

use std::fmt;

use crate::{Error, Result};

/// A reserved word. Matched without regard to case, as SQL requires.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Keyword {
    /// `AND`
    And,
    /// `AS`
    As,
    /// `ASC`
    Asc,
    /// `BEGIN`
    Begin,
    /// `BETWEEN`
    Between,
    /// `BLOB`
    Blob,
    /// `BOOLEAN`
    Boolean,
    /// `BY`
    By,
    /// `COMMIT`
    Commit,
    /// `CREATE`
    Create,
    /// `DEFAULT`
    Default,
    /// `DELETE`
    Delete,
    /// `ALL`
    All,
    /// `DESC`
    Desc,
    /// `DISTINCT`
    Distinct,
    /// `EXISTS`
    Exists,
    /// `EXPLAIN`
    Explain,
    /// `FALSE`
    False,
    /// `FROM`
    From,
    /// `IF`
    If,
    /// `INDEX`
    Index,
    /// `INNER`
    Inner,
    /// `INSERT`
    Insert,
    /// `INTEGER`
    Integer,
    /// `INTO`
    Into,
    /// `IS`
    Is,
    /// `JOIN`
    Join,
    /// `KEY`
    Key,
    /// `LIKE`
    Like,
    /// `LIMIT`
    Limit,
    /// `NOT`
    Not,
    /// `NULL`
    Null,
    /// `OFFSET`
    Offset,
    /// `ON`
    On,
    /// `OR`
    Or,
    /// `ORDER`
    Order,
    /// `PRIMARY`
    Primary,
    /// `REAL`
    Real,
    /// `ROLLBACK`
    Rollback,
    /// `SELECT`
    Select,
    /// `SET`
    Set,
    /// `TABLE`
    Table,
    /// `TEXT`
    Text,
    /// `TRUE`
    True,
    /// `UNIQUE`
    Unique,
    /// `UPDATE`
    Update,
    /// `VACUUM`
    Vacuum,
    /// `VALUES`
    Values,
    /// `WHERE`
    Where,
}

impl Keyword {
    /// Matches a word against the reserved list, ignoring case.
    pub fn parse(word: &str) -> Option<Keyword> {
        use Keyword::*;
        Some(match word.to_ascii_uppercase().as_str() {
            "AND" => And,
            "AS" => As,
            "ASC" => Asc,
            "BEGIN" => Begin,
            "BETWEEN" => Between,
            "BLOB" => Blob,
            "BOOLEAN" => Boolean,
            "BY" => By,
            "COMMIT" => Commit,
            "CREATE" => Create,
            "DEFAULT" => Default,
            "DELETE" => Delete,
            "ALL" => All,
            "DESC" => Desc,
            "DISTINCT" => Distinct,
            "EXISTS" => Exists,
            "EXPLAIN" => Explain,
            "FALSE" => False,
            "FROM" => From,
            "IF" => If,
            "INDEX" => Index,
            "INNER" => Inner,
            "INSERT" => Insert,
            "INTEGER" => Integer,
            "INTO" => Into,
            "IS" => Is,
            "JOIN" => Join,
            "KEY" => Key,
            "LIKE" => Like,
            "LIMIT" => Limit,
            "NOT" => Not,
            "NULL" => Null,
            "OFFSET" => Offset,
            "ON" => On,
            "OR" => Or,
            "ORDER" => Order,
            "PRIMARY" => Primary,
            "REAL" => Real,
            "ROLLBACK" => Rollback,
            "SELECT" => Select,
            "SET" => Set,
            "TABLE" => Table,
            "TEXT" => Text,
            "TRUE" => True,
            "UNIQUE" => Unique,
            "UPDATE" => Update,
            "VACUUM" => Vacuum,
            "VALUES" => Values,
            "WHERE" => Where,
            _ => return None,
        })
    }

    /// The word as it is written in a statement.
    pub fn as_str(&self) -> &'static str {
        use Keyword::*;
        match self {
            And => "AND",
            As => "AS",
            Asc => "ASC",
            Begin => "BEGIN",
            Between => "BETWEEN",
            Blob => "BLOB",
            Boolean => "BOOLEAN",
            By => "BY",
            Commit => "COMMIT",
            Create => "CREATE",
            Default => "DEFAULT",
            Delete => "DELETE",
            All => "ALL",
            Desc => "DESC",
            Distinct => "DISTINCT",
            Exists => "EXISTS",
            Explain => "EXPLAIN",
            False => "FALSE",
            From => "FROM",
            If => "IF",
            Index => "INDEX",
            Inner => "INNER",
            Insert => "INSERT",
            Integer => "INTEGER",
            Into => "INTO",
            Is => "IS",
            Join => "JOIN",
            Key => "KEY",
            Like => "LIKE",
            Limit => "LIMIT",
            Not => "NOT",
            Null => "NULL",
            Offset => "OFFSET",
            On => "ON",
            Or => "OR",
            Order => "ORDER",
            Primary => "PRIMARY",
            Real => "REAL",
            Rollback => "ROLLBACK",
            Select => "SELECT",
            Set => "SET",
            Table => "TABLE",
            Text => "TEXT",
            True => "TRUE",
            Unique => "UNIQUE",
            Update => "UPDATE",
            Vacuum => "VACUUM",
            Values => "VALUES",
            Where => "WHERE",
        }
    }
}

/// What a token is.
#[derive(Debug, Clone, PartialEq)]
pub enum TokenKind {
    /// A name: a table, a column, an alias.
    Ident(String),
    /// A reserved word.
    Word(Keyword),
    /// A whole number.
    Int(i64),
    /// A number with a fractional part or an exponent.
    Real(f64),
    /// A string between single quotes.
    Text(String),

    /// `,`
    Comma,
    /// `(`
    LeftParen,
    /// `)`
    RightParen,
    /// `.`
    Dot,
    /// `;`
    Semicolon,
    /// `*`
    Star,
    /// `=`
    Eq,
    /// `<>` or `!=`
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
    Plus,
    /// `-`
    Minus,
    /// `/`
    Slash,
    /// `%`
    Percent,

    /// The end of the input.
    End,
}

impl fmt::Display for TokenKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TokenKind::Ident(name) => write!(f, "the name {name}"),
            TokenKind::Word(word) => write!(f, "{}", word.as_str()),
            TokenKind::Int(value) => write!(f, "{value}"),
            TokenKind::Real(value) => write!(f, "{value}"),
            TokenKind::Text(_) => write!(f, "a string"),
            TokenKind::Comma => write!(f, "','"),
            TokenKind::LeftParen => write!(f, "'('"),
            TokenKind::RightParen => write!(f, "')'"),
            TokenKind::Dot => write!(f, "'.'"),
            TokenKind::Semicolon => write!(f, "';'"),
            TokenKind::Star => write!(f, "'*'"),
            TokenKind::Eq => write!(f, "'='"),
            TokenKind::NotEq => write!(f, "'<>'"),
            TokenKind::Less => write!(f, "'<'"),
            TokenKind::LessEq => write!(f, "'<='"),
            TokenKind::Greater => write!(f, "'>'"),
            TokenKind::GreaterEq => write!(f, "'>='"),
            TokenKind::Plus => write!(f, "'+'"),
            TokenKind::Minus => write!(f, "'-'"),
            TokenKind::Slash => write!(f, "'/'"),
            TokenKind::Percent => write!(f, "'%'"),
            TokenKind::End => write!(f, "the end of the statement"),
        }
    }
}

/// A token and where it started.
#[derive(Debug, Clone, PartialEq)]
pub struct Token {
    /// What it is.
    pub kind: TokenKind,
    /// The byte offset it began at, for pointing at errors.
    pub at: usize,
}

/// Splits `sql` into tokens, ending with [`TokenKind::End`].
///
/// Comments are dropped: `--` to the end of the line, and `/* */` which does not
/// nest.
pub fn tokenize(sql: &str) -> Result<Vec<Token>> {
    let bytes = sql.as_bytes();
    let mut tokens = Vec::new();
    let mut at = 0usize;

    while at < bytes.len() {
        let byte = bytes[at];

        if byte.is_ascii_whitespace() {
            at += 1;
            continue;
        }
        if byte == b'-' && bytes.get(at + 1) == Some(&b'-') {
            while at < bytes.len() && bytes[at] != b'\n' {
                at += 1;
            }
            continue;
        }
        if byte == b'/' && bytes.get(at + 1) == Some(&b'*') {
            let start = at;
            at += 2;
            loop {
                if at + 1 >= bytes.len() {
                    return Err(sql_error("unterminated block comment", start));
                }
                if bytes[at] == b'*' && bytes[at + 1] == b'/' {
                    at += 2;
                    break;
                }
                at += 1;
            }
            continue;
        }

        let start = at;
        let kind = if byte == b'\'' {
            let (text, next) = read_string(sql, at)?;
            at = next;
            TokenKind::Text(text)
        } else if byte.is_ascii_digit() {
            let (kind, next) = read_number(sql, at)?;
            at = next;
            kind
        } else if byte == b'_' || byte.is_ascii_alphabetic() {
            let mut end = at;
            while end < bytes.len() && (bytes[end] == b'_' || bytes[end].is_ascii_alphanumeric()) {
                end += 1;
            }
            let word = &sql[at..end];
            at = end;
            match Keyword::parse(word) {
                Some(keyword) => TokenKind::Word(keyword),
                None => TokenKind::Ident(word.to_string()),
            }
        } else {
            let (kind, width) = match byte {
                b',' => (TokenKind::Comma, 1),
                b'(' => (TokenKind::LeftParen, 1),
                b')' => (TokenKind::RightParen, 1),
                b'.' => (TokenKind::Dot, 1),
                b';' => (TokenKind::Semicolon, 1),
                b'*' => (TokenKind::Star, 1),
                b'+' => (TokenKind::Plus, 1),
                b'-' => (TokenKind::Minus, 1),
                b'/' => (TokenKind::Slash, 1),
                b'%' => (TokenKind::Percent, 1),
                b'=' => (TokenKind::Eq, 1),
                b'<' => match bytes.get(at + 1) {
                    Some(b'>') => (TokenKind::NotEq, 2),
                    Some(b'=') => (TokenKind::LessEq, 2),
                    _ => (TokenKind::Less, 1),
                },
                b'>' => match bytes.get(at + 1) {
                    Some(b'=') => (TokenKind::GreaterEq, 2),
                    _ => (TokenKind::Greater, 1),
                },
                b'!' => match bytes.get(at + 1) {
                    Some(b'=') => (TokenKind::NotEq, 2),
                    _ => return Err(sql_error("'!' is only valid as part of '!='", at)),
                },
                other => {
                    let shown = char::from(other);
                    return Err(sql_error(format!("unexpected character {shown:?}"), at));
                }
            };
            at += width;
            kind
        };

        tokens.push(Token { kind, at: start });
    }

    tokens.push(Token {
        kind: TokenKind::End,
        at: sql.len(),
    });
    Ok(tokens)
}

/// Reads a single-quoted string. A quote inside one is written twice.
fn read_string(sql: &str, start: usize) -> Result<(String, usize)> {
    let bytes = sql.as_bytes();
    let mut out = String::new();
    let mut at = start + 1;

    loop {
        if at >= bytes.len() {
            return Err(sql_error("unterminated string", start));
        }
        if bytes[at] == b'\'' {
            if bytes.get(at + 1) == Some(&b'\'') {
                out.push('\'');
                at += 2;
                continue;
            }
            return Ok((out, at + 1));
        }
        // Multi-byte characters are copied whole, so a string may hold any text.
        let rest = &sql[at..];
        let character = rest.chars().next().expect("inside the string");
        out.push(character);
        at += character.len_utf8();
    }
}

/// Reads an integer, or a real if a fractional part or exponent follows.
fn read_number(sql: &str, start: usize) -> Result<(TokenKind, usize)> {
    let bytes = sql.as_bytes();
    let mut at = start;
    while at < bytes.len() && bytes[at].is_ascii_digit() {
        at += 1;
    }

    let mut is_real = false;
    // A dot is only part of the number when a digit follows, so that `1.foo`
    // stays a column reference rather than becoming a malformed number.
    if bytes.get(at) == Some(&b'.') && bytes.get(at + 1).is_some_and(u8::is_ascii_digit) {
        is_real = true;
        at += 1;
        while at < bytes.len() && bytes[at].is_ascii_digit() {
            at += 1;
        }
    }
    if matches!(bytes.get(at), Some(b'e') | Some(b'E')) {
        let mut lookahead = at + 1;
        if matches!(bytes.get(lookahead), Some(b'+') | Some(b'-')) {
            lookahead += 1;
        }
        if bytes.get(lookahead).is_some_and(u8::is_ascii_digit) {
            is_real = true;
            at = lookahead;
            while at < bytes.len() && bytes[at].is_ascii_digit() {
                at += 1;
            }
        }
    }

    let text = &sql[start..at];
    if is_real {
        let value = text
            .parse::<f64>()
            .ok()
            .filter(|value| value.is_finite())
            .ok_or_else(|| sql_error(format!("{text} is out of range for a real"), start))?;
        Ok((TokenKind::Real(value), at))
    } else {
        let value = text
            .parse::<i64>()
            .map_err(|_| sql_error(format!("{text} does not fit in a 64 bit integer"), start))?;
        Ok((TokenKind::Int(value), at))
    }
}

fn sql_error(message: impl Into<String>, at: usize) -> Error {
    Error::Sql {
        message: message.into(),
        at,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(sql: &str) -> Vec<TokenKind> {
        tokenize(sql).unwrap().into_iter().map(|t| t.kind).collect()
    }

    #[test]
    fn splits_a_statement() {
        assert_eq!(
            kinds("SELECT a FROM t WHERE a >= 1;"),
            vec![
                TokenKind::Word(Keyword::Select),
                TokenKind::Ident("a".into()),
                TokenKind::Word(Keyword::From),
                TokenKind::Ident("t".into()),
                TokenKind::Word(Keyword::Where),
                TokenKind::Ident("a".into()),
                TokenKind::GreaterEq,
                TokenKind::Int(1),
                TokenKind::Semicolon,
                TokenKind::End,
            ]
        );
    }

    #[test]
    fn keywords_ignore_case_and_names_do_not() {
        assert_eq!(
            kinds("sElEcT Gado"),
            vec![
                TokenKind::Word(Keyword::Select),
                TokenKind::Ident("Gado".into()),
                TokenKind::End,
            ]
        );
    }

    #[test]
    fn reads_numbers() {
        assert_eq!(kinds("42")[0], TokenKind::Int(42));
        assert_eq!(kinds("431.5")[0], TokenKind::Real(431.5));
        assert_eq!(kinds("1e3")[0], TokenKind::Real(1000.0));
        assert_eq!(kinds("1E-2")[0], TokenKind::Real(0.01));
    }

    #[test]
    fn a_dot_after_a_number_is_only_part_of_it_when_a_digit_follows() {
        // `1.5` is one number; `t.a` is a qualified column and must stay three
        // tokens even when the table name starts with a digit-like shape.
        assert_eq!(
            kinds("1 . a"),
            vec![
                TokenKind::Int(1),
                TokenKind::Dot,
                TokenKind::Ident("a".into()),
                TokenKind::End,
            ]
        );
    }

    #[test]
    fn reads_strings_with_doubled_quotes() {
        assert_eq!(kinds("'it''s'")[0], TokenKind::Text("it's".into()));
        assert_eq!(kinds("''")[0], TokenKind::Text(String::new()));
        assert_eq!(kinds("'ração'")[0], TokenKind::Text("ração".into()));
    }

    #[test]
    fn drops_comments() {
        assert_eq!(
            kinds("SELECT -- everything\n a /* and this */ FROM t"),
            vec![
                TokenKind::Word(Keyword::Select),
                TokenKind::Ident("a".into()),
                TokenKind::Word(Keyword::From),
                TokenKind::Ident("t".into()),
                TokenKind::End,
            ]
        );
    }

    #[test]
    fn unfinished_input_points_at_where_it_started() {
        let error = tokenize("SELECT 'abc").unwrap_err();
        assert!(matches!(error, Error::Sql { at: 7, .. }), "{error}");

        let error = tokenize("SELECT /* abc").unwrap_err();
        assert!(matches!(error, Error::Sql { at: 7, .. }), "{error}");
    }

    #[test]
    fn an_unknown_character_is_reported_where_it_is() {
        let error = tokenize("SELECT a # b").unwrap_err();
        assert!(matches!(error, Error::Sql { at: 9, .. }), "{error}");
    }

    #[test]
    fn both_spellings_of_not_equal() {
        assert_eq!(kinds("<>")[0], TokenKind::NotEq);
        assert_eq!(kinds("!=")[0], TokenKind::NotEq);
    }
}
