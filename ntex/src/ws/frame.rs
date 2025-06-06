use nanorand::{Rng, WyRand};

use super::proto::{CloseCode, CloseReason, OpCode};
use super::{error::ProtocolError, mask::apply_mask};
use crate::util::{Buf, BufMut, Bytes, BytesMut};

/// WebSocket frame parser.
#[derive(Debug)]
pub struct Parser;

impl Parser {
    fn parse_metadata(
        src: &[u8],
        server: bool,
        max_size: usize,
    ) -> Result<Option<(usize, bool, OpCode, usize, Option<u32>)>, ProtocolError> {
        let chunk_len = src.len();

        let mut idx = 2;
        if chunk_len < 2 {
            return Ok(None);
        }

        let first = src[0];
        let second = src[1];
        let finished = first & 0x80 != 0;

        // check masking
        let masked = second & 0x80 != 0;
        if !masked && server {
            return Err(ProtocolError::UnmaskedFrame);
        } else if masked && !server {
            return Err(ProtocolError::MaskedFrame);
        }

        // Op code
        let opcode = OpCode::from(first & 0x0F);

        if let OpCode::Bad = opcode {
            return Err(ProtocolError::InvalidOpcode(first & 0x0F));
        }

        let len = second & 0x7F;
        let length = if len == 126 {
            if chunk_len < 4 {
                return Ok(None);
            }
            let len = usize::from(u16::from_be_bytes(
                TryFrom::try_from(&src[idx..idx + 2]).unwrap(),
            ));
            idx += 2;
            len
        } else if len == 127 {
            if chunk_len < 10 {
                return Ok(None);
            }
            let len = u64::from_be_bytes(TryFrom::try_from(&src[idx..idx + 8]).unwrap());
            if len > max_size as u64 {
                return Err(ProtocolError::Overflow);
            }
            idx += 8;
            len as usize
        } else {
            len as usize
        };

        // check for max allowed size
        if length > max_size {
            return Err(ProtocolError::Overflow);
        }

        let mask = if server {
            if chunk_len < idx + 4 {
                return Ok(None);
            }

            let mask = u32::from_le_bytes(TryFrom::try_from(&src[idx..idx + 4]).unwrap());
            idx += 4;
            Some(mask)
        } else {
            None
        };

        Ok(Some((idx, finished, opcode, length, mask)))
    }

    /// Parse the input stream into a frame.
    pub fn parse(
        src: &mut BytesMut,
        server: bool,
        max_size: usize,
    ) -> Result<Option<(bool, OpCode, Option<Bytes>)>, ProtocolError> {
        // try to parse ws frame metadata
        let (idx, finished, opcode, length, mask) =
            match Parser::parse_metadata(src, server, max_size)? {
                None => return Ok(None),
                Some(res) => res,
            };

        // not enough data
        if src.len() < idx + length {
            return Ok(None);
        }

        // remove prefix
        src.advance(idx);

        // no need for body
        if length == 0 {
            return Ok(Some((finished, opcode, None)));
        }

        // control frames must have length <= 125
        match opcode {
            OpCode::Ping | OpCode::Pong if length > 125 => {
                return Err(ProtocolError::InvalidLength(length));
            }
            OpCode::Close if length > 125 => {
                log::trace!("Received close frame with payload length exceeding 125. Morphing to protocol close frame.");
                return Ok(Some((true, OpCode::Close, None)));
            }
            _ => (),
        }

        // unmask
        if let Some(mask) = mask {
            apply_mask(&mut src[..length], mask);
        }

        Ok(Some((
            finished,
            opcode,
            Some(src.split_to(length).freeze()),
        )))
    }

