//! The SQL front end.
//!
//! The load-bearing test here is the round trip: every tree renders back into
//! SQL that parses to the same tree. It costs almost nothing to write and it
//! catches whole classes of precedence and associativity mistakes that nobody
//! would think to write a case for.

use lastro::sql::{parse, parse_many, Expr, Literal, Projection, Statement};
use lastro::Error;
use proptest::prelude::*;

/// Every statement the implemented subset accepts, as one corpus.
const CORPUS: &[&str] = &[
    "BEGIN",
    "COMMIT",
    "ROLLBACK",
    "SELECT * FROM gado",
    "SELECT brinco FROM gado",
    "SELECT brinco, peso FROM gado",
    "SELECT brinco AS tag, peso * 2 FROM gado",
    "SELECT g.brinco FROM gado AS g",
    "SELECT g.brinco FROM gado g",
    "SELECT * FROM gado WHERE peso > 400",
    "SELECT * FROM gado WHERE peso > 400 AND ativo = TRUE",
    "SELECT * FROM gado WHERE peso > 400 OR peso < 100",
    "SELECT * FROM gado WHERE NOT ativo",
    "SELECT * FROM gado WHERE peso IS NULL",
    "SELECT * FROM gado WHERE peso IS NOT NULL",
    "SELECT * FROM gado WHERE brinco LIKE 'BR-%'",
    "SELECT * FROM gado WHERE brinco NOT LIKE 'BR-%'",
    "SELECT * FROM gado WHERE peso BETWEEN 300 AND 500",
    "SELECT * FROM gado WHERE peso NOT BETWEEN 300 AND 500",
    "SELECT * FROM gado ORDER BY peso",
    "SELECT * FROM gado ORDER BY peso DESC, brinco ASC",
    "SELECT * FROM gado LIMIT 10",
    "SELECT * FROM gado LIMIT 10 OFFSET 20",
    "SELECT g.brinco, p.data FROM gado g JOIN pesagem p ON p.gado_id = g.id",
    "SELECT g.brinco FROM gado g INNER JOIN pesagem p ON p.gado_id = g.id",
    "SELECT g.brinco, p.data FROM gado g JOIN pesagem p ON p.gado_id = g.id \
     WHERE g.peso > 400 ORDER BY g.peso DESC LIMIT 10",
    "INSERT INTO gado VALUES (1)",
    "INSERT INTO gado (id, brinco, peso) VALUES (1, 'BR-0042', 431.5)",
    "INSERT INTO gado (id) VALUES (1), (2), (3)",
    "UPDATE gado SET peso = 450.0",
    "UPDATE gado SET peso = 450.0, ativo = FALSE WHERE id = 1",
    "DELETE FROM gado",
    "DELETE FROM gado WHERE ativo = FALSE",
    "CREATE TABLE gado (id INTEGER PRIMARY KEY, brinco TEXT NOT NULL, peso REAL)",
    "CREATE TABLE IF NOT EXISTS gado (id INTEGER)",
    "CREATE TABLE gado (id INTEGER, ativo BOOLEAN DEFAULT TRUE, dados BLOB)",
    "CREATE TABLE gado (id INTEGER UNIQUE)",
    "CREATE INDEX idx_brinco ON gado (brinco)",
    "CREATE UNIQUE INDEX idx_brinco ON gado (brinco, peso)",
    "EXPLAIN SELECT * FROM gado WHERE peso > 400",
    "SELECT -1 FROM t",
    "SELECT (1 + 2) * 3 FROM t",
    "SELECT 'it''s' FROM t",
    "SELECT 1e3, 1E-2, 0.5 FROM t",
    "SELECT NULL FROM t WHERE a <> b",
    "SELECT a % b FROM t",
];

#[test]
fn the_whole_corpus_parses() {
    for sql in CORPUS {
        parse(sql).unwrap_or_else(|error| panic!("{sql}\n  {error}"));
    }
}

#[test]
fn every_tree_renders_back_into_the_same_tree() {
    for sql in CORPUS {
        let first = parse(sql).unwrap();
        let rendered = first.to_string();
        let second = parse(&rendered)
            .unwrap_or_else(|error| panic!("{sql}\n  rendered as {rendered}\n  {error}"));
        assert_eq!(first, second, "{sql}\n  rendered as {rendered}");

        // And rendering is stable, so the round trip has a fixed point rather
        // than drifting a little further each time.
        assert_eq!(rendered, second.to_string(), "{sql}");
    }
}

