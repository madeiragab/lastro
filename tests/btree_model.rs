//! The B+Tree, checked against `BTreeMap`.
//!
//! The standard library's map is the oracle: every operation goes to both and
//! the results are compared. `check_tree` runs after each one, so a broken
//! invariant is reported at the operation that broke it rather than ten
//! thousand operations later, when the trail has gone cold.

use std::collections::BTreeMap;

use lastro::index::BTree;
use lastro::storage::{BufferPool, Pager};
use proptest::prelude::*;

/// A pool over a fresh temporary database.
fn fresh(capacity: usize) -> (tempfile::TempDir, BufferPool) {
    let dir = tempfile::tempdir().unwrap();
    let pager = Pager::create(dir.path().join("tree.lastro")).unwrap();
    (dir, BufferPool::new(pager, capacity))
}

/// Values large enough that a leaf holds only a handful of them, so a few
/// hundred keys already build a tree several levels deep. Uniform tiny values
/// would leave everything in one page and test nothing.
fn payload(seed: u8, len: usize) -> Vec<u8> {
    (0..len).map(|i| seed.wrapping_add(i as u8)).collect()
}

#[derive(Debug, Clone)]
enum Op {
    Insert(Vec<u8>, u8, usize),
    Delete(Vec<u8>),
    Get(Vec<u8>),
}

fn any_key() -> impl Strategy<Value = Vec<u8>> {
    prop::collection::vec(any::<u8>(), 1..6)
}

fn any_op() -> impl Strategy<Value = Op> {
    prop_oneof![
        6 => (any_key(), any::<u8>(), 80usize..400).prop_map(|(k, s, l)| Op::Insert(k, s, l)),
        4 => any_key().prop_map(Op::Delete),
        2 => any_key().prop_map(Op::Get),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(48))]

    #[test]
    fn tree_agrees_with_btreemap(ops in prop::collection::vec(any_op(), 0..220)) {
        let (_dir, mut pool) = fresh(16);
        let mut tree = BTree::create(&mut pool).unwrap();
        let mut model: BTreeMap<Vec<u8>, Vec<u8>> = BTreeMap::new();

        for op in ops {
            match op {
                Op::Insert(key, seed, len) => {
                    let value = payload(seed, len);
                    tree.insert(&mut pool, &key, &value).unwrap();
                    model.insert(key, value);
                }
                Op::Delete(key) => {
                    let removed = tree.delete(&mut pool, &key).unwrap();
                    prop_assert_eq!(removed, model.remove(&key).is_some());
                }
                Op::Get(key) => {
                    let found = tree.get(&mut pool, &key).unwrap();
                    prop_assert_eq!(found.as_deref(), model.get(&key).map(Vec::as_slice));
                }
            }

            tree.check_tree(&mut pool).unwrap();
        }

        let scanned = tree.iter(&mut pool).unwrap();
        let expected: Vec<(Vec<u8>, Vec<u8>)> = model.into_iter().collect();
        prop_assert_eq!(scanned, expected);
        pool.check_invariants().unwrap();
    }

    #[test]
    fn range_scans_agree_with_btreemap(
        keys in prop::collection::vec(any_key(), 0..120),
        lower in prop::option::of(any_key()),
        upper in prop::option::of(any_key()),
    ) {
        let (_dir, mut pool) = fresh(16);
        let mut tree = BTree::create(&mut pool).unwrap();
        let mut model: BTreeMap<Vec<u8>, Vec<u8>> = BTreeMap::new();

        for (index, key) in keys.iter().enumerate() {
            let value = payload(index as u8, 120);
            tree.insert(&mut pool, key, &value).unwrap();
            model.insert(key.clone(), value);
        }

        let scanned = tree
            .range(&mut pool, lower.as_deref(), upper.as_deref())
            .unwrap();

        let expected: Vec<(Vec<u8>, Vec<u8>)> = model
            .into_iter()
            .filter(|(key, _)| lower.as_ref().is_none_or(|low| key >= low))
            .filter(|(key, _)| upper.as_ref().is_none_or(|high| key < high))
            .collect();

        prop_assert_eq!(scanned, expected);
    }
}

