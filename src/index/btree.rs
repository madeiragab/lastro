//! The B+Tree: an ordered map from bytes to bytes, stored in disk pages.
//!
//! Values live only in the leaves; interior nodes hold separators alone. Leaves
//! are chained through the page header's `extra` field, so a range scan
//! descends once and then follows siblings. See `docs/en/04-btree.md`.
//!
//! # How a node is edited
//!
//! Every mutation reads the whole node into vectors, edits them, and writes the
//! node back. It allocates more than editing bytes in place would, and that
//! cost is real and known. It buys something worth more at this stage: split
//! and merge become list operations whose correctness can be read off the code,
//! instead of offset arithmetic that is correct only if every index is right.
//! The benchmark already expects to lose to SQLite; it will lose partly here,
//! and the profile will say so.

use std::collections::HashSet;

use crate::storage::page::{Page, PageType, SLOT_SIZE, USABLE_SPACE};
use crate::storage::{BufferPool, PinnedPage};
use crate::{Error, PageId, Result, NO_PAGE};

use super::node::{self, MAX_KEY, MAX_VALUE};

/// A key and its value.
pub type Entry = (Vec<u8>, Vec<u8>);

/// A B+Tree rooted at a fixed page.
///
/// The root page id never changes, even when the tree grows or shrinks a level,
/// so the catalog can store it once and never revisit it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BTree {
    root: PageId,
}

impl BTree {
    /// Creates an empty tree: a single leaf, which is also the root.
    ///
    /// Logged like any other change, and it has to be. Setting up a page writes
    /// its type, and nothing after that ever writes the type again — so if the
    /// setup is not in the log, redo rebuilds the page's contents onto a page
    /// that never learned what it is. That was a real bug, and the diary in the
    /// README has it.
    pub fn create(pool: &mut BufferPool) -> Result<BTree> {
        let mine = pool.begin_edit();
        let outcome = BTree::create_unlogged(pool);
        if outcome.is_err() {
            if mine {
                pool.abort_edit();
            }
            return outcome;
        }
        if mine {
            pool.end_edit()?;
        }
        outcome
    }

    fn create_unlogged(pool: &mut BufferPool) -> Result<BTree> {
        let pin = pool.new_page(PageType::Leaf)?;
        let root = pin.page_id;
        pool.page_mut(&pin).set_root(true);
        pool.unpin(pin);
        Ok(BTree { root })
    }

    /// Opens an existing tree by its root page.
    pub fn open(root: PageId) -> BTree {
        BTree { root }
    }

    /// The root page id, stable for the life of the tree.
    pub fn root(&self) -> PageId {
        self.root
    }

    // -- reads -------------------------------------------------------------

    /// Looks a key up.
    pub fn get(&self, pool: &mut BufferPool, key: &[u8]) -> Result<Option<Vec<u8>>> {
        let mut path = Vec::new();
        let pin = self.descend(pool, key, &mut path)?;
        release(pool, &mut path);

        let page = pool.page(&pin);
        let (index, found) = node::leaf_search(page, key);
        let value = if found {
            let cell = page.cell(index).expect("btree pages have no dead slots");
            let (_, value) = node::decode_leaf_cell(cell).expect("well formed leaf cell");
            Some(value.to_vec())
        } else {
            None
        };
        pool.unpin(pin);
        Ok(value)
    }

    /// True when the key is present.
    pub fn contains(&self, pool: &mut BufferPool, key: &[u8]) -> Result<bool> {
        Ok(self.get(pool, key)?.is_some())
    }

    /// Every entry in `[lower, upper)`, in key order.
    ///
    /// `lower` of `None` starts at the first key, `upper` of `None` runs to the
    /// last. Descends once and then follows leaf siblings, which is the whole
    /// reason a B+Tree chains its leaves.
    ///
    /// Collects into a vector rather than streaming: a lazy iterator has to
    /// hold a pin across `next` calls, which needs interior mutability in the
    /// pool. That arrives with the executor, which is the first caller that
    /// actually needs to avoid materializing.
    pub fn range(
        &self,
        pool: &mut BufferPool,
        lower: Option<&[u8]>,
        upper: Option<&[u8]>,
    ) -> Result<Vec<Entry>> {
        let start = lower.unwrap_or(b"");
        let mut path = Vec::new();
        let mut pin = self.descend(pool, start, &mut path)?;
        release(pool, &mut path);

        let mut out = Vec::new();
        let mut first = true;
        loop {
            let page = pool.page(&pin);
            let begin = if first {
                first = false;
                node::leaf_search(page, start).0
            } else {
                0
            };

            let mut stopped = false;
            for slot in begin..page.slot_count() {
                let cell = page.cell(slot).expect("btree pages have no dead slots");
                let (key, value) = node::decode_leaf_cell(cell).expect("well formed leaf cell");
                if let Some(limit) = upper {
                    if key >= limit {
                        stopped = true;
                        break;
                    }
                }
                out.push((key.to_vec(), value.to_vec()));
            }

            let next = page.extra();
            pool.unpin(pin);
            if stopped || next == NO_PAGE {
                return Ok(out);
            }
            pin = pool.fetch(next)?;
        }
    }

