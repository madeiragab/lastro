//! The SQLite project's own test files, run against lastro.
//!
//! Every other test in this repository was written by the same person who wrote
//! the code being tested, which is the weakest kind of evidence there is: it
//! only proves the engine does what its author expected. `sqllogictest` is a
//! corpus written by somebody else, for a different database, years before this
//! one existed. It has no idea what lastro finds easy.
//!
//! The corpus is not vendored — it is megabytes of generated SQL, and a
//! repository that carries its own evidence around is a repository whose
//! evidence can be edited. CI fetches it and points this test at it:
//!
//! ```text
//! LASTRO_SQLLOGIC_DIR=/path/to/test cargo test --test sqllogic -- --nocapture
//! ```
//!
//! Without that variable the test reports why it did nothing and passes, so a
//! plain `cargo test` needs no network.
//!
//! # The denominator ships with the numerator
//!
//! `docs/en/08-testing.md` commits to this and it is the whole reason the
//! report looks the way it does. lastro implements a fraction of SQLite's SQL.
//! Most assertions in this corpus use something it does not have — scalar
//! subqueries, `CASE`, aggregates — and those are **not** failures, they are
//! absences. But quoting a pass rate over only the assertions that ran, without
//! saying how few that was, would be a statistical lie. So the report carries,
//! every time:
//!
//! * files considered, and how many ran to the end
//! * assertions attempted, passed, failed
//! * assertions skipped for a missing feature, with the reasons ranked
//!
//! A wrong answer fails the test. A missing feature does not.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use lastro::error::Error;
use lastro::sql::{Database, Outcome};
use lastro::storage::page::encoding::Value;

// -- MD5 -------------------------------------------------------------------
//
// Most expected results in the corpus are given as `N values hashing to <md5>`
// rather than as the values themselves, so reading the corpus at all requires
// MD5. It is written here for the same reason CRC32C is written in
// `src/storage/checksum.rs`: a dependency for eighty lines of arithmetic that
// has not changed since 1992 buys nothing.
//
// MD5 is broken for every purpose that involves an adversary. Nothing here has
// one — it is a fingerprint of a list of strings, chosen by the corpus twenty
// years ago, and the only thing it has to do is match.

mod md5 {
    use std::fmt::Write as _;

    const K: [u32; 64] = [
        0xd76aa478, 0xe8c7b756, 0x242070db, 0xc1bdceee, 0xf57c0faf, 0x4787c62a, 0xa8304613,
        0xfd469501, 0x698098d8, 0x8b44f7af, 0xffff5bb1, 0x895cd7be, 0x6b901122, 0xfd987193,
        0xa679438e, 0x49b40821, 0xf61e2562, 0xc040b340, 0x265e5a51, 0xe9b6c7aa, 0xd62f105d,
        0x02441453, 0xd8a1e681, 0xe7d3fbc8, 0x21e1cde6, 0xc33707d6, 0xf4d50d87, 0x455a14ed,
        0xa9e3e905, 0xfcefa3f8, 0x676f02d9, 0x8d2a4c8a, 0xfffa3942, 0x8771f681, 0x6d9d6122,
        0xfde5380c, 0xa4beea44, 0x4bdecfa9, 0xf6bb4b60, 0xbebfbc70, 0x289b7ec6, 0xeaa127fa,
        0xd4ef3085, 0x04881d05, 0xd9d4d039, 0xe6db99e5, 0x1fa27cf8, 0xc4ac5665, 0xf4292244,
        0x432aff97, 0xab9423a7, 0xfc93a039, 0x655b59c3, 0x8f0ccc92, 0xffeff47d, 0x85845dd1,
        0x6fa87e4f, 0xfe2ce6e0, 0xa3014314, 0x4e0811a1, 0xf7537e82, 0xbd3af235, 0x2ad7d2bb,
        0xeb86d391,
    ];

