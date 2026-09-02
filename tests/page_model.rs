//! The slotted page, checked against a `Vec<Option<Vec<u8>>>` model.
//!
//! Every operation is applied to both and the states compared. A divergence is
//! a bug, and proptest shrinks the sequence to the smallest failing example.

use lastro::storage::page::{Page, PageType};
use proptest::prelude::*;

#[derive(Debug, Clone)]
enum Op {
    Push(Vec<u8>),
    Delete(u16),
    Compact,
}

fn any_op() -> impl Strategy<Value = Op> {
    prop_oneof![
        4 => prop::collection::vec(any::<u8>(), 0..400).prop_map(Op::Push),
        2 => (0u16..48).prop_map(Op::Delete),
        1 => Just(Op::Compact),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    #[test]
    fn page_agrees_with_the_model(ops in prop::collection::vec(any_op(), 0..200)) {
        let mut page = Page::zeroed();
        page.init(PageType::Leaf);
        let mut model: Vec<Option<Vec<u8>>> = Vec::new();

        for op in ops {
            match op {
                Op::Push(bytes) => {
                    // A failed push means the page is full, and the model does
                    // not grow either.
                    if let Ok(slot) = page.push_cell(&bytes) {
                        prop_assert_eq!(slot as usize, model.len());
                        model.push(Some(bytes));
                    }
                }
                Op::Delete(slot) => {
                    if (slot as usize) < model.len() {
                        page.delete_cell(slot).unwrap();
                        model[slot as usize] = None;
                    }
                }
                Op::Compact => page.compact(),
            }

            page.check_invariants().unwrap();
            prop_assert_eq!(page.slot_count() as usize, model.len());
        }

        for (index, expected) in model.iter().enumerate() {
            prop_assert_eq!(page.cell(index as u16), expected.as_deref());
        }

        let live_model = model.iter().filter(|cell| cell.is_some()).count();
        prop_assert_eq!(page.live_count() as usize, live_model);
    }

    #[test]
    fn compaction_never_changes_what_is_readable(
        cells in prop::collection::vec(prop::collection::vec(any::<u8>(), 0..200), 0..30),
        to_delete in prop::collection::vec(0u16..30, 0..15),
    ) {
        let mut page = Page::zeroed();
        page.init(PageType::Leaf);

        let mut accepted = 0u16;
        for bytes in &cells {
            if page.push_cell(bytes).is_ok() {
                accepted += 1;
            }
        }
        for slot in &to_delete {
            if *slot < accepted {
                page.delete_cell(*slot).unwrap();
            }
        }

        let before: Vec<Option<Vec<u8>>> = (0..page.slot_count())
            .map(|slot| page.cell(slot).map(|bytes| bytes.to_vec()))
            .collect();
        let free_before = page.total_free();

        page.compact();
        page.check_invariants().unwrap();

        let after: Vec<Option<Vec<u8>>> = (0..page.slot_count())
            .map(|slot| page.cell(slot).map(|bytes| bytes.to_vec()))
            .collect();

        prop_assert_eq!(before, after, "compaction must not change cell contents");
        prop_assert_eq!(page.fragmented(), 0);
        prop_assert_eq!(page.total_free(), free_before, "no space may be lost");
    }
}
