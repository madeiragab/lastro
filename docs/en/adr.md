[Português](../pt/adr.md) · [English](adr.md) · [↑ README](../../README.en.md)

# Architecture decision record

Every decision carries its context, the alternatives considered, and what is lost by choosing.
A decision with no recorded alternative is not a decision, it is an accident.

---

## ADR-001 · Rust

**Status:** accepted

**Context.** A database manipulates raw bytes, manages memory explicitly and needs predictable
latency. The languages I already know — Python and JavaScript — are unsuitable for the hot path.

**Alternatives.**

*C* — the classic language of the domain. SQLite is C. Manual management with no safety net, and
every buffer bug becomes silent corruption. A small market for someone starting out.

*Go* — garbage collected, easy concurrency, shallow curve. The collector introduces pauses, which
in a database show up as tail latency. Strong backend market in Brazil.

*Zig* — an interesting project for this domain, but the language is still unstable and the job
market is essentially nonexistent.

*Rust* — no garbage collector, predictable latency, and a type system that turns most buffer
management bugs into compile errors. `Drop` solves the pin leak structurally. A smaller market
than Go in Brazil, but growing and well paid.

**Decision.** Rust.

**What is lost.** A steep learning curve stacked on an already hard domain — two simultaneous
risks. Mitigation: stage 1 is the pager, the simplest subsystem, deliberately chosen as the place
to pay that curve. Compile times also hurt the iteration loop.

**The side benefit that mattered.** My portfolio is entirely Python and JavaScript. Rust here
signals range, not just preference.

---

## ADR-002 · 4 KB pages

**Status:** accepted

**Context.** The database needs a fixed unit of I/O.

**Alternatives.** 512 B is the traditional sector, too small: low fanout, tall tree.
8 KB is the Postgres default, good for scans, worse for random access.
16 KB is the InnoDB default, even more scan-biased.
A configurable size multiplies the test space with no proportional gain.

**Decision.** 4096 bytes, fixed, stored in the header for future validation.

**Rationale.** It matches the typical block size of ext4, NTFS and APFS, and the physical sector
of modern SSDs. One dirty page is one write, not two. It also matches the memory page size, which
leaves the door open for `mmap` later.

**What is lost.** Pure scan workloads would do better with larger pages. Not the target.

---

## ADR-003 · A single writer

**Status:** accepted

**Context.** Concurrent writers require hierarchical locking, deadlock detection and lock
scheduling. That is a large subsystem.

**Alternatives.** Row-level locking, with a wait-for graph and cycle detection — the full model,
and an entire project. Table-level locking — a middle ground that still needs all the deadlock
machinery. A single writer — serializes writes behind a database-level mutex.

**Decision.** One writer, many readers. Readers never block, thanks to MVCC.

**Rationale.** It removes an entire class of problems and frees that time for recovery, where the
real learning is. It is also SQLite's model, which runs on billions of devices.

**What is lost.** Write throughput does not scale with core count. For the target workload — an
embedded database — that is acceptable and is declared in the README.

---

## ADR-004 · MVCC instead of two-phase locking

**Status:** accepted

**Context.** Readers and writers need to coexist without waiting on each other.

**Alternatives.** *2PL* is simpler to implement and reaches serializable for free, but readers
block writers and vice versa, and it needs deadlock detection even with a single writer. *MVCC*
never blocks reads, but requires version chains, a visibility rule and garbage collection.
*MVCC with serializable snapshot isolation* closes write skew at the cost of tracking read/write
dependencies in a graph.

**Decision.** MVCC with snapshot isolation, without SSI.

**Rationale.** It is the Postgres model, which makes the learning transferable. The visibility
rule is a small, highly testable piece of logic. And the anomalies it prevents and does not
prevent are measurable, which turns into a results section in the README.

