//! On-disk page layout and encodings.
//!
//! Implements the formats specified in `docs/en/02-file-format.md`.

pub mod encoding;
pub mod layout;

pub use encoding::{
    decode_key, decode_tuple, encode_key, encode_tuple, get_varint, put_varint, Value, ValueType,
};
pub use layout::{
    Page, PageType, MAX_CELL, MAX_INLINE_CELL, PAGE_HEADER_SIZE, SLOT_SIZE, USABLE_SPACE,
};