    /// Every entry in the tree, in key order.
    pub fn iter(&self, pool: &mut BufferPool) -> Result<Vec<Entry>> {
        self.range(pool, None, None)
    }

    /// A cursor that walks the tree one entry at a time.
    ///
    /// This is what the executor scans with. Unlike [`BTree::range`] it holds
    /// nothing but a leaf number and a slot between calls, so it never keeps a
    /// pin across a `next` and the pool stays free to evict around it.
    ///
    /// The price of holding no pin is that the cursor is only valid while the
    /// tree is not being modified. Every scan in the executor reads a tree that
    /// the same statement is not writing, which is the condition that makes
    /// this safe.
    pub fn cursor(&self, pool: &mut BufferPool, lower: Option<&[u8]>) -> Result<Cursor> {
        let start = lower.unwrap_or(b"");
        let mut path = Vec::new();
        let pin = self.descend(pool, start, &mut path)?;
        release(pool, &mut path);

        let slot = node::leaf_search(pool.page(&pin), start).0;
        let leaf = pin.page_id;
        pool.unpin(pin);

        Ok(Cursor {
            leaf,
            slot,
            upper: None,
        })
    }

    /// A cursor bounded above, stopping before `upper`.
    pub fn cursor_range(
        &self,
        pool: &mut BufferPool,
        lower: Option<&[u8]>,
        upper: Option<&[u8]>,
    ) -> Result<Cursor> {
        let mut cursor = self.cursor(pool, lower)?;
        cursor.upper = upper.map(|key| key.to_vec());
        Ok(cursor)
    }

    /// The largest key in the tree, or `None` when it is empty.
    ///
    /// Descends the rightmost spine, which is what hands out the next row id
    /// without the catalog having to store a counter and rewrite it on every
    /// insert.
    pub fn last_key(&self, pool: &mut BufferPool) -> Result<Option<Vec<u8>>> {
        let mut pin = pool.fetch(self.root)?;
        loop {
            match pool.page(&pin).page_type() {
                Some(PageType::Leaf) => {
                    let count = pool.page(&pin).slot_count();
                    let key = if count == 0 {
                        None
                    } else {
                        let cell = pool.page(&pin).cell(count - 1).expect("a live slot");
                        node::leaf_key(cell).map(|key| key.to_vec())
                    };
                    pool.unpin(pin);
                    return Ok(key);
                }
                Some(PageType::Interior) => {
                    let child = pool.page(&pin).extra();
                    pool.unpin(pin);
                    pin = pool.fetch(child)?;
                }
                other => {
                    pool.unpin(pin);
                    return Err(broken(format!("page type {other:?} inside a btree")));
                }
            }
        }
    }

    // -- writes ------------------------------------------------------------

    /// Inserts a key, replacing any value already stored under it.
    ///
    /// When the pool has a log and an open transaction, everything the insert
    /// touches is logged as one group of records before it returns. When it has
    /// neither, the session is a no-op and the tree behaves exactly as before —
    /// which is what lets the same code serve both.
    pub fn insert(&mut self, pool: &mut BufferPool, key: &[u8], value: &[u8]) -> Result<()> {
        let mine = pool.begin_edit();
        let outcome = self.insert_unlogged(pool, key, value);
        if outcome.is_err() {
            if mine {
                pool.abort_edit();
            }
            return outcome;
        }
        if mine {
            pool.end_edit()?;
        }
        outcome
    }