    const SHIFT: [u32; 64] = [
        7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 5, 9, 14, 20, 5, 9, 14, 20, 5,
        9, 14, 20, 5, 9, 14, 20, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 6, 10,
        15, 21, 6, 10, 15, 21, 6, 10, 15, 21, 6, 10, 15, 21,
    ];

    /// The digest of `message`, as the lowercase hex the corpus writes.
    pub fn hex(message: &[u8]) -> String {
        let mut state: [u32; 4] = [0x67452301, 0xefcdab89, 0x98badcfe, 0x10325476];

        // The padding: a one bit, zeroes up to eight short of a block, then the
        // length in bits, little endian.
        let mut padded = message.to_vec();
        padded.push(0x80);
        while padded.len() % 64 != 56 {
            padded.push(0);
        }
        padded.extend_from_slice(&(message.len() as u64 * 8).to_le_bytes());

        for block in padded.chunks_exact(64) {
            let mut words = [0u32; 16];
            for (index, word) in words.iter_mut().enumerate() {
                let at = index * 4;
                *word =
                    u32::from_le_bytes([block[at], block[at + 1], block[at + 2], block[at + 3]]);
            }

            let [mut a, mut b, mut c, mut d] = state;
            for i in 0..64 {
                let (mixed, take) = match i / 16 {
                    0 => ((b & c) | (!b & d), i),
                    1 => ((d & b) | (!d & c), (5 * i + 1) % 16),
                    2 => (b ^ c ^ d, (3 * i + 5) % 16),
                    _ => (c ^ (b | !d), (7 * i) % 16),
                };
                let sum = a
                    .wrapping_add(mixed)
                    .wrapping_add(K[i])
                    .wrapping_add(words[take]);
                a = d;
                d = c;
                c = b;
                b = b.wrapping_add(sum.rotate_left(SHIFT[i]));
            }

            state[0] = state[0].wrapping_add(a);
            state[1] = state[1].wrapping_add(b);
            state[2] = state[2].wrapping_add(c);
            state[3] = state[3].wrapping_add(d);
        }

        let mut out = String::with_capacity(32);
        for word in state {
            for byte in word.to_le_bytes() {
                let _ = write!(out, "{byte:02x}");
            }
        }
        out
    }
}

#[test]
fn md5_matches_the_published_vectors() {
    // RFC 1321, appendix A.5. If this fails, nothing below means anything.
    assert_eq!(md5::hex(b""), "d41d8cd98f00b204e9800998ecf8427e");
    assert_eq!(md5::hex(b"abc"), "900150983cd24fb0d6963f7d28e17f72");
    assert_eq!(
        md5::hex(b"message digest"),
        "f96b697d7cb7938d525a2f31aaf161d0"
    );
    assert_eq!(
        md5::hex(b"abcdefghijklmnopqrstuvwxyz"),
        "c3fcd3d76192e4007dfb496cca67e13b"
    );
    // Two blocks, so the chunking is exercised too.
    assert_eq!(
        md5::hex(
            b"12345678901234567890123456789012345678901234567890123456789012345678901234567890"
        ),
        "57edf4a22be3c955ac49da2e2107b67a"
    );
}

// -- the file format -------------------------------------------------------

/// How a query's rows are put in order before they are compared.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Sort {
    /// Compare in the order the engine produced them.
    None,
    /// Sort whole rows.
    Rows,
    /// Sort every value independently of its row.
    Values,
}

/// What the corpus expects a query to produce.
#[derive(Debug, Clone)]
enum Expected {
    /// The values themselves, one per line.
    Values(Vec<String>),
    /// A count and the MD5 of the values, which is how the corpus writes
    /// anything longer than the file's `hash-threshold`.
    Hash { count: usize, digest: String },
}