#[test]
fn the_fill_factor_is_measured_not_assumed() {
    // The specification asked for a per-node occupancy floor. No such floor
    // survives variable-length cells, so the shape of the tree is measured
    // instead. See BTree::check_tree for the two floors that were tried and
    // why each one failed.
    let (_dir, mut pool) = fresh(64);
    let mut tree = BTree::create(&mut pool).unwrap();

    for index in 0..4_000u32 {
        tree.insert(&mut pool, &index.to_be_bytes(), &payload(index as u8, 200))
            .unwrap();
    }

    let loaded = tree.stats(&mut pool).unwrap();
    assert_eq!(loaded.entries, 4_000);
    assert!(
        loaded.height >= 3,
        "expected a tree several levels deep, got {loaded:?}"
    );
    assert!(
        loaded.mean_occupancy_percent >= 40,
        "a freshly loaded tree should be about half full, got {loaded:?}"
    );

    // Delete three quarters of it. Merging keeps the tree compact; nothing
    // guarantees a floor for any single page, but the whole must not collapse
    // into a long chain of nearly empty ones.
    for index in 0..4_000u32 {
        if index % 4 != 0 {
            assert!(tree.delete(&mut pool, &index.to_be_bytes()).unwrap());
        }
    }
    tree.check_tree(&mut pool).unwrap();

    let pruned = tree.stats(&mut pool).unwrap();
    assert_eq!(pruned.entries, 1_000);
    assert!(
        pruned.pages * 3 < loaded.pages,
        "deleting three quarters should have given most pages back: {pruned:?} against {loaded:?}"
    );
    assert!(
        pruned.mean_occupancy_percent >= 40,
        "merging should keep the survivors packed, got {pruned:?}"
    );
}

// -- adversarial patterns --------------------------------------------------
//
// Uniform random keys do not stress splitting: they spread evenly and never
// hammer one page. These do.

#[test]
fn ascending_keys_always_fill_the_rightmost_page() {
    let (_dir, mut pool) = fresh(32);
    let mut tree = BTree::create(&mut pool).unwrap();

    for index in 0..2_000u32 {
        tree.insert(&mut pool, &index.to_be_bytes(), &payload(index as u8, 200))
            .unwrap();
    }
    tree.check_tree(&mut pool).unwrap();

    for index in 0..2_000u32 {
        let value = tree.get(&mut pool, &index.to_be_bytes()).unwrap();
        assert_eq!(value.as_deref(), Some(payload(index as u8, 200).as_slice()));
    }
}

#[test]
fn descending_keys_always_fill_the_leftmost_page() {
    let (_dir, mut pool) = fresh(32);
    let mut tree = BTree::create(&mut pool).unwrap();

    for index in (0..2_000u32).rev() {
        tree.insert(&mut pool, &index.to_be_bytes(), &payload(index as u8, 200))
            .unwrap();
    }
    tree.check_tree(&mut pool).unwrap();

    let scanned = tree.iter(&mut pool).unwrap();
    assert_eq!(scanned.len(), 2_000);
    for (index, (key, _)) in scanned.iter().enumerate() {
        assert_eq!(key.as_slice(), &(index as u32).to_be_bytes()[..]);
    }
}

#[test]
fn keys_sharing_a_long_prefix() {
    let (_dir, mut pool) = fresh(32);
    let mut tree = BTree::create(&mut pool).unwrap();

    let prefix = vec![0xAB; 400];
    for index in 0..600u32 {
        let mut key = prefix.clone();
        key.extend_from_slice(&index.to_be_bytes());
        tree.insert(&mut pool, &key, b"v").unwrap();
    }
    tree.check_tree(&mut pool).unwrap();
    assert_eq!(tree.iter(&mut pool).unwrap().len(), 600);
}

