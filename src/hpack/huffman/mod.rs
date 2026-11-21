use ntex_bytes::{BufMut, Bytes, BytesMut, SpareWriter};

mod table;

use self::table::ENCODE_TABLE;
use crate::hpack::DecoderError;

/// Bits looked up at once. Every printable ASCII character except
/// `$<@[\]^{}~` and the backtick has a code of at most this length.
const PEEK_BITS: u32 = 12;
/// Longest code in the HPACK Huffman table (EOS).
const MAX_BITS: usize = 30;
const EOS: u16 = 256;

/// Short codes indexed by the next `PEEK_BITS` input bits.
///
/// Bits 0..8 of an entry hold the symbol of the first code and bits 16..20
/// its length; zero length means the code is longer than `PEEK_BITS`. If the
/// next code also fits in the index, bits 8..16 hold its symbol and bits
/// 20..24 its length, otherwise they are zero.
static PEEK_TABLE: [u32; 1 << PEEK_BITS] = build_peek_table();

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

const fn build_peek_table() -> [u32; 1 << PEEK_BITS] {
    const MASK: usize = (1 << PEEK_BITS) - 1;

    // First code only
    let mut single = [0; 1 << PEEK_BITS];
    let mut sym = 0;
    while sym < ENCODE_TABLE.len() {
        let (len, code) = ENCODE_TABLE[sym];
        if len <= PEEK_BITS as usize {
            let shift = PEEK_BITS as usize - len;
            let start = (code as usize) << shift;
            let mut i = 0;
            while i < 1 << shift {
                single[start + i] = sym as u32 | (len as u32) << 16;
                i += 1;
            }
        }
        sym += 1;
    }

    // Add the next code if it fits in the bits left after the first one
    let mut table = single;
    let mut idx = 0;
    while idx < 1 << PEEK_BITS {
        let len = single[idx] >> 16;
        if len != 0 {
            let next = single[(idx << len) & MASK];
            let next_len = next >> 16;
            if next_len != 0 && len + next_len <= PEEK_BITS {
                table[idx] |= (next & 0xff) << 8 | next_len << 20;
            }
        }
        idx += 1;
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
    let start = buf.len();
    // The shortest code is 5 bits
    let res = decode_into(src, &mut buf.spare_writer(src.len() * 8 / 5));

    match res {
        Ok(()) => Ok(buf.take()),
        Err(e) => {
            buf.truncate(start);
            Err(e)
        }
    }
}

/// Decodes `src` into `out`.
fn decode_into(src: &[u8], out: &mut SpareWriter<'_>) -> Result<(), DecoderError> {
    // Pending input bits, most significant first. The bits below them may
    // already hold the start of the next input byte.
    let mut acc: u64 = 0;
    let mut bits: u32 = 0;
    let mut pos = 0;

    loop {
        if let Some(word) = src[pos..].first_chunk::<8>() {
            // Keep the whole bytes that fit, 56 to 63 bits. The bits of the
            // next byte that also land in `acc` are loaded again at the same
            // place by the next refill.
            acc |= u64::from_be_bytes(*word) >> bits;
            let n = (63 - bits) / 8;
            pos += n as usize;
            bits += 8 * n;
        } else {
            while bits <= 56 && pos < src.len() {
                acc |= u64::from(src[pos]) << (56 - bits);
                pos += 1;
                bits += 8;
            }
        }
        let last = pos == src.len();

        // Before the end of the input, a short code always fits in `bits`
        while bits >= PEEK_BITS || (last && bits != 0) {
            let entry = PEEK_TABLE[(acc >> (64 - PEEK_BITS)) as usize];
            let len = (entry >> 16) & 0xf;
            let pair_len = len + (entry >> 20);

            if pair_len > len && pair_len <= bits {
                out.extend_from_slice(&(entry as u16).to_le_bytes());
                acc <<= pair_len;
                bits -= pair_len;
            } else if len != 0 && len <= bits {
                out.push(entry as u8);
                acc <<= len;
                bits -= len;
            } else if len == 0 {
                // `None` means the code may continue in the next input bytes
                match decode_long(acc, bits) {
                    Some((EOS, _)) => return Err(DecoderError::InvalidHuffmanCode),
                    Some((sym, len)) => {
                        out.push(sym as u8);
                        acc <<= len;
                        bits -= len;
                    }
                    None => break,
                }
            } else {
                break;
            }
        }

        if last {
            break;
        }
    }

    // Remaining bits are padding: at most 7 bits of the EOS prefix (all ones)
    if bits > 7 || (bits != 0 && acc >> (64 - bits) != (1 << bits) - 1) {
        return Err(DecoderError::InvalidHuffmanCode);
    }

    Ok(())
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

    /// STATEMENT: The table decoder agrees with a bit-by-bit decoder on valid, truncated, and
    /// corrupted input, wherever codes and padding fall relative to refills.
    ///
    /// HOW:
    /// - Encode pseudo-random strings of 0 to 47 bytes that mix printable characters with
    ///   arbitrary bytes, so short, paired, and long codes cross refill boundaries.
    /// - Decode each encoding, every prefix of it, and every single-bit flip of it.
    /// - Assert each result, success or `InvalidHuffmanCode`, matches a reference decoder that
    ///   matches codes from `ENCODE_TABLE` one bit at a time.
    #[test]
    fn decode_matches_bitwise_decoder() {
        let codes: std::collections::HashMap<_, _> = ENCODE_TABLE
            .iter()
            .enumerate()
            .map(|(sym, &code)| (code, sym))
            .collect();
        let reference = |src: &[u8]| {
            let mut out = Vec::new();
            let (mut len, mut code) = (0, 0);
            for i in 0..src.len() * 8 {
                code = code << 1 | u64::from(src[i / 8] >> (7 - i % 8) & 1);
                len += 1;
                if let Some(&sym) = codes.get(&(len, code)) {
                    if sym == usize::from(EOS) {
                        return None;
                    }
                    out.push(sym as u8);
                    (len, code) = (0, 0);
                }
            }
            (len <= 7 && code == (1 << len) - 1).then_some(out)
        };
        let check = |src: &[u8]| {
            let got = decode(src).ok().map(|b| b.to_vec());
            assert_eq!(got, reference(src), "input {src:x?}");
        };

        let mut seed = 0x9e37_79b9_u32;
        for len in 0..48 {
            for _ in 0..8 {
                let s: Vec<u8> = (0..len)
                    .map(|_| {
                        seed ^= seed << 13;
                        seed ^= seed >> 17;
                        seed ^= seed << 5;
                        if seed.is_multiple_of(4) {
                            seed as u8
                        } else {
                            b' ' + (seed >> 8) as u8 % 95
                        }
                    })
                    .collect();
                let mut enc = BytesMut::new();
                encode(&s, &mut enc);
                assert_eq!(decode(&enc).unwrap(), s);

                for cut in 0..enc.len() {
                    check(&enc[..cut]);
                }
                for bit in 0..enc.len() * 8 {
                    let mut flipped = enc.to_vec();
                    flipped[bit / 8] ^= 0x80 >> (bit % 8);
                    check(&flipped);
                }
            }
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
