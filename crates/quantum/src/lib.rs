//! # Quantum
//!
//! A context-mixing compressor and the `.quantum` archive format, with no
//! dependencies.
//!
//! Quantum spends CPU to buy size. Three things happen in sequence:
//! content-defined chunks are deduplicated across the whole input, what
//! remains is packed into large solid blocks, and each block is coded one bit
//! at a time by a model that mixes a dozen predictions into a single
//! probability. Nothing about the model is stored -- the decoder rebuilds it
//! from the bits it has already decoded.
//!
//! That buys roughly 20-30% over `xz -9e` on text, source and structured
//! data, at about 1 MB/s per core. Decompression costs the same as
//! compression, because it runs the identical model.
//!
//! # Archives
//!
//! ```no_run
//! use quantum::archive::{self, ExtractOptions, Options};
//! use std::path::PathBuf;
//!
//! let opts = Options { level: 9, ..Default::default() };
//! archive::create(
//!     "backup.quantum".as_ref(),
//!     &[PathBuf::from("data")],
//!     &opts,
//!     &mut |_event| {},
//! )?;
//!
//! let mut archive = archive::open("backup.quantum".as_ref())?;
//! for entry in archive.entries() {
//!     println!("{} ({} bytes)", entry.path, entry.size);
//! }
//! archive.extract("restored".as_ref(), &ExtractOptions::default(), 4, &mut |_| {})?;
//! # Ok::<(), quantum::Error>(())
//! ```
//!
//! # Single buffers
//!
//! [`block::pack`] applies a preprocessing filter, compresses, and falls back
//! to storing the data verbatim if that came out no smaller -- so
//! incompressible input never grows.
//!
//! ```
//! use quantum::{Config, block};
//!
//! let cfg = Config::new(5);
//! let data = b"hello hello hello hello hello".repeat(100);
//! let packed = block::pack(&data, &cfg, None);
//! assert!(packed.data.len() < data.len() / 20);
//! assert_eq!(block::unpack(&packed, &cfg)?, data);
//! # Ok::<(), quantum::Error>(())
//! ```
//!
//! For a self-describing byte stream, [`block::write_raw`] and
//! [`block::read_raw`] add a small header naming the level and filter.
//!
//! The lowest layer, [`compress_block`] and [`decompress_block`], is the codec
//! alone: no framing, no integrity check, and the caller must remember the
//! original length.
//!
//! The on-disk layout is specified in `FORMAT.md`.

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
