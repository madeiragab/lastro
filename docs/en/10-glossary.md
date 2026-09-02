[Português](../pt/10-glossario.md) · [English](10-glossary.md) · [↑ README](../../README.en.md)

# 10 · Glossary

Vocabulary used across the documentation. Short definitions, no circularity.

---

**ARIES** — the recovery algorithm published by Mohan et al. in 1992. Three passes: analysis,
redo, undo. The basis of essentially every relational database in use. See [05](05-wal-recovery.md).

**B+Tree** — a balanced search tree where values live only in the leaves and interior nodes hold
separators alone. Leaves are chained for sequential scanning. See [04](04-btree.md).

**Buffer pool** — a fixed-size in-memory cache of disk pages with a replacement policy. See
[03](03-pager.md#buffer-pool).

**Cell** — a variable-length record inside a slotted page. See [02](02-file-format.md#cells).

**Checkpoint** — a mark in the log bounding where recovery must start. *Fuzzy* means the database
keeps running during it.

**CLR** *(compensation log record)* — a record documenting an undo already performed. Never
undone, only redone. It is what makes a crash during recovery safe.

**Dirty page** — a page modified in memory whose on-disk copy is stale.

**Dirty read** — reading data from a transaction that has not committed. Prevented here.

**Fanout** — the number of children of an interior node. High fanout means a shallow tree, and
height is disk read count.

**Freelist** — a linked list of pages that were allocated and later freed, available for reuse.

**`fsync`** — the system call that forces a file's data to the physical medium. It returns only
after that — in theory; some devices lie.

**Fuzzy checkpoint** — a checkpoint that does not block the database. See
[05](05-wal-recovery.md#checkpoint).

**Horizon** — the smallest `xmin` among active snapshots. No version created above it can be
collected. See [07](07-mvcc.md#dead-version-collection).

**Idempotent** — applying twice has the same effect as applying once. A mandatory property of
redo.

**Isolation** — how much a transaction sees of other transactions' in-flight work. Here: snapshot
isolation.

**Lost update** — two transactions read the same value, both write, and one silently overwrites
the other. Prevented here by first-updater-wins.

**LSN** *(log sequence number)* — a monotonic identifier for a log record. Here it is the
record's own offset in the file.

**`memcmp`** — byte-wise comparison. The entire key encoding strategy exists so that `memcmp`
yields the correct logical order. See
[02](02-file-format.md#order-preserving-key-encoding).

**MVCC** *(multiversion concurrency control)* — writing creates a new version rather than
overwriting, so readers never block writers. See [07](07-mvcc.md).

**No-force** — the policy under which dirty pages need not reach disk at commit. Requires redo.

**Overflow page** — an extra page holding the remainder of an oversized cell. See
[02](02-file-format.md#overflow).

**Pager** — the lowest layer. Reads and writes pages, allocates and frees. See [03](03-pager.md).

**Page** — the database's unit of I/O. Here, 4096 bytes.

**Phantom read** — the same predicate query returns new rows when repeated in one transaction.
Prevented here.

**Pin** — marking a page as in use so the buffer pool will not evict it. Every pin needs a
matching unpin.

**`pread` and `pwrite`** — positional read and write, taking the offset as an argument rather
than using the file cursor.

**Clock policy** — a cheap LRU approximation using a reference bit and a circular hand. See
[03](03-pager.md#clock-policy).

**Redo** — reapplying changes recorded in the log. The second recovery pass.

**WAL rule** — the log record goes to disk before the page it describes. The rule that defines
this project.

**Repeatable read** — the same query returns the same result throughout a transaction. An
automatic consequence of the snapshot being immutable.

**RowId** — the `(page_id, slot_id)` pair identifying a heap tuple. Stable even after page
compaction.

**Serializable** — the isolation level where concurrent execution is equivalent to some serial
execution. **Not implemented here**, and declared as such.

**Slot** — a 4-byte entry holding an offset and a length, pointing at a cell. The slot is the
stable address; the cell may move.

**Slotted page** — a layout with slots growing from the start, cells from the end, and free space
in between. See [02](02-file-format.md#slotted-page).

**Snapshot** — a photograph of the concurrency state at the start of a transaction. It determines
what that transaction sees.

**Split** — dividing a full page in two, promoting a separator to the parent. See
[04](04-btree.md#insertion-and-split).

**Steal** — the policy allowing eviction of a dirty page belonging to an uncommitted transaction.
Requires undo.

**Tombstone** — a deletion marker that leaves the bytes in place. An alternative to immediate
merging.

**Undo** — reversing the changes of transactions that never committed. The third recovery pass.

**Vacuum** — collecting versions that no active or future snapshot can see. See
[07](07-mvcc.md#dead-version-collection).

**Varint** — a variable-length integer, 7 payload bits per byte. See
[02](02-file-format.md#varint).

**Volcano** — the execution model where each operator exposes a `next()` pulling one tuple from
the operator below. Also called the iterator model. See [06](06-sql.md#executor).

**WAL** *(write-ahead log)* — a sequential log written before the data pages. See
[05](05-wal-recovery.md).

**Write skew** — two transactions read shared state, write to different rows, and together
violate an invariant each alone would respect. **Not prevented here**, by declared choice. See
[07](07-mvcc.md#write-skew-and-why-it-stays).

**`xmin` and `xmax`** — the txids that created and removed a tuple version. The basis of the
visibility rule.

---

Previous: [09 · Roadmap](09-roadmap.md) · Next: [ADR](adr.md)
