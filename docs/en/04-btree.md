[Português](../pt/04-btree.md) · [English](04-btree.md) · [↑ README](../../README.en.md)

# 04 · B+Tree

An ordered map from bytes to bytes, stored in disk pages. It backs both the indexes and the
database catalog.

## Why B+Tree and not B-Tree

In a B-Tree, values live in every node. In a B+Tree, values live **only in the leaves**, and
interior nodes hold separators alone.

Two consequences decide it:

**Higher fanout.** An interior node with no payload fits far more keys. With 8-byte keys, a 4 KB
interior node holds roughly 340 entries, versus far fewer if each carried a value too. That
lowers tree height, and height is the disk read count.

**Cheap range scans.** Leaves are chained through the page's `extra` field. Scanning a range means
descending once to the starting leaf and then following sibling pointers, never returning to the
root. `WHERE id BETWEEN 100 AND 5000` becomes a sequential read.

```mermaid
flowchart TD
    R["interior root<br/>| 40 | 80 |"]
    R --> A["interior<br/>| 15 | 28 |"]
    R --> B["interior<br/>| 55 |"]
    R --> C["interior<br/>| 92 |"]

    A --> L1["leaf<br/>4, 9, 12"]
    A --> L2["leaf<br/>15, 21"]
    A --> L3["leaf<br/>28, 33, 37"]
    B --> L4["leaf<br/>40, 48"]
    B --> L5["leaf<br/>55, 61, 70"]
    C --> L6["leaf<br/>80, 85"]
    C --> L7["leaf<br/>92, 97"]

    L1 -.->|extra| L2
    L2 -.->|extra| L3
    L3 -.->|extra| L4
    L4 -.->|extra| L5
    L5 -.->|extra| L6
    L6 -.->|extra| L7
```

The dotted arrows are the sibling pointers. They are what turns a tree into an ordered list when
you need one.

## Separator convention

In an interior node with separators `k1, k2, ..., kn` and children `c0, c1, ..., cn`:

```
c0 holds keys < k1
c1 holds keys in [k1, k2)
c2 holds keys in [k2, k3)
...
cn holds keys >= kn        <- this one is the page's `extra` field
```

Closed on the left, open on the right. Picking that convention and never deviating matters more
than which of the two was picked: half of all B-Tree bugs come from a `<` where a `<=` belonged.

The rightmost child has no separator of its own, which is why it lives in the page header's
`extra` field rather than in a cell.

---

## Search

```
search(key):
    page = root
    while page is interior:
        i = binary_search(page.separators, key)
        page = child(page, i)
    return binary_search(page.cells, key)
```

Cost: one read per level. With a fanout of 340, a million keys fit in three levels and a billion
in four. In practice the root and first level stay resident in the buffer pool, so a lookup costs
one or two physical reads.

