use ntex_bytes::{BufMut, BytesMut};
use ntex_http::header::{HeaderName, HeaderValue};

use super::{Header, huffman, table::Index, table::Table};

/// Stateful HPACK header encoder.
#[derive(Debug)]
pub struct Encoder {
    table: Table,
    size_update: Option<SizeUpdate>,
}

#[derive(Debug, Copy, Clone, Eq, PartialEq)]
enum SizeUpdate {
    One(usize),
    Two(usize, usize), // min, max
}

impl Encoder {
    /// Creates an encoder with the specified table size and storage capacity.
    pub fn new(max_size: usize, capacity: usize) -> Encoder {
        Encoder {
            table: Table::new(max_size, capacity),
            size_update: None,
        }
    }

    /// Releases the dynamic table.
    ///
    /// The peer's table state is lost, the encoder must not be used for the
    /// same connection afterwards.
    pub(crate) fn release(&mut self) {
        self.size_update = None;
        self.table = Table::new(self.table.max_size(), 0);
    }

    #[cfg(test)]
    /// Returns the dynamic table size and index capacity.
    pub(crate) fn retained(&self) -> (usize, usize) {
        (self.table.size(), self.table.capacity())
    }

    /// Queues a maximum dynamic table size update.
    ///
    /// The next call to `encode` will include a dynamic size update frame.
    pub fn update_max_size(&mut self, val: usize) {
        match self.size_update {
            Some(SizeUpdate::One(old)) => {
                if val > old {
                    if old > self.table.max_size() {
                        self.size_update = Some(SizeUpdate::One(val));
                    } else {
                        self.size_update = Some(SizeUpdate::Two(old, val));
                    }
                } else {
                    self.size_update = Some(SizeUpdate::One(val));
                }
            }
            Some(SizeUpdate::Two(min, _)) => {
                if val < min {
                    self.size_update = Some(SizeUpdate::One(val));
                } else {
                    self.size_update = Some(SizeUpdate::Two(min, val));
                }
            }
            None => {
                if val != self.table.max_size() {
                    // Don't bother writing a frame if the value already matches
                    // the table's max size.
                    self.size_update = Some(SizeUpdate::One(val));
                }
            }
        }
    }

    #[allow(clippy::missing_panics_doc)]
    /// Encodes a set of headers into `dst`.
    pub fn encode<I>(&mut self, headers: I, dst: &mut BytesMut)
    where
        I: IntoIterator<Item = Header<Option<HeaderName>>>,
    {
        self.encode_size_updates(dst);

        let mut last_index = None;

        for header in headers {
            match header.reify() {
                // The header has an associated name. In which case, try to
                // index it in the table.
                Ok(header) => {
                    let index = self.table.index(header);
                    self.encode_header(&index, dst);

                    last_index = Some(index);
                }
                // The header does not have an associated name. This means that
                // the name is the same as the previously yielded header. In
                // which case, we skip table lookup and just use the same index
                // as the previous entry.
                Err(value) => {
                    self.encode_header_without_name(
                        last_index.as_ref().unwrap_or_else(|| {
                            panic!("encoding header without name, but no previous index to use for name");
                        }),
                        &value,
                        dst,
                    );
                }
            }
        }
    }

    fn encode_size_updates(&mut self, dst: &mut BytesMut) {
        match self.size_update.take() {
            Some(SizeUpdate::One(val)) => {
                self.table.resize(val);
                encode_size_update(val, dst);
            }
            Some(SizeUpdate::Two(min, max)) => {
                self.table.resize(min);
                self.table.resize(max);
                encode_size_update(min, dst);
                encode_size_update(max, dst);
            }
            None => {}
        }
    }

