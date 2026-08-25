//! # Quantum
//!
//! A context-mixing compressor and the `.quantum` archive format.
//!
//! Quantum trades speed for size. Where ZIP stores each file with a 1993-era
//! LZ77 + Huffman coder and never looks across file boundaries, Quantum
//! deduplicates content across the whole input, packs what remains into large
//! solid blocks, and codes them with an adaptive context-mixing model.
//!
//! ```no_run
//! use quantum::{Config, compress_block, decompress_block};
//!
//! let cfg = Config::new(5);
//! let packed = compress_block(b"hello hello hello", &cfg);
//! let original = decompress_block(&packed, 17, &cfg).unwrap();
//! assert_eq!(original, b"hello hello hello");
//! ```

pub mod archive;
pub mod block;
pub mod chunker;
mod codec;
pub mod filters;
pub mod error;
mod hash;
mod parallel;
mod varint;

pub use codec::{
    Config, DEFAULT_LEVEL, MAX_LEVEL, MIN_LEVEL, compress_block, decompress_block,
};
pub use error::{Error, Result};
pub use hash::{Sha256, fast_hash};

/// The canonical file extension for a Quantum archive.
pub const EXTENSION: &str = "quantum";