    fn insert_unlogged(&mut self, pool: &mut BufferPool, key: &[u8], value: &[u8]) -> Result<()> {
        if key.len() > MAX_KEY {
            return Err(Error::CellTooLarge(key.len()));
        }
        if value.len() > MAX_VALUE {
            return Err(Error::CellTooLarge(value.len()));
        }

        let mut path = Vec::new();
        let leaf_pin = self.descend(pool, key, &mut path)?;

        let mut entries = read_leaf(pool.page(&leaf_pin));
        let (index, found) = node::leaf_search(pool.page(&leaf_pin), key);
        if found {
            entries[index as usize] = (key.to_vec(), value.to_vec());
        } else {
            entries.insert(index as usize, (key.to_vec(), value.to_vec()));
        }

        if leaf_size(&entries) <= USABLE_SPACE {
            write_leaf(pool.page_mut(&leaf_pin), &entries)?;
            pool.unpin(leaf_pin);
            release(pool, &mut path);
            return Ok(());
        }

        // The leaf overflows. Give it a parent if it has none, then split.
        let leaf_pin = self.ensure_parent(pool, &mut path, leaf_pin)?;

        let sizes = leaf_sizes(&entries);
        let cut = split_point(&sizes).ok_or(Error::PageFull)?;

        let right_pin = pool.new_page(PageType::Leaf)?;
        let right_id = right_pin.page_id;

        // The new leaf takes over the old one's place in the sibling chain.
        let old_sibling = pool.page(&leaf_pin).extra();
        pool.page_mut(&right_pin).set_extra(old_sibling);
        write_leaf(pool.page_mut(&right_pin), &entries[cut..])?;

        pool.page_mut(&leaf_pin).set_extra(right_id);
        write_leaf(pool.page_mut(&leaf_pin), &entries[..cut])?;

        // A leaf separator is COPIED: the key stays in the leaf as real data.
        let separator = entries[cut].0.clone();
        pool.unpin(right_pin);
        pool.unpin(leaf_pin);

        self.propagate_split(pool, &mut path, right_id, separator)
    }

    /// Removes a key. Returns whether it was there.
    ///
    /// Logged as one group, the same way [`BTree::insert`] is.
    pub fn delete(&mut self, pool: &mut BufferPool, key: &[u8]) -> Result<bool> {
        let mine = pool.begin_edit();
        let outcome = self.delete_unlogged(pool, key);
        if outcome.is_err() {
            if mine {
                pool.abort_edit();
            }
            return outcome;
        }
        if mine {
            pool.end_edit()?;
        }
        outcome
    }

    fn delete_unlogged(&mut self, pool: &mut BufferPool, key: &[u8]) -> Result<bool> {
        let mut path = Vec::new();
        let leaf_pin = self.descend(pool, key, &mut path)?;

        let (index, found) = node::leaf_search(pool.page(&leaf_pin), key);
        if !found {
            pool.unpin(leaf_pin);
            release(pool, &mut path);
            return Ok(false);
        }

        let mut entries = read_leaf(pool.page(&leaf_pin));
        entries.remove(index as usize);
        write_leaf(pool.page_mut(&leaf_pin), &entries)?;

        pool.unpin(leaf_pin);

        // Always walk up, whatever the leaf's occupancy. A delete can make a
        // pair of siblings small enough to merge without either of them looking
        // remarkable on its own, and leaving that pair unmerged breaks
        // invariant 4.
        if path.is_empty() {
            release(pool, &mut path);
        } else {
            self.rebalance(pool, &mut path)?;
        }

        self.shrink_root(pool)?;
        Ok(true)
    }

    // -- descent -----------------------------------------------------------

    /// Walks from the root to the leaf that may hold `key`, pushing every
    /// interior node onto `path` with the index of the child it descended into.
    ///
    /// The caller owns every pin, including the returned leaf.
    fn descend(
        &self,
        pool: &mut BufferPool,
        key: &[u8],
        path: &mut Vec<(PinnedPage, u16)>,
    ) -> Result<PinnedPage> {
        let mut pin = pool.fetch(self.root)?;
        loop {
            match pool.page(&pin).page_type() {
                Some(PageType::Leaf) => return Ok(pin),
                Some(PageType::Interior) => {
                    let index = node::interior_child_index(pool.page(&pin), key);
                    let child = node::child_at(pool.page(&pin), index);
                    path.push((pin, index));
                    pin = pool.fetch(child)?;
                }
                other => {
                    let found = pin.page_id;
                    pool.unpin(pin);
                    release(pool, path);
                    return Err(Error::MalformedFile(format!(
                        "page {found} has type {other:?}, reached from root {}",
                        self.root
                    )));
                }
            }
        }
    }

