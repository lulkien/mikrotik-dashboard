//! Wire framing for the RouterOS binary API.
//!
//! A *word* is a length prefix followed by that many bytes. A *sentence* is a
//! sequence of words terminated by a zero-length word. A command is a sentence
//! whose first word is the command name (`/system/resource/print`) and whose
//! remaining words are attributes: `=key=value` for a setting, `?key=value` for a
//! query, `=.proplist=a,b` to pick fields, `.tag=N` to tag the reply. A reply
//! sentence starts with `!re` (a row), `!done` (end of reply), `!trap` (the
//! router refused) or `!fatal` (the router is closing the connection).
//!
//! The length prefix is variable-width, which is the one detail that has to be
//! read exactly as RouterOS writes it:
//!
//! ```text
//!   0xxxxxxx                                            len 0..0x7f     1 byte
//!   10xxxxxx xxxxxxxx                                   len ..0x3fff    2 bytes
//!   110xxxxx xxxxxxxx xxxxxxxx                          len ..0x1fffff  3 bytes
//!   1110xxxx xxxxxxxx xxxxxxxx xxxxxxxx                 len ..0xfffffff 4 bytes
//!   11110000 xxxxxxxx xxxxxxxx xxxxxxxx xxxxxxxx        len ..u32::MAX  5 bytes
//! ```

use std::io::{self, Read, Write};

/// Encode `len` the way RouterOS expects it on the wire.
pub fn encode_len(len: usize) -> Vec<u8> {
    match len {
        l if l < 0x80 => vec![l as u8],
        l if l < 0x4000 => vec![0x80 | (l >> 8) as u8, (l & 0xff) as u8],
        l if l < 0x20_0000 => vec![
            0xC0 | (l >> 16) as u8,
            ((l >> 8) & 0xff) as u8,
            (l & 0xff) as u8,
        ],
        l if l < 0x1000_0000 => vec![
            0xE0 | (l >> 24) as u8,
            ((l >> 16) & 0xff) as u8,
            ((l >> 8) & 0xff) as u8,
            (l & 0xff) as u8,
        ],
        l => {
            let mut v = vec![0xF0u8];
            v.extend_from_slice(&(l as u32).to_be_bytes());
            v
        }
    }
}

/// Read one length prefix. `Ok(0)` is the end-of-sentence word.
pub fn read_len<R: Read>(r: &mut R) -> io::Result<usize> {
    let mut b = [0u8; 1];
    r.read_exact(&mut b)?;
    let b0 = b[0];
    let len = match b0 {
        0x00..=0x7F => b0 as usize,
        0x80..=0xBF => {
            let mut b1 = [0u8; 1];
            r.read_exact(&mut b1)?;
            (((b0 & 0x3F) as usize) << 8) | b1[0] as usize
        }
        0xC0..=0xDF => {
            let mut rest = [0u8; 2];
            r.read_exact(&mut rest)?;
            (((b0 & 0x1F) as usize) << 16) | ((rest[0] as usize) << 8) | rest[1] as usize
        }
        0xE0..=0xEF => {
            let mut rest = [0u8; 3];
            r.read_exact(&mut rest)?;
            (((b0 & 0x0F) as usize) << 24)
                | ((rest[0] as usize) << 16)
                | ((rest[1] as usize) << 8)
                | rest[2] as usize
        }
        0xF0 => {
            let mut rest = [0u8; 4];
            r.read_exact(&mut rest)?;
            u32::from_be_bytes(rest) as usize
        }
        _ => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("routeros: bad length prefix 0x{b0:02x}"),
            ));
        }
    };
    Ok(len)
}

/// Write one sentence: every word length-prefixed, then the zero-length terminator.
pub fn write_sentence<W: Write>(w: &mut W, words: &[String]) -> io::Result<()> {
    let mut out = Vec::new();
    for word in words {
        out.extend_from_slice(&encode_len(word.len()));
        out.extend_from_slice(word.as_bytes());
    }
    out.push(0);
    w.write_all(&out)?;
    w.flush()
}

/// Read one sentence. `Ok(None)` is a clean EOF at a sentence boundary.
///
/// Words are returned with their leading tag character intact (`=name=ether1`,
/// `!re`, `.tag=1`); `client::RosClient` is what decides what they mean.
pub fn read_sentence<R: Read>(r: &mut R) -> io::Result<Option<Vec<String>>> {
    let mut words: Vec<String> = Vec::new();
    loop {
        let len = match read_len(r) {
            Ok(l) => l,
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => {
                return if words.is_empty() {
                    Ok(None)
                } else {
                    Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "routeros: connection closed mid-sentence",
                    ))
                };
            }
            Err(e) => return Err(e),
        };
        if len == 0 {
            return Ok(Some(words));
        }
        let mut buf = vec![0u8; len];
        r.read_exact(&mut buf)?;
        match String::from_utf8(buf) {
            Ok(s) => words.push(s),
            // The API is byte-transparent: `/file/read` and `/export` answer with
            // raw bytes that need not be UTF-8. Decode lossily rather than dropping
            // the row.
            Err(e) => words.push(String::from_utf8_lossy(e.as_bytes()).into_owned()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn length_prefixes_round_trip_at_every_width() {
        for len in [
            0usize,
            1,
            0x7f,
            0x80,
            0x3fff,
            0x4000,
            0x1f_ffff,
            0x20_0000,
            0x0fff_ffff,
        ] {
            let enc = encode_len(len);
            let mut cur = Cursor::new(enc.clone());
            assert_eq!(read_len(&mut cur).unwrap(), len, "len {len}: {enc:02x?}");
        }
    }

    #[test]
    fn sentence_round_trips() {
        let words = vec![
            "/ip/firewall/connection/print".to_string(),
            "=.proplist=src-address,dst-address".to_string(),
        ];
        let mut buf = Vec::new();
        write_sentence(&mut buf, &words).unwrap();
        let mut cur = Cursor::new(buf);
        assert_eq!(read_sentence(&mut cur).unwrap().unwrap(), words);
        assert!(read_sentence(&mut cur).unwrap().is_none());
    }

    #[test]
    fn sentence_over_127_bytes_uses_a_two_byte_prefix() {
        let big = "x".repeat(300);
        let mut buf = Vec::new();
        write_sentence(&mut buf, std::slice::from_ref(&big)).unwrap();
        assert_eq!(buf[0], 0x80 | 1, "300 = 0x12c encodes as 0x81 0x2c");
        let mut cur = Cursor::new(buf);
        assert_eq!(read_sentence(&mut cur).unwrap().unwrap(), vec![big]);
    }
}
