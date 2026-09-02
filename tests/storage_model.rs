//! The pager and buffer pool, checked against an in-memory model.
//!
//! The oracle is a `HashMap<PageId, Vec<u8>>` holding what each page should
//! contain. Every operation is applied to both, and the whole map is verified
//! after each one. A divergence is a bug in the cache, the eviction path, or
//! the freelist.

use std::collections::HashMap;

use lastro::storage::page::PageType;
use lastro::storage::{BufferPool, Pager};
use lastro::PageId;
use proptest::prelude::*;

#[derive(Debug, Clone)]
enum Op {
    /// Allocate a page and write a marker into it.
    Create(u8),
    /// Overwrite a known page with a new marker.
    Overwrite(usize, u8),
    /// Read a known page back.
    Read(usize),
    /// Return a known page to the freelist.
    Free(usize),
    /// Flush everything and sync.
    Flush,
    /// Close the pool and reopen it from the file.
    Reopen,
}

fn any_op() -> impl Strategy<Value = Op> {
    prop_oneof![
        5 => any::<u8>().prop_map(Op::Create),
        4 => (0usize..40, any::<u8>()).prop_map(|(i, m)| Op::Overwrite(i, m)),
        4 => (0usize..40).prop_map(Op::Read),
        2 => (0usize..40).prop_map(Op::Free),
        1 => Just(Op::Flush),
        1 => Just(Op::Reopen),
    ]
}

/// The payload written for a marker. Long enough to span several slots so that
/// page-level bookkeeping is exercised, short enough to always fit.
fn payload(marker: u8) -> Vec<u8> {
    vec![marker; 64 + marker as usize]
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(96))]

    #[test]
    fn pool_agrees_with_the_model(
        ops in prop::collection::vec(any_op(), 0..120),
        capacity in 1usize..6,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("model.lastro");

        let mut pool = BufferPool::new(Pager::create(&path).unwrap(), capacity);
        let mut model: HashMap<PageId, Vec<u8>> = HashMap::new();
        let mut known: Vec<PageId> = Vec::new();

        for op in ops {
            match op {
                Op::Create(marker) => {
                    let pin = pool.new_page(PageType::Heap).unwrap();
                    let id = pin.page_id;
                    let bytes = payload(marker);
                    pool.page_mut(&pin).push_cell(&bytes).unwrap();
                    pool.unpin(pin);
                    model.insert(id, bytes);
                    known.push(id);
                }
                Op::Overwrite(index, marker) => {
                    if let Some(&id) = known.get(index) {
                        let pin = pool.fetch(id).unwrap();
                        let bytes = payload(marker);
                        let page = pool.page_mut(&pin);
                        page.init(PageType::Heap);
                        page.push_cell(&bytes).unwrap();
                        pool.unpin(pin);
                        model.insert(id, bytes);
                    }
                }
                Op::Read(index) => {
                    if let Some(&id) = known.get(index) {
                        let pin = pool.fetch(id).unwrap();
                        let expected = model.get(&id).unwrap();
                        prop_assert_eq!(pool.page(&pin).cell(0), Some(expected.as_slice()));
                        pool.unpin(pin);
                    }
                }
                Op::Free(index) => {
                    if index < known.len() {
                        let id = known.remove(index);
                        let pin = pool.fetch(id).unwrap();
                        pool.free_page(pin).unwrap();
                        model.remove(&id);
                    }
                }
                Op::Flush => {
                    pool.flush_all().unwrap();
                }
                Op::Reopen => {
                    pool.flush_all().unwrap();
                    drop(pool);
                    pool = BufferPool::new(Pager::open(&path).unwrap(), capacity);
                }
            }

            pool.check_invariants().unwrap();
        }

        // Everything the model still holds must survive a flush and a reopen.
        pool.flush_all().unwrap();
        drop(pool);

        let mut pool = BufferPool::new(Pager::open(&path).unwrap(), capacity);
        for (&id, expected) in &model {
            let pin = pool.fetch(id).unwrap();
            prop_assert_eq!(pool.page(&pin).cell(0), Some(expected.as_slice()));
            pool.page(&pin).check_invariants().unwrap();
            pool.unpin(pin);
        }
        pool.check_invariants().unwrap();
    }
}

#[test]
fn freed_pages_are_reused_before_the_file_grows() {
    let dir = tempfile::tempdir().unwrap();
    let pager = Pager::create(dir.path().join("reuse.lastro")).unwrap();
    let mut pool = BufferPool::new(pager, 8);

    let mut ids = Vec::new();
    for _ in 0..20 {
        let pin = pool.new_page(PageType::Heap).unwrap();
        ids.push(pin.page_id);
        pool.unpin(pin);
    }
    let high_water = pool.pager().page_count();

    for id in &ids {
        let pin = pool.fetch(*id).unwrap();
        pool.free_page(pin).unwrap();
    }
    assert_eq!(pool.pager().meta().freelist_count, 20);

    for _ in 0..20 {
        let pin = pool.new_page(PageType::Heap).unwrap();
        pool.unpin(pin);
    }
    assert_eq!(
        pool.pager().page_count(),
        high_water,
        "twenty freed pages must satisfy twenty allocations"
    );
    pool.check_invariants().unwrap();
}

#[test]
fn a_page_written_through_a_full_pool_survives() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("evict.lastro");

    // One frame only: every operation evicts the previous page.
    let mut pool = BufferPool::new(Pager::create(&path).unwrap(), 1);

    let mut ids = Vec::new();
    for index in 0..50u32 {
        let pin = pool.new_page(PageType::Heap).unwrap();
        ids.push(pin.page_id);
        pool.page_mut(&pin).push_cell(&index.to_le_bytes()).unwrap();
        pool.unpin(pin);
    }
    pool.flush_all().unwrap();
    drop(pool);

    let mut pool = BufferPool::new(Pager::open(&path).unwrap(), 1);
    for (index, id) in ids.iter().enumerate() {
        let pin = pool.fetch(*id).unwrap();
        let expected = (index as u32).to_le_bytes();
        assert_eq!(pool.page(&pin).cell(0), Some(&expected[..]));
        pool.unpin(pin);
    }
    pool.check_invariants().unwrap();
}