    fn encode_header(&mut self, index: &Index, dst: &mut BytesMut) {
        match *index {
            Index::Indexed(idx, _) => {
                encode_int(idx, 7, 0x80, dst);
            }
            Index::Name(idx, _) => {
                let header = self.table.resolve(index);

                encode_not_indexed(idx, header.value_slice(), header.is_sensitive(), dst);
            }
            Index::Inserted(_) => {
                let header = self.table.resolve(index);

                assert!(!header.is_sensitive());

                dst.put_u8(0b0100_0000);

                encode_str(header.name().as_slice(), dst);
                encode_str(header.value_slice(), dst);
            }
            Index::InsertedValue(idx, _) => {
                let header = self.table.resolve(index);

                assert!(!header.is_sensitive());

                encode_int(idx, 6, 0b0100_0000, dst);
                encode_str(header.value_slice(), dst);
            }
            Index::NotIndexed(_) => {
                let header = self.table.resolve(index);

                encode_not_indexed2(
                    header.name().as_slice(),
                    header.value_slice(),
                    header.is_sensitive(),
                    dst,
                );
            }
        }
    }

    fn encode_header_without_name(&mut self, last: &Index, value: &HeaderValue, dst: &mut BytesMut) {
        match *last {
            Index::Indexed(..) | Index::Name(..) | Index::Inserted(..) | Index::InsertedValue(..) => {
                let idx = Table::resolve_idx(last);

                encode_not_indexed(idx, value.as_ref(), value.is_sensitive(), dst);
            }
            Index::NotIndexed(_) => {
                let last = self.table.resolve(last);

                encode_not_indexed2(last.name().as_slice(), value.as_ref(), value.is_sensitive(), dst);
            }
        }
    }
}

impl Default for Encoder {
    fn default() -> Encoder {
        Encoder::new(4096, 0)
    }
}

fn encode_size_update(val: usize, dst: &mut BytesMut) {
    encode_int(val, 5, 0b0010_0000, dst);
}

fn encode_not_indexed(name: usize, value: &[u8], sensitive: bool, dst: &mut BytesMut) {
    if sensitive {
        encode_int(name, 4, 0b10000, dst);
    } else {
        encode_int(name, 4, 0, dst);
    }

    encode_str(value, dst);
}

fn encode_not_indexed2(name: &[u8], value: &[u8], sensitive: bool, dst: &mut BytesMut) {
    if sensitive {
        dst.put_u8(0b10000);
    } else {
        dst.put_u8(0);
    }

    encode_str(name, dst);
    encode_str(value, dst);
}

fn encode_str(val: &[u8], dst: &mut BytesMut) {
    if val.is_empty() {
        // Write an empty string
        dst.put_u8(0);
    } else {
        let idx = position(dst);

        // Push a placeholder byte for the length header
        dst.put_u8(0);

        // Encode with huffman
        huffman::encode(val, dst);

        let huff_len = position(dst) - (idx + 1);

        if encode_int_one_byte(huff_len, 7) {
            // Write the string head
            dst[idx] = 0x80 | huff_len as u8;
        } else {
            // Write the head to a placeholder
            const PLACEHOLDER_LEN: usize = 8;
            let mut buf = [0u8; PLACEHOLDER_LEN];

            let head_len = {
                let mut head_dst = &mut buf[..];
                encode_int(huff_len, 7, 0x80, &mut head_dst);
                PLACEHOLDER_LEN - head_dst.remaining_mut()
            };

            // This is just done to reserve space in the destination
            dst.put_slice(&buf[1..head_len]);

            // Shift the header forward
            for i in 0..huff_len {
                let src_i = idx + 1 + (huff_len - (i + 1));
                let dst_i = idx + head_len + (huff_len - (i + 1));
                dst[dst_i] = dst[src_i];
            }

            // Copy in the head
            for i in 0..head_len {
                dst[idx + i] = buf[i];
            }
        }
    }
}

/// Encode an integer into the given destination buffer
fn encode_int<B: BufMut>(
    mut value: usize,   // The integer to encode
    prefix_bits: usize, // The number of bits in the prefix
    first_byte: u8,     // The base upon which to start encoding the int
    dst: &mut B,
) {
    if encode_int_one_byte(value, prefix_bits) {
        dst.put_u8(first_byte | value as u8);
        return;
    }

    let low = (1 << prefix_bits) - 1;

    value -= low;

    dst.put_u8(first_byte | low as u8);

    while value >= 128 {
        dst.put_u8(0b1000_0000 | value as u8);

        value >>= 7;
    }

    dst.put_u8(value as u8);
}

/// Returns true if the in the int can be fully encoded in the first byte.
fn encode_int_one_byte(value: usize, prefix_bits: usize) -> bool {
    value < (1 << prefix_bits) - 1
}