/// One assertion from a `.test` file.
#[derive(Debug, Clone)]
enum Record {
    /// A statement that has to succeed.
    StatementOk(String),
    /// A statement that has to fail.
    StatementError(String),
    /// A query, its column types, and what it should produce.
    Query {
        sql: String,
        types: String,
        sort: Sort,
        expect: Expected,
    },
    /// Stop reading this file here.
    Halt,
}

/// Reads a `.test` file into records, dropping what is aimed at another engine.
///
/// `skipif <engine>` and `onlyif <engine>` name the database a record is for.
/// lastro is in nobody's list, so `onlyif` records are somebody else's and
/// `skipif` records are ours by default.
fn parse(text: &str) -> (Vec<Record>, usize) {
    let mut records = Vec::new();
    let mut foreign = 0;
    let mut lines = text.lines().peekable();

    while let Some(line) = lines.next() {
        let line = line.trim_end();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }

        // The conditional prefixes apply to the record that follows.
        let mut skip = false;
        let mut header = line;
        loop {
            let mut word = header.split_whitespace();
            match (word.next(), word.next()) {
                (Some("skipif"), Some(engine)) => skip |= engine == "lastro",
                (Some("onlyif"), Some(_)) => skip = true,
                _ => break,
            }
            match lines.next() {
                Some(next) => header = next.trim_end(),
                None => return (records, foreign),
            }
        }
        if skip {
            foreign += 1;
        }

        let mut word = header.split_whitespace();
        let record = match word.next() {
            Some("halt") => Some(Record::Halt),
            Some("hash-threshold") | Some("control") => None,
            Some("statement") => {
                let outcome = word.next().unwrap_or("ok");
                let sql = take_sql(&mut lines);
                match outcome {
                    "ok" => Some(Record::StatementOk(sql)),
                    _ => Some(Record::StatementError(sql)),
                }
            }
            Some("query") => {
                let types = word.next().unwrap_or("I").to_string();
                let sort = match word.next() {
                    Some("rowsort") => Sort::Rows,
                    Some("valuesort") => Sort::Values,
                    _ => Sort::None,
                };
                let sql = take_sql(&mut lines);
                let expect = take_expected(&mut lines);
                Some(Record::Query {
                    sql,
                    types,
                    sort,
                    expect,
                })
            }
            _ => None,
        };

        match record {
            Some(record) if !skip => records.push(record),
            _ => {}
        }
    }

    (records, foreign)
}

/// The SQL of a record: every line up to `----` or a blank line.
fn take_sql<'a, I: Iterator<Item = &'a str>>(lines: &mut std::iter::Peekable<I>) -> String {
    let mut sql = String::new();
    while let Some(line) = lines.peek() {
        let line = line.trim_end();
        if line.is_empty() || line == "----" {
            break;
        }
        if !sql.is_empty() {
            sql.push('\n');
        }
        sql.push_str(line);
        lines.next();
    }
    sql
}

/// The expected results, which start after the `----` separator.
fn take_expected<'a, I: Iterator<Item = &'a str>>(lines: &mut std::iter::Peekable<I>) -> Expected {
    // Step over the separator, if the record has one. A query with no results
    // at all has none.
    if lines.peek().map(|line| line.trim_end()) == Some("----") {
        lines.next();
    }

    let mut values = Vec::new();
    while let Some(line) = lines.peek() {
        let line = line.trim_end();
        if line.is_empty() {
            break;
        }
        values.push(line.to_string());
        lines.next();
    }

    // `30 values hashing to 3c13dee48d9356ae19af2515e05e6b54`
    if values.len() == 1 {
        let words: Vec<&str> = values[0].split_whitespace().collect();
        if words.len() == 5 && words[1] == "values" && words[2] == "hashing" {
            if let Ok(count) = words[0].parse() {
                return Expected::Hash {
                    count,
                    digest: words[4].to_string(),
                };
            }
        }
    }

    Expected::Values(values)
}

// -- turning rows into the corpus's strings --------------------------------