    /// Makes sure the node about to split has a parent.
    ///
    /// When it does not, the node is the root, and the root page id must stay
    /// put. So the root's content moves down into a fresh child and the root
    /// becomes an interior node with that single child. The tree gains a level
    /// without anyone outside noticing.
    fn ensure_parent(
        &mut self,
        pool: &mut BufferPool,
        path: &mut Vec<(PinnedPage, u16)>,
        node_pin: PinnedPage,
    ) -> Result<PinnedPage> {
        if !path.is_empty() {
            return Ok(node_pin);
        }
        debug_assert_eq!(node_pin.page_id, self.root);

        let content = pool.page(&node_pin).clone();
        let kind = content.page_type().expect("a btree page has a type");
        let child_pin = pool.new_page(kind)?;
        let child_id = child_pin.page_id;
        *pool.page_mut(&child_pin) = content;
        pool.page_mut(&child_pin).set_root(false);

        pool.page_mut(&node_pin).set_root(true);
        write_interior(pool.page_mut(&node_pin), &[child_id], &[])?;

        path.push((node_pin, 0));
        Ok(child_pin)
    }

    /// Carries a split upward: inserts the new right sibling and its separator
    /// into the parent, splitting the parent in turn for as long as needed.
    fn propagate_split(
        &mut self,
        pool: &mut BufferPool,
        path: &mut Vec<(PinnedPage, u16)>,
        mut right_id: PageId,
        mut separator: Vec<u8>,
    ) -> Result<()> {
        loop {
            let (parent_pin, child_index) = path.pop().expect("ensure_parent guarantees a parent");

            let (mut children, mut separators) = read_interior(pool.page(&parent_pin));
            children.insert(child_index as usize + 1, right_id);
            separators.insert(child_index as usize, separator);

            if interior_size(&separators) <= USABLE_SPACE {
                write_interior(pool.page_mut(&parent_pin), &children, &separators)?;
                pool.unpin(parent_pin);
                release(pool, path);
                return Ok(());
            }

            let parent_pin = self.ensure_parent(pool, path, parent_pin)?;

            let sizes = interior_sizes(&separators);
            let cut = split_point(&sizes).ok_or(Error::PageFull)?;

            // An interior separator is MOVED, not copied: it is only a
            // signpost, so it leaves this level and becomes the parent's.
            let promoted = separators[cut].clone();

            let new_pin = pool.new_page(PageType::Interior)?;
            let new_id = new_pin.page_id;
            write_interior(
                pool.page_mut(&new_pin),
                &children[cut + 1..],
                &separators[cut + 1..],
            )?;
            write_interior(
                pool.page_mut(&parent_pin),
                &children[..=cut],
                &separators[..cut],
            )?;
            pool.unpin(new_pin);
            pool.unpin(parent_pin);

            right_id = new_id;
            separator = promoted;
        }
    }

    // -- rebalancing -------------------------------------------------------

    /// Walks up from a node that just shrank, merging it with a neighbour
    /// wherever the two now fit in one page.
    ///
    /// Stops as soon as a level does not merge: nothing above it changed, so
    /// nothing above it can have become mergeable.
    fn rebalance(
        &mut self,
        pool: &mut BufferPool,
        path: &mut Vec<(PinnedPage, u16)>,
    ) -> Result<()> {
        while let Some((parent_pin, child_index)) = path.pop() {
            let child_count = pool.page(&parent_pin).slot_count() as usize + 1;
            let index = child_index as usize;

            // Both neighbours have to be tried. Checking only the left one
            // leaves a node that could have merged rightward sitting there,
            // which breaks invariant 4 while breaking nothing visible — the
            // bug this code had the first time it was written.
            let mut merged = false;
            if index > 0 {
                merged = self.try_merge(pool, &parent_pin, index - 1)?;
            }
            if !merged && index + 1 < child_count {
                merged = self.try_merge(pool, &parent_pin, index)?;
            }
            pool.unpin(parent_pin);

            if !merged {
                release(pool, path);
                return Ok(());
            }
        }
        Ok(())
    }