fn position(buf: &BytesMut) -> usize {
    buf.len()
}

#[cfg(test)]
#[allow(clippy::decimal_bitwise_operands)]
mod test {
    use ntex_bytes::Bytes;
    use ntex_http::*;

    use super::*;

    /// STATEMENT: `GET` uses its static HPACK index without populating the dynamic table.
    ///
    /// HOW:
    /// - Encode a `GET` method header with the default `Encoder`.
    /// - Assert the single byte references static index 2 and the dynamic table is empty.
    #[test]
    fn test_encode_method_get() {
        let mut encoder = Encoder::default();
        let res = encode(&mut encoder, vec![method("GET")]);
        assert_eq!(*res, [0x80 | 2]);
        assert_eq!(encoder.table.len(), 0);
    }

    /// STATEMENT: `POST` uses its static HPACK index without populating the dynamic table.
    ///
    /// HOW:
    /// - Encode a `POST` method header with the default `Encoder`.
    /// - Assert the single byte references static index 3 and the dynamic table is empty.
    #[test]
    fn test_encode_method_post() {
        let mut encoder = Encoder::default();
        let res = encode(&mut encoder, vec![method("POST")]);
        assert_eq!(*res, [0x80 | 3]);
        assert_eq!(encoder.table.len(), 0);
    }

    /// STATEMENT: `PATCH` is inserted as an indexed-name literal and subsequently encoded by
    /// dynamic index.
    ///
    /// HOW:
    /// - Encode `PATCH` and check static name index 2, its Huffman value, and one table entry.
    /// - Encode it again and assert the output is a single reference to dynamic index 62.
    #[test]
    fn test_encode_method_patch() {
        let mut encoder = Encoder::default();
        let res = encode(&mut encoder, vec![method("PATCH")]);

        assert_eq!(res[0], 0b0100_0000 | 2); // Incremental indexing w/ name pulled from table
        assert_eq!(res[1], 0x80 | 5); // header value w/ huffman coding

        assert_eq!("PATCH", huff_decode(&res[2..7]));
        assert_eq!(encoder.table.len(), 1);

        let res = encode(&mut encoder, vec![method("PATCH")]);

        assert_eq!(1 << 7 | 62, res[0]);
        assert_eq!(1, res.len());
    }

    /// STATEMENT: Different values for a static header name use incrementally indexed literals.
    ///
    /// HOW:
    /// - Encode `content-language` first as `foo`, then as `bar`, with the same `Encoder`.
    /// - Check both literals use static name index 27 and the expected Huffman-encoded values.
    #[test]
    fn test_encode_indexed_name_literal_value() {
        let mut encoder = Encoder::default();
        let res = encode(&mut encoder, vec![header("content-language", "foo")]);

        assert_eq!(res[0], 0b0100_0000 | 27); // Indexed name
        assert_eq!(res[1], 0x80 | 2); // header value w/ huffman coding

        assert_eq!("foo", huff_decode(&res[2..4]));

        // Same name, new value should still use incremental
        let res = encode(&mut encoder, vec![header("content-language", "bar")]);
        assert_eq!(res[0], 0b0100_0000 | 27); // Indexed name
        assert_eq!(res[1], 0x80 | 3); // header value w/ huffman coding
        assert_eq!("bar", huff_decode(&res[2..5]));
    }

    /// STATEMENT: A repeated custom header reuses its dynamic-table entry.
    ///
    /// HOW:
    /// - Encode `foo: hello` and check its literal bytes, Huffman name/value, and one table
    ///   entry.
    /// - Encode it again and assert a single dynamic index 62 byte with no additional entry.
    #[test]
    fn test_repeated_headers_are_indexed() {
        let mut encoder = Encoder::default();
        let res = encode(&mut encoder, vec![header("foo", "hello")]);

        assert_eq!(&[0b0100_0000, 0x80 | 2], &res[0..2]);
        assert_eq!("foo", huff_decode(&res[2..4]));
        assert_eq!(0x80 | 4, res[4]);
        assert_eq!("hello", huff_decode(&res[5..]));
        assert_eq!(9, res.len());

        assert_eq!(1, encoder.table.len());

        let res = encode(&mut encoder, vec![header("foo", "hello")]);
        assert_eq!([0x80 | 62], *res);

        assert_eq!(encoder.table.len(), 1);
    }

