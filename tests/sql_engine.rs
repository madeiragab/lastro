//! The database end to end: text in, rows out.

use lastro::sql::{Database, Outcome};
use lastro::storage::page::encoding::Value;

fn open() -> (tempfile::TempDir, Database) {
    let dir = tempfile::tempdir().unwrap();
    let db = Database::open(dir.path().join("t.lastro")).unwrap();
    (dir, db)
}

fn rows(db: &mut Database, sql: &str) -> Vec<Vec<Value>> {
    match db.query(sql).unwrap() {
        Outcome::Rows { rows, .. } => rows,
        other => panic!("{sql} produced {other:?}"),
    }
}

fn plan_of(db: &mut Database, sql: &str) -> String {
    match db.query(&format!("EXPLAIN {sql}")).unwrap() {
        Outcome::Plan(plan) => plan,
        other => panic!("{sql} produced {other:?}"),
    }
}

fn herd(db: &mut Database) {
    db.execute(
        "CREATE TABLE gado (id INTEGER PRIMARY KEY, brinco TEXT NOT NULL, peso REAL, ativo BOOLEAN);
         INSERT INTO gado VALUES
            (1, 'BR-0001', 431.5, TRUE),
            (2, 'BR-0002', 380.0, TRUE),
            (3, 'BR-0003', 512.25, FALSE),
            (4, 'BR-0004', NULL, TRUE),
            (5, 'BR-0005', 299.0, FALSE);",
    )
    .unwrap();
}

#[test]
fn a_table_can_be_created_written_and_read_back() {
    let (_dir, mut db) = open();
    herd(&mut db);

    let all = rows(&mut db, "SELECT * FROM gado");
    assert_eq!(all.len(), 5);
    assert_eq!(all[0][0], Value::Int(1));
    assert_eq!(all[0][1], Value::Text("BR-0001".into()));
    assert_eq!(all[0][2], Value::Real(431.5));
    assert_eq!(all[0][3], Value::Bool(true));
}

#[test]
fn rows_come_back_in_primary_key_order() {
    let (_dir, mut db) = open();
    db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, n INTEGER)")
        .unwrap();
    db.execute("INSERT INTO t VALUES (30, 3), (10, 1), (20, 2)")
        .unwrap();

    let found = rows(&mut db, "SELECT id FROM t");
    let ids: Vec<i64> = found
        .iter()
        .map(|row| match row[0] {
            Value::Int(id) => id,
            _ => panic!(),
        })
        .collect();
    assert_eq!(ids, vec![10, 20, 30], "a scan follows the key order");
}

#[test]
fn a_row_id_is_handed_out_when_none_is_given() {
    let (_dir, mut db) = open();
    db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, n INTEGER)")
        .unwrap();
    db.execute("INSERT INTO t (n) VALUES (10), (20), (30)")
        .unwrap();

    let found = rows(&mut db, "SELECT id, n FROM t");
    assert_eq!(found[0][0], Value::Int(1));
    assert_eq!(found[1][0], Value::Int(2));
    assert_eq!(found[2][0], Value::Int(3));
}

#[test]
fn a_predicate_selects_and_null_does_not_pass() {
    let (_dir, mut db) = open();
    herd(&mut db);

    assert_eq!(
        rows(&mut db, "SELECT id FROM gado WHERE peso > 400").len(),
        2
    );
    assert_eq!(rows(&mut db, "SELECT id FROM gado WHERE ativo").len(), 3);
    assert_eq!(
        rows(&mut db, "SELECT id FROM gado WHERE peso IS NULL").len(),
        1
    );

    // The row with a null weight satisfies neither the comparison nor its
    // negation. Unknown is not false, and only true admits a row.
    let above = rows(&mut db, "SELECT id FROM gado WHERE peso > 400").len();
    let below = rows(&mut db, "SELECT id FROM gado WHERE peso <= 400").len();
    assert_eq!(above + below, 4, "the null row is in neither half");
}

#[test]
fn ordering_and_limiting() {
    let (_dir, mut db) = open();
    herd(&mut db);

    let heaviest = rows(
        &mut db,
        "SELECT brinco FROM gado ORDER BY peso DESC LIMIT 2",
    );
    assert_eq!(heaviest.len(), 2);
    assert_eq!(heaviest[0][0], Value::Text("BR-0003".into()));
    assert_eq!(heaviest[1][0], Value::Text("BR-0001".into()));

    let skipped = rows(&mut db, "SELECT id FROM gado ORDER BY id LIMIT 2 OFFSET 3");
    assert_eq!(skipped.len(), 2);
    assert_eq!(skipped[0][0], Value::Int(4));

    // Nulls sort first, the same way they do in an encoded key.
    let ascending = rows(&mut db, "SELECT id FROM gado ORDER BY peso ASC");
    assert_eq!(ascending[0][0], Value::Int(4));
}

