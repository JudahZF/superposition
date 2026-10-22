//! Length-prefixed framing and the bounded little-endian field codec.

use std::io::{self, Read, Write};

use thiserror::Error;

/// Largest message body a peer may send or accept, excluding the 4-byte length prefix.
pub const MAX_MESSAGE_BYTES: usize = 8 * 1024 * 1024;

const PREFIX_BYTES: usize = 4;

/// Every way a message can fail to encode, decode or travel.
#[derive(Debug, Error)]
pub enum WireError {
    /// The peer closed the stream cleanly between two messages.
    #[error("stream closed")]
    Closed,
    /// Transport failure, including EOF in the middle of a message.
    #[error("stream I/O failed: {0}")]
    Io(#[from] io::Error),
    /// The length prefix or a field exceeds its bound.
    #[error("{what} is {len} bytes or items; the limit is {max}")]
    TooLarge {
        /// The bounded field.
        what: &'static str,
        /// The offending length.
        len: usize,
        /// The bound.
        max: usize,
    },
    /// The body ended before a field did.
    #[error("message is truncated")]
    Truncated,
    /// The body has bytes after its last field.
    #[error("message has {0} trailing bytes")]
    TrailingBytes(usize),
    /// The message type byte is unknown.
    #[error("unknown message tag {0}")]
    UnknownTag(u8),
    /// A field holds a value outside its domain.
    #[error("invalid {0}")]
    Invalid(&'static str),
    /// The Hello does not start with the protocol magic.
    #[error("not a window-stream peer")]
    BadMagic,
    /// A string field is not UTF-8.
    #[error("string field is not UTF-8")]
    Utf8,
}

/// The body layout of one message direction. Sealed: only this crate defines message types.
pub trait Body: Sized {
    /// Appends the body fields, without the length prefix.
    fn encode_body(&self, w: &mut Writer<'_>) -> Result<(), WireError>;
    /// Parses the body fields; trailing bytes are checked by the caller.
    fn decode_body(r: &mut Reader<'_>) -> Result<Self, WireError>;
}

/// Framing shared by [`crate::ClientMessage`] and [`crate::ServerMessage`].
pub trait WireMessage: Body {
    /// Appends one length-prefixed message to `out`. On error `out` is left unchanged.
    fn encode_into(&self, out: &mut Vec<u8>) -> Result<(), WireError> {
        let start = out.len();
        out.extend_from_slice(&[0; PREFIX_BYTES]);
        let result = self.encode_body(&mut Writer { out });
        let len = out.len() - start - PREFIX_BYTES;
        match result {
            Ok(()) if len <= MAX_MESSAGE_BYTES => {
                let len = u32::try_from(len).expect("bounded by MAX_MESSAGE_BYTES");
                out[start..start + PREFIX_BYTES].copy_from_slice(&len.to_le_bytes());
                Ok(())
            }
            Ok(()) => {
                out.truncate(start);
                Err(WireError::TooLarge {
                    what: "message",
                    len,
                    max: MAX_MESSAGE_BYTES,
                })
            }
            Err(error) => {
                out.truncate(start);
                Err(error)
            }
        }
    }

    /// Encodes one length-prefixed message.
    fn encode(&self) -> Result<Vec<u8>, WireError> {
        let mut out = Vec::new();
        self.encode_into(&mut out)?;
        Ok(out)
    }

    /// Decodes one message body (the bytes after the length prefix).
    fn decode(body: &[u8]) -> Result<Self, WireError> {
        if body.len() > MAX_MESSAGE_BYTES {
            return Err(WireError::TooLarge {
                what: "message",
                len: body.len(),
                max: MAX_MESSAGE_BYTES,
            });
        }
        let mut r = Reader { buf: body };
        let message = Self::decode_body(&mut r)?;
        match r.buf.len() {
            0 => Ok(message),
            extra => Err(WireError::TrailingBytes(extra)),
        }
    }

    /// Reads one message, reusing `buf` for the body. Returns [`WireError::Closed`] on a clean
    /// EOF before the first prefix byte.
    fn read_from(reader: &mut impl Read, buf: &mut Vec<u8>) -> Result<Self, WireError> {
        let mut prefix = [0; PREFIX_BYTES];
        let mut filled = 0;
        while filled < PREFIX_BYTES {
            match reader.read(&mut prefix[filled..]) {
                Ok(0) if filled == 0 => return Err(WireError::Closed),
                Ok(0) => return Err(io::Error::from(io::ErrorKind::UnexpectedEof).into()),
                Ok(n) => filled += n,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) => return Err(error.into()),
            }
        }
        let len = u32::from_le_bytes(prefix) as usize;
        if len > MAX_MESSAGE_BYTES {
            return Err(WireError::TooLarge {
                what: "message",
                len,
                max: MAX_MESSAGE_BYTES,
            });
        }
        buf.clear();
        buf.resize(len, 0);
        reader.read_exact(buf)?;
        Self::decode(buf)
    }

    /// Encodes and writes one message.
    fn write_to(&self, writer: &mut impl Write) -> Result<(), WireError> {
        writer.write_all(&self.encode()?)?;
        Ok(())
    }
}

impl<T: Body> WireMessage for T {}

/// Appends little-endian fields to a message body.
pub struct Writer<'a> {
    out: &'a mut Vec<u8>,
}

impl Writer<'_> {
    pub(crate) fn u8(&mut self, value: u8) {
        self.out.push(value);
    }
    pub(crate) fn bool(&mut self, value: bool) {
        self.out.push(u8::from(value));
    }
    pub(crate) fn u16(&mut self, value: u16) {
        self.out.extend_from_slice(&value.to_le_bytes());
    }
    pub(crate) fn i16(&mut self, value: i16) {
        self.out.extend_from_slice(&value.to_le_bytes());
    }
    pub(crate) fn u32(&mut self, value: u32) {
        self.out.extend_from_slice(&value.to_le_bytes());
    }
    pub(crate) fn u64(&mut self, value: u64) {
        self.out.extend_from_slice(&value.to_le_bytes());
    }
    pub(crate) fn opt_u32(&mut self, value: Option<u32>) {
        self.bool(value.is_some());
        if let Some(value) = value {
            self.u32(value);
        }
    }
    /// A `u32` item count bounded by `max`.
    pub(crate) fn count(
        &mut self,
        what: &'static str,
        len: usize,
        max: usize,
    ) -> Result<(), WireError> {
        if len > max {
            return Err(WireError::TooLarge { what, len, max });
        }
        self.u32(u32::try_from(len).map_err(|_| WireError::TooLarge { what, len, max })?);
        Ok(())
    }
    /// A `u32`-length byte string bounded by `max`.
    pub(crate) fn bytes(
        &mut self,
        what: &'static str,
        value: &[u8],
        max: usize,
    ) -> Result<(), WireError> {
        self.count(what, value.len(), max)?;
        self.out.extend_from_slice(value);
        Ok(())
    }
    pub(crate) fn str(
        &mut self,
        what: &'static str,
        value: &str,
        max: usize,
    ) -> Result<(), WireError> {
        self.bytes(what, value.as_bytes(), max)
    }
}

/// Reads little-endian fields from a message body; every read is bounds-checked.
pub struct Reader<'a> {
    buf: &'a [u8],
}