    /// STATEMENT: A full dynamic table evicts its oldest header and preserves indexes for
    /// retained headers.
    ///
    /// HOW:
    /// - Insert 64 distinct headers, verify their literal bytes and indexed reuse, and check a
    ///   4096-byte table.
    /// - Confirm all existing headers are indexed, then insert a 65th literal without growing
    ///   the entry count.
    /// - Re-encode the 64 retained headers and verify their expected dynamic indexes.
    #[test]
    fn test_evicting_headers() {
        let mut encoder = Encoder::default();

        // Fill the table
        for i in 0..64 {
            let key = format!("x-hello-world-{i:02}");
            let res = encode(&mut encoder, vec![header(&key, &key)]);

            assert_eq!(&[0b0100_0000, 0x80 | 12], &res[0..2]);
            assert_eq!(key, huff_decode(&res[2..14]));
            assert_eq!(0x80 | 12, res[14]);
            assert_eq!(key, huff_decode(&res[15..]));
            assert_eq!(27, res.len());

            // Make sure the header can be found...
            let res = encode(&mut encoder, vec![header(&key, &key)]);

            // Only check that it is found
            assert_eq!(0x80, res[0] & 0x80);
        }

        assert_eq!(4096, encoder.table.size());
        assert_eq!(64, encoder.table.len());

        // Find existing headers
        for i in 0..64 {
            let key = format!("x-hello-world-{i:02}");
            let res = encode(&mut encoder, vec![header(&key, &key)]);
            assert_eq!(0x80, res[0] & 0x80);
        }

        // Insert a new header
        let key = "x-hello-world-64";
        let res = encode(&mut encoder, vec![header(key, key)]);

        assert_eq!(&[0b0100_0000, 0x80 | 12], &res[0..2]);
        assert_eq!(key, huff_decode(&res[2..14]));
        assert_eq!(0x80 | 12, res[14]);
        assert_eq!(key, huff_decode(&res[15..]));
        assert_eq!(27, res.len());

        assert_eq!(64, encoder.table.len());

        // Now try encoding entries that should exist in the table
        for i in 1..65 {
            let key = format!("x-hello-world-{i:02}");
            let res = encode(&mut encoder, vec![header(&key, &key)]);
            assert_eq!(0x80 | (61 + (65 - i)), res[0]);
        }
    }

    /// STATEMENT: A large custom header is encoded without indexing and leaves the dynamic
    /// table empty.
    ///
    /// HOW:
    /// - Encode a long name/value pair with `Encoder::new(128, 0)`.
    /// - Check the non-indexed literal prefix and zero dynamic-table entry count and byte size.
    #[test]
    fn test_large_headers_are_not_indexed() {
        let mut encoder = Encoder::new(128, 0);
        let key = "hello-world-hello-world-HELLO-zzz";

        let res = encode(&mut encoder, vec![header(key, key)]);

        assert_eq!(&[0, 0x80 | 25], &res[..2]);

        assert_eq!(0, encoder.table.len());
        assert_eq!(0, encoder.table.size());
    }

