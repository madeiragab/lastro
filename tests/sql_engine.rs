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

// -- changing rows ---------------------------------------------------------

#[test]
fn update_changes_only_what_the_filter_admits() {
    let (_dir, mut db) = open();
    herd(&mut db);

    let outcome = db
        .query("UPDATE gado SET peso = 500.0 WHERE id = 2")
        .unwrap();
    assert_eq!(outcome, Outcome::Affected(1));

    let found = rows(&mut db, "SELECT peso FROM gado WHERE id = 2");
    assert_eq!(found[0][0], Value::Real(500.0));
    assert_eq!(
        rows(&mut db, "SELECT peso FROM gado WHERE id = 1")[0][0],
        Value::Real(431.5),
        "the other rows are untouched"
    );
}

#[test]
fn update_can_read_the_row_it_is_changing() {
    let (_dir, mut db) = open();
    herd(&mut db);

    db.query("UPDATE gado SET peso = peso + 100 WHERE peso IS NOT NULL")
        .unwrap();
    assert_eq!(
        rows(&mut db, "SELECT peso FROM gado WHERE id = 1")[0][0],
        Value::Real(531.5)
    );
    assert_eq!(
        rows(&mut db, "SELECT peso FROM gado WHERE id = 4")[0][0],
        Value::Null,
        "the null row was not admitted"
    );
}

#[test]
fn update_without_a_filter_touches_every_row() {
    let (_dir, mut db) = open();
    herd(&mut db);
    assert_eq!(
        db.query("UPDATE gado SET ativo = FALSE").unwrap(),
        Outcome::Affected(5)
    );
    assert_eq!(rows(&mut db, "SELECT id FROM gado WHERE ativo").len(), 0);
}

#[test]
fn changing_the_primary_key_moves_the_row() {
    let (_dir, mut db) = open();
    herd(&mut db);

    db.query("UPDATE gado SET id = 99 WHERE id = 1").unwrap();
    assert_eq!(rows(&mut db, "SELECT id FROM gado WHERE id = 1").len(), 0);

    let moved = rows(&mut db, "SELECT brinco FROM gado WHERE id = 99");
    assert_eq!(moved.len(), 1);
    assert_eq!(moved[0][0], Value::Text("BR-0001".into()));
    assert_eq!(
        rows(&mut db, "SELECT id FROM gado").len(),
        5,
        "moving is not duplicating"
    );
}

#[test]
fn delete_removes_only_what_the_filter_admits() {
    let (_dir, mut db) = open();
    herd(&mut db);

    assert_eq!(
        db.query("DELETE FROM gado WHERE ativo = FALSE").unwrap(),
        Outcome::Affected(2)
    );
    assert_eq!(rows(&mut db, "SELECT id FROM gado").len(), 3);

    assert_eq!(db.query("DELETE FROM gado").unwrap(), Outcome::Affected(3));
    assert_eq!(rows(&mut db, "SELECT id FROM gado").len(), 0);
}

#[test]
fn a_write_narrows_to_a_descent_the_same_way_a_query_does() {
    let (_dir, mut db) = open();
    herd(&mut db);

    let plan = plan_of(&mut db, "DELETE FROM gado WHERE id = 3");
    assert!(plan.contains("RowIdScan gado (= 3)"), "{plan}");

    let plan = plan_of(&mut db, "UPDATE gado SET peso = 1.0 WHERE id > 2 AND ativo");
    assert!(plan.contains("RowIdScan gado (> 2"), "{plan}");
    assert!(plan.contains("Filter"), "{plan}");
}

#[test]
fn a_failed_update_leaves_every_row_as_it_was() {
    let (_dir, mut db) = open();
    db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, nome TEXT NOT NULL)")
        .unwrap();
    db.execute("INSERT INTO t VALUES (1, 'a'), (2, 'b'), (3, 'c')")
        .unwrap();

    assert!(db.query("UPDATE t SET nome = NULL").is_err());
    let names = rows(&mut db, "SELECT nome FROM t");
    assert_eq!(names.len(), 3);
    assert_eq!(names[0][0], Value::Text("a".into()));
}

