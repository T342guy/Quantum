//! Error type for the library.

use std::fmt;

#[derive(Debug)]
pub enum Error {
    /// The stream is not a Quantum archive, or its header is damaged.
    BadMagic,
    /// The archive was written by a newer, incompatible format version.
    UnsupportedVersion(u8),
    /// Structural damage: a length, offset or checksum does not add up.
    Corrupt(&'static str),
    /// Stored content did not match its recorded hash.
    IntegrityFailure(String),
    /// A path in the archive is unsafe to write (absolute, or escaping the
    /// destination directory).
    UnsafePath(String),
    Io(std::io::Error),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::BadMagic => write!(f, "not a quantum archive"),
            Error::UnsupportedVersion(v) => {
                write!(f, "archive uses format version {v}, which this build does not support")
            }
            Error::Corrupt(what) => write!(f, "archive is corrupt: {what}"),
            Error::IntegrityFailure(what) => write!(f, "integrity check failed for {what}"),
            Error::UnsafePath(p) => write!(f, "refusing to extract unsafe path {p:?}"),
            Error::Io(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Io(e)
    }
}

pub type Result<T> = std::result::Result<T, Error>;
