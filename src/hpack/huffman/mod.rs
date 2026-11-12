use ntex_bytes::{BufMut, Bytes, BytesMut};

mod table;

use self::table::ENCODE_TABLE;
use crate::hpack::DecoderError;

/// Bits looked up at once. Every printable ASCII character except
/// `#$<>@[\]^{}~` and the backtick has a code of at most this length.
const PEEK_BITS: u32 = 11;
/// Longest code in the HPACK Huffman table (EOS).
const MAX_BITS: usize = 30;
const EOS: u16 = 256;

/// Short codes indexed by the next `PEEK_BITS` input bits.
///
/// Each entry is `symbol << 4 | code length`; zero means the code is longer
/// than `PEEK_BITS`.
static PEEK_TABLE: [u16; 1 << PEEK_BITS] = build_peek_table();

/// Symbols ordered by `(code length, symbol)`. The HPACK code is canonical,
/// so codes of each length are consecutive in this order.
static CANONICAL: Canonical = build_canonical();

struct Canonical {
    symbols: [u16; 257],
    /// First code of each length.
    first: [u32; MAX_BITS + 1],
    /// Number of codes of each length.
    count: [u32; MAX_BITS + 1],
    /// Index in `symbols` of the first code of each length.
    offset: [u16; MAX_BITS + 1],
}

const fn build_peek_table() -> [u16; 1 << PEEK_BITS] {
    let mut table = [0; 1 << PEEK_BITS];
    let mut sym = 0;
    while sym < ENCODE_TABLE.len() {
        let (len, code) = ENCODE_TABLE[sym];
        if len <= PEEK_BITS as usize {
            let shift = PEEK_BITS as usize - len;
            let start = (code as usize) << shift;
            let mut i = 0;
            while i < 1 << shift {
                table[start + i] = ((sym as u16) << 4) | len as u16;
                i += 1;
            }
        }
        sym += 1;
    }
    table
}

const fn build_canonical() -> Canonical {
    let mut c = Canonical {
        symbols: [0; 257],
        first: [0; MAX_BITS + 1],
        count: [0; MAX_BITS + 1],
        offset: [0; MAX_BITS + 1],
    };
    let mut n = 0;
    let mut len = 1;
    while len <= MAX_BITS {
        c.offset[len] = n as u16;
        let mut sym = 0;
        while sym < ENCODE_TABLE.len() {
            let (l, code) = ENCODE_TABLE[sym];
            if l == len {
                if c.count[len] == 0 {
                    c.first[len] = code as u32;
                }
                c.count[len] += 1;
                c.symbols[n] = sym as u16;
                n += 1;
            }
            sym += 1;
        }
        len += 1;
    }
    c
}

pub(crate) fn decode(src: &[u8], buf: &mut BytesMut) -> Result<Bytes, DecoderError> {
    // The shortest code is 5 bits
    let start = buf.len();
    buf.resize(start + src.len() * 8 / 5, 0);

    match decode_into(src, &mut buf[start..]) {
        Ok(written) => {
            buf.truncate(start + written);
            Ok(buf.take())
        }
        Err(e) => {
            buf.truncate(start);
            Err(e)
        }
    }
}

/// Decodes `src` into `out`, which must hold `src.len() * 8 / 5` bytes, and
/// returns the decoded length.
fn decode_into(src: &[u8], out: &mut [u8]) -> Result<usize, DecoderError> {
    let mut written = 0;

    // Pending input bits, most significant first
    let mut acc: u64 = 0;
    let mut bits: u32 = 0;
    let mut input = src.iter();

    loop {
        while bits <= 56 {
            let Some(&b) = input.next() else { break };
            acc |= u64::from(b) << (56 - bits);
            bits += 8;
        }

        if bits == 0 {
            break;
        }

        let entry = PEEK_TABLE[(acc >> (64 - PEEK_BITS)) as usize];
        let (sym, len) = match u32::from(entry & 0xf) {
            0 => match decode_long(acc, bits) {
                Some((EOS, _)) => return Err(DecoderError::InvalidHuffmanCode),
                Some(code) => code,
                None => break,
            },
            len if len <= bits => (entry >> 4, len),
            _ => break,
        };

        acc <<= len;
        bits -= len;
        out[written] = sym as u8;
        written += 1;
    }

    // Remaining bits are padding: at most 7 bits of the EOS prefix (all ones)
    if bits > 7 || (bits != 0 && acc >> (64 - bits) != (1 << bits) - 1) {
        return Err(DecoderError::InvalidHuffmanCode);
    }

    Ok(written)
}