    /// STATEMENT: Sensitive header values use the never-indexed representation for every name
    /// source.
    ///
    /// HOW:
    /// - Mark `12345` sensitive and encode it with a new `my-password` name and static
    ///   `authorization` name.
    /// - Insert a non-sensitive `my-password` field, then encode the sensitive value using its
    ///   dynamic name.
    /// - Check never-indexed prefixes, name indexes or literal name, and the decoded Huffman
    ///   value.
    #[test]
    fn test_sensitive_headers_are_never_indexed() {
        use ntex_http::HeaderValue;

        let name = "my-password".parse().unwrap();
        let mut value = HeaderValue::from_bytes(b"12345").unwrap();
        value.set_sensitive(true);

        let header = Header::Field {
            name: Some(name),
            value,
        };

        // Now, try to encode the sensitive header

        let mut encoder = Encoder::default();
        let res = encode(&mut encoder, vec![header]);

        assert_eq!(&[0b10000, 0x80 | 8], &res[..2]);
        assert_eq!("my-password", huff_decode(&res[2..10]));
        assert_eq!(0x80 | 4, res[10]);
        assert_eq!("12345", huff_decode(&res[11..]));

        // Now, try to encode a sensitive header w/ a name in the static table
        let name = "authorization".parse().unwrap();
        let mut value = HeaderValue::from_bytes(b"12345").unwrap();
        value.set_sensitive(true);

        let header = Header::Field {
            name: Some(name),
            value,
        };

        let mut encoder = Encoder::default();
        let res = encode(&mut encoder, vec![header]);

        assert_eq!(&[0b11111, 8], &res[..2]);
        assert_eq!(0x80 | 4, res[2]);
        assert_eq!("12345", huff_decode(&res[3..]));

        // Using the name component of a previously indexed header (without
        // sensitive flag set)

        let _ = encode(&mut encoder, vec![self::header("my-password", "not-so-secret")]);

        let name = "my-password".parse().unwrap();
        let mut value = HeaderValue::from_bytes(b"12345").unwrap();
        value.set_sensitive(true);

        let header = Header::Field {
            name: Some(name),
            value,
        };
        let res = encode(&mut encoder, vec![header]);

        assert_eq!(&[0b11111, 47], &res[..2]);
        assert_eq!(0x80 | 4, res[2]);
        assert_eq!("12345", huff_decode(&res[3..]));
    }

    /// STATEMENT: `content-length` values use a non-indexed literal with the static name index.
    ///
    /// HOW:
    /// - Encode `content-length: 1234` with the default `Encoder`.
    /// - Check the literal prefix and Huffman length, decode `1234`, and assert six output
    ///   bytes.
    #[test]
    fn test_content_length_value_not_indexed() {
        let mut encoder = Encoder::default();
        let res = encode(&mut encoder, vec![header("content-length", "1234")]);

        assert_eq!(&[15, 13, 0x80 | 3], &res[0..3]);
        assert_eq!("1234", huff_decode(&res[3..]));
        assert_eq!(6, res.len());
    }

    /// STATEMENT: Distinct values sharing a header name can be indexed and retrieved
    /// independently.
    ///
    /// HOW:
    /// - Encode `hello: one`, then verify `hello: two` reuses the dynamic name in a new
    ///   literal.
    /// - Encode both fields again and check dynamic indexes 63 and 62 respectively.
    #[test]
    fn test_encoding_headers_with_same_name() {
        let mut encoder = Encoder::default();
        let name = "hello";

        // Encode first one
        let _ = encode(&mut encoder, vec![header(name, "one")]);

        // Encode second one
        let res = encode(&mut encoder, vec![header(name, "two")]);
        assert_eq!(&[0x40 | 62, 0x80 | 3], &res[0..2]);
        assert_eq!("two", huff_decode(&res[2..]));
        assert_eq!(5, res.len());

        // Encode the first one again
        let res = encode(&mut encoder, vec![header(name, "one")]);
        assert_eq!(&[0x80 | 63], &res[..]);

        // Now the second one
        let res = encode(&mut encoder, vec![header(name, "two")]);
        assert_eq!(&[0x80 | 62], &res[..]);
    }

    /// STATEMENT: A literal may reference a dynamic name while inserting it evicts the
    /// referenced entry.
    ///
    /// HOW:
    /// - Fill a two-entry, 76-byte table with `foo: bar` and `bar: foo`.
    /// - Insert `foo: baz`, checking its reference to name index 63 and the unchanged entry
    ///   count.
    /// - Encode `foo: baz` again and check dynamic index 62 and two retained entries.
    #[test]
    fn test_evicting_headers_when_multiple_of_same_name_are_in_table() {
        // The encoder only has space for 2 headers
        let mut encoder = Encoder::new(76, 0);

        let _ = encode(&mut encoder, vec![header("foo", "bar")]);
        assert_eq!(1, encoder.table.len());

        let _ = encode(&mut encoder, vec![header("bar", "foo")]);
        assert_eq!(2, encoder.table.len());

        // This will evict the first header, while still referencing the header
        // name
        let res = encode(&mut encoder, vec![header("foo", "baz")]);
        assert_eq!(&[0x40 | 63, 0, 0x80 | 3], &res[..3]);
        assert_eq!(2, encoder.table.len());

        // Try adding the same header again
        let res = encode(&mut encoder, vec![header("foo", "baz")]);
        assert_eq!(&[0x80 | 62], &res[..]);
        assert_eq!(2, encoder.table.len());
    }

