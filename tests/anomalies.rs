//! The isolation anomalies, and which of them this engine admits.
//!
//! Two levels, because they answer different questions.
//!
//! The first set drives the visibility rule directly, with the version
//! histories each anomaly's schedule would produce and snapshots taken where
//! the schedule takes them. That is what tests the isolation *level*: it asks
//! what the rule permits, independently of whether this engine can produce the
//! schedule.
//!
//! The second set drives the engine. It cannot interleave two transactions —
//! there is one writer at a time by design, see `docs/en/adr.md`, ADR-003 — so
//! what it checks is that the engine refuses the interleaving rather than
//! mishandling it.
//!
//! Both are needed. A battery that only ran the engine would report "prevented"
//! for anomalies the isolation level does not actually prevent, and would be
//! reporting the concurrency model while appearing to report the isolation
//! level. That distinction is the whole point of this file.

use lastro::sql::{Database, Outcome, Snapshot, Version};
use lastro::storage::page::encoding::Value;

// -- the visibility rule ---------------------------------------------------

#[test]
fn dirty_read_is_prevented() {
    // T10 writes and has not finished. T20 reads.
    let reader = Snapshot {
        own: Some(20),
        xmax: 21,
        active: Some(10),
    };
    assert!(
        !reader.sees(Version::created_by(10)),
        "an unfinished write must not be readable"
    );
    assert!(
        reader.sees(Version { xmin: 5, xmax: 10 }),
        "and neither must its delete be"
    );
}

#[test]
fn a_non_repeatable_read_is_prevented() {
    // T10 begins. T15 then changes the row and finishes. T10 reads again, and
    // has to find what it found the first time.
    let reader = Snapshot {
        own: Some(10),
        xmax: 11,
        active: None,
    };
    let before = Version { xmin: 5, xmax: 15 };
    let after = Version::created_by(15);

    assert!(reader.sees(before), "the version it read the first time");
    assert!(!reader.sees(after), "and not the one that replaced it");
}

#[test]
fn a_phantom_read_is_prevented() {
    // A row inserted after the reader started is a row the reader never sees,
    // however often it repeats the query.
    let reader = Snapshot {
        own: Some(10),
        xmax: 11,
        active: None,
    };
    assert!(!reader.sees(Version::created_by(15)));
    assert!(!reader.sees(Version::created_by(11)));
    assert!(reader.sees(Version::created_by(9)));
}

#[test]
fn write_skew_is_what_the_rule_would_admit() {
    // The rule on its own does not stop write skew, and this says so rather
    // than leaving the impression that it does.
    //
    // Two transactions read the same two rows and each writes a different one.
    // Under the visibility rule alone, neither sees the other's write, so both
    // would go through and an invariant spanning the two rows would break.
    let first = Snapshot {
        own: Some(10),
        xmax: 11,
        active: Some(20),
    };
    let second = Snapshot {
        own: Some(20),
        xmax: 21,
        active: Some(10),
    };

    let ana = Version::created_by(5);
    let bruno = Version::created_by(5);

    assert!(first.sees(ana) && first.sees(bruno), "both read both rows");
    assert!(second.sees(ana) && second.sees(bruno), "and so do both");

    // Each removes a different one, and neither removal is visible to the
    // other. Nothing in the rule objects.
    assert!(
        first.sees(Version { xmin: 5, xmax: 20 }),
        "the other transaction's removal is invisible"
    );
    assert!(
        second.sees(Version { xmin: 5, xmax: 10 }),
        "and so is this one's, to it"
    );
}

// -- the engine ------------------------------------------------------------

fn open() -> (tempfile::TempDir, Database) {
    let dir = tempfile::tempdir().unwrap();
    let db = Database::open(dir.path().join("anomalies.lastro")).unwrap();
    (dir, db)
}

fn count(db: &mut Database, sql: &str) -> usize {
    match db.query(sql).unwrap() {
        Outcome::Rows { rows, .. } => rows.len(),
        other => panic!("{sql} produced {other:?}"),
    }
}

#[test]
fn the_engine_refuses_the_interleaving_rather_than_mishandling_it() {
    // Lost update and write skew both need two transactions writing at once.
    // This engine takes one writer at a time, so the schedule cannot be built
    // at all — and that, not the isolation level, is what prevents them here.
    let (_dir, mut db) = open();
    db.execute("CREATE TABLE plantao (id INTEGER PRIMARY KEY, nome TEXT)")
        .unwrap();

    db.execute("BEGIN").unwrap();
    assert!(
        db.execute("BEGIN").is_err(),
        "a second writer is refused, which is the guarantee"
    );
    db.execute("ROLLBACK").unwrap();
}

#[test]
fn the_write_skew_schedule_run_in_series_keeps_the_invariant() {
    // The classic counterexample, with the two transactions serialized because
    // that is all this engine allows. The business rule is that at least one
    // vet stays on call.
    let (_dir, mut db) = open();
    db.execute(
        "CREATE TABLE plantao (id INTEGER PRIMARY KEY, nome TEXT, ativo BOOLEAN);
         INSERT INTO plantao VALUES (1, 'ana', TRUE), (2, 'bruno', TRUE);",
    )
    .unwrap();

    // First transaction: reads two on call, stands one down.
    db.execute("BEGIN").unwrap();
    assert_eq!(count(&mut db, "SELECT id FROM plantao WHERE ativo"), 2);
    db.query("UPDATE plantao SET ativo = FALSE WHERE nome = 'ana'")
        .unwrap();
    db.execute("COMMIT").unwrap();

    // Second transaction: reads again, and now sees one rather than two, so a
    // rule written against that count would hold.
    db.execute("BEGIN").unwrap();
    assert_eq!(
        count(&mut db, "SELECT id FROM plantao WHERE ativo"),
        1,
        "serialized, the second transaction sees the first's work"
    );
    db.execute("ROLLBACK").unwrap();

    assert_eq!(count(&mut db, "SELECT id FROM plantao WHERE ativo"), 1);
}

#[test]
fn a_reader_inside_a_transaction_is_not_disturbed_by_its_own_later_writes() {
    // Repeatable read, from the engine rather than from the rule: a row this
    // transaction has not touched reads the same however often it is asked for.
    let (_dir, mut db) = open();
    db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, n INTEGER)")
        .unwrap();
    db.execute("INSERT INTO t VALUES (1, 10), (2, 20)").unwrap();

    db.execute("BEGIN").unwrap();
    let before = db.query("SELECT n FROM t WHERE id = 2").unwrap();
    db.query("UPDATE t SET n = 99 WHERE id = 1").unwrap();
    db.query("INSERT INTO t VALUES (3, 30)").unwrap();
    let after = db.query("SELECT n FROM t WHERE id = 2").unwrap();
    assert_eq!(before, after);
    db.execute("COMMIT").unwrap();
}

#[test]
fn nothing_uncommitted_is_ever_readable_after_the_fact() {
    // The engine's version of the dirty read check: whatever an abandoned
    // transaction wrote leaves no trace for anybody who reads afterwards.
    let (_dir, mut db) = open();
    db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, n INTEGER)")
        .unwrap();
    db.execute("INSERT INTO t VALUES (1, 10)").unwrap();

    db.execute("BEGIN").unwrap();
    db.query("UPDATE t SET n = 999 WHERE id = 1").unwrap();
    db.query("INSERT INTO t VALUES (2, 999)").unwrap();
    db.execute("ROLLBACK").unwrap();

    let Outcome::Rows { rows, .. } = db.query("SELECT id, n FROM t").unwrap() else {
        panic!()
    };
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][1], Value::Int(10));
}
