//! Cell formats for B+Tree pages, and the searches over them.
//!
//! A leaf cell carries a key and a value. An interior cell carries a child
//! pointer and a separator — the separator is not data, it only answers "left
//! or right", and need not exist in any leaf.
//!
//! See `docs/en/02-file-format.md` for the byte layouts and
//! `docs/en/04-btree.md` for the separator convention.

use std::cmp::Ordering;

use crate::storage::page::{get_varint, put_varint, Page};
use crate::PageId;

/// The largest key the index accepts.
///
/// Bounds the size of an interior cell, and so bounds fanout from below: with
/// 512-byte keys an interior page still holds seven separators, which keeps the
/// tree logarithmic. Longer keys need overflow chains, which are specified in
/// `docs/en/02-file-format.md` but not yet implemented.
pub const MAX_KEY: usize = 512;

/// The largest value the index accepts. Same reason as [`MAX_KEY`].
pub const MAX_VALUE: usize = 1024;

/// Bytes a varint occupies.
pub fn varint_len(value: u64) -> usize {
    let mut len = 1;
    let mut rest = value >> 7;
    while rest != 0 {
        len += 1;
        rest >>= 7;
    }
    len
}

// -- leaf cells ------------------------------------------------------------

/// `key_len` varint, key, `value_len` varint, value.
pub fn encode_leaf_cell(key: &[u8], value: &[u8], out: &mut Vec<u8>) {
    out.clear();
    put_varint(out, key.len() as u64);
    out.extend_from_slice(key);
    put_varint(out, value.len() as u64);
    out.extend_from_slice(value);
}

/// How many bytes [`encode_leaf_cell`] would produce.
pub fn leaf_cell_len(key: &[u8], value: &[u8]) -> usize {
    varint_len(key.len() as u64) + key.len() + varint_len(value.len() as u64) + value.len()
}

/// Splits a leaf cell back into its key and value.
pub fn decode_leaf_cell(cell: &[u8]) -> Option<(&[u8], &[u8])> {
    let (key_len, read) = get_varint(cell)?;
    let key_len = key_len as usize;
    let key = cell.get(read..read + key_len)?;
    let rest = cell.get(read + key_len..)?;
    let (value_len, read) = get_varint(rest)?;
    let value = rest.get(read..read + value_len as usize)?;
    Some((key, value))
}

/// Just the key of a leaf cell, without touching the value.
pub fn leaf_key(cell: &[u8]) -> Option<&[u8]> {
    let (key_len, read) = get_varint(cell)?;
    cell.get(read..read + key_len as usize)
}

// -- interior cells --------------------------------------------------------

/// `left_child` u32, `key_len` varint, separator.
pub fn encode_interior_cell(child: PageId, separator: &[u8], out: &mut Vec<u8>) {
    out.clear();
    out.extend_from_slice(&child.to_le_bytes());
    put_varint(out, separator.len() as u64);
    out.extend_from_slice(separator);
}

/// How many bytes [`encode_interior_cell`] would produce.
pub fn interior_cell_len(separator: &[u8]) -> usize {
    4 + varint_len(separator.len() as u64) + separator.len()
}

/// Splits an interior cell into its child pointer and separator.
pub fn decode_interior_cell(cell: &[u8]) -> Option<(PageId, &[u8])> {
    let head = cell.get(..4)?;
    let mut buf = [0u8; 4];
    buf.copy_from_slice(head);
    let child = PageId::from_le_bytes(buf);

    let rest = cell.get(4..)?;
    let (key_len, read) = get_varint(rest)?;
    let separator = rest.get(read..read + key_len as usize)?;
    Some((child, separator))
}

/// Just the separator of an interior cell.
pub fn interior_key(cell: &[u8]) -> Option<&[u8]> {
    decode_interior_cell(cell).map(|(_, separator)| separator)
}

// -- searches --------------------------------------------------------------

/// Binary search in a leaf.
///
/// Returns the index of an exact match with `true`, or the position the key
/// would be inserted at with `false`.
pub fn leaf_search(page: &Page, key: &[u8]) -> (u16, bool) {
    let mut low = 0u16;
    let mut high = page.slot_count();
    while low < high {
        let mid = low + (high - low) / 2;
        let cell = page.cell(mid).expect("btree pages have no dead slots");
        let mid_key = leaf_key(cell).expect("well formed leaf cell");
        match mid_key.cmp(key) {
            Ordering::Less => low = mid + 1,
            Ordering::Equal => return (mid, true),
            Ordering::Greater => high = mid,
        }
    }
    (low, false)
}

/// Picks the child of an interior node that may hold `key`.
///
/// The convention, fixed once and never deviated from: child *i* holds keys
/// below separator *i*, and child *i+1* holds keys at or above it. So the
/// answer is the first separator strictly greater than `key`, and the rightmost
/// child when there is none.
///
/// Returns the child's index — which equals `slot_count` for the rightmost
/// child, since that one lives in the page header rather than in a cell.
pub fn interior_child_index(page: &Page, key: &[u8]) -> u16 {
    let mut low = 0u16;
    let mut high = page.slot_count();
    while low < high {
        let mid = low + (high - low) / 2;
        let cell = page.cell(mid).expect("btree pages have no dead slots");
        let separator = interior_key(cell).expect("well formed interior cell");
        if separator <= key {
            low = mid + 1;
        } else {
            high = mid;
        }
    }
    low
}