    /// STATEMENT: A zero-capacity dynamic table still permits static indexes and non-indexed
    /// literals.
    ///
    /// HOW:
    /// - Encode `GET` by static index and check that the table remains empty.
    /// - Encode `foo: bar` without indexing and check its Huffman name/value and empty table.
    /// - Encode `transfer-encoding: chunked` using its static name and verify the Huffman
    ///   value.
    #[test]
    fn test_max_size_zero() {
        // Static table only
        let mut encoder = Encoder::new(0, 0);
        let res = encode(&mut encoder, vec![method("GET")]);
        assert_eq!(*res, [0x80 | 2]);
        assert_eq!(encoder.table.len(), 0);

        let res = encode(&mut encoder, vec![header("foo", "bar")]);
        assert_eq!(&[0, 0x80 | 2], &res[..2]);
        assert_eq!("foo", huff_decode(&res[2..4]));
        assert_eq!(0x80 | 3, res[4]);
        assert_eq!("bar", huff_decode(&res[5..8]));
        assert_eq!(0, encoder.table.len());

        // Encode a custom value
        let res = encode(&mut encoder, vec![header("transfer-encoding", "chunked")]);
        assert_eq!(&[15, 42, 0x80 | 6], &res[..3]);
        assert_eq!("chunked", huff_decode(&res[3..]));
    }

    /// STATEMENT: Pending table-size updates retain the required minimum and final sizes.
    ///
    /// HOW:
    /// - Check the default capacity and that setting the same size queues no update.
    /// - Queue a shrink to zero followed by growth, checking `SizeUpdate::One` and
    ///   `SizeUpdate::Two`.
    /// - Apply growth and successive shrinks and verify the minimum and final queued sizes each
    ///   time.
    #[test]
    fn test_update_max_size_combos() {
        let mut encoder = Encoder::default();
        assert!(encoder.size_update.is_none());
        assert_eq!(4096, encoder.table.max_size());

        encoder.update_max_size(4096); // Default size
        assert!(encoder.size_update.is_none());

        encoder.update_max_size(0);
        assert_eq!(Some(SizeUpdate::One(0)), encoder.size_update);

        encoder.update_max_size(100);
        assert_eq!(Some(SizeUpdate::Two(0, 100)), encoder.size_update);

        let mut encoder = Encoder::default();
        encoder.update_max_size(8000);
        assert_eq!(Some(SizeUpdate::One(8000)), encoder.size_update);

        encoder.update_max_size(100);
        assert_eq!(Some(SizeUpdate::One(100)), encoder.size_update);

        encoder.update_max_size(8000);
        assert_eq!(Some(SizeUpdate::Two(100, 8000)), encoder.size_update);

        encoder.update_max_size(4000);
        assert_eq!(Some(SizeUpdate::Two(100, 4000)), encoder.size_update);

        encoder.update_max_size(50);
        assert_eq!(Some(SizeUpdate::One(50)), encoder.size_update);
    }

    /// STATEMENT: Table-size changes are emitted and applied when the next header block is
    /// encoded.
    ///
    /// HOW:
    /// - Insert `foo: bar`, request a one-byte table, and confirm the entry persists until
    ///   encoding `GET`.
    /// - Check the size-update prefix, ensuing eviction, and non-indexed re-encoding of the
    ///   custom field.
    /// - Resize to 100 and then zero, verifying update bytes and indexed versus non-indexed
    ///   literal prefixes.
    #[test]
    fn test_resizing_table() {
        let mut encoder = Encoder::default();

        // Add a header
        let _ = encode(&mut encoder, vec![header("foo", "bar")]);

        encoder.update_max_size(1);
        assert_eq!(1, encoder.table.len());

        let res = encode(&mut encoder, vec![method("GET")]);
        assert_eq!(&[32 | 1, 0x80 | 2], &res[..]);
        assert_eq!(0, encoder.table.len());

        let res = encode(&mut encoder, vec![header("foo", "bar")]);
        assert_eq!(0, res[0]);

        encoder.update_max_size(100);
        let res = encode(&mut encoder, vec![header("foo", "bar")]);
        assert_eq!(&[32 | 31, 69, 64], &res[..3]);

        encoder.update_max_size(0);
        let res = encode(&mut encoder, vec![header("foo", "bar")]);
        assert_eq!(&[32, 0], &res[..2]);
    }