/// Decodes a code longer than `PEEK_BITS` from the top of `acc`.
///
/// Returns `None` if `bits` is too short to contain a complete code.
#[cold]
fn decode_long(acc: u64, bits: u32) -> Option<(u16, u32)> {
    let c = &CANONICAL;
    let mut len = PEEK_BITS + 1;
    while len <= bits.min(MAX_BITS as u32) {
        let code = (acc >> (64 - len)) as u32;
        let i = code.wrapping_sub(c.first[len as usize]);
        if i < c.count[len as usize] {
            return Some((c.symbols[c.offset[len as usize] as usize + i as usize], len));
        }
        len += 1;
    }
    None
}

pub(super) fn encode(src: &[u8], dst: &mut BytesMut) {
    let mut bits: u64 = 0;
    let mut bits_left = 40;

    for &b in src {
        let (nbits, code) = ENCODE_TABLE[b as usize];

        bits |= code << (bits_left - nbits);
        bits_left -= nbits;

        while bits_left <= 32 {
            dst.put_u8((bits >> 32) as u8);

            bits <<= 8;
            bits_left += 8;
        }
    }

    if bits_left != 40 {
        // This writes the EOS token
        bits |= (1 << bits_left) - 1;
        dst.put_u8((bits >> 32) as u8);
    }
}

#[cfg(test)]
mod test {
    use super::*;

    fn decode(src: &[u8]) -> Result<Bytes, DecoderError> {
        let mut buf = BytesMut::new();
        super::decode(src, &mut buf)
    }

    /// STATEMENT: Single-byte Huffman encodings decode to their expected characters.
    ///
    /// HOW:
    /// - Decode the fixed one-byte encodings for `o`, `0`, and `A`.
    /// - Assert each result equals its expected character.
    #[test]
    fn decode_single_byte() {
        assert_eq!("o", decode(&[0b0011_1111]).unwrap());
        assert_eq!("0", decode(&[7]).unwrap());
        assert_eq!("A", decode(&[(0x21 << 2) + 3]).unwrap());
    }

    /// STATEMENT: Huffman characters spanning multiple bytes decode correctly.
    ///
    /// HOW:
    /// - Decode the fixed two-byte encodings for `#` and `$` and four-byte encoding for a
    ///   newline.
    /// - Assert each result equals its expected character.
    #[test]
    fn single_char_multi_byte() {
        assert_eq!("#", decode(&[255, 160 + 15]).unwrap());
        assert_eq!("$", decode(&[255, 200 + 7]).unwrap());
        assert_eq!("\x0a", decode(&[255, 255, 255, 240 + 3]).unwrap());
    }

    /// STATEMENT: Adjacent Huffman characters decode correctly across byte boundaries.
    ///
    /// HOW:
    /// - Decode two fixed two-byte sequences containing adjacent character codes.
    /// - Assert the resulting strings are `!0` and ` !`.
    #[test]
    fn multi_char() {
        assert_eq!("!0", decode(&[254, 1]).unwrap());
        assert_eq!(" !", decode(&[0b0101_0011, 0b1111_1000]).unwrap());
    }

    /// STATEMENT: Every byte value round-trips, including codes longer than the lookup table.
    ///
    /// HOW:
    /// - Encode all 256 byte values, forward and reversed, so codes of every length up to 30
    ///   bits follow both short and long codes.
    /// - Decode each output and assert it equals the input.
    #[test]
    fn encode_decode_all_bytes() {
        let forward: Vec<u8> = (0..=255).collect();
        let reversed: Vec<u8> = (0..=255).rev().collect();

        for s in [forward, reversed] {
            let mut dst = BytesMut::new();
            encode(&s, &mut dst);
            assert_eq!(&decode(&dst).unwrap()[..], &s[..]);
        }
    }