impl<'a> Reader<'a> {
    fn take<const N: usize>(&mut self) -> Result<[u8; N], WireError> {
        let (head, rest) = self
            .buf
            .split_first_chunk::<N>()
            .ok_or(WireError::Truncated)?;
        self.buf = rest;
        Ok(*head)
    }
    pub(crate) fn u8(&mut self) -> Result<u8, WireError> {
        Ok(self.take::<1>()?[0])
    }
    pub(crate) fn bool(&mut self) -> Result<bool, WireError> {
        match self.u8()? {
            0 => Ok(false),
            1 => Ok(true),
            _ => Err(WireError::Invalid("boolean")),
        }
    }
    pub(crate) fn u16(&mut self) -> Result<u16, WireError> {
        Ok(u16::from_le_bytes(self.take()?))
    }
    pub(crate) fn i16(&mut self) -> Result<i16, WireError> {
        Ok(i16::from_le_bytes(self.take()?))
    }
    pub(crate) fn u32(&mut self) -> Result<u32, WireError> {
        Ok(u32::from_le_bytes(self.take()?))
    }
    pub(crate) fn u64(&mut self) -> Result<u64, WireError> {
        Ok(u64::from_le_bytes(self.take()?))
    }
    pub(crate) fn opt_u32(&mut self) -> Result<Option<u32>, WireError> {
        Ok(if self.bool()? {
            Some(self.u32()?)
        } else {
            None
        })
    }
    /// A `u32` item count bounded by `max` and by the remaining bytes at `min_item_bytes` each,
    /// so a hostile count cannot reserve memory the body does not back.
    pub(crate) fn count(
        &mut self,
        what: &'static str,
        max: usize,
        min_item_bytes: usize,
    ) -> Result<usize, WireError> {
        let len = self.u32()? as usize;
        if len > max {
            return Err(WireError::TooLarge { what, len, max });
        }
        if len.saturating_mul(min_item_bytes) > self.buf.len() {
            return Err(WireError::Truncated);
        }
        Ok(len)
    }
    pub(crate) fn bytes(&mut self, what: &'static str, max: usize) -> Result<&'a [u8], WireError> {
        let len = self.count(what, max, 1)?;
        let (head, rest) = self.buf.split_at(len);
        self.buf = rest;
        Ok(head)
    }
    pub(crate) fn string(&mut self, what: &'static str, max: usize) -> Result<String, WireError> {
        let bytes = self.bytes(what, max)?;
        String::from_utf8(bytes.to_vec()).map_err(|_| WireError::Utf8)
    }
}