#[test]
fn expressions_and_aliases() {
    let (_dir, mut db) = open();
    herd(&mut db);

    let outcome = db
        .query("SELECT brinco AS tag, peso * 2 AS dobro FROM gado WHERE id = 1")
        .unwrap();
    let Outcome::Rows { columns, rows } = outcome else {
        panic!()
    };
    assert_eq!(columns, vec!["tag", "dobro"]);
    assert_eq!(rows[0][1], Value::Real(863.0));
}

#[test]
fn like_and_between() {
    let (_dir, mut db) = open();
    herd(&mut db);

    assert_eq!(
        rows(&mut db, "SELECT id FROM gado WHERE brinco LIKE 'BR-000_'").len(),
        5
    );
    assert_eq!(
        rows(&mut db, "SELECT id FROM gado WHERE brinco LIKE '%3'").len(),
        1
    );
    assert_eq!(
        rows(
            &mut db,
            "SELECT id FROM gado WHERE peso BETWEEN 300 AND 450"
        )
        .len(),
        2
    );
}

#[test]
fn a_predicate_on_the_primary_key_becomes_a_descent() {
    let (_dir, mut db) = open();
    herd(&mut db);

    // Rule 1: an equality on the row id turns the scan into a lookup.
    let plan = plan_of(&mut db, "SELECT * FROM gado WHERE id = 3");
    assert!(plan.contains("RowIdScan gado (= 3)"), "{plan}");
    assert!(!plan.contains("SeqScan"), "{plan}");
    assert_eq!(
        rows(&mut db, "SELECT brinco FROM gado WHERE id = 3").len(),
        1
    );

    // A range works the same way, on either side of the comparison.
    let plan = plan_of(&mut db, "SELECT * FROM gado WHERE id >= 2 AND id < 5");
    assert!(plan.contains("RowIdScan gado (>= 2, < 5)"), "{plan}");
    assert_eq!(
        rows(&mut db, "SELECT id FROM gado WHERE id >= 2 AND id < 5").len(),
        3
    );

    let plan = plan_of(&mut db, "SELECT * FROM gado WHERE 3 < id");
    assert!(plan.contains("RowIdScan gado (> 3"), "{plan}");

    // A predicate the range cannot express stays behind as a filter.
    let plan = plan_of(&mut db, "SELECT * FROM gado WHERE id > 1 AND peso > 400");
    assert!(plan.contains("RowIdScan gado (> 1"), "{plan}");
    assert!(plan.contains("Filter"), "{plan}");

    // A bound under an OR is not a bound at all, so the scan stays whole.
    let plan = plan_of(&mut db, "SELECT * FROM gado WHERE id = 1 OR peso > 400");
    assert!(plan.contains("SeqScan"), "{plan}");
}

#[test]
fn sorting_by_the_primary_key_is_work_already_done() {
    let (_dir, mut db) = open();
    herd(&mut db);

    // Rule 5: the scan already yields rows in row id order.
    let plan = plan_of(&mut db, "SELECT * FROM gado ORDER BY id");
    assert!(!plan.contains("Sort"), "{plan}");

    let plan = plan_of(&mut db, "SELECT * FROM gado ORDER BY id DESC");
    assert!(plan.contains("Sort"), "descending still needs one: {plan}");

    let plan = plan_of(&mut db, "SELECT * FROM gado ORDER BY peso");
    assert!(plan.contains("Sort"), "{plan}");
}

#[test]
fn a_limit_over_a_sort_becomes_a_top_n() {
    let (_dir, mut db) = open();
    herd(&mut db);

    // Rule 6: only as many rows as are wanted are kept.
    let plan = plan_of(&mut db, "SELECT * FROM gado ORDER BY peso DESC LIMIT 2");
    assert!(plan.contains("(top-2)"), "{plan}");

    let plan = plan_of(
        &mut db,
        "SELECT * FROM gado ORDER BY peso DESC LIMIT 2 OFFSET 3",
    );
    assert!(plan.contains("(top-5)"), "the offset counts too: {plan}");
}

