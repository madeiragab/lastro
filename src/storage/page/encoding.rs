//! On-disk encodings: varints, order-preserving keys, and tuples.
//!
//! The key encoding is the reason the B+Tree never needs to know a type. If
//! `memcmp` over encoded keys yields the logical order of the values, the whole
//! index reduces to byte comparison. See `docs/en/02-file-format.md`.

use crate::{Error, Result};

/// A SQL value.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    /// The absence of a value. Sorts before everything else.
    Null,
    /// A signed 64-bit integer.
    Int(i64),
    /// A 64-bit float. `NaN` is not a legal key.
    Real(f64),
    /// UTF-8 text.
    Text(String),
    /// An opaque byte string.
    Blob(Vec<u8>),
    /// A boolean.
    Bool(bool),
}

/// The type of a column, needed to decode a value back from its bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ValueType {
    /// See [`Value::Int`].
    Int,
    /// See [`Value::Real`].
    Real,
    /// See [`Value::Text`].
    Text,
    /// See [`Value::Blob`].
    Blob,
    /// See [`Value::Bool`].
    Bool,
}

/// Marks an absent value in an encoded key.
pub const TAG_NULL: u8 = 0x00;
/// Marks a present value in an encoded key.
pub const TAG_PRESENT: u8 = 0x01;

// -- varint ---------------------------------------------------------------

/// Appends `value` as a LEB128 varint: seven payload bits per byte, high bit
/// signalling continuation.
pub fn put_varint(out: &mut Vec<u8>, value: u64) {
    let mut rest = value;
    while rest >= 0x80 {
        out.push((rest as u8) | 0x80);
        rest >>= 7;
    }
    out.push(rest as u8);
}

/// Reads a varint from the front of `buf`, returning it and how many bytes it
/// occupied. `None` if the buffer ends mid-varint or the value overflows 64 bits.
pub fn get_varint(buf: &[u8]) -> Option<(u64, usize)> {
    let mut result: u64 = 0;
    let mut shift: u32 = 0;
    for (index, &byte) in buf.iter().enumerate() {
        if shift >= 64 || (shift == 63 && byte > 1) {
            return None;
        }
        result |= ((byte & 0x7F) as u64) << shift;
        if byte & 0x80 == 0 {
            return Some((result, index + 1));
        }
        shift += 7;
    }
    None
}

// -- order-preserving scalars ---------------------------------------------

/// Encodes an `i64` so that byte comparison matches numeric comparison.
///
/// Flipping the sign bit moves negatives, whose two's complement form starts
/// with a one bit, below the positives. Big-endian puts the most significant
/// byte first, so `memcmp` reaches it before any other.
///
/// ```
/// use lastro::storage::page::encoding::encode_i64;
/// assert!(encode_i64(-1) < encode_i64(0));
/// assert!(encode_i64(0) < encode_i64(1));
/// assert!(encode_i64(i64::MIN) < encode_i64(i64::MAX));
/// ```
pub fn encode_i64(value: i64) -> [u8; 8] {
    ((value as u64) ^ (1u64 << 63)).to_be_bytes()
}

/// The inverse of [`encode_i64`].
pub fn decode_i64(bytes: [u8; 8]) -> i64 {
    (u64::from_be_bytes(bytes) ^ (1u64 << 63)) as i64
}

/// Encodes an `f64` so that byte comparison matches numeric comparison.
///
/// Positive numbers need only their sign bit flipped. Negative numbers need
/// every bit flipped, because IEEE 754 orders negative magnitudes backwards.
///
/// `-0.0` is canonicalized to `0.0` so that two values that compare equal also
/// encode equal. `NaN` is rejected: a value that is not equal to itself has no
/// place in a search tree.
pub fn encode_f64(value: f64) -> Result<[u8; 8]> {
    if value.is_nan() {
        return Err(Error::InvalidKey("NaN cannot be used as a key"));
    }
    let value = if value == 0.0 { 0.0 } else { value };
    let bits = value.to_bits();
    let encoded = if value.is_sign_negative() {
        !bits
    } else {
        bits ^ (1u64 << 63)
    };
    Ok(encoded.to_be_bytes())
}

/// The inverse of [`encode_f64`].
pub fn decode_f64(bytes: [u8; 8]) -> f64 {
    let encoded = u64::from_be_bytes(bytes);
    let bits = if encoded & (1u64 << 63) != 0 {
        encoded ^ (1u64 << 63)
    } else {
        !encoded
    };
    f64::from_bits(bits)
}

