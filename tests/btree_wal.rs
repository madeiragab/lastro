//! The B+Tree through the write-ahead log.
//!
//! The tree does not know the log exists. Every mutation it makes goes through
//! the buffer pool, and the pool records what changed and logs it when the
//! operation closes. These tests check that the arrangement actually holds up:
//! a committed tree survives losing its pages, and an uncommitted one leaves
//! nothing behind.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use lastro::index::BTree;
use lastro::storage::{BufferPool, Pager};
use lastro::wal::{recover, Wal};

fn temp() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("tree.lastro");
    (dir, path)
}

/// Opens the database and its log, running recovery the way a real open would.
fn open(path: &Path, capacity: usize) -> BufferPool {
    let pager = Pager::open_or_create(path).unwrap();
    // The log's numbering continues across checkpoints, and where it continues
    // from lives in the metadata page.
    let base = pager.meta().last_checkpoint_lsn;
    let mut pool = BufferPool::new(pager, capacity);
    pool.attach_wal(Wal::open(Wal::path_for(path), base).unwrap());
    recover(&mut pool).unwrap();
    pool
}

fn payload(seed: u32) -> Vec<u8> {
    (0..200u32).map(|i| (seed.wrapping_add(i)) as u8).collect()
}

/// Builds a tree and records its root in the metadata page, so a later open can
/// find it again.
fn plant(pool: &mut BufferPool) -> BTree {
    pool.begin_transaction().unwrap();
    let tree = BTree::create(pool).unwrap();
    pool.pager_mut().meta_mut().catalog_root = tree.root();
    pool.commit_transaction().unwrap();
    lastro::wal::checkpoint(pool).unwrap();
    tree
}

#[test]
fn a_committed_tree_survives_losing_every_page() {
    let (_dir, path) = temp();

    let root = {
        let mut pool = open(&path, 64);
        let mut tree = plant(&mut pool);

        pool.begin_transaction().unwrap();
        for key in 0..600u32 {
            tree.insert(&mut pool, &key.to_be_bytes(), &payload(key))
                .unwrap();
        }
        pool.commit_transaction().unwrap();

        // The commit is durable. The pages are not: the pool is dropped without
        // a flush, so every dirty page in memory is simply gone.
        drop(pool);
        tree.root()
    };

    let mut pool = open(&path, 64);
    let tree = BTree::open(root);
    tree.check_tree(&mut pool).unwrap();

    let entries = tree.iter(&mut pool).unwrap();
    assert_eq!(entries.len(), 600, "redo had to rebuild the whole tree");
    for key in 0..600u32 {
        let value = tree.get(&mut pool, &key.to_be_bytes()).unwrap();
        assert_eq!(value.as_deref(), Some(payload(key).as_slice()));
    }
}

#[test]
fn an_uncommitted_tree_leaves_nothing_behind() {
    let (_dir, path) = temp();

    let root = {
        let mut pool = open(&path, 8);
        let mut tree = plant(&mut pool);

        pool.begin_transaction().unwrap();
        for key in 0..400u32 {
            tree.insert(&mut pool, &key.to_be_bytes(), &payload(key))
                .unwrap();
        }
        // No commit. A small pool means many of these pages were evicted and
        // are already on disk, which is the steal policy doing its job.
        pool.wal_mut().unwrap().sync().unwrap();
        drop(pool);
        tree.root()
    };

    let mut pool = open(&path, 8);
    let tree = BTree::open(root);
    tree.check_tree(&mut pool).unwrap();
    assert_eq!(
        tree.iter(&mut pool).unwrap(),
        vec![],
        "an uncommitted tree must come back empty"
    );
}

#[test]
fn a_rollback_leaves_the_tree_as_it_was() {
    let (_dir, path) = temp();
    let mut pool = open(&path, 32);
    let mut tree = plant(&mut pool);

    pool.begin_transaction().unwrap();
    for key in 0..300u32 {
        tree.insert(&mut pool, &key.to_be_bytes(), &payload(key))
            .unwrap();
    }
    pool.commit_transaction().unwrap();

    let committed = tree.iter(&mut pool).unwrap();
    assert_eq!(committed.len(), 300);

    // A second transaction changes a lot and then gives up.
    pool.begin_transaction().unwrap();
    for key in 300..600u32 {
        tree.insert(&mut pool, &key.to_be_bytes(), &payload(key))
            .unwrap();
    }
    for key in 0..150u32 {
        assert!(tree.delete(&mut pool, &key.to_be_bytes()).unwrap());
    }
    pool.rollback_transaction().unwrap();

    tree.check_tree(&mut pool).unwrap();
    assert_eq!(
        tree.iter(&mut pool).unwrap(),
        committed,
        "rollback must restore exactly what was committed"
    );
}