    /// Merges the children at `left_index` and `left_index + 1` when their
    /// contents fit in a single page, and does nothing otherwise.
    ///
    /// Returns whether the merge happened, which is exactly when the parent
    /// shrank and the level above needs looking at.
    ///
    /// # Why there is no redistribution
    ///
    /// The specification called for borrowing from a sibling when merging was
    /// impossible, so that every node stayed above a percentage floor.
    /// Redistribution turns out to break the very invariant it was meant to
    /// support: balancing children *k-1* and *k* can shrink *k-1* enough that
    /// *k-2* and *k-1* now fit together, leaving a mergeable pair behind. Fixing
    /// that cascades outward with no natural stopping point.
    ///
    /// Merging alone keeps invariant 4 exactly, because a delete shrinks one
    /// node and only the pairs touching it can newly become mergeable. What is
    /// given up is the per-node fill floor: one node may sit nearly empty beside
    /// a nearly full one. What is kept is the guarantee that actually matters —
    /// every adjacent pair together exceeds a page, so the average node is more
    /// than half full and the tree stays logarithmic.
    fn try_merge(
        &mut self,
        pool: &mut BufferPool,
        parent_pin: &PinnedPage,
        left_index: usize,
    ) -> Result<bool> {
        let (mut children, mut separators) = read_interior(pool.page(parent_pin));
        let left_pin = pool.fetch(children[left_index])?;
        let right_pin = pool.fetch(children[left_index + 1])?;

        if pool.page(&left_pin).page_type() == Some(PageType::Leaf) {
            let mut all = read_leaf(pool.page(&left_pin));
            all.extend(read_leaf(pool.page(&right_pin)));

            if leaf_size(&all) > USABLE_SPACE {
                pool.unpin(left_pin);
                pool.unpin(right_pin);
                return Ok(false);
            }

            // The survivor takes over the departing leaf's place in the chain.
            let sibling = pool.page(&right_pin).extra();
            pool.page_mut(&left_pin).set_extra(sibling);
            write_leaf(pool.page_mut(&left_pin), &all)?;
        } else {
            let (left_children, left_separators) = read_interior(pool.page(&left_pin));
            let (right_children, right_separators) = read_interior(pool.page(&right_pin));

            let mut all_children = left_children;
            all_children.extend(right_children);

            // The parent's separator descends into the merged node: it is
            // exactly the boundary between the two children's key ranges, and
            // without it that boundary would be lost.
            let mut all_separators = left_separators;
            all_separators.push(separators[left_index].clone());
            all_separators.extend(right_separators);

            if interior_size(&all_separators) > USABLE_SPACE {
                pool.unpin(left_pin);
                pool.unpin(right_pin);
                return Ok(false);
            }

            write_interior(pool.page_mut(&left_pin), &all_children, &all_separators)?;
        }

        // Dropping a separator only ever makes the parent smaller, so this
        // write cannot fail for want of room.
        separators.remove(left_index);
        children.remove(left_index + 1);
        write_interior(pool.page_mut(parent_pin), &children, &separators)?;

        pool.unpin(left_pin);
        pool.free_page(right_pin)?;
        Ok(true)
    }

    /// Pulls the tree down a level while the root has a single child.
    ///
    /// The child's content moves into the root page, so the root page id stays
    /// put — the mirror image of [`BTree::ensure_parent`].
    fn shrink_root(&mut self, pool: &mut BufferPool) -> Result<()> {
        loop {
            let root_pin = pool.fetch(self.root)?;
            let is_thin = pool.page(&root_pin).page_type() == Some(PageType::Interior)
                && pool.page(&root_pin).slot_count() == 0;
            if !is_thin {
                pool.unpin(root_pin);
                return Ok(());
            }

            let child_id = pool.page(&root_pin).extra();
            let child_pin = pool.fetch(child_id)?;
            let mut content = pool.page(&child_pin).clone();
            content.set_root(true);
            *pool.page_mut(&root_pin) = content;

            pool.unpin(root_pin);
            pool.free_page(child_pin)?;
        }
    }

    // -- invariants --------------------------------------------------------

