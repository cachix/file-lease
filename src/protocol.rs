//! Framing shared by the lease helper and volume manager.

use std::io::{self, Read, Write};
use zeroize::Zeroizing;

pub(crate) const ALLOCATE: u8 = 1;
pub(crate) const HANDOFF: u8 = 2;
pub(crate) const CLOSE: u8 = 3;
pub(crate) const RELEASE: u8 = 4;
pub(crate) const MAX_SECRET_BYTES: u64 = 64 * 1024 * 1024;
pub(crate) const MAX_MESSAGE_BYTES: usize = 16384;
const MAX_SUFFIX_BYTES: usize = 80;

fn validate_suffix(suffix: &str) -> io::Result<()> {
    if suffix.len() > MAX_SUFFIX_BYTES
        || !suffix
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"._-".contains(&byte))
    {
        return Err(io::Error::other("Invalid secret file suffix"));
    }
    Ok(())
}

pub(crate) fn write_allocation(
    stream: &mut impl Write,
    bytes: &[u8],
    suffix: &str,
) -> io::Result<()> {
    // Validate the entire request before writing any part of its frame.
    if bytes.len() as u64 > MAX_SECRET_BYTES {
        return Err(io::Error::other("Secret exceeds file lease size limit"));
    }
    validate_suffix(suffix)?;
    stream.write_all(&[ALLOCATE])?;
    stream.write_all(&(bytes.len() as u64).to_le_bytes())?;
    write_string(stream, suffix)?;
    stream.write_all(bytes)
}

/// Read the request body after the allocation opcode has been consumed.
pub(crate) fn read_allocation(stream: &mut impl Read) -> io::Result<(Zeroizing<Vec<u8>>, String)> {
    let mut length = [0; 8];
    stream.read_exact(&mut length)?;
    let length = u64::from_le_bytes(length);
    if length > MAX_SECRET_BYTES {
        return Err(io::Error::other("Secret exceeds file lease size limit"));
    }
    let suffix = read_string(stream, MAX_SUFFIX_BYTES)?;
    validate_suffix(&suffix)?;
    let mut bytes = Zeroizing::new(vec![0; length as usize]);
    stream.read_exact(&mut bytes)?;
    Ok((bytes, suffix))
}

pub(crate) fn write_string(stream: &mut impl Write, value: &str) -> io::Result<()> {
    if value.len() > MAX_MESSAGE_BYTES {
        return Err(io::Error::other("Oversized protocol message"));
    }
    stream.write_all(&(value.len() as u32).to_le_bytes())?;
    stream.write_all(value.as_bytes())
}

pub(crate) fn read_string(stream: &mut impl Read, maximum: usize) -> io::Result<String> {
    let mut length = [0; 4];
    stream.read_exact(&mut length)?;
    read_string_body(stream, u32::from_le_bytes(length) as usize, maximum)
}

pub(crate) fn read_ack(stream: &mut impl Read, failure: &'static str) -> io::Result<()> {
    let mut ack = [0];
    stream.read_exact(&mut ack)?;
    if ack == [1] {
        Ok(())
    } else {
        Err(io::Error::other(failure))
    }
}

pub(crate) fn write_ack(stream: &mut impl Write, success: bool) -> io::Result<()> {
    stream.write_all(&[u8::from(success)])
}

/// The keeper reserves a string length for errors; volume scopes instead
/// prefix every reply with a status byte. Both carry the same bounded strings.
pub(crate) struct ReplyCodec {
    error_marker: Option<u32>,
    oversized_error: &'static str,
}

pub(crate) const KEEPER_REPLY: ReplyCodec = ReplyCodec {
    error_marker: Some(u32::MAX),
    oversized_error: "Secret storage allocation failed (oversized helper error)",
};
#[cfg(all(
    feature = "shared-volumes",
    any(target_os = "macos", all(test, target_os = "linux"))
))]
pub(crate) const SCOPE_REPLY: ReplyCodec = ReplyCodec {
    error_marker: None,
    oversized_error: "Volume allocation failed",
};

impl ReplyCodec {
    pub(crate) fn read(&self, stream: &mut impl Read) -> io::Result<Result<String, String>> {
        let failed = if let Some(marker) = self.error_marker {
            let mut length = [0; 4];
            stream.read_exact(&mut length)?;
            let length = u32::from_le_bytes(length);
            if length != marker {
                return read_string_body(stream, length as usize, MAX_MESSAGE_BYTES).map(Ok);
            }
            true
        } else {
            let mut status = [0];
            stream.read_exact(&mut status)?;
            status != [1]
        };
        let message = read_string(stream, MAX_MESSAGE_BYTES)?;
        Ok(if failed { Err(message) } else { Ok(message) })
    }

