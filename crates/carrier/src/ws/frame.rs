//! RFC 6455 frame headers and masking.

pub(crate) const OP_CONTINUATION: u8 = 0;
pub(crate) const OP_TEXT: u8 = 1;
pub(crate) const OP_BINARY: u8 = 2;
pub(crate) const OP_CLOSE: u8 = 8;
pub(crate) const OP_PING: u8 = 9;
pub(crate) const OP_PONG: u8 = 10;

pub(crate) const MAX_CONTROL_PAYLOAD: usize = 125;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Header {
    pub fin: bool,
    pub rsv1: bool,
    pub rsv2: bool,
    pub rsv3: bool,
    pub opcode: u8,
    pub len: u64,
    pub mask: Option<[u8; 4]>,
}

/// The length's top bit set, which coder/websocket reads as negative.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct NegativeLength(pub u64);

/// Parses the header at the front of b, returning it and its size, or None
/// when b doesn't hold all of it yet.
pub(crate) fn parse_header(b: &[u8]) -> Result<Option<(Header, usize)>, NegativeLength> {
    if b.len() < 2 {
        return Ok(None);
    }
    let masked = b[1] & 0x80 != 0;
    let (len, mut n) = match b[1] & 0x7f {
        126 => {
            if b.len() < 4 {
                return Ok(None);
            }
            (u16::from_be_bytes([b[2], b[3]]) as u64, 4)
        }
        127 => {
            if b.len() < 10 {
                return Ok(None);
            }
            let len = u64::from_be_bytes(b[2..10].try_into().unwrap());
            if len >> 63 != 0 {
                return Err(NegativeLength(len));
            }
            (len, 10)
        }
        l => (l as u64, 2),
    };
    let mask = if masked {
        if b.len() < n + 4 {
            return Ok(None);
        }
        let m = b[n..n + 4].try_into().unwrap();
        n += 4;
        Some(m)
    } else {
        None
    };
    Ok(Some((
        Header {
            fin: b[0] & 0x80 != 0,
            rsv1: b[0] & 0x40 != 0,
            rsv2: b[0] & 0x20 != 0,
            rsv3: b[0] & 0x10 != 0,
            opcode: b[0] & 0x0f,
            len,
            mask,
        },
        n,
    )))
}

/// Appends a final frame's header to out, in the shortest length encoding
/// (as coder/websocket writes it).
pub(crate) fn put_header(out: &mut Vec<u8>, rsv1: bool, opcode: u8, len: usize, mask: Option<[u8; 4]>) {
    let mut b0 = 0x80 | opcode;
    if rsv1 {
        b0 |= 0x40;
    }
    out.push(b0);
    let m = if mask.is_some() { 0x80 } else { 0 };
    if len > u16::MAX as usize {
        out.push(m | 127);
        out.extend_from_slice(&(len as u64).to_be_bytes());
    } else if len > 125 {
        out.push(m | 126);
        out.extend_from_slice(&(len as u16).to_be_bytes());
    } else {
        out.push(m | len as u8);
    }
    if let Some(k) = mask {
        out.extend_from_slice(&k);
    }
}

/// XORs b with the masking key, starting at the key's first byte.
pub(crate) fn mask(b: &mut [u8], key: [u8; 4]) {
    let k8 = u64::from_ne_bytes([key[0], key[1], key[2], key[3], key[0], key[1], key[2], key[3]]);
    let mut chunks = b.chunks_exact_mut(8);
    for c in &mut chunks {
        let v = u64::from_ne_bytes((&*c).try_into().unwrap()) ^ k8;
        c.copy_from_slice(&v.to_ne_bytes());
    }
    for (i, x) in chunks.into_remainder().iter_mut().enumerate() {
        *x ^= key[i & 3];
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_round_trip() {
        for len in [0usize, 1, 125, 126, 127, 65535, 65536, 1 << 20] {
            for mask in [None, Some([1, 2, 3, 4])] {
                let mut out = Vec::new();
                put_header(&mut out, len % 2 == 0, OP_BINARY, len, mask);
                let want_n = match len {
                    0..=125 => 2,
                    126..=65535 => 4,
                    _ => 10,
                } + if mask.is_some() { 4 } else { 0 };
                assert_eq!(out.len(), want_n);
                let (h, n) = parse_header(&out).unwrap().unwrap();
                assert_eq!(n, want_n);
                assert_eq!(
                    h,
                    Header { fin: true, rsv1: len % 2 == 0, rsv2: false, rsv3: false, opcode: OP_BINARY, len: len as u64, mask }
                );
                for short in 0..out.len() {
                    assert_eq!(parse_header(&out[..short]), Ok(None));
                }
            }
        }
    }

    #[test]
    fn header_bits() {
        let (h, _) = parse_header(&[0x39, 0x00]).unwrap().unwrap();
        assert!(!h.fin && !h.rsv1 && h.rsv2 && h.rsv3);
        assert_eq!(h.opcode, OP_PING);
    }

    #[test]
    fn negative_length() {
        let mut b = vec![0x82, 127];
        b.extend_from_slice(&(1u64 << 63).to_be_bytes());
        assert_eq!(parse_header(&b), Err(NegativeLength(1 << 63)));
    }

    #[test]
    fn mask_matches_bytewise() {
        let key = [0x12, 0x34, 0x56, 0x78];
        for n in 0..40 {
            let orig: Vec<u8> = (0..n as u8).collect();
            let mut b = orig.clone();
            mask(&mut b, key);
            for (i, x) in b.iter().enumerate() {
                assert_eq!(*x, orig[i] ^ key[i % 4]);
            }
            mask(&mut b, key);
            assert_eq!(b, orig);
        }
    }
}