/// The child page at `index`, where `slot_count` means the rightmost child.
pub fn child_at(page: &Page, index: u16) -> PageId {
    if index < page.slot_count() {
        let cell = page.cell(index).expect("btree pages have no dead slots");
        decode_interior_cell(cell)
            .expect("well formed interior cell")
            .0
    } else {
        page.extra()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::page::PageType;

    #[test]
    fn varint_len_matches_the_encoder() {
        for value in [0u64, 1, 127, 128, 16_383, 16_384, u64::MAX] {
            let mut buf = Vec::new();
            put_varint(&mut buf, value);
            assert_eq!(varint_len(value), buf.len(), "value {value}");
        }
    }

    #[test]
    fn leaf_cells_round_trip() {
        let mut cell = Vec::new();
        encode_leaf_cell(b"brinco", b"BR-0042", &mut cell);
        assert_eq!(cell.len(), leaf_cell_len(b"brinco", b"BR-0042"));
        assert_eq!(
            decode_leaf_cell(&cell),
            Some((&b"brinco"[..], &b"BR-0042"[..]))
        );
        assert_eq!(leaf_key(&cell), Some(&b"brinco"[..]));
    }

    #[test]
    fn empty_keys_and_values_round_trip() {
        let mut cell = Vec::new();
        encode_leaf_cell(b"", b"", &mut cell);
        assert_eq!(decode_leaf_cell(&cell), Some((&b""[..], &b""[..])));
    }

    #[test]
    fn interior_cells_round_trip() {
        let mut cell = Vec::new();
        encode_interior_cell(4242, b"sep", &mut cell);
        assert_eq!(cell.len(), interior_cell_len(b"sep"));
        assert_eq!(decode_interior_cell(&cell), Some((4242, &b"sep"[..])));
    }

    #[test]
    fn truncated_cells_decode_to_none() {
        let mut cell = Vec::new();
        encode_leaf_cell(b"key", b"value", &mut cell);
        for cut in 0..cell.len() {
            assert_eq!(decode_leaf_cell(&cell[..cut]), None, "cut at {cut}");
        }
    }

    fn leaf_with(keys: &[&[u8]]) -> Page {
        let mut page = Page::zeroed();
        page.init(PageType::Leaf);
        let mut cell = Vec::new();
        for key in keys {
            encode_leaf_cell(key, b"v", &mut cell);
            page.push_cell(&cell).unwrap();
        }
        page
    }

    fn interior_with(separators: &[&[u8]]) -> Page {
        let mut page = Page::zeroed();
        page.init(PageType::Interior);
        let mut cell = Vec::new();
        for (index, separator) in separators.iter().enumerate() {
            encode_interior_cell(index as PageId + 1, separator, &mut cell);
            page.push_cell(&cell).unwrap();
        }
        page.set_extra(separators.len() as PageId + 1);
        page
    }

    #[test]
    fn leaf_search_finds_and_places() {
        let page = leaf_with(&[b"b", b"d", b"f"]);
        assert_eq!(leaf_search(&page, b"b"), (0, true));
        assert_eq!(leaf_search(&page, b"d"), (1, true));
        assert_eq!(leaf_search(&page, b"f"), (2, true));

        assert_eq!(leaf_search(&page, b"a"), (0, false));
        assert_eq!(leaf_search(&page, b"c"), (1, false));
        assert_eq!(leaf_search(&page, b"e"), (2, false));
        assert_eq!(leaf_search(&page, b"g"), (3, false));
    }

    #[test]
    fn leaf_search_on_an_empty_page() {
        let page = leaf_with(&[]);
        assert_eq!(leaf_search(&page, b"anything"), (0, false));
    }

    #[test]
    fn interior_search_respects_the_half_open_convention() {
        // separators b and d, so children hold: <b, [b,d), >=d
        let page = interior_with(&[b"b", b"d"]);

        assert_eq!(interior_child_index(&page, b"a"), 0);
        // The boundary case: a key equal to the separator belongs on the right.
        assert_eq!(interior_child_index(&page, b"b"), 1);
        assert_eq!(interior_child_index(&page, b"c"), 1);
        assert_eq!(interior_child_index(&page, b"d"), 2);
        assert_eq!(interior_child_index(&page, b"z"), 2);

        assert_eq!(child_at(&page, 0), 1);
        assert_eq!(child_at(&page, 1), 2);
        assert_eq!(child_at(&page, 2), 3, "the rightmost child lives in extra");
    }

    #[test]
    fn interior_search_on_a_single_child_node() {
        let page = interior_with(&[]);
        assert_eq!(interior_child_index(&page, b"anything"), 0);
        assert_eq!(child_at(&page, 0), 1);
    }
}