#[test]
fn changes_survive_a_crash_and_a_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("changed.lastro");

    {
        let mut db = Database::open(&path).unwrap();
        db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, n INTEGER)")
            .unwrap();
        db.execute("BEGIN").unwrap();
        for id in 1..=300i64 {
            db.execute(&format!("INSERT INTO t VALUES ({id}, {id})"))
                .unwrap();
        }
        db.execute("COMMIT").unwrap();

        db.query("UPDATE t SET n = n * 2 WHERE id <= 100").unwrap();
        db.query("DELETE FROM t WHERE id > 200").unwrap();
    }

    let mut db = Database::open(&path).unwrap();
    assert_eq!(rows(&mut db, "SELECT id FROM t").len(), 200);
    assert_eq!(
        rows(&mut db, "SELECT n FROM t WHERE id = 50")[0][0],
        Value::Int(100)
    );
    assert_eq!(
        rows(&mut db, "SELECT n FROM t WHERE id = 150")[0][0],
        Value::Int(150)
    );
}

#[test]
fn emptying_a_large_table_gives_its_pages_back() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = Database::with_capacity(dir.path().join("empty.lastro"), 32).unwrap();
    db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, texto TEXT)")
        .unwrap();

    let filler = "y".repeat(300);
    db.execute("BEGIN").unwrap();
    for id in 1..=600i64 {
        db.execute(&format!("INSERT INTO t VALUES ({id}, '{filler}')"))
            .unwrap();
    }
    db.execute("COMMIT").unwrap();
    db.checkpoint().unwrap();
    let grown = db.pool_mut().pager().page_count();

    db.query("DELETE FROM t").unwrap();
    db.checkpoint().unwrap();

    assert_eq!(rows(&mut db, "SELECT id FROM t").len(), 0);
    let freed = db.pool_mut().pager().meta().freelist_count;
    assert!(
        freed > grown / 2,
        "expected most of the {grown} pages back, got {freed}"
    );
    db.pool_mut().check_invariants().unwrap();
}

// -- joins -----------------------------------------------------------------

fn weighings(db: &mut Database) {
    herd(db);
    db.execute(
        "CREATE TABLE pesagem (id INTEGER PRIMARY KEY, gado_id INTEGER, kg REAL);
         INSERT INTO pesagem VALUES
            (1, 1, 431.5),
            (2, 1, 445.0),
            (3, 2, 380.0),
            (4, 3, 512.25),
            (5, 99, 1.0);",
    )
    .unwrap();
}

#[test]
fn a_join_pairs_the_rows_that_match() {
    let (_dir, mut db) = open();
    weighings(&mut db);

    let found = rows(
        &mut db,
        "SELECT g.brinco, p.kg FROM gado g JOIN pesagem p ON p.gado_id = g.id",
    );
    assert_eq!(
        found.len(),
        4,
        "the weighing of a cow that is gone drops out"
    );

    let ordered = rows(
        &mut db,
        "SELECT g.brinco, p.kg FROM gado g JOIN pesagem p ON p.gado_id = g.id ORDER BY p.kg DESC",
    );
    assert_eq!(ordered[0][0], Value::Text("BR-0003".into()));
    assert_eq!(ordered[0][1], Value::Real(512.25));
}

#[test]
fn an_equality_becomes_a_hash_join() {
    let (_dir, mut db) = open();
    weighings(&mut db);

    // Rule 4: one side reads only the left input, the other only the right.
    let plan = plan_of(
        &mut db,
        "SELECT g.brinco FROM gado g JOIN pesagem p ON p.gado_id = g.id",
    );
    assert!(plan.contains("HashJoin"), "{plan}");
    assert!(plan.contains("build:") && plan.contains("probe:"), "{plan}");

    // Anything else has to be checked pair by pair.
    let plan = plan_of(
        &mut db,
        "SELECT g.brinco FROM gado g JOIN pesagem p ON p.kg > g.peso",
    );
    assert!(plan.contains("NestedLoopJoin"), "{plan}");
}

#[test]
fn a_nested_loop_join_agrees_with_a_hash_join() {
    let (_dir, mut db) = open();
    weighings(&mut db);

    let hashed = rows(
        &mut db,
        "SELECT g.id, p.id FROM gado g JOIN pesagem p ON p.gado_id = g.id ORDER BY p.id",
    );
    // The same pairing written so that no equality qualifies, which forces the
    // other operator. Both must find exactly the same pairs.
    let looped = rows(
        &mut db,
        "SELECT g.id, p.id FROM gado g JOIN pesagem p ON p.gado_id >= g.id AND p.gado_id <= g.id \
         ORDER BY p.id",
    );
    assert_eq!(hashed, looped);
}