#[test]
fn maximum_sized_keys_and_values() {
    let (_dir, mut pool) = fresh(32);
    let mut tree = BTree::create(&mut pool).unwrap();

    for index in 0..80u32 {
        let mut key = index.to_be_bytes().to_vec();
        key.resize(512, 0);
        tree.insert(&mut pool, &key, &payload(index as u8, 1024))
            .unwrap();
        tree.check_tree(&mut pool).unwrap();
    }
    assert_eq!(tree.iter(&mut pool).unwrap().len(), 80);
}

#[test]
fn oversized_keys_and_values_are_refused() {
    let (_dir, mut pool) = fresh(8);
    let mut tree = BTree::create(&mut pool).unwrap();

    assert!(tree.insert(&mut pool, &vec![0u8; 513], b"v").is_err());
    assert!(tree.insert(&mut pool, b"k", &vec![0u8; 1025]).is_err());
    tree.check_tree(&mut pool).unwrap();
}

#[test]
fn alternating_insert_and_delete_at_the_boundary() {
    // The pathological case for a naive threshold: an operation that repeatedly
    // crosses the split/merge boundary makes the tree churn on every step.
    let (_dir, mut pool) = fresh(16);
    let mut tree = BTree::create(&mut pool).unwrap();

    for index in 0..400u32 {
        tree.insert(&mut pool, &index.to_be_bytes(), &payload(1, 300))
            .unwrap();
    }
    let settled = tree.page_count(&mut pool).unwrap();

    for _ in 0..300 {
        tree.insert(&mut pool, &9999u32.to_be_bytes(), &payload(2, 300))
            .unwrap();
        tree.delete(&mut pool, &9999u32.to_be_bytes()).unwrap();
    }
    tree.check_tree(&mut pool).unwrap();

    // The point is that the churn is bounded, not that it is zero: whether the
    // split that a key causes can be undone by deleting it again depends on how
    // close the leaf was to the boundary. What must never happen is unbounded
    // growth from an operation that leaves the contents unchanged.
    let after = tree.page_count(&mut pool).unwrap();
    assert!(
        after <= settled + 1,
        "churning one key grew the tree from {settled} to {after} pages"
    );
}

// -- structural behaviour --------------------------------------------------

#[test]
fn an_empty_tree_is_one_leaf() {
    let (_dir, mut pool) = fresh(8);
    let tree = BTree::create(&mut pool).unwrap();

    assert_eq!(tree.get(&mut pool, b"absent").unwrap(), None);
    assert_eq!(tree.iter(&mut pool).unwrap(), vec![]);
    assert_eq!(tree.page_count(&mut pool).unwrap(), 1);
    tree.check_tree(&mut pool).unwrap();
}

#[test]
fn inserting_the_same_key_replaces_its_value() {
    let (_dir, mut pool) = fresh(8);
    let mut tree = BTree::create(&mut pool).unwrap();

    tree.insert(&mut pool, b"k", b"first").unwrap();
    tree.insert(&mut pool, b"k", b"second").unwrap();

    assert_eq!(
        tree.get(&mut pool, b"k").unwrap().as_deref(),
        Some(&b"second"[..])
    );
    assert_eq!(tree.iter(&mut pool).unwrap().len(), 1);
    tree.check_tree(&mut pool).unwrap();
}

#[test]
fn deleting_an_absent_key_reports_it() {
    let (_dir, mut pool) = fresh(8);
    let mut tree = BTree::create(&mut pool).unwrap();
    tree.insert(&mut pool, b"present", b"v").unwrap();

    assert!(!tree.delete(&mut pool, b"absent").unwrap());
    assert!(tree.delete(&mut pool, b"present").unwrap());
    assert!(!tree.delete(&mut pool, b"present").unwrap());
    tree.check_tree(&mut pool).unwrap();
}

