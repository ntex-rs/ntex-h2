use ntex::util::{ByteString, Bytes, BytesMut};
use std::io::Cursor;

use crate::hpack::{Decoder, Encoder, header::Header};

mod fixture;
mod fuzz;

/// STATEMENT: Standard and sensitive HPACK headers and pseudo-headers correctly preserve their indexability
/// flags through a full encode and decode roundtrip cycle.
///
/// HOW:
/// - Define a set of test headers covering both sensitive and non-sensitive variants of `Field`, `:authority`, and `:path`.
/// - Encode each header using the `Encoder`.
/// - Decode the resulting bytes using a `Cursor` and the `Decoder`.
/// - Assert that each decoded header precisely matches the expected variant and sensitivity state.
#[test]
fn test_roundtrip_header_sensitivity_matrix() {
    let name = Bytes::from_static(b"authorization");
    let plain_name = Bytes::from_static(b"x-custom-header");

    // Define our test matrix: (Input Header, Closure to validate the Decoded Header)
    let test_cases: Vec<(Header<Option<ntex_http::HeaderName>>, Box<dyn Fn(Header)>)> = vec![
        // 1. Regular non-sensitive Field
        {
            let h = Header::new(&plain_name, Bytes::from("plain-value"), false).unwrap();
            let opt = match h {
                Header::Field { name, value } => Header::Field {
                    name: Some(name),
                    value,
                },
                other => other.into(),
            };
            let validator = Box::new(|decoded| match decoded {
                Header::Field { value, .. } => {
                    assert!(!value.is_sensitive(), "Expected non-sensitive field");
                    assert_eq!(value.to_str().unwrap(), "plain-value");
                }
                other => panic!("Expected Header::Field, got: {:?}", other),
            });
            (opt, validator)
        },
        // 2. Regular sensitive Field (never-indexed)
        {
            let h = Header::new(&name, Bytes::from("secret-token-123"), true).unwrap();
            let opt = match h {
                Header::Field { name, value } => Header::Field {
                    name: Some(name),
                    value,
                },
                other => other.into(),
            };
            let validator = Box::new(|decoded| match decoded {
                Header::Field { value, .. } => {
                    assert!(value.is_sensitive(), "Expected sensitive field");
                    assert_eq!(value.to_str().unwrap(), "secret-token-123");
                }
                other => panic!("Expected Header::Field, got: {:?}", other),
            });
            (opt, validator)
        },
        // 3. Non-sensitive Authority pseudo-header
        {
            let h = Header::Authority(ByteString::from("example.com"));
            let validator = Box::new(|decoded| match decoded {
                Header::Authority(val) => {
                    assert_eq!(val.as_str(), "example.com");
                }
                other => panic!("Expected Header::Authority, got: {:?}", other),
            });
            (h, validator)
        },
        // 4. Sensitive Authority pseudo-header
        {
            let h = Header::AuthoritySensitive(ByteString::from("secure.example.com"));
            let validator = Box::new(|decoded| match decoded {
                Header::AuthoritySensitive(val) => {
                    assert_eq!(val.as_str(), "secure.example.com");
                }
                other => panic!("Expected Header::AuthoritySensible, got: {:?}", other),
            });
            (h, validator)
        },
        // 5. Non-sensitive Path pseudo-header
        {
            let h = Header::Path(ByteString::from("/index.html"));
            let validator = Box::new(|decoded| match decoded {
                Header::Path(val) => {
                    assert_eq!(val.as_str(), "/index.html");
                }
                other => panic!("Expected Header::Path, got: {:?}", other),
            });
            (h, validator)
        },
        // 6. Sensitive Path pseudo-header
        {
            let h = Header::PathSensitive(ByteString::from("/api/v1/sensitive-data"));
            let validator = Box::new(|decoded| match decoded {
                Header::PathSensitive(val) => {
                    assert_eq!(val.as_str(), "/api/v1/sensitive-data");
                }
                other => panic!("Expected Header::PathSensible, got: {:?}", other),
            });
            (h, validator)
        },
    ];

    for (input_header, validate) in test_cases {
        let mut encoder = Encoder::new(4096, 4096);
        let mut decoder = Decoder::new(4096);

        let mut dst = BytesMut::new();
        encoder.encode(std::iter::once(input_header), &mut dst);

        let mut encoded_bytes = dst.freeze();
        let mut cursor = Cursor::new(&mut encoded_bytes);
        let mut decoded_result = None;

        let decode_res = decoder.decode(&mut cursor, |h| {
            decoded_result = Some(h);
        });

        assert!(decode_res.is_ok(), "Decoder should decode successfully");
        let decoded = decoded_result.expect("A header should have been emitted");

        validate(decoded);
    }
}