/// Renders one value the way the corpus's reference implementation does.
///
/// The rules are not negotiable and are the difference between a run that means
/// something and a run where everything fails for the same silly reason:
/// integers plain, reals to three places, empty text as `(empty)`, anything
/// unprintable as `@`, null as `NULL`.
fn render(value: &Value, kind: char) -> String {
    if matches!(value, Value::Null) {
        return "NULL".to_string();
    }
    match kind {
        'I' => match value {
            Value::Int(n) => n.to_string(),
            Value::Bool(b) => i64::from(*b).to_string(),
            Value::Real(r) => (*r as i64).to_string(),
            Value::Text(t) => t.trim().parse::<i64>().unwrap_or(0).to_string(),
            _ => "0".to_string(),
        },
        'R' => match value {
            Value::Real(r) => format!("{r:.3}"),
            Value::Int(n) => format!("{:.3}", *n as f64),
            Value::Bool(b) => format!("{:.3}", f64::from(*b)),
            Value::Text(t) => format!("{:.3}", t.trim().parse::<f64>().unwrap_or(0.0)),
            _ => "0.000".to_string(),
        },
        // 'T' and anything else the corpus writes.
        _ => {
            let text = match value {
                Value::Text(t) => t.clone(),
                Value::Int(n) => n.to_string(),
                Value::Real(r) => format!("{r:.3}"),
                Value::Bool(b) => i64::from(*b).to_string(),
                Value::Blob(bytes) => String::from_utf8_lossy(bytes).to_string(),
                Value::Null => unreachable!("handled above"),
            };
            if text.is_empty() {
                return "(empty)".to_string();
            }
            text.chars()
                .map(|c| if (' '..='~').contains(&c) { c } else { '@' })
                .collect()
        }
    }
}

/// The flat list of value strings a query produced, ordered as the record asks.
fn flatten(rows: &[Vec<Value>], types: &str, sort: Sort) -> Vec<String> {
    let kinds: Vec<char> = types.chars().collect();
    let mut rendered: Vec<Vec<String>> = rows
        .iter()
        .map(|row| {
            row.iter()
                .enumerate()
                .map(|(index, value)| render(value, *kinds.get(index).unwrap_or(&'T')))
                .collect()
        })
        .collect();

    match sort {
        Sort::None => {}
        // The reference implementation joins each row into one buffer and sorts
        // those with strcmp, so a row of ["10", "2"] sorts by "10 2".
        Sort::Rows => rendered.sort_by_key(|row| row.join(" ")),
        Sort::Values => {
            let mut values: Vec<String> = rendered.into_iter().flatten().collect();
            values.sort();
            return values;
        }
    }
    rendered.into_iter().flatten().collect()
}

/// Whether a produced list matches what the record expects.
fn matches(produced: &[String], expect: &Expected) -> bool {
    match expect {
        Expected::Values(wanted) => produced == wanted.as_slice(),
        Expected::Hash { count, digest } => {
            if produced.len() != *count {
                return false;
            }
            let mut buffer = String::new();
            for value in produced {
                buffer.push_str(value);
                buffer.push('\n');
            }
            &md5::hex(buffer.as_bytes()) == digest
        }
    }
}

// -- the report ------------------------------------------------------------

/// Everything the run has to publish. The absences are counted as carefully as
/// the passes, because a pass rate over an undisclosed subset is not a number.
#[derive(Default)]
struct Report {
    files_seen: usize,
    files_finished: usize,
    /// Files whose setup failed, so nothing after it could be trusted.
    files_abandoned: Vec<(String, String)>,
    /// Records aimed at another database by `onlyif`.
    foreign: usize,
    attempted: usize,
    passed: usize,
    /// Wrong answers. The only category that fails the test.
    wrong: Vec<(String, String)>,
    /// Assertions the engine could not begin, by the reason it gave.
    missing: BTreeMap<String, usize>,
}

impl Report {
    fn skipped(&self) -> usize {
        self.missing.values().sum()
    }