#[test]
fn a_join_condition_may_carry_more_than_the_equality() {
    let (_dir, mut db) = open();
    weighings(&mut db);

    let found = rows(
        &mut db,
        "SELECT p.id FROM gado g JOIN pesagem p ON p.gado_id = g.id AND p.kg > 400",
    );
    assert_eq!(found.len(), 3);

    let plan = plan_of(
        &mut db,
        "SELECT p.id FROM gado g JOIN pesagem p ON p.gado_id = g.id AND p.kg > 400",
    );
    assert!(plan.contains("HashJoin"), "{plan}");
    assert!(
        plan.contains("and ("),
        "the rest stays as a residual: {plan}"
    );
}

#[test]
fn a_null_join_key_matches_nothing() {
    let (_dir, mut db) = open();
    db.execute(
        "CREATE TABLE a (id INTEGER PRIMARY KEY, k INTEGER);
         CREATE TABLE b (id INTEGER PRIMARY KEY, k INTEGER);
         INSERT INTO a VALUES (1, NULL), (2, 7);
         INSERT INTO b VALUES (1, NULL), (2, 7);",
    )
    .unwrap();

    // Two nulls are not equal to each other, so only the sevens pair up.
    let found = rows(&mut db, "SELECT a.id, b.id FROM a JOIN b ON a.k = b.k");
    assert_eq!(found.len(), 1);
    assert_eq!(found[0][0], Value::Int(2));
}

#[test]
fn a_star_over_a_join_returns_both_sides() {
    let (_dir, mut db) = open();
    weighings(&mut db);

    let outcome = db
        .query("SELECT * FROM gado g JOIN pesagem p ON p.gado_id = g.id")
        .unwrap();
    let Outcome::Rows { columns, rows } = outcome else {
        panic!()
    };
    assert_eq!(columns.len(), 7, "four columns plus three");
    assert_eq!(rows[0].len(), 7);
}

#[test]
fn an_ambiguous_column_is_refused_rather_than_guessed() {
    let (_dir, mut db) = open();
    weighings(&mut db);

    // `id` names a column in both tables.
    assert!(db
        .query("SELECT id FROM gado g JOIN pesagem p ON p.gado_id = g.id")
        .is_err());
    // Qualified, it is unambiguous again.
    assert!(db
        .query("SELECT g.id FROM gado g JOIN pesagem p ON p.gado_id = g.id")
        .is_ok());
    // And a qualifier nobody declared is an error too.
    assert!(db
        .query("SELECT x.id FROM gado g JOIN pesagem p ON p.gado_id = g.id")
        .is_err());
}

#[test]
fn three_tables_join_left_deep() {
    let (_dir, mut db) = open();
    db.execute(
        "CREATE TABLE a (id INTEGER PRIMARY KEY, n INTEGER);
         CREATE TABLE b (id INTEGER PRIMARY KEY, a_id INTEGER);
         CREATE TABLE c (id INTEGER PRIMARY KEY, b_id INTEGER);
         INSERT INTO a VALUES (1, 10), (2, 20);
         INSERT INTO b VALUES (1, 1), (2, 2);
         INSERT INTO c VALUES (1, 1), (2, 1), (3, 2);",
    )
    .unwrap();

    let found = rows(
        &mut db,
        "SELECT a.n, c.id FROM a JOIN b ON b.a_id = a.id JOIN c ON c.b_id = b.id ORDER BY c.id",
    );
    assert_eq!(found.len(), 3);
    assert_eq!(found[0][0], Value::Int(10));
    assert_eq!(found[2][0], Value::Int(20));
}

// -- secondary indexes -----------------------------------------------------

#[test]
fn an_index_turns_an_equality_into_a_lookup() {
    let (_dir, mut db) = open();
    herd(&mut db);

    // Without an index the only way through is a scan.
    let plan = plan_of(&mut db, "SELECT * FROM gado WHERE brinco = 'BR-0003'");
    assert!(plan.contains("SeqScan"), "{plan}");

    db.query("CREATE INDEX idx_brinco ON gado (brinco)")
        .unwrap();

    let plan = plan_of(&mut db, "SELECT * FROM gado WHERE brinco = 'BR-0003'");
    assert!(plan.contains("IndexScan gado using idx_brinco"), "{plan}");
    assert!(!plan.contains("SeqScan"), "{plan}");

    let found = rows(&mut db, "SELECT id FROM gado WHERE brinco = 'BR-0003'");
    assert_eq!(found.len(), 1);
    assert_eq!(found[0][0], Value::Int(3));
}

