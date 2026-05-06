//! Wire-format encoder / decoder.
//!
//! Every transfer over EP1 (in either direction) is a single frame:
//!
//! ```text
//! +------+------+------+----------------------+
//! | len_lo | len_hi | op  |  payload …        |
//! +------+------+------+----------------------+
//!   ^^^ uint16 LE ^^^   ^^^ length-1 bytes ^^^
//! ```
//!
//! - `length` is the number of bytes that follow the length field, which
//!   *includes* the opcode byte. So an empty payload (just an ACK) has
//!   length = 1.
//! - 64-byte max packet at the USB layer means the bulk endpoint handles
//!   chunking transparently when we transferOut more than 64 bytes; we
//!   don't need to do anything special for that.

use crate::stemplayer::error::{Error, Result};

/// Generic acknowledgement / no-payload opcode.
pub const OP_ACK: u8 = 0x00;
/// Negative acknowledgement (the device's "no" reply).
pub const OP_NAK: u8 = 0x01;
/// Application-level CONNECT — host says "I'm taking the session".
/// The device replies with ACK and then accepts CONTROL/FILE commands.
pub const OP_CONNECT: u8 = 0x02;
/// DISCONNECT — politely close the application-level session.
pub const OP_DISCONNECT: u8 = 0x03;
/// Command opcode. Payload is a sub-command byte, optional JSON.
pub const OP_CMD: u8 = 0x04;
/// Device's RESPONSE to a CONTROL query. Payload: [sub_byte, ...json, NUL].
pub const OP_RESPONSE: u8 = 0x05;
/// Announce next file. Payload: JSON `{size, type, name}` + NUL terminator.
pub const OP_FILE_HEADER: u8 = 0x06;
/// File content chunk. Payload: u32 LE chunk size + 1 byte flag + bytes.
pub const OP_FILE_CHUNK: u8 = 0x07;
/// ABORT — cancel in-progress operation.
pub const OP_ABORT: u8 = 0x08;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Frame {
    pub op: u8,
    pub payload: Vec<u8>,
}

impl Frame {
    pub fn new(op: u8, payload: Vec<u8>) -> Self {
        Self { op, payload }
    }

    /// `01 00 00` — generic ACK frame.
    pub fn ack() -> Self {
        Self::new(OP_ACK, Vec::new())
    }

    pub fn encode(&self) -> Result<Vec<u8>> {
        let length: u16 = (1 + self.payload.len())
            .try_into()
            .map_err(|_| Error::FrameTooLarge(1 + self.payload.len()))?;
        let mut out = Vec::with_capacity(2 + length as usize);
        out.push((length & 0xff) as u8);
        out.push((length >> 8) as u8);
        out.push(self.op);
        out.extend_from_slice(&self.payload);
        Ok(out)
    }

    pub fn decode(buf: &[u8]) -> Result<Self> {
        if buf.len() < 3 {
            return Err(Error::FrameTruncated {
                expected: 3,
                got: buf.len(),
            });
        }
        let length = u16::from_le_bytes([buf[0], buf[1]]) as usize;
        if buf.len() < 2 + length {
            return Err(Error::FrameTruncated {
                expected: 2 + length,
                got: buf.len(),
            });
        }
        let op = buf[2];
        let payload = buf[3..2 + length].to_vec();
        Ok(Self { op, payload })
    }

    pub fn is_ack(&self) -> bool {
        self.op == OP_ACK
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ack_round_trip() {
        let bytes = Frame::ack().encode().unwrap();
        assert_eq!(bytes, vec![0x01, 0x00, 0x00]);
        let decoded = Frame::decode(&bytes).unwrap();
        assert!(decoded.is_ack());
        assert!(decoded.payload.is_empty());
    }

    #[test]
    fn cmd04_with_json_payload() {
        // Replicates a captured frame:
        //   length=0x11 (17), op=0x04, sub=0x05, JSON {"album":"A1"}, NUL
        let json = b"{\"album\":\"A1\"}\0";
        let mut payload = Vec::new();
        payload.push(0x05); // sub-command
        payload.extend_from_slice(json);
        let frame = Frame::new(OP_CMD, payload);
        let bytes = frame.encode().unwrap();
        assert_eq!(bytes[0], 0x11);
        assert_eq!(bytes[1], 0x00);
        assert_eq!(bytes[2], 0x04);
        assert_eq!(bytes[3], 0x05);
        let decoded = Frame::decode(&bytes).unwrap();
        assert_eq!(frame, decoded);
    }

    #[test]
    fn truncated_returns_error() {
        let buf = [0x10u8, 0x00]; // length says 16, but no payload
        assert!(matches!(
            Frame::decode(&buf),
            Err(Error::FrameTruncated { .. })
        ));
    }
}