    /// A reason, trimmed to something that aggregates. The messages carry a
    /// position and the offending token; the position is noise across a
    /// thousand statements, the token is the signal.
    fn note_missing(&mut self, error: &Error) {
        let reason = match error {
            Error::Unsupported(what) => format!("unsupported: {what}"),
            Error::Sql { message, .. } => format!("parse: {message}"),
            other => format!("{other}"),
        };
        *self.missing.entry(reason).or_insert(0) += 1;
    }
}

/// Decides whether an error means "lastro has not got this" or "lastro got it
/// wrong". The line is where the failure happened: a statement the front end
/// refused never reached the engine, so it is an absence. Anything the front
/// end accepted and the engine then failed is this project's problem.
fn is_absence(error: &Error) -> bool {
    matches!(error, Error::Unsupported(_) | Error::Sql { .. })
}

fn run_file(path: &Path, report: &mut Report) {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) => {
            report
                .files_abandoned
                .push((name(path), format!("unreadable: {e}")));
            return;
        }
    };
    let (records, foreign) = parse(&text);
    report.foreign += foreign;

    let dir = tempfile::tempdir().expect("temp dir");
    let mut db = match Database::open(dir.path().join("slt.lastro")) {
        Ok(db) => db,
        Err(e) => {
            report
                .files_abandoned
                .push((name(path), format!("could not open a database: {e}")));
            return;
        }
    };

    for record in records {
        match record {
            Record::Halt => break,

            Record::StatementOk(sql) => match db.execute(&sql) {
                Ok(_) => {}
                Err(e) => {
                    // Setup failed, so every later record in this file would be
                    // running against a table that is not there. Reporting
                    // those as failures would inflate the denominator with
                    // consequences of one absence, so the file stops here.
                    if is_absence(&e) {
                        report.note_missing(&e);
                    }
                    report
                        .files_abandoned
                        .push((name(path), format!("{}: {e}", first_line(&sql))));
                    return;
                }
            },

            Record::StatementError(sql) => {
                match db.execute(&sql) {
                    // The corpus wanted an error and got one — but if the error
                    // is that lastro cannot parse the statement, the engine
                    // agreed by accident. That is not a pass.
                    Err(e) if is_absence(&e) => report.note_missing(&e),
                    Err(_) => {
                        report.attempted += 1;
                        report.passed += 1;
                    }
                    Ok(_) => {
                        report.attempted += 1;
                        report.wrong.push((
                            name(path),
                            format!("{}: succeeded, should have failed", first_line(&sql)),
                        ));
                    }
                }
            }

            Record::Query {
                sql,
                types,
                sort,
                expect,
            } => match db.query(&sql) {
                Ok(Outcome::Rows { rows, .. }) => {
                    report.attempted += 1;
                    let produced = flatten(&rows, &types, sort);
                    if matches(&produced, &expect) {
                        report.passed += 1;
                    } else {
                        report
                            .wrong
                            .push((name(path), describe(&sql, &produced, &expect)));
                    }
                }
                Ok(other) => {
                    report.attempted += 1;
                    report.wrong.push((
                        name(path),
                        format!("{}: produced {other:?}, not rows", first_line(&sql)),
                    ));
                }
                Err(e) if is_absence(&e) => report.note_missing(&e),
                Err(e) => {
                    report.attempted += 1;
                    report
                        .wrong
                        .push((name(path), format!("{}: {e}", first_line(&sql))));
                }
            },
        }
    }

    report.files_finished += 1;
}

fn name(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default()
}

fn first_line(sql: &str) -> String {
    let line = sql.lines().next().unwrap_or("").trim();
    if line.len() > 70 {
        format!("{}...", &line[..70])
    } else {
        line.to_string()
    }
}