    /// STATEMENT: An encoded EOS, padding longer than 7 bits, and padding that is not all
    /// ones are rejected without leaving output in the buffer.
    ///
    /// HOW:
    /// - Decode an encoded EOS, `o` followed by a full byte of ones, a lone byte of ones, and
    ///   `o` padded with a zero bit, each into a buffer holding a prefix.
    /// - Assert each fails with `InvalidHuffmanCode` and the buffer still holds only the prefix.
    #[test]
    fn decode_rejects_eos_and_bad_padding() {
        let cases: &[&[u8]] = &[&[0xff, 0xff, 0xff, 0xfc], &[0x3f, 0xff], &[0xff], &[0b0011_1110]];

        for src in cases {
            let mut buf = BytesMut::from(&b"prefix"[..]);
            assert!(matches!(
                super::decode(src, &mut buf),
                Err(DecoderError::InvalidHuffmanCode)
            ));
            assert_eq!(&buf[..], b"prefix");
        }
    }

    /// STATEMENT: Short Huffman codes are padded into the expected single output byte.
    ///
    /// HOW:
    /// - Encode `o`, `0`, and `A`, clearing the destination between inputs.
    /// - Compare each complete output with its expected one-byte representation.
    #[test]
    fn encode_single_byte() {
        let mut dst = BytesMut::with_capacity(1);

        encode(b"o", &mut dst);
        assert_eq!(&dst[..], &[0b0011_1111]);

        dst.clear();
        encode(b"0", &mut dst);
        assert_eq!(&dst[..], &[7]);

        dst.clear();
        encode(b"A", &mut dst);
        assert_eq!(&dst[..], &[(0x21 << 2) + 3]);
    }

    /// STATEMENT: Huffman encoding and decoding round-trip representative text strings.
    ///
    /// HOW:
    /// - Encode header names, values, URLs, mixed-case strings, and longer prose samples.
    /// - Decode each output and compare its bytes with the original string.
    #[test]
    fn encode_decode_str() {
        const DATA: &[&str] = &[
            "hello world",
            ":method",
            ":scheme",
            ":authority",
            "yahoo.co.jp",
            "GET",
            "http",
            ":path",
            "/images/top/sp2/cmn/logo-ns-130528.png",
            "example.com",
            "hpack-test",
            "xxxxxxx1",
            "Mozilla/5.0 (Macintosh; Intel Mac OS X 10.8; rv:16.0) Gecko/20100101 Firefox/16.0",
            "accept",
            "Accept",
            "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8",
            "cookie",
            "B=76j09a189a6h4&b=3&s=0b",
            "TE",
            "Lorem ipsum dolor sit amet, consectetur adipiscing elit. Morbi non bibendum libero. \
             Etiam ultrices lorem ut.",
        ];

        for s in DATA {
            let mut dst = BytesMut::with_capacity(s.len());

            encode(s.as_bytes(), &mut dst);

            let decoded = decode(&dst).unwrap();

            assert_eq!(&decoded[..], s.as_bytes());
        }
    }

    /// STATEMENT: Huffman encoding and decoding round-trip binary bytes, including zero and
    /// high values.
    ///
    /// HOW:
    /// - Encode byte slices containing single and repeated zeros, control bytes, and high-
    ///   valued bytes.
    /// - Decode each output and assert exact equality with the original byte slice.
    #[test]
    fn encode_decode_u8() {
        const DATA: &[&[u8]] = &[b"\0", b"\0\0\0", b"\0\x01\x02\x03\x04\x05", b"\xFF\xF8"];

        for s in DATA {
            let mut dst = BytesMut::with_capacity(s.len());

            encode(s, &mut dst);

            let decoded = decode(&dst).unwrap();

            assert_eq!(&decoded[..], &s[..]);
        }
    }
}