    /// STATEMENT: Shrinking the table without exceeding the new capacity preserves indexed
    /// headers.
    ///
    /// HOW:
    /// - Insert `foo: bar`, reduce the maximum size to 100, and check one entry remains.
    /// - Encode the field again and assert the size update is followed by dynamic index 62.
    #[test]
    fn test_decreasing_table_size_without_eviction() {
        let mut encoder = Encoder::default();

        // Add a header
        let _ = encode(&mut encoder, vec![header("foo", "bar")]);

        encoder.update_max_size(100);
        assert_eq!(1, encoder.table.len());

        let res = encode(&mut encoder, vec![header("foo", "bar")]);
        assert_eq!(&[32 | 31, 69, 0x80 | 62], &res[..]);
    }

    /// STATEMENT: A nameless field reuses the previous header name without indexing its new
    /// value.
    ///
    /// HOW:
    /// - Encode `hello: world` followed by a field with no name and value `zomg`.
    /// - Check the first indexed literal and its Huffman name/value.
    /// - Check the second non-indexed literal references dynamic name index 62 and decodes to
    ///   `zomg`.
    #[test]
    fn test_nameless_header() {
        let mut encoder = Encoder::default();

        let res = encode(
            &mut encoder,
            vec![
                Header::Field {
                    name: Some("hello".parse().unwrap()),
                    value: HeaderValue::from_bytes(b"world").unwrap(),
                },
                Header::Field {
                    name: None,
                    value: HeaderValue::from_bytes(b"zomg").unwrap(),
                },
            ],
        );

        assert_eq!(&[0x40, 0x80 | 4], &res[0..2]);
        assert_eq!("hello", huff_decode(&res[2..6]));
        assert_eq!(0x80 | 4, res[6]);
        assert_eq!("world", huff_decode(&res[7..11]));

        // Next is not indexed
        assert_eq!(&[15, 47, 0x80 | 3], &res[11..14]);
        assert_eq!("zomg", huff_decode(&res[14..]));
    }

    /// STATEMENT: Large dynamic-table size updates use the expected multibyte HPACK integer
    /// encoding.
    ///
    /// HOW:
    /// - Request a maximum size of 1,912,930,560 and assert the pending `SizeUpdate::One`.
    /// - Encode size updates and compare all six output bytes with the expected representation.
    #[test]
    fn test_large_size_update() {
        let mut encoder = Encoder::default();

        encoder.update_max_size(1_912_930_560);
        assert_eq!(Some(SizeUpdate::One(1_912_930_560)), encoder.size_update);

        let mut dst = BytesMut::with_capacity(6);
        encoder.encode_size_updates(&mut dst);
        assert_eq!([63, 225, 129, 148, 144, 7], &dst[..]);
    }

    fn encode(e: &mut Encoder, hdrs: Vec<Header<Option<HeaderName>>>) -> BytesMut {
        let mut dst = BytesMut::with_capacity(1024);
        e.encode(hdrs, &mut dst);
        dst
    }

    fn method(s: &str) -> Header<Option<HeaderName>> {
        Header::Method(Method::from_bytes(s.as_bytes()).unwrap())
    }

    fn header(name: &str, val: &str) -> Header<Option<HeaderName>> {
        let name = HeaderName::from_bytes(name.as_bytes()).unwrap();
        let value = HeaderValue::from_bytes(val.as_bytes()).unwrap();

        Header::Field {
            name: Some(name),
            value,
        }
    }

    fn huff_decode(src: &[u8]) -> Bytes {
        let mut buf = BytesMut::new();
        huffman::decode(src, &mut buf).unwrap()
    }
}