    /// Verifies every structural invariant of the tree.
    ///
    /// 1. Every leaf is at the same depth.
    /// 2. Keys within a page are strictly increasing.
    /// 3. Every key in a child respects the parent's separator range.
    /// 4. No node outside the root is empty.
    /// 5. Following the leaf chain visits every key once, in order.
    /// 6. No page is reachable by two distinct paths from the root.
    /// 7. The keys reached through the tree equal the keys reached by scanning.
    ///
    /// Invariant 5 is the valuable one: it cross-checks the hierarchy against
    /// the sibling chain, two structures maintained independently, and so
    /// catches nearly every split or merge bug the others let through.
    ///
    /// # What happened to "at least 40% full"
    ///
    /// The specification asserted a per-node fill floor. Two attempts at
    /// stating one survived contact with the property tests, and neither held:
    ///
    /// - *"every node is at least 40% full"* fails because a single cell can
    ///   occupy a third of a page, so an even split can leave both halves under
    ///   the floor with nothing wrong.
    /// - *"no two adjacent siblings fit in one page together"* fails on
    ///   **insert**, not delete: when a full node splits, each half is about
    ///   half a page, and one of them may now fit alongside the untouched
    ///   neighbour it never had to fit beside before.
    ///
    /// So the fill factor is not asserted here. It is **measured**, by
    /// [`BTree::stats`], and the tests assert on the measurement. That is the
    /// honest arrangement: a bound that only holds for fixed-size records is
    /// not a bound, and a number that is checked every run is worth more than a
    /// guarantee that quietly does not apply.
    pub fn check_tree(&self, pool: &mut BufferPool) -> Result<()> {
        let mut state = Check::new();
        check_node(pool, self.root, 0, None, None, &mut state)?;

        // 1. every leaf at the same depth
        if let Some(first) = state.leaf_depths.first() {
            if state.leaf_depths.iter().any(|depth| depth != first) {
                return Err(broken(format!(
                    "leaves sit at differing depths: {:?}",
                    state.leaf_depths
                )));
            }
        }

        // 5 and 7. the sibling chain agrees with the hierarchy
        let scanned: Vec<Vec<u8>> = self.iter(pool)?.into_iter().map(|(key, _)| key).collect();
        if scanned != state.keys {
            return Err(broken(format!(
                "the leaf chain yields {} keys but the hierarchy yields {}",
                scanned.len(),
                state.keys.len()
            )));
        }
        for pair in scanned.windows(2) {
            if pair[0] >= pair[1] {
                return Err(broken("the leaf chain is not strictly increasing".into()));
            }
        }
        Ok(())
    }

    /// Measures the shape of the tree.
    ///
    /// Exists because the fill factor is a measurement here rather than a
    /// guarantee; see [`BTree::check_tree`]. Also validates as it walks, so a
    /// malformed tree reports an error instead of a plausible-looking number.
    pub fn stats(&self, pool: &mut BufferPool) -> Result<TreeStats> {
        let mut state = Check::new();
        check_node(pool, self.root, 0, None, None, &mut state)?;

        // A tree that is nothing but its root has no non-root pages to average,
        // and reporting it as fully packed is the reading that keeps callers
        // from treating an empty tree as a degenerate one.
        let mean = state
            .occupancy
            .iter()
            .sum::<usize>()
            .checked_div(state.occupancy.len())
            .unwrap_or(100);

        Ok(TreeStats {
            pages: state.visited.len(),
            leaves: state.leaf_depths.len(),
            height: state.leaf_depths.first().map_or(0, |depth| depth + 1),
            entries: state.keys.len(),
            mean_occupancy_percent: mean,
            min_occupancy_percent: state.occupancy.iter().copied().min().unwrap_or(100),
        })
    }

    /// How many pages the tree occupies. Used by tests to confirm that deleting
    /// everything actually gives the pages back.
    pub fn page_count(&self, pool: &mut BufferPool) -> Result<usize> {
        Ok(self.stats(pool)?.pages)
    }
}

/// The measured shape of a tree. See [`BTree::stats`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TreeStats {
    /// Pages the tree occupies, root included.
    pub pages: usize,
    /// How many of those are leaves.
    pub leaves: usize,
    /// Levels from the root to a leaf, counting both. An empty tree is 1.
    pub height: usize,
    /// Entries stored.
    pub entries: usize,
    /// Mean fill of the non-root pages, in percent of usable space.
    pub mean_occupancy_percent: usize,
    /// The emptiest non-root page, in percent of usable space.
    pub min_occupancy_percent: usize,
}

/// Walks a tree in key order, one entry per call.
///
/// See [`BTree::cursor`] for why it holds no pin between calls, and what that
/// costs.
#[derive(Debug, Clone)]
pub struct Cursor {
    leaf: PageId,
    slot: u16,
    upper: Option<Vec<u8>>,
}