/// Encodes a byte string so that byte comparison matches lexicographic order,
/// and so that a prefix always sorts before what extends it.
///
/// A literal `0x00` is escaped to `0x00 0xFF`, which frees `0x00 0x00` to act as
/// an unambiguous terminator. Without the terminator, `"abc"` and `"abcd"` would
/// be indistinguishable inside a composite key.
pub fn encode_bytes(bytes: &[u8], out: &mut Vec<u8>) {
    for &byte in bytes {
        out.push(byte);
        if byte == 0x00 {
            out.push(0xFF);
        }
    }
    out.push(0x00);
    out.push(0x00);
}

/// The inverse of [`encode_bytes`], returning the bytes and how many were read.
pub fn decode_bytes(buf: &[u8]) -> Option<(Vec<u8>, usize)> {
    let mut out = Vec::new();
    let mut index = 0;
    while index < buf.len() {
        let byte = buf[index];
        if byte != 0x00 {
            out.push(byte);
            index += 1;
            continue;
        }
        match *buf.get(index + 1)? {
            0x00 => return Some((out, index + 2)),
            0xFF => {
                out.push(0x00);
                index += 2;
            }
            _ => return None,
        }
    }
    None
}

// -- keys ------------------------------------------------------------------

/// Encodes a composite key. Each column is preceded by a presence byte, so
/// nulls sort before every real value, and the concatenation of self-delimiting
/// encodings still preserves order.
pub fn encode_key(values: &[Value], out: &mut Vec<u8>) -> Result<()> {
    for value in values {
        match value {
            Value::Null => out.push(TAG_NULL),
            Value::Int(v) => {
                out.push(TAG_PRESENT);
                out.extend_from_slice(&encode_i64(*v));
            }
            Value::Real(v) => {
                out.push(TAG_PRESENT);
                out.extend_from_slice(&encode_f64(*v)?);
            }
            Value::Text(v) => {
                out.push(TAG_PRESENT);
                encode_bytes(v.as_bytes(), out);
            }
            Value::Blob(v) => {
                out.push(TAG_PRESENT);
                encode_bytes(v, out);
            }
            Value::Bool(v) => {
                out.push(TAG_PRESENT);
                out.push(if *v { 1 } else { 0 });
            }
        }
    }
    Ok(())
}

/// Decodes a composite key given the column types it was built from.
///
/// Only used for debugging and for tests: nothing on the hot path ever needs to
/// turn a key back into values.
pub fn decode_key(buf: &[u8], types: &[ValueType]) -> Option<Vec<Value>> {
    let mut out = Vec::with_capacity(types.len());
    let mut rest = buf;
    for &column_type in types {
        let (&tag, tail) = rest.split_first()?;
        rest = tail;
        if tag == TAG_NULL {
            out.push(Value::Null);
            continue;
        }
        if tag != TAG_PRESENT {
            return None;
        }
        match column_type {
            ValueType::Int => {
                let (head, tail) = split_array(rest)?;
                out.push(Value::Int(decode_i64(head)));
                rest = tail;
            }
            ValueType::Real => {
                let (head, tail) = split_array(rest)?;
                out.push(Value::Real(decode_f64(head)));
                rest = tail;
            }
            ValueType::Text => {
                let (bytes, used) = decode_bytes(rest)?;
                out.push(Value::Text(String::from_utf8(bytes).ok()?));
                rest = &rest[used..];
            }
            ValueType::Blob => {
                let (bytes, used) = decode_bytes(rest)?;
                out.push(Value::Blob(bytes));
                rest = &rest[used..];
            }
            ValueType::Bool => {
                let (&byte, tail) = rest.split_first()?;
                out.push(Value::Bool(byte != 0));
                rest = tail;
            }
        }
    }
    Some(out)
}

fn split_array(buf: &[u8]) -> Option<([u8; 8], &[u8])> {
    if buf.len() < 8 {
        return None;
    }
    let mut head = [0u8; 8];
    head.copy_from_slice(&buf[..8]);
    Some((head, &buf[8..]))
}

// -- tuples ----------------------------------------------------------------

/// Encodes a heap tuple.
///
/// Heap tuples never need order preservation, so the format is the cheap one: a
/// column count, a null bitmap, then the present values in schema order. Null
/// columns occupy no bytes at all — the bitmap already said they are absent.
///
/// The column count is stored so that `ALTER TABLE ADD COLUMN` need not rewrite
/// old tuples: a tuple with fewer columns than the current schema yields nulls
/// for the ones it predates.
pub fn encode_tuple(values: &[Value], out: &mut Vec<u8>) {
    put_varint(out, values.len() as u64);

    let bitmap_len = values.len().div_ceil(8);
    let bitmap_at = out.len();
    out.resize(bitmap_at + bitmap_len, 0);

    for (index, value) in values.iter().enumerate() {
        if matches!(value, Value::Null) {
            out[bitmap_at + index / 8] |= 1 << (index % 8);
            continue;
        }
        match value {
            Value::Null => unreachable!("handled above"),
            Value::Int(v) => out.extend_from_slice(&v.to_le_bytes()),
            Value::Real(v) => out.extend_from_slice(&v.to_le_bytes()),
            Value::Bool(v) => out.push(if *v { 1 } else { 0 }),
            Value::Text(v) => {
                put_varint(out, v.len() as u64);
                out.extend_from_slice(v.as_bytes());
            }
            Value::Blob(v) => {
                put_varint(out, v.len() as u64);
                out.extend_from_slice(v);
            }
        }
    }
}