    /// Parse the payload of a close frame.
    pub fn parse_close_payload(payload: &[u8]) -> Option<CloseReason> {
        if payload.len() >= 2 {
            let raw_code = u16::from_be_bytes(TryFrom::try_from(&payload[..2]).unwrap());
            let code = CloseCode::from(raw_code);
            let description = if payload.len() > 2 {
                Some(String::from_utf8_lossy(&payload[2..]).into())
            } else {
                None
            };
            Some(CloseReason { code, description })
        } else {
            None
        }
    }

    /// Generate binary representation
    pub fn write_message<B: AsRef<[u8]>>(
        dst: &mut BytesMut,
        pl: B,
        op: OpCode,
        fin: bool,
        mask: bool,
    ) {
        let payload = pl.as_ref();
        let one: u8 = if fin {
            0x80 | Into::<u8>::into(op)
        } else {
            op.into()
        };
        let payload_len = payload.len();
        let (two, p_len) = if mask {
            (0x80, payload_len + 4)
        } else {
            (0, payload_len)
        };

        if payload_len < 126 {
            dst.reserve(p_len + 2);
            dst.extend_from_slice(&[one, two | payload_len as u8]);
        } else if payload_len <= 65_535 {
            dst.reserve(p_len + 4);
            dst.extend_from_slice(&[one, two | 126]);
            dst.put_u16(payload_len as u16);
        } else {
            dst.reserve(p_len + 10);
            dst.extend_from_slice(&[one, two | 127]);
            dst.put_u64(payload_len as u64);
        };

        if mask {
            let mask: u32 = WyRand::new().generate();
            dst.put_u32_le(mask);
            dst.extend_from_slice(payload);
            let pos = dst.len() - payload_len;
            apply_mask(&mut dst[pos..], mask);
        } else {
            dst.extend_from_slice(payload);
        }
    }