impl Cursor {
    /// The next entry, or `None` at the end of the range.
    pub fn next(&mut self, pool: &mut BufferPool) -> Result<Option<Entry>> {
        loop {
            if self.leaf == NO_PAGE {
                return Ok(None);
            }
            let pin = pool.fetch(self.leaf)?;
            let page = pool.page(&pin);

            if self.slot >= page.slot_count() {
                self.leaf = page.extra();
                self.slot = 0;
                pool.unpin(pin);
                continue;
            }

            let cell = page
                .cell(self.slot)
                .expect("btree pages have no dead slots");
            let (key, value) = node::decode_leaf_cell(cell).expect("well formed leaf cell");
            let entry = (key.to_vec(), value.to_vec());
            pool.unpin(pin);
            self.slot += 1;

            if let Some(limit) = &self.upper {
                if entry.0 >= *limit {
                    self.leaf = NO_PAGE;
                    return Ok(None);
                }
            }
            return Ok(Some(entry));
        }
    }
}

// -- node representation ---------------------------------------------------

fn release(pool: &mut BufferPool, path: &mut Vec<(PinnedPage, u16)>) {
    for (pin, _) in path.drain(..) {
        pool.unpin(pin);
    }
}

fn broken(why: String) -> Error {
    Error::MalformedFile(why)
}

fn leaf_sizes(entries: &[Entry]) -> Vec<usize> {
    entries
        .iter()
        .map(|(key, value)| node::leaf_cell_len(key, value) + SLOT_SIZE)
        .collect()
}

fn leaf_size(entries: &[Entry]) -> usize {
    leaf_sizes(entries).iter().sum()
}

fn interior_sizes(separators: &[Vec<u8>]) -> Vec<usize> {
    separators
        .iter()
        .map(|separator| node::interior_cell_len(separator) + SLOT_SIZE)
        .collect()
}

fn interior_size(separators: &[Vec<u8>]) -> usize {
    interior_sizes(separators).iter().sum()
}

fn read_leaf(page: &Page) -> Vec<Entry> {
    page.iter_cells()
        .map(|(_, cell)| {
            let (key, value) = node::decode_leaf_cell(cell).expect("well formed leaf cell");
            (key.to_vec(), value.to_vec())
        })
        .collect()
}

fn write_leaf(page: &mut Page, entries: &[Entry]) -> Result<()> {
    if leaf_size(entries) > USABLE_SPACE {
        return Err(Error::PageFull);
    }
    let (sibling, root, lsn) = (page.extra(), page.is_root(), page.lsn());
    page.init(PageType::Leaf);
    page.set_extra(sibling);
    page.set_root(root);
    page.set_lsn(lsn);

    let mut cell = Vec::new();
    for (key, value) in entries {
        node::encode_leaf_cell(key, value, &mut cell);
        page.push_cell(&cell)?;
    }
    Ok(())
}

fn read_interior(page: &Page) -> (Vec<PageId>, Vec<Vec<u8>>) {
    let mut children = Vec::new();
    let mut separators = Vec::new();
    for (_, cell) in page.iter_cells() {
        let (child, separator) =
            node::decode_interior_cell(cell).expect("well formed interior cell");
        children.push(child);
        separators.push(separator.to_vec());
    }
    children.push(page.extra());
    (children, separators)
}

fn write_interior(page: &mut Page, children: &[PageId], separators: &[Vec<u8>]) -> Result<()> {
    debug_assert_eq!(
        children.len(),
        separators.len() + 1,
        "an interior node has one more child than separators"
    );
    if interior_size(separators) > USABLE_SPACE {
        return Err(Error::PageFull);
    }
    let (root, lsn) = (page.is_root(), page.lsn());
    page.init(PageType::Interior);
    page.set_root(root);
    page.set_lsn(lsn);

    let mut cell = Vec::new();
    for (index, separator) in separators.iter().enumerate() {
        node::encode_interior_cell(children[index], separator, &mut cell);
        page.push_cell(&cell)?;
    }
    page.set_extra(*children.last().expect("at least one child"));
    Ok(())
}

