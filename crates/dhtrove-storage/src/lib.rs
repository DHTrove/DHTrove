//! Versioned, lossless block encodings for BitTorrent metadata.
//!
//! The format is experimental (v1) and intentionally not yet a persistent database.
//! A block can contain multiple torrents, so dictionaries span torrent boundaries.
//! Paths are raw bytes, not assumed to be valid UTF-8.

mod dictionary;
mod format;

pub use format::{decode_block, encode_best, encode_block};

use std::fmt;

/// Upper bounds protect decoders from unreasonable or hostile input.
pub const MAX_BLOCK_BYTES: usize = 64 * 1024 * 1024;
pub const MAX_PATH_BYTES: usize = 64 * 1024;
pub const MAX_RECORDS: usize = 20_000;
pub const MAX_FILES: usize = 100_000;
pub const MAX_DICTIONARY_ENTRIES: usize = 4_096;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FileEntry {
    pub path: Vec<u8>,
    pub length: u64,
}

/// Either infohash version or both for a hybrid torrent.
/// The ordering of files is preserved without sorting.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TorrentRecord {
    pub v1: Option<[u8; 20]>,
    pub v2: Option<[u8; 32]>,
    pub name: Vec<u8>,
    pub files: Vec<FileEntry>,
}

#[repr(u8)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Codec {
    Raw = 0,
    Prefix = 1,
    Dictionary = 2,
    RawZstd = 3,
    PrefixZstd = 4,
    DictionaryZstd = 5,
}

impl Codec {
    pub const ALL: [Self; 6] = [
        Self::Raw,
        Self::Prefix,
        Self::Dictionary,
        Self::RawZstd,
        Self::PrefixZstd,
        Self::DictionaryZstd,
    ];

    pub const fn name(self) -> &'static str {
        match self {
            Self::Raw => "raw",
            Self::Prefix => "prefix",
            Self::Dictionary => "dictionary",
            Self::RawZstd => "raw+zstd",
            Self::PrefixZstd => "prefix+zstd",
            Self::DictionaryZstd => "dictionary+zstd",
        }
    }

    pub(crate) const fn base(self) -> Self {
        match self {
            Self::RawZstd => Self::Raw,
            Self::PrefixZstd => Self::Prefix,
            Self::DictionaryZstd => Self::Dictionary,
            other => other,
        }
    }

    pub(crate) const fn compressed(self) -> bool {
        matches!(
            self,
            Self::RawZstd | Self::PrefixZstd | Self::DictionaryZstd
        )
    }
}

impl TryFrom<u8> for Codec {
    type Error = Error;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(Self::Raw),
            1 => Ok(Self::Prefix),
            2 => Ok(Self::Dictionary),
            3 => Ok(Self::RawZstd),
            4 => Ok(Self::PrefixZstd),
            5 => Ok(Self::DictionaryZstd),
            _ => Err(Error::Invalid("unknown codec")),
        }
    }
}

#[derive(Debug)]
pub enum Error {
    Invalid(&'static str),
    Limit(&'static str),
    Io(std::io::Error),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Invalid(msg) => write!(f, "invalid storage block: {msg}"),
            Self::Limit(msg) => write!(f, "storage limit exceeded: {msg}"),
            Self::Io(err) => write!(f, "compression error: {err}"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(err) => Some(err),
            _ => None,
        }
    }
}

impl From<std::io::Error> for Error {
    fn from(value: std::io::Error) -> Self {
        Self::Io(value)
    }
}