/// Decodes a heap tuple against the current schema.
///
/// Columns the stored tuple predates come back as [`Value::Null`].
pub fn decode_tuple(buf: &[u8], types: &[ValueType]) -> Option<Vec<Value>> {
    let (stored_columns, mut offset) = get_varint(buf)?;
    let stored_columns = stored_columns as usize;

    let bitmap_len = stored_columns.div_ceil(8);
    if offset + bitmap_len > buf.len() {
        return None;
    }
    let bitmap = &buf[offset..offset + bitmap_len];
    offset += bitmap_len;

    let mut out = Vec::with_capacity(types.len());
    for (index, &column_type) in types.iter().enumerate() {
        if index >= stored_columns {
            out.push(Value::Null);
            continue;
        }
        if bitmap[index / 8] & (1 << (index % 8)) != 0 {
            out.push(Value::Null);
            continue;
        }
        match column_type {
            ValueType::Int => {
                let (head, used) = fixed8(buf, offset)?;
                out.push(Value::Int(i64::from_le_bytes(head)));
                offset += used;
            }
            ValueType::Real => {
                let (head, used) = fixed8(buf, offset)?;
                out.push(Value::Real(f64::from_le_bytes(head)));
                offset += used;
            }
            ValueType::Bool => {
                let byte = *buf.get(offset)?;
                out.push(Value::Bool(byte != 0));
                offset += 1;
            }
            ValueType::Text | ValueType::Blob => {
                let (length, used) = get_varint(buf.get(offset..)?)?;
                offset += used;
                let length = length as usize;
                let bytes = buf.get(offset..offset + length)?.to_vec();
                offset += length;
                out.push(match column_type {
                    ValueType::Text => Value::Text(String::from_utf8(bytes).ok()?),
                    _ => Value::Blob(bytes),
                });
            }
        }
    }
    Some(out)
}