**What is lost.** Write skew happens. It is
[documented explicitly](07-mvcc.md#write-skew-and-why-it-stays) and has a test that reproduces it
on purpose. A database that declares what it does not do is more trustworthy than one that
promises serializability and delivers snapshot isolation — which is, incidentally, what Oracle
does.

---

## ADR-005 · Physiological logging

**Status:** accepted

**Context.** The WAL must record changes in a way that redo can reapply and undo can reverse.

**Alternatives.**

*Logical* — records the operation ("inserted key 42"). Compact, but redo must re-execute the
operation, and re-executing a B+Tree split during recovery produces the hardest divergence to
debug there is.

*Full-page physical* — records the resulting 4 KB. Trivially idempotent, but spends 4 KB of log
to change one byte.

*Physiological* — records the before and after image of a byte range within an identified page.
Logical across pages, physical within a page.

**Decision.** Physiological, with before and after images.

**Rationale.** Idempotence without full-page cost. Applying the after image twice has the same
effect as once, which is redo's absolute requirement. The before image gives undo for free, which
enables the *steal* policy.

**What is lost.** Larger log volume than logical. An `UPDATE` changing 8 bytes writes 16 bytes of
images plus 36 of header. Acceptable, and the log is sequential.

---

## ADR-006 · Order-preserving key encoding

**Status:** accepted

**Context.** The B+Tree must compare keys. Either it knows the types, or keys arrive encoded such
that byte-wise comparison already yields the right order.

**Alternatives.** Passing a per-type comparator into the tree couples the index to the type system
and costs an indirect call per comparison, on the hottest path there is. Encoding keys so that
`memcmp` suffices concentrates the complexity in one pure, testable function, at the cost of an
encoding step and of encoded keys not being human-readable.

**Decision.** Order-preserving encoding, detailed in
[02](02-file-format.md#order-preserving-key-encoding).

**Rationale.** The B+Tree becomes fully type-agnostic — it only understands bytes. Comparison is
`memcmp`, which the CPU executes vectorized. Composite keys are concatenation, with no special
cases.

**What is lost.** Debugging requires decoding a key to read it. Mitigation: a
`decode_key_for_debug` function from day one.

---

## ADR-007 · Rule-based planner

**Status:** accepted

**Context.** The planner must choose between `SeqScan` and `IndexScan`, and between join
strategies.

**Alternatives.** *Cost-based* is what real databases use: collect statistics, estimate
cardinality, enumerate plans, pick the cheapest. It needs histograms, a cost model and
enumeration — an entire project, and without good statistics it chooses worse than simple rules.
*Rule-based* applies fixed heuristics in order: predictable, testable, explainable in one page.

**Decision.** Rule-based, with the six rules listed in [06](06-sql.md#planner).

**Rationale.** The project's goal is storage and durability, not query optimization. Fixed rules
produce reasonable plans for a fraction of the effort, and `EXPLAIN` makes every decision
inspectable.

**What is lost.** Queries where the right choice depends on actual cardinality will get a poor
plan. Recorded as future work, not as an omission.

---

## ADR-008 · Catalog in the database's own tables

**Status:** accepted

**Context.** The schema has to live somewhere.

**Alternatives.** A JSON file next to the `.lastro` would be easy to inspect, but DDL would stop
being transactional: a `CREATE TABLE` that crashed halfway would leave file and database
diverged. A reserved region with a bespoke format would need its own serialization, versioning
and recovery code. Ordinary tables inherit everything that already exists.

**Decision.** `lastro_tables`, `lastro_columns` and `lastro_indexes`, ordinary tables with fixed
ids. `catalog_root` in page 0 is the sole entry pointer.

**Rationale.** DDL becomes an ordinary transaction, with WAL, redo and undo for free. There is no
special code to make DDL atomic, which is a classic bug source. And the database describing
itself is elegant enough to earn a README section.

**What is lost.** A circular dependency at startup: reading the catalog requires knowing the
catalog's schema. Resolved by embedding the three internal tables' schema as constants in the
code — which is the same solution SQLite uses with `sqlite_master`.

---

## ADR-009 · No `mmap`

**Status:** accepted

**Context.** Memory-mapping the file would eliminate the buffer pool and the copies.

**Alternatives.** *`mmap`* delegates paging to the operating system. It is fast and simple to
start with. But control over when a page reaches disk is lost, and without that control **the WAL
rule cannot be guaranteed** — the OS may write a dirty page at any moment, before its log record.
On top of that, I/O errors become `SIGBUS` instead of `Result`, and the well-known paper *Are You
Sure You Want to Use MMAP in Your Database Management System?* documents the rest.

**Decision.** Explicit `pread` and `pwrite`, with a hand-written buffer pool.

**Rationale.** Full control over when each page reaches disk is a prerequisite of the WAL rule,
which is the project's thesis. Errors become `Result`, and are handleable. And implementing the
buffer pool is part of what is meant to be learned.

**What is lost.** One extra copy per read, and the work of writing page replacement. Both are
acceptable costs, and the second is the point.

---

## ADR-010 · Rejected

Recorded so they do not come back as fresh ideas three months from now.

**LSM-tree storage instead of B+Tree.** Faster writes, and it is what RocksDB and Cassandra use.
But compaction is a large subsystem, range scans require merging several levels, and the model
drifts away from the classic relational database, which is what is meant to be understood. A good
idea for a second project.

**Compiling queries to bytecode.** It is what SQLite does, with real gains. But it hides the
semantics behind one more layer precisely while the semantics are still being defined.

**Client-server with a network protocol.** It would shift focus to serialization, connection
pooling and authentication — none of which teaches anything about storage.

**Postgres wire protocol compatibility.** Tempting, since it would allow using `psql` as a client.
But it would require implementing far more SQL than the scope calls for, purely so `psql` does not
break on the introspection queries it fires on connect.

---

Previous: [10 · Glossary](10-glossary.md) · [↑ README](../../README.en.md)
