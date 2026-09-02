# Bug diary

[Português](POSTMORTEM.md) · **English**

The [README](README.en.md) says what `lastro` does when it works. This document is about the
times it did not.

It is here because it is the part that taught the most. A database is easy to write and hard to
believe: almost every serious defect in this project passed the tests that existed, returned no
error at all, and only surfaced when something external — a fuzzer, a corpus, a different
operating system — asked a question I had not thought to ask.

Each entry below is a real commit. The symptom comes before the cause, in the order it appeared.

---

## 1 · The log restarted from zero after a checkpoint

`17e553f` — `src/wal/`

**The symptom.** None. That is what makes this the worst one here.

**The cause.** After a checkpoint the WAL is truncated — its records are already applied to the
data file and serve no further purpose. But the numbering restarted from zero along with the
file.

The LSN is not a decorative counter. Every page on disk carries the LSN of the last change that
touched it, and redo uses that comparison to decide whether a log record is already applied: if
the page's LSN is higher, skip it. A log renumbered from zero looks **older than every page on
disk**. Recovery reads the whole log, concludes each record has already been applied, and skips
all of it — silently, no error, no warning.

The result is the worst failure a database can have: a committed transaction, with `COMMIT`
already returned to the client, vanishes after a crash. And recovery reports success.

**The fix.** The numbering continues across the truncation, and where it continues from is now
durable — `last_checkpoint_lsn` on the metadata page, synced.

An ordering mistake I had made for the same reason came out with it: only once the log is empty
is it safe to write freelist headers over the pages committed transactions gave up. Before that,
something is still waiting to be replayed on top of them.

**What stayed.** Durable state and in-memory state have to be checked separately. The crash
fuzzer only catches this because it genuinely reopens the database at every cut, rather than
inspecting structures that are already loaded.

---

## 2 · `ORDER BY 1` sorted by nothing

`bc257e0` — `src/sql/plan.rs`

**The symptom.** Every value correct, the order wrong, and no error anywhere.

**The cause.** `ORDER BY 1` means "the first output column". The planner read it literally: sort
by the number one. Sorting by a constant does nothing, so rows came back in whatever order the
scan produced. Which is sometimes the right order — worse than being always wrong, because it
disappears from any test done by hand.

**How it surfaced.** Not through a test of mine. Through SQLite's own `sqllogictest` corpus,
which uses the ordinal form in **43 places**. None of my parser tests did, because I write
`ORDER BY name`, and I was the one writing the tests.

**The fix.** `bind_order_key` resolves an integer literal against the projection's columns, and
an out-of-range ordinal is an error rather than silence:

```
ORDER BY 7 names an output column, and there are 3
```

**What stayed.** The reason to run someone else's corpus is not coverage. It is that the corpus
does not share my blind spots. My tests checked what I had thought to implement, and that
intersection is exactly where no bug lives.

---

## 3 · Unstable `pread` in the browser build

`5c97a76` — `src/storage/pager.rs`

**The symptom.** The `wasm32-wasip1` build did not compile on the stable toolchain.

**The cause.** `std::os::wasi::fs::FileExt` — positioned I/O, reading at an offset without moving
the file cursor — exists on WASI but sits behind an unstable feature. The Unix and Windows builds
use `pread`/`pwrite` from their respective extensions; the WASI one had nowhere to go.

**The fix.** Under WASI, seek and then transfer.

That trades correctness for portability rather than being equivalent to it, and the reason it
holds here is written next to it: the browser runs **one thread**, and every call seeks
immediately before it transfers. No other caller can move the cursor in between. On a threaded
host it would be a race — which is precisely why the other two platforms do not do it this way.

**What stayed.** Correctness that depends on the platform needs its argument written beside the
code, not outside it. Without the comment the pattern gets copied somewhere it does not hold, and
the race that shows up has nothing to explain it.

---

## 4 · An accent in a comment corrupted memory

`e1603fa` — `src/bin/lastro-cli.rs`, `web/app.js`

**The symptom.** The engine complaining about a column named `C` that nobody had written.

**The cause.** The browser demo passed SQL as a command-line argument. The WASI shim sizes the
argument buffer using JavaScript's `arg.length`, which counts **UTF-16 code units**, and then
fills that buffer with the string's **UTF-8 bytes**.

While everything is ASCII the two numbers agree and nothing happens. A single accented character
— in a comment, even — is one UTF-16 unit and two UTF-8 bytes. The buffer comes up one byte
short, the write runs past the end, and corrupts whatever sits next to it in memory. The phantom
`C` was another argument, overwritten.

**The fix.** `lastro-cli sql <file> -` reads the statements from standard input, and the demo
uses that. A file descriptor carries bytes and has no such divergence. It is also the ordinary
convention, and the only way to hand the tool a script that does not fit in an argument.

**What stayed.** This was not my bug — it was the shim's. But the error message pointed at my
parser, and that is where I looked first. When a symptom makes no sense at all in the layer where
it appears, the layer being looked at is the wrong one.

---

## 5 · An invariant the spec asserted and `proptest` knocked down — twice

`docs/en/04-btree.md`

Not a bug in the code. A mistake in the specification, and worth more than the four above put
together.

The spec asserted an occupancy floor per B+Tree node. Two formulations were tried, and neither
survived:

- **"every node is at least 40% full"** falls because a single cell can occupy a third of a page.
  A balanced split can leave both halves under the floor with nothing wrong at all.
- **"no pair of adjacent siblings fits together in one page"** falls on **insertion**, not
  deletion. When a full node splits down the middle, each half holds half a page, and one of them
  can now fit together with the untouched neighbour beside it — a neighbour it never had to fit
  with before. The minimal case `proptest` shrank to contains no deletion at all.

So the fill factor is **not asserted, it is measured** — `BTree::stats` — and the tests assert
about the measurement.

**What stayed.** A bound that only holds for fixed-size records is not a bound. And when the test
disagrees with the specification, the default hypothesis is not that the test is wrong.

---

## The pattern

Four of the five returned no error at all. Two were found by something someone else wrote —
SQLite's corpus and `proptest`'s shrinker. One existed on a single platform. None of them showed
up running the program by hand.

That is why this repository's proofs take the shape they do, and why the denominator always
travels with the numerator in [08 · Testing](docs/en/08-testing.md): the number that matters is
not how many tests pass, it is how many questions I would not have asked got asked for me.
