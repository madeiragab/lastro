//! The order-preserving key encoding, checked by property.
//!
//! The claim under test is narrow and total: sorting encoded keys byte-wise
//! must produce the same order as sorting the values logically. If that holds,
//! the B+Tree never needs to know a type.

use lastro::storage::page::{decode_key, decode_tuple, encode_key, encode_tuple, Value, ValueType};
use proptest::prelude::*;

fn key_of(values: &[Value]) -> Vec<u8> {
    let mut out = Vec::new();
    encode_key(values, &mut out).expect("no NaN in these strategies");
    out
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    #[test]
    fn integers_sort_as_bytes(values in prop::collection::vec(any::<i64>(), 0..64)) {
        let mut encoded: Vec<Vec<u8>> = values.iter().map(|v| key_of(&[Value::Int(*v)])).collect();
        encoded.sort();

        let mut sorted = values.clone();
        sorted.sort_unstable();
        let expected: Vec<Vec<u8>> = sorted.iter().map(|v| key_of(&[Value::Int(*v)])).collect();

        prop_assert_eq!(encoded, expected);
    }

    #[test]
    fn byte_strings_sort_as_bytes(values in prop::collection::vec(
        prop::collection::vec(any::<u8>(), 0..12), 0..40
    )) {
        let mut encoded: Vec<Vec<u8>> =
            values.iter().map(|v| key_of(&[Value::Blob(v.clone())])).collect();
        encoded.sort();

        let mut sorted = values.clone();
        sorted.sort();
        let expected: Vec<Vec<u8>> =
            sorted.iter().map(|v| key_of(&[Value::Blob(v.clone())])).collect();

        prop_assert_eq!(encoded, expected, "embedded NULs must not break ordering");
    }

    #[test]
    fn finite_floats_sort_as_bytes(values in prop::collection::vec(-1e18f64..1e18f64, 0..64)) {
        let mut encoded: Vec<Vec<u8>> = values.iter().map(|v| key_of(&[Value::Real(*v)])).collect();
        encoded.sort();

        let mut sorted = values.clone();
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let expected: Vec<Vec<u8>> = sorted.iter().map(|v| key_of(&[Value::Real(*v)])).collect();

        prop_assert_eq!(encoded, expected);
    }

    #[test]
    fn composite_keys_sort_column_by_column(
        values in prop::collection::vec((any::<i64>(), "[a-c]{0,6}"), 0..40)
    ) {
        let build = |pair: &(i64, String)| {
            key_of(&[Value::Int(pair.0), Value::Text(pair.1.clone())])
        };

        let mut encoded: Vec<Vec<u8>> = values.iter().map(&build).collect();
        encoded.sort();

        let mut sorted = values.clone();
        sorted.sort();
        let expected: Vec<Vec<u8>> = sorted.iter().map(&build).collect();

        prop_assert_eq!(encoded, expected);
    }

    #[test]
    fn nulls_sort_before_every_value(value in any::<i64>()) {
        let null_key = key_of(&[Value::Null]);
        let real_key = key_of(&[Value::Int(value)]);
        prop_assert!(null_key < real_key);
    }

    #[test]
    fn keys_round_trip(
        int in any::<i64>(),
        text in "[a-z]{0,10}",
        present in any::<bool>(),
    ) {
        let values = vec![
            Value::Int(int),
            if present { Value::Text(text) } else { Value::Null },
        ];
        let types = [ValueType::Int, ValueType::Text];
        prop_assert_eq!(decode_key(&key_of(&values), &types), Some(values));
    }

    #[test]
    fn tuples_round_trip(
        int in any::<i64>(),
        real in any::<f64>().prop_filter("finite", |v| v.is_finite()),
        text in "[a-z ]{0,20}",
        blob in prop::collection::vec(any::<u8>(), 0..30),
        flag in any::<bool>(),
        null_at in 0usize..5,
    ) {
        let mut values = vec![
            Value::Int(int),
            Value::Real(real),
            Value::Text(text),
            Value::Blob(blob),
            Value::Bool(flag),
        ];
        values[null_at] = Value::Null;

        let types = [
            ValueType::Int,
            ValueType::Real,
            ValueType::Text,
            ValueType::Blob,
            ValueType::Bool,
        ];

        let mut buf = Vec::new();
        encode_tuple(&values, &mut buf);
        prop_assert_eq!(decode_tuple(&buf, &types), Some(values));
    }

    #[test]
    fn truncated_tuples_never_panic(
        values in prop::collection::vec(any::<u8>(), 0..80),
    ) {
        let types = [ValueType::Int, ValueType::Text, ValueType::Bool];
        // Arbitrary bytes are not a valid tuple. Decoding must return None
        // rather than panicking or reading out of bounds.
        let _ = decode_tuple(&values, &types);
    }
}
