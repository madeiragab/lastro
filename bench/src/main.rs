//! lastro against SQLite, on the same workloads and the same durability.
//!
//! The point of this program is not to win. SQLite is twenty-five years of work
//! by people who do this full time, and the documentation says up front that
//! the number will be published showing the loss. The point is to know *where*
//! the time goes, because a ratio without an explanation teaches nothing.
//!
//! It lives in its own crate so that the library keeps the single dependency it
//! claims to have. `cargo test` at the root never compiles SQLite.
//!
//! ```text
//! cargo run --release -p lastro-bench
//! LASTRO_BENCH_ROWS=20000 LASTRO_BENCH_RUNS=5 cargo run --release -p lastro-bench
//! ```
//!
//! # What is held equal
//!
//! * **Durability.** SQLite runs `synchronous = FULL` in WAL mode, which fsyncs
//!   at every commit. That is what lastro does and there is no way to ask it
//!   not to. Measuring against `synchronous = NORMAL` would be comparing a
//!   database that survives power loss against one that does not.
//! * **Cache.** Two mebibytes each: 512 frames of 4 KiB here, `cache_size =
//!   -2048` there.
//! * **The statement path.** lastro has no prepared statements, so every write
//!   re-parses. SQLite is driven the same way, on a string rather than a cached
//!   handle, so the parser cost is on both sides rather than only on one.
//!
//! What is not equal, and flatters lastro, is said out loud in the report.

use std::fmt::Write as _;
use std::time::{Duration, Instant};

use lastro::sql::{Database, Outcome};

// -- knobs -----------------------------------------------------------------

fn env(name: &str, fallback: usize) -> usize {
    std::env::var(name)
        .ok()
        .and_then(|raw| raw.parse().ok())
        .unwrap_or(fallback)
}

/// Frames of 4 KiB in the lastro buffer pool, and the matching SQLite cache.
const CACHE_FRAMES: usize = 512;

// -- a deterministic shuffle ----------------------------------------------

/// A linear congruential generator, so both engines see byte-identical
/// workloads and a rerun reproduces a surprise.
struct Lcg(u64);

impl Lcg {
    fn new(seed: u64) -> Lcg {
        Lcg(seed.wrapping_mul(6364136223846793005).wrapping_add(1))
    }

    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        self.0 >> 11
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

/// A permutation of `0..rows`, for the out-of-order insert.
fn shuffled(rows: usize) -> Vec<usize> {
    let mut keys: Vec<usize> = (0..rows).collect();
    let mut rng = Lcg::new(0x1A5730);
    for i in (1..rows).rev() {
        keys.swap(i, rng.below(i + 1));
    }
    keys
}

// -- the statements both engines run --------------------------------------

fn create() -> &'static str {
    "CREATE TABLE gado (id INTEGER PRIMARY KEY, peso INTEGER, brinco TEXT)"
}

fn insert(key: usize) -> String {
    format!(
        "INSERT INTO gado VALUES ({key}, {}, 'BR{key:07}')",
        300 + (key % 400)
    )
}

fn lookup(key: usize) -> String {
    format!("SELECT peso FROM gado WHERE id = {key}")
}

fn range(from: usize, span: usize) -> String {
    let to = from + span;
    format!("SELECT peso FROM gado WHERE id >= {from} AND id < {to}")
}

fn update(key: usize) -> String {
    format!("UPDATE gado SET peso = {} WHERE id = {key}", 700 + key % 50)
}

// -- the two engines behind one trait -------------------------------------

/// Whatever a workload can be run against.
trait Engine {
    /// Runs a statement whose result is not worth counting.
    fn run(&mut self, sql: &str);
    /// Runs a query and returns how many rows came back, so that a read
    /// workload cannot quietly become a no-op.
    fn rows(&mut self, sql: &str) -> usize;

    fn begin(&mut self) {
        self.run("BEGIN");
    }

    fn commit(&mut self) {
        self.run("COMMIT");
    }
}

struct Lastro {
    db: Database,
    _dir: tempfile::TempDir,
}

impl Lastro {
    fn open() -> Lastro {
        let dir = tempfile::tempdir().expect("temp dir");
        let db = Database::with_capacity(dir.path().join("bench.lastro"), CACHE_FRAMES)
            .expect("open lastro");
        Lastro { db, _dir: dir }
    }
}