#[test]
fn the_root_page_id_never_moves() {
    let (_dir, mut pool) = fresh(32);
    let mut tree = BTree::create(&mut pool).unwrap();
    let root = tree.root();

    // Grow several levels, then collapse all the way back.
    for index in 0..1_500u32 {
        tree.insert(&mut pool, &index.to_be_bytes(), &payload(index as u8, 250))
            .unwrap();
        assert_eq!(tree.root(), root);
    }
    for index in 0..1_500u32 {
        assert!(tree.delete(&mut pool, &index.to_be_bytes()).unwrap());
        assert_eq!(tree.root(), root);
    }

    tree.check_tree(&mut pool).unwrap();
    assert_eq!(tree.iter(&mut pool).unwrap(), vec![]);
    assert_eq!(
        tree.page_count(&mut pool).unwrap(),
        1,
        "an emptied tree must collapse back to a single leaf"
    );
}

#[test]
fn emptying_the_tree_returns_its_pages() {
    let (_dir, mut pool) = fresh(32);
    let mut tree = BTree::create(&mut pool).unwrap();

    for index in 0..1_200u32 {
        tree.insert(&mut pool, &index.to_be_bytes(), &payload(index as u8, 250))
            .unwrap();
    }
    let grown = pool.pager().page_count();
    assert!(grown > 20, "the tree should have spread over many pages");

    for index in 0..1_200u32 {
        tree.delete(&mut pool, &index.to_be_bytes()).unwrap();
    }
    tree.check_tree(&mut pool).unwrap();

    // The file never shrinks, but every page the tree gave up must be on the
    // freelist and ready for reuse.
    let freed = pool.pager().meta().freelist_count;
    assert!(
        freed as usize >= grown as usize - 3,
        "expected nearly every page back on the freelist, got {freed} of {grown}"
    );
    pool.check_invariants().unwrap();
}

#[test]
fn a_tree_survives_being_closed_and_reopened() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("reopen.lastro");

    let root = {
        let mut pool = BufferPool::new(Pager::create(&path).unwrap(), 16);
        let mut tree = BTree::create(&mut pool).unwrap();
        for index in 0..800u32 {
            tree.insert(&mut pool, &index.to_be_bytes(), &payload(index as u8, 200))
                .unwrap();
        }
        pool.pager_mut().meta_mut().catalog_root = tree.root();
        pool.flush_all().unwrap();
        tree.root()
    };

    let mut pool = BufferPool::new(Pager::open(&path).unwrap(), 16);
    assert_eq!(pool.pager().meta().catalog_root, root);

    let tree = BTree::open(root);
    tree.check_tree(&mut pool).unwrap();
    assert_eq!(tree.iter(&mut pool).unwrap().len(), 800);
    let expected = payload(500u32 as u8, 200);
    assert_eq!(
        tree.get(&mut pool, &500u32.to_be_bytes())
            .unwrap()
            .as_deref(),
        Some(expected.as_slice())
    );
}

#[test]
fn a_small_pool_still_serves_a_tree_several_levels_deep() {
    // Long keys keep the interior fanout low, so a few hundred entries already
    // build a tree three levels tall. Six frames is barely more than a descent
    // plus the pages a split touches: if a pin is ever leaked, this is where it
    // surfaces as AllFramesPinned rather than as a slow leak under load.
    let (_dir, mut pool) = fresh(6);
    let mut tree = BTree::create(&mut pool).unwrap();

    let key_of = |index: u32| {
        let mut key = index.to_be_bytes().to_vec();
        key.resize(400, 0);
        key
    };

    for index in 0..500u32 {
        tree.insert(&mut pool, &key_of(index), &payload(index as u8, 300))
            .unwrap();
    }
    tree.check_tree(&mut pool).unwrap();
    assert_eq!(tree.iter(&mut pool).unwrap().len(), 500);

    for index in 0..500u32 {
        assert!(tree.delete(&mut pool, &key_of(index)).unwrap());
    }
    tree.check_tree(&mut pool).unwrap();
    assert_eq!(tree.page_count(&mut pool).unwrap(), 1);
    pool.check_invariants().unwrap();
}