/// STATEMENT: Mixed sequences of sensitive and non-sensitive headers (including repeated names)
/// correctly roundtrip without state corruption.
///
/// HOW:
/// - Encode a sequence containing a plain header, a sensitive header, and a repeated header name without index.
/// - Decode the entire byte stream sequentially.
/// - Assert that all values and their respective sensitivity flags match the original inputs.
#[test]
fn test_roundtrip_mixed_sensitivity_sequence() {
    use crate::hpack::{Decoder, Encoder, header::Header};
    use ntex::util::{Bytes, BytesMut};
    use std::io::Cursor;

    let mut encoder = Encoder::new(4096, 4096);
    let mut decoder = Decoder::new(4096);

    let name1 = Bytes::from_static(b"x-custom");
    let name2 = Bytes::from_static(b"authorization");

    let h1 = Header::new(&name1, Bytes::from("value1"), false).unwrap();
    let h2 = Header::new(&name2, Bytes::from("token-secret"), true).unwrap();
    let h3 = Header::new(&name1, Bytes::from("value2"), false).unwrap();

    let headers = vec![
        match h1 {
            Header::Field { name, value } => Header::Field {
                name: Some(name),
                value,
            },
            other => other.into(),
        },
        match h2 {
            Header::Field { name, value } => Header::Field {
                name: Some(name),
                value,
            },
            other => other.into(),
        },
        match h3 {
            Header::Field { name, value } => Header::Field {
                name: Some(name),
                value,
            },
            other => other.into(),
        },
    ];

    let mut dst = BytesMut::new();
    encoder.encode(headers, &mut dst);

    let mut encoded_bytes = dst.freeze();
    let mut cursor = Cursor::new(&mut encoded_bytes);

    let mut decoded_list = Vec::new();
    let decode_res = decoder.decode(&mut cursor, |h| {
        decoded_list.push(h);
    });

    assert!(decode_res.is_ok(), "Decoder should decode sequence successfully");
    assert_eq!(decoded_list.len(), 3);

    match &decoded_list[0] {
        Header::Field { value, .. } => {
            assert!(!value.is_sensitive());
            assert_eq!(value.to_str().unwrap(), "value1");
        }
        _ => panic!("Expected Header::Field"),
    }

    match &decoded_list[1] {
        Header::Field { value, .. } => {
            assert!(value.is_sensitive());
            assert_eq!(value.to_str().unwrap(), "token-secret");
        }
        _ => panic!("Expected Header::Field"),
    }

    match &decoded_list[2] {
        Header::Field { value, .. } => {
            assert!(!value.is_sensitive());
            assert_eq!(value.to_str().unwrap(), "value2");
        }
        _ => panic!("Expected Header::Field"),
    }
}

/// STATEMENT: Never-indexed (sensitive) headers are strictly excluded from entering the HPACK dynamic table.
///
/// HOW:
/// - Encode a sensitive header repeatedly.
/// - Verify via the encoder/decoder state that the dynamic table size or index references do not store or index sensitive values.
/// - Decode and ensure correct retrieval without side effects on subsequent table lookups.
#[test]
fn test_sensitive_headers_never_touch_dynamic_table() {
    use crate::hpack::{Decoder, Encoder, header::Header};
    use ntex::util::{Bytes, BytesMut};
    use std::io::Cursor;

    let mut encoder = Encoder::new(4096, 4096);
    let mut decoder = Decoder::new(4096);

    let encoder_table_size = encoder.table.size();
    let decoder_table_size = decoder.table.size();
    let name = Bytes::from_static(b"cookie");
    let sensitive_header = Header::new(&name, Bytes::from("session=supersecretval"), true).unwrap();

    let opt_header = match sensitive_header {
        Header::Field { name, value } => Header::Field {
            name: Some(name),
            value,
        },
        other => other.into(),
    };

    let mut dst = BytesMut::new();
    encoder.encode(vec![opt_header.clone(), opt_header], &mut dst);

    assert_eq!(
        encoder_table_size,
        encoder.table.size(),
        "Encoder table size should not increment"
    );

    let mut encoded_bytes = dst.freeze();
    let mut cursor = Cursor::new(&mut encoded_bytes);

    let mut count = 0;
    let decode_res = decoder.decode(&mut cursor, |h| {
        count += 1;
        match h {
            Header::Field { value, .. } => {
                assert!(value.is_sensitive());
                assert_eq!(value.to_str().unwrap(), "session=supersecretval");
            }
            _ => panic!("Expected Field"),
        }
    });

    assert!(decode_res.is_ok());
    assert_eq!(
        decoder_table_size,
        decoder.table.size(),
        "Decoder table size should not increment"
    );
    assert_eq!(count, 2, "Both sensitive headers must be decoded correctly");
}