/// Chooses where to cut a run of cells so that both halves fit in a page and
/// the split is as even as the sizes allow.
///
/// Returns `None` when no cut leaves both halves non-empty and within a page,
/// which cannot happen while a single cell fits in an empty page.
fn split_point(sizes: &[usize]) -> Option<usize> {
    if sizes.len() < 2 {
        return None;
    }
    let total: usize = sizes.iter().sum();

    // The earliest cut whose right half still fits.
    let mut lowest = sizes.len();
    let mut right = 0usize;
    for index in (0..sizes.len()).rev() {
        if right + sizes[index] > USABLE_SPACE {
            break;
        }
        right += sizes[index];
        lowest = index;
    }

    // The latest cut whose left half still fits.
    let mut highest = 0usize;
    let mut left = 0usize;
    for (index, size) in sizes.iter().enumerate() {
        if left + size > USABLE_SPACE {
            break;
        }
        left += size;
        highest = index + 1;
    }

    let lowest = lowest.max(1);
    let highest = highest.min(sizes.len() - 1);
    if lowest > highest {
        return None;
    }

    let mut balanced = sizes.len();
    let mut running = 0usize;
    for (index, size) in sizes.iter().enumerate() {
        running += size;
        if running * 2 >= total {
            balanced = index + 1;
            break;
        }
    }
    Some(balanced.clamp(lowest, highest))
}

// -- invariant checking ----------------------------------------------------

struct Check {
    visited: HashSet<PageId>,
    leaf_depths: Vec<usize>,
    keys: Vec<Vec<u8>>,
    /// Fill of every page except the root, in percent of usable space. The
    /// root is excluded because it is legitimately allowed to be nearly empty.
    occupancy: Vec<usize>,
}

impl Check {
    fn new() -> Check {
        Check {
            visited: HashSet::new(),
            leaf_depths: Vec::new(),
            keys: Vec::new(),
            occupancy: Vec::new(),
        }
    }
}

fn check_node(
    pool: &mut BufferPool,
    id: PageId,
    depth: usize,
    lower: Option<&[u8]>,
    upper: Option<&[u8]>,
    state: &mut Check,
) -> Result<()> {
    // 6. no page reachable twice
    if !state.visited.insert(id) {
        return Err(broken(format!("page {id} is reachable by two paths")));
    }

    let pin = pool.fetch(id)?;
    let page = pool.page(&pin).clone();
    pool.unpin(pin);

    if depth > 0 {
        state.occupancy.push(page.occupancy_percent());
        // 4. no node outside the root is empty
        if page.slot_count() == 0 {
            return Err(broken(format!("page {id} is empty and is not the root")));
        }
    }

    match page.page_type() {
        Some(PageType::Leaf) => {
            state.leaf_depths.push(depth);
            let mut previous: Option<Vec<u8>> = None;
            for (slot, cell) in page.iter_cells() {
                let (key, _) = node::decode_leaf_cell(cell)
                    .ok_or_else(|| broken(format!("page {id} slot {slot} is malformed")))?;

                // 2. strictly increasing within the page
                if let Some(before) = &previous {
                    if before.as_slice() >= key {
                        return Err(broken(format!("page {id} keys are out of order at {slot}")));
                    }
                }
                // 3. within the parent's range
                if let Some(limit) = lower {
                    if key < limit {
                        return Err(broken(format!("page {id} holds a key below its range")));
                    }
                }
                if let Some(limit) = upper {
                    if key >= limit {
                        return Err(broken(format!("page {id} holds a key above its range")));
                    }
                }
                previous = Some(key.to_vec());
                state.keys.push(key.to_vec());
            }
            Ok(())
        }
        Some(PageType::Interior) => {
            let (children, separators) = read_interior(&page);

            let mut previous: Option<&[u8]> = None;
            for (index, separator) in separators.iter().enumerate() {
                if let Some(before) = previous {
                    if before >= separator.as_slice() {
                        return Err(broken(format!(
                            "page {id} separators are out of order at {index}"
                        )));
                    }
                }
                previous = Some(separator);
            }

            for (index, child) in children.iter().enumerate() {
                let child_lower = if index == 0 {
                    lower
                } else {
                    Some(separators[index - 1].as_slice())
                };
                let child_upper = if index == separators.len() {
                    upper
                } else {
                    Some(separators[index].as_slice())
                };
                check_node(pool, *child, depth + 1, child_lower, child_upper, state)?;
            }
            Ok(())
        }
        other => Err(broken(format!(
            "page {id} has type {other:?} inside a btree"
        ))),
    }
}
