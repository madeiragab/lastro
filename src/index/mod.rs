//! Access methods: the B+Tree that backs every index and the catalog.
//!
//! Nothing here knows what a transaction is. It understands ordered bytes, and
//! it gets them ordered because the key encoding in
//! [`crate::storage::page::encoding`] makes `memcmp` agree with logical order.

pub mod btree;
pub mod node;

pub use btree::{BTree, Entry, TreeStats};
