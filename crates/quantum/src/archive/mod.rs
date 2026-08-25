//! The `.quantum` archive: a content-addressed, solid, parallel container.
//!
//! Three ideas do the work, and they compose:
//!
//! 1. **Deduplicate first.** Content-defined chunking finds repeated data
//!    anywhere in the input -- across files, across directories, at any
//!    alignment -- and stores it once. A compressor's window can never reach
//!    that far.
//! 2. **Then go solid.** What survives is concatenated into large blocks so
//!    the model builds statistics across many files instead of restarting at
//!    every one, which is ZIP's central weakness on trees of small files.
//! 3. **Then model hard.** Each block is coded by the context-mixing codec.
//!
//! Blocks stay independent, so both directions parallelise and a single file
//! can be extracted without decoding the whole archive.

pub mod format;
mod reader;
mod writer;

use std::path::{Path, PathBuf};

pub use format::{Entry, Kind, Metadata};
pub use reader::Archive;

use crate::chunker::{self, ChunkSizes};
use crate::codec::DEFAULT_LEVEL;
use crate::error::Result;
use crate::filters::Filter;

/// Default uncompressed bytes per block.
///
/// Bigger blocks compress slightly better and parallelise slightly worse.
/// 16 MiB keeps every core busy on archives from a few hundred megabytes up,
/// which is the range that matters.
pub const DEFAULT_BLOCK_SIZE: usize = 16 * 1024 * 1024;

/// Settings for building an archive.
#[derive(Clone, Debug)]
pub struct Options {
    pub level: u8,
    pub block_size: usize,
    pub dedup: bool,
    pub threads: usize,
    /// Force a preprocessing filter instead of probing for one.
    pub filter: Option<Filter>,
    pub follow_symlinks: bool,
    /// Group files by extension so solid blocks see similar content.
    pub sort: bool,
    pub chunk_sizes: ChunkSizes,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            level: DEFAULT_LEVEL,
            block_size: DEFAULT_BLOCK_SIZE,
            dedup: true,
            threads: crate::parallel::default_threads(),
            filter: None,
            follow_symlinks: false,
            sort: true,
            chunk_sizes: chunker::DEFAULT_SIZES,
        }
    }
}

/// Settings for extraction.
#[derive(Clone, Debug)]
pub struct ExtractOptions {
    /// Only extract entries at or under one of these archive paths. Empty
    /// means everything.
    pub select: Vec<String>,
    pub overwrite: bool,
    pub restore_mtime: bool,
    pub restore_mode: bool,
}

impl Default for ExtractOptions {
    fn default() -> Self {
        ExtractOptions {
            select: Vec::new(),
            overwrite: true,
            restore_mtime: true,
            restore_mode: true,
        }
    }
}

impl ExtractOptions {
    fn selects(&self, path: &str) -> bool {
        if self.select.is_empty() {
            return true;
        }
        self.select.iter().any(|want| {
            path == want
                || path.starts_with(&format!("{}/", want.trim_end_matches('/')))
        })
    }
}

/// What happened during a create, extract or verify.
#[derive(Clone, Copy, Debug, Default)]
pub struct Stats {
    pub files: u64,
    pub dirs: u64,
    pub symlinks: u64,
    /// Uncompressed bytes of file content.
    pub raw_bytes: u64,
    /// Bytes actually written to (or read from) the data blocks.
    pub stored_bytes: u64,
    /// Bytes that deduplication removed before compression.
    pub deduped_bytes: u64,
    pub archive_bytes: u64,
    pub metadata_bytes: u64,
}

/// Progress notifications.
#[derive(Clone, Copy, Debug)]
pub enum Event<'a> {
    /// A path is about to be processed.
    Entry { path: &'a str, size: u64 },
}

/// Build an archive at `dest` from `roots`.
pub fn create(
    dest: &Path,
    roots: &[PathBuf],
    opts: &Options,
    listener: &mut dyn FnMut(Event<'_>),
) -> Result<Stats> {
    writer::create(dest, roots, opts, listener)
}

/// Open an archive for listing, extraction or verification.
pub fn open(path: &Path) -> Result<Archive> {
    Archive::open(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selection_matches_paths_and_prefixes() {
        let opts = ExtractOptions { select: vec!["src".into(), "a.txt".into()], ..Default::default() };
        assert!(opts.selects("src"));
        assert!(opts.selects("src/main.rs"));
        assert!(opts.selects("src/deep/x"));
        assert!(opts.selects("a.txt"));
        assert!(!opts.selects("srcfoo"), "prefix must respect path boundaries");
        assert!(!opts.selects("a.txt.bak"));
        assert!(!opts.selects("other"));

        let all = ExtractOptions::default();
        assert!(all.selects("anything"));
    }
}