#[test]
fn precedence_follows_the_rule_hierarchy() {
    // `a OR b AND c` has to bind as `a OR (b AND c)`, and the fully
    // parenthesized rendering is where that becomes visible.
    let rendered = parse("SELECT * FROM t WHERE a OR b AND c")
        .unwrap()
        .to_string();
    assert!(rendered.contains("(a OR (b AND c))"), "{rendered}");

    let rendered = parse("SELECT 1 + 2 * 3 FROM t").unwrap().to_string();
    assert!(rendered.contains("(1 + (2 * 3))"), "{rendered}");

    let rendered = parse("SELECT 1 - 2 - 3 FROM t").unwrap().to_string();
    assert!(
        rendered.contains("((1 - 2) - 3)"),
        "left associative: {rendered}"
    );

    let rendered = parse("SELECT * FROM t WHERE NOT a AND b")
        .unwrap()
        .to_string();
    assert!(rendered.contains("((NOT a) AND b)"), "{rendered}");
}

#[test]
fn keywords_ignore_case() {
    let lower = parse("select * from gado where peso > 400").unwrap();
    let upper = parse("SELECT * FROM gado WHERE peso > 400").unwrap();
    assert_eq!(lower, upper);
}

#[test]
fn a_statement_may_end_with_a_semicolon() {
    assert_eq!(
        parse("SELECT * FROM t;").unwrap(),
        parse("SELECT * FROM t").unwrap()
    );
}

#[test]
fn several_statements_come_back_in_order() {
    let statements = parse_many("BEGIN; SELECT * FROM t; COMMIT;").unwrap();
    assert_eq!(statements.len(), 3);
    assert!(matches!(statements[0], Statement::Begin));
    assert!(matches!(statements[1], Statement::Select(_)));
    assert!(matches!(statements[2], Statement::Commit));

    assert!(parse_many("").unwrap().is_empty());
    assert!(parse_many("  ;;  ").unwrap().is_empty());
}

#[test]
fn star_is_recognised_as_a_projection_and_not_as_multiplication() {
    let Statement::Select(select) = parse("SELECT * FROM t").unwrap() else {
        panic!("expected a select");
    };
    assert_eq!(select.projection, Projection::Star);
}

#[test]
fn a_whole_number_written_as_a_real_stays_a_real() {
    // The rendering has to keep the decimal point, or 1.0 comes back as an
    // integer and the round trip quietly changes the type.
    let Statement::Select(select) = parse("SELECT 1.0 FROM t").unwrap() else {
        panic!("expected a select");
    };
    let Projection::Items(items) = &select.projection else {
        panic!("expected a list");
    };
    assert_eq!(items[0].expr, Expr::Literal(Literal::Real(1.0)));
    assert!(select.to_string().contains("1.0"), "{select}");
}

#[test]
fn errors_point_at_the_offending_token() {
    let cases = [
        ("SELECT FROM t", 7),
        ("SELECT a FROM", 13),
        ("SELECT a FROM t WHERE", 21),
        ("INSERT INTO t VALUES", 20),
        ("CREATE TABLE t (a NOTATYPE)", 18),
        ("SELECT a FROM t LIMIT -1", 22),
    ];
    for (sql, at) in cases {
        match parse(sql) {
            Err(Error::Sql { at: found, message }) => {
                assert_eq!(found, at, "{sql}: {message} pointed at {found}");
            }
            other => panic!("{sql} should not have parsed: {other:?}"),
        }
    }
}

#[test]
fn malformed_input_is_refused_rather_than_half_accepted() {
    let cases = [
        "SELECT",
        "SELECT * FROM",
        "SELECT * FROM t WHERE (",
        "SELECT * FROM t ORDER",
        "SELECT * FROM t ORDER BY",
        "INSERT INTO t (a VALUES (1)",
        "UPDATE t SET",
        "UPDATE t SET a",
        "DELETE",
        "CREATE",
        "CREATE TABLE t",
        "CREATE TABLE t ()",
        "CREATE INDEX i ON t",
        "SELECT * FROM t t2 t3",
        "SELECT 'unterminated FROM t",
        "SELECT /* unterminated FROM t",
        "nonsense",
        "",
    ];
    for sql in cases {
        assert!(parse(sql).is_err(), "{sql} should not have parsed");
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(20_000))]

    #[test]
    fn arbitrary_text_never_panics(sql in ".{0,80}") {
        // Whatever comes in, the parser returns a structured error or a tree.
        // It never unwinds, never indexes past the end, never loops.
        let _ = parse(&sql);
        let _ = parse_many(&sql);
    }

    #[test]
    fn mutated_statements_never_panic(
        index in 0usize..CORPUS.len(),
        cut in 0usize..200,
        insert in ".{0,4}",
    ) {
        // Truncating and splicing real statements reaches the parser's corners
        // far more often than random text does.
        let sql = CORPUS[index];
        let cut = cut.min(sql.len());
        let boundary = (0..=cut).rev().find(|at| sql.is_char_boundary(*at)).unwrap_or(0);

        let truncated = &sql[..boundary];
        let _ = parse(truncated);

        let spliced = format!("{truncated}{insert}{}", &sql[boundary..]);
        let _ = parse(&spliced);
    }
}