#[test]
fn an_index_is_built_over_the_rows_already_there_and_kept_up_to_date() {
    let (_dir, mut db) = open();
    db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, cor TEXT)")
        .unwrap();
    db.execute("INSERT INTO t VALUES (1, 'preto'), (2, 'branco'), (3, 'preto')")
        .unwrap();

    assert_eq!(
        db.query("CREATE INDEX idx_cor ON t (cor)").unwrap(),
        Outcome::Affected(3),
        "the index is built over what is already there"
    );
    assert_eq!(
        rows(&mut db, "SELECT id FROM t WHERE cor = 'preto'").len(),
        2
    );

    // And every later write keeps it in step.
    db.execute("INSERT INTO t VALUES (4, 'preto')").unwrap();
    assert_eq!(
        rows(&mut db, "SELECT id FROM t WHERE cor = 'preto'").len(),
        3
    );

    db.query("UPDATE t SET cor = 'branco' WHERE id = 1")
        .unwrap();
    assert_eq!(
        rows(&mut db, "SELECT id FROM t WHERE cor = 'preto'").len(),
        2
    );
    assert_eq!(
        rows(&mut db, "SELECT id FROM t WHERE cor = 'branco'").len(),
        2
    );

    db.query("DELETE FROM t WHERE id = 3").unwrap();
    assert_eq!(
        rows(&mut db, "SELECT id FROM t WHERE cor = 'preto'").len(),
        1
    );
}

#[test]
fn an_index_and_a_scan_agree() {
    let (_dir, mut db) = open();
    db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, n INTEGER)")
        .unwrap();
    db.execute("BEGIN").unwrap();
    for id in 1..=400i64 {
        db.execute(&format!("INSERT INTO t VALUES ({id}, {})", id % 7))
            .unwrap();
    }
    db.execute("COMMIT").unwrap();

    let scanned = rows(&mut db, "SELECT id FROM t WHERE n = 3 ORDER BY id");
    db.query("CREATE INDEX idx_n ON t (n)").unwrap();
    let looked_up = rows(&mut db, "SELECT id FROM t WHERE n = 3 ORDER BY id");

    assert_eq!(scanned, looked_up);
    assert!(!scanned.is_empty());
}

#[test]
fn a_unique_index_refuses_a_second_copy() {
    let (_dir, mut db) = open();
    db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, cpf TEXT UNIQUE)")
        .unwrap();
    db.execute("INSERT INTO t VALUES (1, '111')").unwrap();

    assert!(db.query("INSERT INTO t VALUES (2, '111')").is_err());
    assert!(db.query("INSERT INTO t VALUES (2, '222')").is_ok());
    assert!(db.query("UPDATE t SET cpf = '111' WHERE id = 2").is_err());

    // A null is not equal to anything, not even another null, so any number of
    // them fit in a unique index.
    assert!(db.query("INSERT INTO t VALUES (3, NULL)").is_ok());
    assert!(db.query("INSERT INTO t VALUES (4, NULL)").is_ok());
    assert_eq!(rows(&mut db, "SELECT id FROM t").len(), 4);
}

#[test]
fn building_a_unique_index_over_duplicates_is_refused() {
    let (_dir, mut db) = open();
    db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, cor TEXT)")
        .unwrap();
    db.execute("INSERT INTO t VALUES (1, 'preto'), (2, 'preto')")
        .unwrap();
    assert!(db.query("CREATE UNIQUE INDEX i ON t (cor)").is_err());
}

#[test]
fn indexes_survive_a_crash_and_a_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("indexed.lastro");

    {
        let mut db = Database::open(&path).unwrap();
        db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, cor TEXT)")
            .unwrap();
        db.execute("BEGIN").unwrap();
        for id in 1..=300i64 {
            db.execute(&format!("INSERT INTO t VALUES ({id}, 'c{}')", id % 10))
                .unwrap();
        }
        db.execute("COMMIT").unwrap();
        db.query("CREATE INDEX idx_cor ON t (cor)").unwrap();
    }

    let mut db = Database::open(&path).unwrap();
    let plan = plan_of(&mut db, "SELECT * FROM t WHERE cor = 'c3'");
    assert!(plan.contains("IndexScan"), "the index survived: {plan}");
    assert_eq!(rows(&mut db, "SELECT id FROM t WHERE cor = 'c3'").len(), 30);
}

#[test]
fn an_index_on_a_column_that_does_not_exist_is_refused() {
    let (_dir, mut db) = open();
    herd(&mut db);
    assert!(db.query("CREATE INDEX i ON gado (ausente)").is_err());
    assert!(db.query("CREATE INDEX i ON ausente (brinco)").is_err());
    db.query("CREATE INDEX i ON gado (brinco)").unwrap();
    assert!(
        db.query("CREATE INDEX i ON gado (peso)").is_err(),
        "duplicate name"
    );
}