impl Engine for Lastro {
    fn run(&mut self, sql: &str) {
        self.db
            .execute(sql)
            .unwrap_or_else(|e| panic!("{sql}: {e}"));
    }

    fn rows(&mut self, sql: &str) -> usize {
        match self.db.query(sql).unwrap_or_else(|e| panic!("{sql}: {e}")) {
            Outcome::Rows { rows, .. } => rows.len(),
            other => panic!("{sql} produced {other:?}"),
        }
    }
}

struct Sqlite {
    conn: rusqlite::Connection,
    _dir: tempfile::TempDir,
}

impl Sqlite {
    fn open() -> Sqlite {
        let dir = tempfile::tempdir().expect("temp dir");
        let conn = rusqlite::Connection::open(dir.path().join("bench.sqlite")).expect("open");
        // Held equal with lastro: a write-ahead log, fsynced at every commit,
        // and two mebibytes of cache. `journal_mode` answers with the mode it
        // settled on, so it is a query rather than a statement.
        let mode: String = conn
            .query_row("PRAGMA journal_mode = WAL", [], |row| row.get(0))
            .expect("journal mode");
        assert_eq!(mode, "wal", "SQLite would not take the write-ahead log");
        conn.execute_batch("PRAGMA synchronous = FULL; PRAGMA cache_size = -2048;")
            .expect("pragmas");
        Sqlite { conn, _dir: dir }
    }
}

impl Engine for Sqlite {
    fn run(&mut self, sql: &str) {
        // `execute_batch` parses the text every time, which is what lastro has
        // to do for want of prepared statements.
        self.conn
            .execute_batch(sql)
            .unwrap_or_else(|e| panic!("{sql}: {e}"));
    }

    fn rows(&mut self, sql: &str) -> usize {
        let mut statement = self.conn.prepare(sql).expect("prepare");
        let mut found = 0;
        let mut cursor = statement.query([]).expect("query");
        while let Some(row) = cursor.next().expect("row") {
            let _: i64 = row.get(0).expect("column");
            found += 1;
        }
        found
    }
}

// -- the workloads ---------------------------------------------------------

/// The work a measurement performs, once.
type Body = Box<dyn Fn(&mut dyn Engine)>;

/// One measurement: a name, how many units of work it does, whether it needs a
/// table in place first, and the work.
struct Workload {
    name: &'static str,
    unit: &'static str,
    ops: usize,
    /// Whether a filled table is set up before the timer starts. Loading inside
    /// the timer and subtracting it afterwards would report the difference of
    /// two large numbers, which is how a benchmark ends up with negative time.
    needs_rows: bool,
    body: Body,
}

fn workloads(rows: usize) -> Vec<Workload> {
    let random = shuffled(rows);
    let probes: Vec<usize> = {
        let mut rng = Lcg::new(0x9E3779B9);
        (0..rows).map(|_| rng.below(rows)).collect()
    };
    let touched: Vec<usize> = probes.iter().take((rows / 4).max(1)).copied().collect();
    let span = (rows / 10).max(1);
    let commits = (rows / 20).max(1);

    vec![
        Workload {
            name: "insert, in key order",
            unit: "row",
            ops: rows,
            needs_rows: false,
            body: Box::new(move |engine| {
                engine.begin();
                for key in 0..rows {
                    engine.run(&insert(key));
                }
                engine.commit();
            }),
        },
        Workload {
            name: "insert, random order",
            unit: "row",
            ops: rows,
            needs_rows: false,
            body: Box::new(move |engine| {
                engine.begin();
                for key in &random {
                    engine.run(&insert(*key));
                }
                engine.commit();
            }),
        },
        Workload {
            name: "lookup by primary key",
            unit: "lookup",
            ops: rows,
            needs_rows: true,
            body: Box::new(move |engine| {
                let mut found = 0;
                for key in &probes {
                    found += engine.rows(&lookup(*key));
                }
                assert_eq!(found, probes.len(), "every probe should hit a row");
            }),
        },
        Workload {
            name: "range scan, a tenth of the table",
            unit: "scan",
            ops: 20,
            needs_rows: true,
            body: Box::new(move |engine| {
                let mut rng = Lcg::new(0xC0FFEE);
                for _ in 0..20 {
                    let from = rng.below(rows - span);
                    assert_eq!(engine.rows(&range(from, span)), span);
                }
            }),
        },
        Workload {
            name: "update by primary key",
            unit: "update",
            ops: touched.len(),
            needs_rows: true,
            body: Box::new(move |engine| {
                engine.begin();
                for key in &touched {
                    engine.run(&update(*key));
                }
                engine.commit();
            }),
        },
        // The one that measures fsync rather than the engine: every row is its
        // own transaction, so every row costs a durable log write.
        Workload {
            name: "one row per transaction",
            unit: "commit",
            ops: commits,
            needs_rows: false,
            body: Box::new(move |engine| {
                for key in 0..commits {
                    engine.run(&insert(key));
                }
            }),
        },
    ]
}