    /// Create a new Close control frame.
    #[inline]
    pub fn write_close(dst: &mut BytesMut, reason: Option<CloseReason>, mask: bool) {
        let payload = match reason {
            None => Vec::new(),
            Some(reason) => {
                let mut payload = Into::<u16>::into(reason.code).to_be_bytes().to_vec();
                if let Some(description) = reason.description {
                    payload.extend(description.as_bytes());
                }
                payload
            }
        };

        Parser::write_message(dst, payload, OpCode::Close, true, mask)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct F {
        finished: bool,
        opcode: OpCode,
        payload: Bytes,
    }

    fn is_none(frm: &Result<Option<(bool, OpCode, Option<Bytes>)>, ProtocolError>) -> bool {
        matches!(*frm, Ok(None))
    }

    fn extract(frm: Result<Option<(bool, OpCode, Option<Bytes>)>, ProtocolError>) -> F {
        match frm {
            Ok(Some((finished, opcode, payload))) => F {
                finished,
                opcode,
                payload: payload.unwrap_or_else(Bytes::new),
            },
            _ => unreachable!("error"),
        }
    }

    #[test]
    fn test_parse() {
        let mut buf = BytesMut::from(&[0b0000_0001u8, 0b0000_0001u8][..]);
        assert!(is_none(&Parser::parse(&mut buf, false, 1024)));

        let mut buf = BytesMut::from(&[0b0000_0001u8, 0b0000_0001u8][..]);
        buf.extend(b"1");

        let frame = extract(Parser::parse(&mut buf, false, 1024));
        assert!(!frame.finished);
        assert_eq!(frame.opcode, OpCode::Text);
        assert_eq!(frame.payload.as_ref(), &b"1"[..]);
    }

    #[test]
    fn test_parse_length0() {
        let mut buf = BytesMut::from(&[0b0000_0001u8, 0b0000_0000u8][..]);
        let frame = extract(Parser::parse(&mut buf, false, 1024));
        assert!(!frame.finished);
        assert_eq!(frame.opcode, OpCode::Text);
        assert!(frame.payload.is_empty());
    }

    #[test]
    fn test_parse_length2() {
        let mut buf = BytesMut::from(&[0b0000_0001u8, 126u8][..]);
        assert!(is_none(&Parser::parse(&mut buf, false, 1024)));

        let mut buf = BytesMut::from(&[0b0000_0001u8, 126u8][..]);
        buf.extend(&[0u8, 4u8][..]);
        buf.extend(b"1234");

        let frame = extract(Parser::parse(&mut buf, false, 1024));
        assert!(!frame.finished);
        assert_eq!(frame.opcode, OpCode::Text);
        assert_eq!(frame.payload.as_ref(), &b"1234"[..]);
    }

    #[test]
    fn test_parse_length4() {
        let mut buf = BytesMut::from(&[0b0000_0001u8, 127u8][..]);
        assert!(is_none(&Parser::parse(&mut buf, false, 1024)));

        let mut buf = BytesMut::from(&[0b0000_0001u8, 127u8][..]);
        buf.extend(&[0u8, 0u8, 0u8, 0u8, 0u8, 0u8, 0u8, 4u8][..]);
        buf.extend(b"1234");

        let frame = extract(Parser::parse(&mut buf, false, 1024));
        assert!(!frame.finished);
        assert_eq!(frame.opcode, OpCode::Text);
        assert_eq!(frame.payload.as_ref(), &b"1234"[..]);
    }

    #[test]
    fn test_parse_frame_mask() {
        let mut buf = BytesMut::from(&[0b0000_0001u8, 0b1000_0001u8][..]);
        buf.extend(b"0001");
        buf.extend(b"1");

        assert!(Parser::parse(&mut buf, false, 1024).is_err());

        let frame = extract(Parser::parse(&mut buf, true, 1024));
        assert!(!frame.finished);
        assert_eq!(frame.opcode, OpCode::Text);
        assert_eq!(frame.payload, Bytes::from(vec![1u8]));
    }

    #[test]
    fn test_parse_frame_no_mask() {
        let mut buf = BytesMut::from(&[0b0000_0001u8, 0b0000_0001u8][..]);
        buf.extend([1u8]);

        assert!(Parser::parse(&mut buf, true, 1024).is_err());

        let frame = extract(Parser::parse(&mut buf, false, 1024));
        assert!(!frame.finished);
        assert_eq!(frame.opcode, OpCode::Text);
        assert_eq!(frame.payload, Bytes::from(vec![1u8]));
    }

    #[test]
    fn test_parse_frame_max_size() {
        let mut buf = BytesMut::from(&[0b0000_0001u8, 0b0000_0010u8][..]);
        buf.extend([1u8, 1u8]);

        assert!(Parser::parse(&mut buf, true, 1).is_err());

        if let Err(ProtocolError::Overflow) = Parser::parse(&mut buf, false, 0) {
        } else {
            unreachable!("error");
        }
    }

    #[test]
    fn test_ping_frame() {
        let mut buf = BytesMut::new();
        Parser::write_message(&mut buf, Vec::from("data"), OpCode::Ping, true, false);

        let mut v = vec![137u8, 4u8];
        v.extend(b"data");
        assert_eq!(&buf[..], &v[..]);
    }

    #[test]
    fn test_pong_frame() {
        let mut buf = BytesMut::new();
        Parser::write_message(&mut buf, Vec::from("data"), OpCode::Pong, true, false);

        let mut v = vec![138u8, 4u8];
        v.extend(b"data");
        assert_eq!(&buf[..], &v[..]);
    }

    #[test]
    fn test_close_frame() {
        let mut buf = BytesMut::new();
        let reason = (CloseCode::Normal, "data");
        Parser::write_close(&mut buf, Some(reason.into()), false);

        let mut v = vec![136u8, 6u8, 3u8, 232u8];
        v.extend(b"data");
        assert_eq!(&buf[..], &v[..]);
    }

    #[test]
    fn test_empty_close_frame() {
        let mut buf = BytesMut::new();
        Parser::write_close(&mut buf, None, false);
        assert_eq!(&buf[..], &vec![0x88, 0x00][..]);
    }
}