The in-page binary search is pure `memcmp`, with no type interpretation, thanks to the
[order-preserving encoding](02-file-format.md#order-preserving-key-encoding).

---

## Insertion and split

If the cell fits in the leaf, insert and adjust the slots. If it does not, the leaf splits.

```mermaid
flowchart TD
    subgraph BEFORE["before: insert 25 into a full leaf"]
        A1["parent<br/>| 40 |"]
        A1 --> A2["FULL leaf<br/>10, 15, 21, 28, 33"]
    end

    subgraph AFTER["after the split"]
        B1["parent<br/>| 21 | 40 |"]
        B1 --> B2["left leaf<br/>10, 15"]
        B1 --> B3["right leaf<br/>21, 25, 28, 33"]
        B2 -.->|extra| B3
    end

    BEFORE ==> AFTER
```

The procedure:

1. Pick the split point closest to 50/50 **in bytes**, not in cell count. With variable-length
   records, splitting by count produces unbalanced pages.
2. Allocate a new leaf, move the right half into it.
3. Fix the sibling pointers: the new leaf points where the old one pointed, and the old one now
   points at the new one.
4. Promote a separator to the parent.
5. If the parent fills, it splits too, recursively. If the root splits, a new root is created and
   the tree gains a level.

**The difference that matters between leaf and interior:** when splitting a leaf, the first key of
the right half is **copied** to the parent and still exists in the leaf. When splitting an
interior node, the median is **moved** to the parent and disappears from the level below — it is
only a separator, not data.

Swapping copy for move in the leaf deletes a real key from the database. It is one of the most
expensive bugs to diagnose, because the tree stays structurally valid: a single record is simply
missing.

---

## Deletion, merge and rebalancing

Deleting is always harder than inserting.

```
delete(key):
    locate the leaf, remove the entry
    walk back up the descent path, and at every level:
        if the node and its left sibling fit together: merge
        else if the node and its right sibling fit together: merge
        else: stop, because nothing above changed
    if the root ends up with a single child: the tree loses a level
```

**Both neighbours have to be tried.** Looking only leftward leaves a node that could have merged
rightward sitting there. Nothing visibly breaks when that happens — the tree stays correct, it
just drifts sparse — which is exactly why an invariant check exists instead of trusting
behavioural tests.

**There is no redistribution.** The specification called for borrowing from a sibling when a
merge would not fit, to hold up the fill floor. In practice redistribution has a problem of its
own: balancing children *k-1* and *k* can shrink *k-1* enough that *k-2* and *k-1* now fit
together, leaving a mergeable pair behind. Fixing that cascades with no natural stopping point.
Merging alone is simple and checkable, and what is given up in exchange is measured in the
section below.

### Contingency plan

Merge and rebalancing propagating upward is the hardest part of the entire project to get right.
If it stalls for more than two weeks, the cut is this:

> Deletion only marks a tombstone. No merge. An offline compaction, triggered manually, rebuilds
> the tree.

Quality under delete-heavy load is lost, and that is recorded honestly in the README. Correctness
is not lost. A correct database with a declared limitation is worth more than an ambitious broken
one.

---

## Range scan

```rust
pub struct RangeIter<'a> {
    pool: &'a BufferPool,
    current_leaf: PageId,
    slot: u16,
    upper: Option<Vec<u8>>,
}
```

Descend once to the lower bound's leaf, then consume slots. When the leaf is exhausted, follow
`extra` to the next one. Stop when `extra` is zero or the key passes the upper bound.

Cost: one logarithmic descent plus a sequential read. That is the reason B+Trees exist.

---

## Invariants

Checked by a `check_tree()` called at the end of every test:

1. Every leaf is at the same depth. The tree is perfectly height-balanced.
2. Keys within each page are in strictly increasing order.
3. All keys in a child respect the range defined by the parent's separators.
4. No node outside the root is empty.
5. Following sibling pointers from the leftmost leaf visits every key exactly once, in
   ascending order.
6. No page is reachable by two distinct paths from the root.
7. The key count reachable through the tree equals the count obtained by range scan.

### What happened to "at least 40% full"

The specification asserted a per-node fill floor. **Two wordings were tried in
the implementation and neither survived the property tests:**

- *"every node is at least 40% full"* fails because a single cell can occupy a
  third of a page. An even split can leave both halves under the floor with
  nothing actually wrong.
- *"no two adjacent siblings fit in one page together"* fails on **insert**, not
  delete. When a full node splits, each half holds about half a page, and one of
  them may now fit alongside the untouched neighbour beside it — a neighbour it
  never had to fit beside before. The minimal case proptest found contains no
  deletes at all.

So the fill factor is **not asserted here, it is measured**, by `BTree::stats`,
and the tests assert on the measurement. A bound that only holds for fixed-size
records is not a bound, and a number checked on every run is worth more than a
guarantee that quietly does not apply.

Invariant 5 is the most valuable of the seven. It cross-checks two independent structures — the
hierarchy and the linked list — and therefore catches nearly every split or merge bug the others
let through.

---

## Testing this layer

**Model-based** — the standard library's `BTreeMap<Vec<u8>, Vec<u8>>` is the oracle. Every
operation goes to both and is compared, including the results of random range scans.

**Property-based** — a million random keys inserted and deleted in arbitrary order, with
`check_tree()` after every operation in debug mode.

**Adversarial patterns**, because uniform randomness does not stress splitting:

- strictly ascending keys, which always fill the rightmost page
- strictly descending keys
- all keys sharing a long common prefix
- maximum-size keys, forcing overflow chains
- insert/delete alternating exactly at the occupancy threshold, to test the hysteresis

**Reproducibility** — every randomized test prints its seed on failure. A B-Tree bug that does
not reproduce cannot be investigated.

## Definition of done

- One million operations against `BTreeMap` with no divergence.
- `check_tree()` passing after every one of those operations in debug mode.
- All five adversarial patterns in the fixed test set.
- Range scan returning exactly the same set as the hierarchical traversal.

---

Previous: [03 · Pager and buffer pool](03-pager.md) · Next: [05 · WAL and recovery](05-wal-recovery.md)