#[test]
fn schema_and_rows_survive_reopening() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("survive.lastro");

    {
        let mut db = Database::open(&path).unwrap();
        db.execute("CREATE TABLE gado (id INTEGER PRIMARY KEY, brinco TEXT)")
            .unwrap();
        for id in 1..=500i64 {
            db.execute(&format!("INSERT INTO gado VALUES ({id}, 'BR-{id}')"))
                .unwrap();
        }
        // No checkpoint and no clean close: the pages are dropped and only the
        // log has them.
    }

    let mut db = Database::open(&path).unwrap();
    let found = rows(&mut db, "SELECT id FROM gado");
    assert_eq!(found.len(), 500, "redo had to rebuild the table");
    assert_eq!(
        rows(&mut db, "SELECT brinco FROM gado WHERE id = 250")[0][0],
        Value::Text("BR-250".into())
    );
}

#[test]
fn a_rolled_back_transaction_leaves_nothing() {
    let (_dir, mut db) = open();
    db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, n INTEGER)")
        .unwrap();
    db.execute("INSERT INTO t VALUES (1, 10)").unwrap();

    db.execute("BEGIN; INSERT INTO t VALUES (2, 20), (3, 30);")
        .unwrap();
    assert_eq!(rows(&mut db, "SELECT id FROM t").len(), 3);

    db.execute("ROLLBACK").unwrap();
    assert_eq!(rows(&mut db, "SELECT id FROM t").len(), 1);
}

#[test]
fn a_committed_transaction_stays() {
    let (_dir, mut db) = open();
    db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, n INTEGER)")
        .unwrap();
    db.execute("BEGIN; INSERT INTO t VALUES (1, 10); INSERT INTO t VALUES (2, 20); COMMIT;")
        .unwrap();
    assert_eq!(rows(&mut db, "SELECT id FROM t").len(), 2);
}

#[test]
fn a_failed_statement_leaves_the_table_as_it_was() {
    let (_dir, mut db) = open();
    db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, n INTEGER NOT NULL)")
        .unwrap();
    db.execute("INSERT INTO t VALUES (1, 10)").unwrap();

    // The second row violates NOT NULL. The statement is one transaction, so
    // the first row must not survive either.
    assert!(db
        .execute("INSERT INTO t VALUES (2, 20), (3, NULL)")
        .is_err());
    assert_eq!(rows(&mut db, "SELECT id FROM t").len(), 1);
}

#[test]
fn the_schema_is_enforced() {
    let (_dir, mut db) = open();
    db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, nome TEXT NOT NULL)")
        .unwrap();

    assert!(
        db.execute("INSERT INTO t VALUES (1, NULL)").is_err(),
        "NOT NULL"
    );
    assert!(
        db.execute("INSERT INTO t VALUES (1, 42)").is_err(),
        "wrong type"
    );
    assert!(
        db.execute("INSERT INTO t VALUES (1)").is_err(),
        "wrong arity"
    );
    assert!(
        db.execute("SELECT * FROM ausente").is_err(),
        "unknown table"
    );
    assert!(
        db.execute("SELECT ausente FROM t").is_err(),
        "unknown column"
    );
    assert!(
        db.execute("CREATE TABLE t (a INTEGER)").is_err(),
        "duplicate"
    );
    db.execute("CREATE TABLE IF NOT EXISTS t (a INTEGER)")
        .unwrap();
}

#[test]
fn defaults_fill_in_what_a_statement_leaves_out() {
    let (_dir, mut db) = open();
    db.execute(
        "CREATE TABLE t (id INTEGER PRIMARY KEY, ativo BOOLEAN DEFAULT TRUE, nota TEXT DEFAULT 'sem nota')",
    )
    .unwrap();
    db.execute("INSERT INTO t (id) VALUES (1)").unwrap();

    let found = rows(&mut db, "SELECT ativo, nota FROM t");
    assert_eq!(found[0][0], Value::Bool(true));
    assert_eq!(found[0][1], Value::Text("sem nota".into()));
}

#[test]
fn a_table_larger_than_the_pool_still_reads_back() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = Database::with_capacity(dir.path().join("big.lastro"), 8).unwrap();
    db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, texto TEXT)")
        .unwrap();

    let filler = "x".repeat(300);
    db.execute("BEGIN").unwrap();
    for id in 1..=800i64 {
        db.execute(&format!("INSERT INTO t VALUES ({id}, '{filler}')"))
            .unwrap();
    }
    db.execute("COMMIT").unwrap();

    assert_eq!(rows(&mut db, "SELECT id FROM t").len(), 800);
    assert_eq!(
        rows(&mut db, "SELECT id FROM t WHERE id BETWEEN 100 AND 199").len(),
        100
    );
}
