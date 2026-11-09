//! Typed errors for the public API.
//!
//! Internals report `io::Error`. Failures callers can act on carry a private
//! tag through those layers, and the public boundary recovers its kind.

use std::{error, fmt, io};

/// A specialized `Result` for file lease operations.
pub type Result<T, E = Error> = std::result::Result<T, E>;

/// The category of an [`Error`].
#[non_exhaustive]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ErrorKind {
    /// File leases are not supported on this platform.
    Unsupported,
    /// The owner PID is invalid or its process cannot be watched.
    InvalidOwner,
    /// The suffix is longer than 80 bytes or contains characters other than
    /// ASCII letters, digits, `.`, `_`, or `-`.
    InvalidSuffix,
    /// The payload exceeds the 64 MiB request limit.
    TooLarge,
    /// The lease was handed off and cannot allocate more files.
    HandedOff,
    /// The helper could not be started or speaks an incompatible protocol.
    Helper,
    /// The helper could not allocate storage. On Linux this includes an
    /// insufficient `RLIMIT_MEMLOCK`; on macOS, an exhausted RAM volume.
    Storage,
    /// The file does not belong to this lease or could not be revoked.
    Release,
    /// The new owner could not be watched, so the handoff did not happen.
    Handoff,
    /// Communication with the helper or the operating system failed.
    Io,
}

/// An error returned by file lease operations.
///
/// Converts into `io::Error` for callers that propagate I/O results.
#[derive(Debug)]
pub struct Error {
    kind: ErrorKind,
    error: io::Error,
}

impl Error {
    /// The category of this error.
    pub fn kind(&self) -> ErrorKind {
        self.kind
    }

    /// Classify an internal error, using `fallback` for untagged failures.
    pub(crate) fn from_io(error: io::Error, fallback: ErrorKind) -> Self {
        let kind = error
            .get_ref()
            .and_then(|inner| inner.downcast_ref::<Tag>())
            .map_or(fallback, |tag| tag.kind);
        Self { kind, error }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.error.fmt(formatter)
    }
}

impl error::Error for Error {
    fn source(&self) -> Option<&(dyn error::Error + 'static)> {
        self.error.source()
    }
}

impl From<Error> for io::Error {
    fn from(error: Error) -> Self {
        error.error
    }
}

#[derive(Debug)]
struct Tag {
    kind: ErrorKind,
    message: String,
}

impl fmt::Display for Tag {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl error::Error for Tag {}

/// Create an internal error that keeps its public kind across layers.
pub(crate) fn tagged(kind: ErrorKind, message: impl Into<String>) -> io::Error {
    let io_kind = match kind {
        ErrorKind::Unsupported => io::ErrorKind::Unsupported,
        ErrorKind::InvalidOwner | ErrorKind::InvalidSuffix | ErrorKind::TooLarge => {
            io::ErrorKind::InvalidInput
        }
        _ => io::ErrorKind::Other,
    };
    io::Error::new(
        io_kind,
        Tag {
            kind,
            message: message.into(),
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tags_survive_io_layers_and_untagged_errors_use_the_fallback() {
        let error = Error::from_io(tagged(ErrorKind::TooLarge, "big"), ErrorKind::Io);
        assert_eq!(error.kind(), ErrorKind::TooLarge);
        assert_eq!(error.to_string(), "big");
        let io: io::Error = error.into();
        assert_eq!(io.kind(), io::ErrorKind::InvalidInput);
        let error = Error::from_io(io::Error::other("plain"), ErrorKind::Helper);
        assert_eq!(error.kind(), ErrorKind::Helper);
    }
}