fn describe(sql: &str, produced: &[String], expect: &Expected) -> String {
    let wanted = match expect {
        Expected::Values(values) if values.len() <= 4 => values.join(", "),
        Expected::Values(values) => format!("{} values", values.len()),
        Expected::Hash { count, digest } => format!("{count} values hashing to {digest}"),
    };
    let got = if produced.len() <= 4 {
        produced.join(", ")
    } else {
        format!("{} values", produced.len())
    };
    format!("{}: wanted {wanted}, got {got}", first_line(sql))
}

/// Every `.test` file under `root`, in a stable order.
fn collect(root: &Path, into: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    let mut here: Vec<PathBuf> = entries.flatten().map(|entry| entry.path()).collect();
    here.sort();
    for path in here {
        if path.is_dir() {
            collect(&path, into);
        } else if path.extension().is_some_and(|e| e == "test") {
            into.push(path);
        }
    }
}

#[test]
fn the_sqlite_corpus() {
    let Ok(root) = std::env::var("LASTRO_SQLLOGIC_DIR") else {
        println!(
            "skipped: set LASTRO_SQLLOGIC_DIR to a directory of sqllogictest .test files.\n\
             The corpus is fetched by CI rather than vendored; see .github/workflows/ci.yml."
        );
        return;
    };

    let mut files = Vec::new();
    collect(Path::new(&root), &mut files);
    assert!(!files.is_empty(), "no .test files under {root}");

    let mut report = Report::default();
    for path in &files {
        report.files_seen += 1;
        run_file(path, &mut report);
    }

    let rate = if report.attempted > 0 {
        report.passed as f64 * 100.0 / report.attempted as f64
    } else {
        0.0
    };
    let coverage = {
        let total = report.attempted + report.skipped();
        if total > 0 {
            report.attempted as f64 * 100.0 / total as f64
        } else {
            0.0
        }
    };

    println!("\n== sqllogictest, SQLite's corpus ==\n");
    println!("| Measure | Count |");
    println!("|---|---|");
    println!("| Files considered | {} |", report.files_seen);
    println!("| Files run to the end | {} |", report.files_finished);
    println!(
        "| Files abandoned at setup | {} |",
        report.files_abandoned.len()
    );
    println!("| Records aimed at another engine | {} |", report.foreign);
    println!("| Assertions attempted | {} |", report.attempted);
    println!("| Passed | {} |", report.passed);
    println!("| Failed | {} |", report.wrong.len());
    println!(
        "| Skipped, feature not implemented | {} |",
        report.skipped()
    );
    println!("\nPass rate over what ran: {rate:.1}%");
    println!(
        "Share of the corpus that could run at all: {coverage:.1}%  \
         ({} attempted of {} reachable assertions)",
        report.attempted,
        report.attempted + report.skipped()
    );

    if !report.missing.is_empty() {
        println!("\nWhat is missing, by how often the corpus asked for it:\n");
        let mut ranked: Vec<(&String, &usize)> = report.missing.iter().collect();
        ranked.sort_by(|a, b| b.1.cmp(a.1));
        for (reason, count) in ranked.iter().take(20) {
            println!("  {count:>6}  {reason}");
        }
        if ranked.len() > 20 {
            println!("  ... and {} more kinds", ranked.len() - 20);
        }
    }

    if !report.files_abandoned.is_empty() {
        println!("\nFiles that stopped at a setup statement:\n");
        for (file, why) in report.files_abandoned.iter().take(20) {
            println!("  {file}: {why}");
        }
    }

    if !report.wrong.is_empty() {
        println!("\nWrong answers:\n");
        for (file, what) in report.wrong.iter().take(40) {
            println!("  {file}: {what}");
        }
        if report.wrong.len() > 40 {
            println!("  ... and {} more", report.wrong.len() - 40);
        }
    }

    assert!(
        report.wrong.is_empty(),
        "{} assertions produced the wrong answer; a missing feature is fine, a wrong answer is not",
        report.wrong.len()
    );
}