fn fixed8(buf: &[u8], offset: usize) -> Option<([u8; 8], usize)> {
    let slice = buf.get(offset..offset + 8)?;
    let mut head = [0u8; 8];
    head.copy_from_slice(slice);
    Some((head, 8))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn varint_round_trips() {
        for value in [0u64, 1, 127, 128, 300, u32::MAX as u64, u64::MAX] {
            let mut buf = Vec::new();
            put_varint(&mut buf, value);
            assert_eq!(get_varint(&buf), Some((value, buf.len())), "value {value}");
        }
    }

    #[test]
    fn varint_is_compact_for_small_values() {
        let mut buf = Vec::new();
        put_varint(&mut buf, 127);
        assert_eq!(buf.len(), 1);
        buf.clear();
        put_varint(&mut buf, 128);
        assert_eq!(buf.len(), 2);
    }

    #[test]
    fn varint_rejects_truncation() {
        assert_eq!(get_varint(&[0x80]), None);
        assert_eq!(get_varint(&[]), None);
    }

    #[test]
    fn integers_sort_as_bytes() {
        let mut values = [i64::MIN, -1000, -1, 0, 1, 1000, i64::MAX];
        let mut encoded: Vec<[u8; 8]> = values.iter().map(|v| encode_i64(*v)).collect();
        values.sort_unstable();
        encoded.sort_unstable();
        let expected: Vec<[u8; 8]> = values.iter().map(|v| encode_i64(*v)).collect();
        assert_eq!(encoded, expected);
    }

    #[test]
    fn documented_integer_encodings() {
        assert_eq!(
            encode_i64(-1),
            [0x7F, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF]
        );
        assert_eq!(encode_i64(0), [0x80, 0, 0, 0, 0, 0, 0, 0]);
        assert_eq!(encode_i64(1), [0x80, 0, 0, 0, 0, 0, 0, 1]);
    }

    #[test]
    fn floats_sort_as_bytes() {
        let values = [
            f64::NEG_INFINITY,
            -1e300,
            -1.5,
            -0.0,
            0.0,
            1.5,
            1e300,
            f64::INFINITY,
        ];
        for pair in values.windows(2) {
            let left = encode_f64(pair[0]).unwrap();
            let right = encode_f64(pair[1]).unwrap();
            assert!(
                left <= right,
                "{:?} should not sort after {:?}",
                pair[0],
                pair[1]
            );
        }
    }

    #[test]
    fn negative_zero_encodes_like_zero() {
        assert_eq!(encode_f64(-0.0).unwrap(), encode_f64(0.0).unwrap());
    }

    #[test]
    fn nan_is_rejected() {
        assert!(encode_f64(f64::NAN).is_err());
    }

    #[test]
    fn floats_round_trip() {
        for value in [0.0f64, 1.5, -1.5, 1e300, -1e-300, f64::INFINITY] {
            assert_eq!(decode_f64(encode_f64(value).unwrap()), value);
        }
    }

    #[test]
    fn text_prefix_sorts_first() {
        let mut a = Vec::new();
        let mut ab = Vec::new();
        encode_bytes(b"a", &mut a);
        encode_bytes(b"ab", &mut ab);
        assert!(a < ab);
    }

    #[test]
    fn embedded_nulls_keep_their_order() {
        let mut a = Vec::new();
        let mut a_nul = Vec::new();
        let mut aa = Vec::new();
        encode_bytes(b"a", &mut a);
        encode_bytes(b"a\x00", &mut a_nul);
        encode_bytes(b"aa", &mut aa);
        assert!(a < a_nul, "prefix must sort first");
        assert!(a_nul < aa, "NUL is below 'a'");

        let (decoded, used) = decode_bytes(&a_nul).unwrap();
        assert_eq!(decoded, b"a\x00");
        assert_eq!(used, a_nul.len());
    }

    #[test]
    fn nulls_sort_before_everything() {
        let mut null_key = Vec::new();
        let mut min_key = Vec::new();
        encode_key(&[Value::Null], &mut null_key).unwrap();
        encode_key(&[Value::Int(i64::MIN)], &mut min_key).unwrap();
        assert!(null_key < min_key);
    }

    #[test]
    fn composite_keys_round_trip() {
        let values = vec![
            Value::Int(-42),
            Value::Text("gado".to_string()),
            Value::Null,
            Value::Bool(true),
            Value::Real(431.5),
        ];
        let types = [
            ValueType::Int,
            ValueType::Text,
            ValueType::Real,
            ValueType::Bool,
            ValueType::Real,
        ];
        let mut buf = Vec::new();
        encode_key(&values, &mut buf).unwrap();
        assert_eq!(decode_key(&buf, &types), Some(values));
    }

    #[test]
    fn composite_keys_order_by_leading_column_first() {
        let mut low = Vec::new();
        let mut high = Vec::new();
        encode_key(&[Value::Int(1), Value::Text("z".into())], &mut low).unwrap();
        encode_key(&[Value::Int(2), Value::Text("a".into())], &mut high).unwrap();
        assert!(low < high);
    }

    #[test]
    fn tuples_round_trip() {
        let values = vec![
            Value::Int(1),
            Value::Text("BR-0042".to_string()),
            Value::Real(431.5),
            Value::Null,
            Value::Bool(false),
            Value::Blob(vec![0, 1, 2, 255]),
        ];
        let types = [
            ValueType::Int,
            ValueType::Text,
            ValueType::Real,
            ValueType::Int,
            ValueType::Bool,
            ValueType::Blob,
        ];
        let mut buf = Vec::new();
        encode_tuple(&values, &mut buf);
        assert_eq!(decode_tuple(&buf, &types), Some(values));
    }

    #[test]
    fn nulls_cost_no_bytes_in_a_tuple() {
        let mut with_value = Vec::new();
        let mut with_null = Vec::new();
        encode_tuple(&[Value::Int(1), Value::Int(2)], &mut with_value);
        encode_tuple(&[Value::Int(1), Value::Null], &mut with_null);
        assert_eq!(with_value.len() - with_null.len(), 8);
    }

    #[test]
    fn added_columns_read_back_as_null() {
        // A tuple written under a two-column schema, read under a three-column
        // one: the column it predates comes back null.
        let mut buf = Vec::new();
        encode_tuple(&[Value::Int(7), Value::Text("x".into())], &mut buf);
        let types = [ValueType::Int, ValueType::Text, ValueType::Bool];
        assert_eq!(
            decode_tuple(&buf, &types),
            Some(vec![
                Value::Int(7),
                Value::Text("x".to_string()),
                Value::Null
            ])
        );
    }

    #[test]
    fn truncated_tuples_decode_to_none() {
        let mut buf = Vec::new();
        encode_tuple(&[Value::Int(1), Value::Text("abc".into())], &mut buf);
        let types = [ValueType::Int, ValueType::Text];
        for cut in 1..buf.len() {
            let _ = decode_tuple(&buf[..cut], &types);
        }
        assert_eq!(decode_tuple(&buf[..buf.len() - 1], &types), None);
    }
}