    pub(crate) fn write(
        &self,
        stream: &mut impl Write,
        reply: Result<&str, &str>,
    ) -> io::Result<()> {
        match (self.error_marker, reply.is_ok()) {
            (Some(marker), false) => stream.write_all(&marker.to_le_bytes())?,
            (None, success) => write_ack(stream, success)?,
            _ => {}
        }
        write_string(
            stream,
            reply.unwrap_or_else(|message| {
                if message.len() <= MAX_MESSAGE_BYTES {
                    message
                } else {
                    self.oversized_error
                }
            }),
        )
    }
}

fn read_string_body(stream: &mut impl Read, length: usize, maximum: usize) -> io::Result<String> {
    if length > maximum {
        return Err(io::Error::other("Oversized protocol message"));
    }
    let mut bytes = vec![0; length];
    stream.read_exact(&mut bytes)?;
    String::from_utf8(bytes).map_err(io::Error::other)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn check_reply_codec(
        read: impl Fn(&mut &[u8]) -> io::Result<Result<String, String>>,
        write: impl Fn(&mut Vec<u8>, Result<&str, &str>) -> io::Result<()>,
        success: &[u8],
        failure: &[u8],
        fallback: &str,
    ) {
        for (reply, expected) in [(Ok("/key"), success), (Err("full"), failure)] {
            let mut frame = Vec::new();
            write(&mut frame, reply).unwrap();
            assert_eq!(frame, expected);
            assert_eq!(
                read(&mut &expected[..])
                    .unwrap()
                    .as_deref()
                    .map_err(String::as_str),
                reply
            );
            for end in 0..expected.len() {
                assert_eq!(
                    read(&mut &expected[..end]).unwrap_err().kind(),
                    io::ErrorKind::UnexpectedEof,
                );
            }
            *frame.last_mut().unwrap() = 0xff;
            assert!(read(&mut &frame[..]).is_err());
        }

        // Several replies can share one stream. A recoverable allocation
        // failure must leave the following reply readable.
        let mut frames = Vec::new();
        for frame in [success, failure, success] {
            frames.extend_from_slice(frame);
        }
        let mut incoming = &frames[..];
        for expected in [Ok("/key"), Err("full"), Ok("/key")] {
            assert_eq!(
                read(&mut incoming)
                    .unwrap()
                    .as_deref()
                    .map_err(String::as_str),
                expected
            );
        }
        assert!(incoming.is_empty());

        let mut frame = Vec::new();
        let maximum = "a".repeat(MAX_MESSAGE_BYTES);
        for reply in [Ok(maximum.as_str()), Err(maximum.as_str())] {
            frame.clear();
            write(&mut frame, reply).unwrap();
            assert_eq!(
                read(&mut &frame[..])
                    .unwrap()
                    .as_deref()
                    .map_err(String::as_str),
                reply
            );
        }
        frame.clear();
        // A multibyte error is replaced as a whole rather than truncated in
        // the middle of a UTF-8 character.
        write(&mut frame, Err(&"€".repeat(MAX_MESSAGE_BYTES))).unwrap();
        assert_eq!(read(&mut &frame[..]).unwrap(), Err(fallback.to_owned()));
    }

    #[test]
    fn keeper_reply_wire_format_and_error_boundaries() {
        check_reply_codec(
            |stream| KEEPER_REPLY.read(stream),
            |stream, reply| KEEPER_REPLY.write(stream, reply),
            b"\x04\0\0\0/key",
            b"\xff\xff\xff\xff\x04\0\0\0full",
            "Secret storage allocation failed (oversized helper error)",
        );
        for prefix in [&b""[..], &b"\xff\xff\xff\xff"[..]] {
            let mut frame = prefix.to_vec();
            frame.extend_from_slice(&(MAX_MESSAGE_BYTES as u32 + 1).to_le_bytes());
            let error = KEEPER_REPLY.read(&mut &frame[..]).unwrap_err();
            assert_ne!(error.kind(), io::ErrorKind::UnexpectedEof);
        }
    }

    #[cfg(feature = "shared-volumes")]
    #[test]
    fn scope_reply_wire_format_and_error_boundaries() {
        check_reply_codec(
            |stream| SCOPE_REPLY.read(stream),
            |stream, reply| SCOPE_REPLY.write(stream, reply),
            b"\x01\x04\0\0\0/key",
            b"\x00\x04\0\0\0full",
            "Volume allocation failed",
        );
        for status in [0, 1] {
            let mut frame = vec![status];
            frame.extend_from_slice(&(MAX_MESSAGE_BYTES as u32 + 1).to_le_bytes());
            let error = SCOPE_REPLY.read(&mut &frame[..]).unwrap_err();
            assert_ne!(error.kind(), io::ErrorKind::UnexpectedEof);
        }
        // Preserve the existing rule that any non-success status is failure.
        assert_eq!(
            SCOPE_REPLY.read(&mut &b"\x02\x04\0\0\0full"[..]).unwrap(),
            Err("full".to_owned()),
        );
    }

    #[test]
    fn acknowledgements_preserve_wire_format_and_failure_context() {
        let mut frame = Vec::new();
        write_ack(&mut frame, true).unwrap();
        write_ack(&mut frame, false).unwrap();
        assert_eq!(frame, [1, 0]);
        let mut incoming = &frame[..];
        read_ack(&mut incoming, "release failed").unwrap();
        assert_eq!(
            read_ack(&mut incoming, "release failed")
                .unwrap_err()
                .to_string(),
            "release failed"
        );
        assert_eq!(
            read_ack(&mut incoming, "release failed")
                .unwrap_err()
                .kind(),
            io::ErrorKind::UnexpectedEof
        );
        for status in [2, 255] {
            assert!(read_ack(&mut &[status][..], "release failed").is_err());
        }
    }

    #[test]
    fn allocation_frames_preserve_the_existing_wire_format() {
        let mut frame = Vec::new();
        write_allocation(&mut frame, b"key", ".pem").unwrap();
        assert_eq!(frame, b"\x01\x03\0\0\0\0\0\0\0\x04\0\0\0.pemkey");
        let (bytes, suffix) = read_allocation(&mut &frame[1..]).unwrap();
        assert_eq!(&**bytes, b"key");
        assert_eq!(suffix, ".pem");
        for suffix in [String::new(), "a".repeat(MAX_SUFFIX_BYTES)] {
            frame.clear();
            write_allocation(&mut frame, b"", &suffix).unwrap();
            let (bytes, actual) = read_allocation(&mut &frame[1..]).unwrap();
            assert!(bytes.is_empty());
            assert_eq!(actual, suffix);
        }
    }

    #[test]
    fn invalid_suffixes_are_rejected_before_writing_or_reading_payloads() {
        for suffix in ["/outside".to_owned(), "é".to_owned(), "a".repeat(81)] {
            let mut outgoing = Vec::new();
            assert!(write_allocation(&mut outgoing, b"key", &suffix).is_err());
            assert!(outgoing.is_empty());
            // Bypass the writer to model a malformed peer. No payload follows:
            // the receiver must reject the suffix before trying to read it.
            let mut incoming = 3u64.to_le_bytes().to_vec();
            write_string(&mut incoming, &suffix).unwrap();
            let error = read_allocation(&mut &incoming[..]).unwrap_err();
            assert_ne!(error.kind(), io::ErrorKind::UnexpectedEof);
        }
    }

    #[test]
    fn oversized_frames_are_rejected_before_reading_their_contents() {
        let length = (MAX_SECRET_BYTES + 1).to_le_bytes();
        let error = read_allocation(&mut &length[..]).unwrap_err();
        assert_ne!(error.kind(), io::ErrorKind::UnexpectedEof);
        let length = (MAX_MESSAGE_BYTES as u32 + 1).to_le_bytes();
        let error = read_string(&mut &length[..], MAX_MESSAGE_BYTES).unwrap_err();
        assert_ne!(error.kind(), io::ErrorKind::UnexpectedEof);
        let mut outgoing = Vec::new();
        assert!(write_string(&mut outgoing, &"a".repeat(MAX_MESSAGE_BYTES + 1)).is_err());
        assert!(outgoing.is_empty());
    }

    #[test]
    fn truncated_frames_and_non_utf8_strings_are_rejected() {
        let mut frame = Vec::new();
        write_allocation(&mut frame, b"key", ".pem").unwrap();
        for end in 1..frame.len() {
            let error = read_allocation(&mut &frame[1..end]).unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::UnexpectedEof);
        }
        assert!(read_string(&mut &b"\x01\0\0\0\xff"[..], MAX_MESSAGE_BYTES).is_err());
    }
}