/// STATEMENT: Consecutive sensitive headers with identical names trigger the encoder's optimized path
/// (`encode_not_indexed2` with matching name).
///
/// HOW:
/// - Encode two consecutive sensitive headers sharing the exact same name.
/// - Decode both and verify that their sensitivity and values are correctly preserved.
#[test]
fn test_consecutive_sensitive_headers_same_name() {
    use crate::hpack::{Decoder, Encoder, header::Header};
    use ntex::util::{Bytes, BytesMut};
    use std::io::Cursor;

    let mut encoder = Encoder::new(4096, 4096);
    let mut decoder = Decoder::new(4096);

    let name = Bytes::from_static(b"authorization");
    let h1 = Header::new(&name, Bytes::from("token-one"), true).unwrap();
    let h2 = Header::new(&name, Bytes::from("token-two"), true).unwrap();

    let headers = vec![
        match h1 {
            Header::Field { name, value } => Header::Field {
                name: Some(name),
                value,
            },
            other => other.into(),
        },
        match h2 {
            Header::Field { name, value } => Header::Field {
                name: Some(name),
                value,
            },
            other => other.into(),
        },
    ];

    let mut dst = BytesMut::new();
    encoder.encode(headers, &mut dst);
    dbg!(&dst);

    let mut encoded_bytes = dst.freeze();
    let mut cursor = Cursor::new(&mut encoded_bytes);

    let mut decoded_count = 0;
    let decode_res = decoder.decode(&mut cursor, |h| {
        decoded_count += 1;
        match h {
            Header::Field { value, .. } => {
                assert!(value.is_sensitive(), "{value:?}");
                if decoded_count == 1 {
                    assert_eq!(value.to_str().unwrap(), "token-one");
                } else {
                    assert_eq!(value.to_str().unwrap(), "token-two");
                }
            }
            _ => panic!("Expected Field"),
        }
    });

    assert!(decode_res.is_ok());
    assert_eq!(decoded_count, 2);
}

/// STATEMENT: The decoder correctly parses raw HPACK bit patterns representing never-indexed sensitive literals.
///
/// HOW:
/// - Construct a raw byte stream representing a literal header field never-indexed with the sensitivity bit set.
/// - Run it through the `Decoder`.
/// - Assert that the resulting header is marked as sensitive.
#[test]
fn test_decoder_raw_sensitive_literal_parsing() {
    use crate::hpack::{Decoder, header::Header};
    use ntex::util::BytesMut;
    use std::io::Cursor;

    let mut decoder = Decoder::new(4096);

    // HPACK binary representation for a never-indexed literal with sensitive flag:
    // Byte 1: 00010000 (Pattern for literal header field never indexed -> 4-bit prefix with sensitive bit = 1)
    // Name length (7-bit integer): 13 ('authorization' length)
    // Name: "authorization"
    // Value length (7-bit integer): 9 ("secret-123")
    // Value: "secret-123"
    let mut raw_data = BytesMut::new();
    raw_data.extend_from_slice(&[
        0b0001_0000, // Literal Header Field Never Indexed (Sensitive)
        13,          // Name length
    ]);
    raw_data.extend_from_slice(b"authorization");
    raw_data.extend_from_slice(&[
        10, // Value length ("secret-123" is 10 chars)
    ]);
    raw_data.extend_from_slice(b"secret-123");

    let mut encoded_bytes = raw_data.freeze();
    let mut cursor = Cursor::new(&mut encoded_bytes);

    let mut decoded_result = None;
    let decode_res = decoder.decode(&mut cursor, |h| {
        decoded_result = Some(h);
    });

    assert!(
        decode_res.is_ok(),
        "Raw decoder should parse sensitive literal successfully"
    );

    let decoded = decoded_result.expect("Header must be decoded");
    match decoded {
        Header::Field { value, .. } => {
            assert!(
                value.is_sensitive(),
                "Raw decoded field must be marked as sensitive"
            );
            assert_eq!(value.to_str().unwrap(), "secret-123");
        }
        other => panic!("Expected Header::Field, got: {:?}", other),
    }
}
