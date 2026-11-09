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

// Allocation replies reserve one length value as an error marker, so their
// caller reads the first length before deciding which string frame follows.
pub(crate) fn read_string_body(
    stream: &mut impl Read,
    length: usize,
    maximum: usize,
) -> io::Result<String> {
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