/// Fills a table, outside any timer.
fn load(engine: &mut dyn Engine, rows: usize) {
    engine.begin();
    for key in 0..rows {
        engine.run(&insert(key));
    }
    engine.commit();
}

// -- measuring -------------------------------------------------------------

/// Runs a workload `runs` times, each against a database opened for it, and
/// returns the durations sorted.
fn measure<E: Engine, F: Fn() -> E>(
    open: F,
    work: &Workload,
    rows: usize,
    runs: usize,
) -> Vec<Duration> {
    let mut times = Vec::with_capacity(runs);
    for _ in 0..runs {
        let mut engine = open();
        engine.run(create());
        if work.needs_rows {
            load(&mut engine, rows);
        }
        let start = Instant::now();
        (work.body)(&mut engine);
        times.push(start.elapsed());
    }
    times.sort();
    times
}

/// The middle value. The mean would be pulled around by whatever else the
/// machine decided to do during a run.
fn median(times: &[Duration]) -> Duration {
    times[times.len() / 2]
}

/// The spread between the fastest and slowest run, as a share of the median. A
/// large one means the number is not to be trusted, and printing it is more
/// useful than hiding it.
fn spread(times: &[Duration]) -> f64 {
    let mid = median(times).as_secs_f64();
    if mid == 0.0 {
        return 0.0;
    }
    (times[times.len() - 1].as_secs_f64() - times[0].as_secs_f64()) / mid * 100.0
}

fn per_op(total: Duration, ops: usize) -> f64 {
    total.as_secs_f64() * 1e6 / ops as f64
}

fn main() {
    let rows = env("LASTRO_BENCH_ROWS", 10_000);
    let runs = env("LASTRO_BENCH_RUNS", 5);

    println!("lastro against SQLite {}", rusqlite::version());
    println!("{rows} rows, {runs} runs each, median reported.");
    println!("Both in write-ahead logging with an fsync at every commit, 2 MiB of cache.\n");

    let mut table = String::new();
    let _ = writeln!(table, "| Workload | lastro | SQLite | ratio | spread |");
    let _ = writeln!(table, "|---|---|---|---|---|");

    for work in workloads(rows).iter() {
        let mine = measure(Lastro::open, work, rows, runs);
        let theirs = measure(Sqlite::open, work, rows, runs);

        let a = per_op(median(&mine), work.ops);
        let b = per_op(median(&theirs), work.ops);
        let ratio = if b > 0.0 { a / b } else { f64::NAN };

        let _ = writeln!(
            table,
            "| {} | {a:.1} µs/{} | {b:.1} µs/{} | {ratio:.1}× | ±{:.0}% / ±{:.0}% |",
            work.name,
            work.unit,
            work.unit,
            spread(&mine),
            spread(&theirs),
        );
    }

    println!("{table}");

    // The plans behind the read numbers. A ratio says nothing about why; this
    // says whether a lookup was a seek or a walk of the whole table.
    println!("What lastro decided to do:\n");
    let mut engine = Lastro::open();
    engine.run(create());
    engine.run(&insert(1));
    for sql in [lookup(1), range(0, 100), update(1)] {
        match engine.db.query(&format!("EXPLAIN {sql}")) {
            Ok(Outcome::Plan(plan)) => println!("{sql}\n{plan}"),
            other => println!("{sql}\n  no plan: {other:?}\n"),
        }
    }
}