#[test]
fn a_crash_between_transactions_keeps_every_committed_one() {
    let (_dir, path) = temp();
    let mut model: BTreeMap<Vec<u8>, Vec<u8>> = BTreeMap::new();

    let root = {
        let mut pool = open(&path, 32);
        let tree = plant(&mut pool);
        tree.root()
    };

    // Ten transactions, each one crashing the process right after its commit.
    for round in 0..10u32 {
        let mut pool = open(&path, 32);
        let mut tree = BTree::open(root);

        pool.begin_transaction().unwrap();
        for step in 0..40u32 {
            let key = round * 40 + step;
            let value = payload(key);
            tree.insert(&mut pool, &key.to_be_bytes(), &value).unwrap();
            model.insert(key.to_be_bytes().to_vec(), value);
        }
        pool.commit_transaction().unwrap();
        drop(pool);
    }

    let mut pool = open(&path, 32);
    let tree = BTree::open(root);
    tree.check_tree(&mut pool).unwrap();

    let entries = tree.iter(&mut pool).unwrap();
    let expected: Vec<(Vec<u8>, Vec<u8>)> = model.into_iter().collect();
    assert_eq!(
        entries.len(),
        expected.len(),
        "every committed round must be present"
    );
    for (index, (found, wanted)) in entries.iter().zip(&expected).enumerate() {
        assert_eq!(found.0, wanted.0, "key {index} differs");
        assert_eq!(
            found.1.len(),
            wanted.1.len(),
            "value at key {index} differs"
        );
    }
    assert_eq!(entries, expected);
}

#[test]
fn deletes_are_logged_and_recovered_too() {
    let (_dir, path) = temp();

    let root = {
        let mut pool = open(&path, 32);
        let mut tree = plant(&mut pool);

        pool.begin_transaction().unwrap();
        for key in 0..500u32 {
            tree.insert(&mut pool, &key.to_be_bytes(), &payload(key))
                .unwrap();
        }
        pool.commit_transaction().unwrap();
        lastro::wal::checkpoint(&mut pool).unwrap();

        // Deleting most of it forces merges, which touch several pages per
        // operation. Those are what the edit session has to catch.
        pool.begin_transaction().unwrap();
        for key in 0..500u32 {
            if key % 5 != 0 {
                assert!(tree.delete(&mut pool, &key.to_be_bytes()).unwrap());
            }
        }
        pool.commit_transaction().unwrap();
        drop(pool);
        tree.root()
    };

    let mut pool = open(&path, 32);
    let tree = BTree::open(root);
    tree.check_tree(&mut pool).unwrap();

    let entries = tree.iter(&mut pool).unwrap();
    assert_eq!(entries.len(), 100);
    for (index, (key, _)) in entries.iter().enumerate() {
        let expected = (index as u32 * 5).to_be_bytes();
        assert_eq!(key.as_slice(), &expected[..]);
    }
}

#[test]
fn logging_a_tree_costs_far_less_than_its_pages() {
    let (_dir, path) = temp();
    let mut pool = open(&path, 64);
    let mut tree = plant(&mut pool);

    // One key at a time, each its own transaction, so the log carries the cost
    // of many small edits rather than one bulk load.
    let mut total = 0u64;
    for key in 0..200u32 {
        pool.begin_transaction().unwrap();
        let before = pool.wal().unwrap().end_lsn();
        tree.insert(&mut pool, &key.to_be_bytes(), &payload(key))
            .unwrap();
        total += pool.wal().unwrap().end_lsn() - before;
        pool.commit_transaction().unwrap();
    }

    // Each insert stores about 200 bytes, logged as a before and an after
    // image. Whole-page logging would cost 4096 bytes per touched page; this
    // must land nowhere near that.
    let per_insert = total / 200;
    assert!(
        per_insert < 1500,
        "each insert cost {per_insert} bytes of log"
    );
}

#[test]
fn a_transaction_cannot_be_opened_twice() {
    let (_dir, path) = temp();
    let mut pool = open(&path, 8);

    pool.begin_transaction().unwrap();
    assert!(pool.begin_transaction().is_err(), "single writer only");
    pool.commit_transaction().unwrap();
    assert!(pool.commit_transaction().is_err(), "nothing is open now");
}
