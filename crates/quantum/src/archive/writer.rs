//! Building a `.quantum` archive.

use std::collections::HashMap;
use std::fs::File;
use std::io::{BufWriter, Read, Seek, Write};
use std::path::{Path, PathBuf};

use super::format::{
    BlockRec, Entry, FLAG_DEDUP, Footer, HEADER_LEN, Header, Kind, Metadata, VERSION,
};
use super::{Event, Options, Stats};
use crate::block::{self, Effort, Packed};
use crate::chunker::{self, ChunkSizes};
use crate::codec::Config;
use crate::error::{Error, Result};
use crate::hash::{ChunkId, Sha256, chunk_id};
use crate::parallel::Pipeline;

/// A path queued for archiving, with the metadata read during the walk.
struct Source {
    disk: PathBuf,
    arc: String,
    kind: Kind,
    mode: u32,
    mtime: i64,
    size: u64,
    link_target: String,
}

/// Bytes read from disk at a time while chunking.
const READ_WINDOW: usize = 4 * 1024 * 1024;

/// Upper bound on a solid block.
const MAX_BLOCK_SIZE: usize = 1 << 30;

pub fn create(
    dest: &Path,
    roots: &[PathBuf],
    opts: &Options,
    listener: &mut dyn FnMut(Event<'_>),
) -> Result<Stats> {
    let mut sources = Vec::new();
    for root in roots {
        walk(root, &mut sources, opts)?;
    }
    if opts.sort {
        sort_for_solidity(&mut sources);
    }

    let cfg = Config::new(opts.level);
    // Positions inside a block are indexed with 32-bit values, and there is no
    // reason to want a block anywhere near this large anyway.
    let block_size = opts.block_size.clamp(64 * 1024, MAX_BLOCK_SIZE);

    // A worker that never receives a block still costs nothing, but promising
    // one is misleading: each holds a model worth hundreds of megabytes at
    // high levels, and a 40 MB input only ever produces a handful of blocks.
    // Deduplication can only shrink the total, so this is an upper bound.
    let total_input: u64 = sources.iter().filter(|s| s.kind == Kind::File).map(|s| s.size).sum();
    let blocks = total_input.div_ceil(block_size as u64).max(1);
    let threads = opts.threads.clamp(1, blocks.min(usize::MAX as u64) as usize);
    let opts = &Options { block_size, threads, ..opts.clone() };
    let file = File::create(dest)?;
    let mut out = BufWriter::new(file);
    let header = Header {
        version: VERSION,
        level: cfg.level,
        flags: if opts.dedup { FLAG_DEDUP } else { 0 },
        block_size: block_size as u32,
    };
    out.write_all(&header.write())?;

    let sizes = opts.chunk_sizes;
    let mut state = BuildState {
        meta: Metadata::default(),
        seen: HashMap::new(),
        block: Vec::with_capacity(opts.block_size + sizes.max),
        block_first_chunk: 0,
        offset: HEADER_LEN as u64,
        stats: Stats::default(),
    };

    let forced_filter = opts.filter;
    let effort = opts.effort;
    let worker_cfg = cfg.clone();
    let mut pipe = Pipeline::new(opts.threads, move |job: Job| {
        (
            block::pack(&job.data, &worker_cfg, forced_filter, effort),
            job.first_chunk,
            job.n_chunks,
        )
    });

    for src in &sources {
        listener(Event::Entry { path: &src.arc, size: src.size });
        let mut entry = Entry {
            path: src.arc.clone(),
            kind: src.kind,
            mode: src.mode,
            mtime: src.mtime,
            size: src.size,
            content_id: [0; 16],
            chunks: Vec::new(),
            link_target: src.link_target.clone(),
        };
        match src.kind {
            Kind::Dir => state.stats.dirs += 1,
            Kind::Symlink => state.stats.symlinks += 1,
            Kind::File => {
                state.stats.files += 1;
                add_file(&src.disk, &mut entry, &mut state, opts, sizes, &mut pipe, &mut out)?;
            }
        }
        state.meta.entries.push(entry);
    }

    // Flush the tail block, then drain the pipeline.
    if !state.block.is_empty() {
        submit_block(&mut state, &mut pipe, &mut out)?;
    }
    for done in pipe.finish()? {
        write_block(&mut state, done, &mut out)?;
    }

    // The index is compressed with the same codec as the data. It is highly
    // repetitive (sorted paths, runs of consecutive chunk ids), so this is
    // usually a 5-10x saving on the archive's fixed overhead.
    let raw_meta = state.meta.encode();
    // The index is always worth modelling: it is small, and it is repetitive
    // enough that the sample check would only waste a decision on it.
    let meta_packed = block::pack(&raw_meta, &Config::new(opts.level), None, Effort::Always);
    let meta_offset = state.offset;
    out.write_all(&meta_packed.data)?;
    let footer = Footer {
        meta_offset,
        meta_comp_len: meta_packed.data.len() as u64,
        meta_raw_len: meta_packed.raw_len as u64,
        meta_checksum: meta_packed.checksum,
        meta_method: meta_packed.method_byte(),
        meta_filter: meta_packed.filter.to_byte(),
    };
    out.write_all(&footer.write())?;
    out.flush()?;

    let mut stats = state.stats;
    stats.archive_bytes = out.get_mut().stream_position()?;
    stats.metadata_bytes = meta_packed.data.len() as u64 + super::format::FOOTER_LEN as u64;
    stats.threads = threads;
    stats.memory_bytes = cfg.memory_for_block(block_size) as u64 * threads as u64;
    Ok(stats)
}

struct BuildState {
    meta: Metadata,
    seen: HashMap<ChunkId, u32>,
    block: Vec<u8>,
    block_first_chunk: u64,
    offset: u64,
    stats: Stats,
}

struct Job {
    data: Vec<u8>,
    first_chunk: u64,
    n_chunks: u64,
}

type Done = (Packed, u64, u64);

/// Read one file, split it into chunks, and append the ones not seen before.
fn add_file(
    disk: &Path,
    entry: &mut Entry,
    state: &mut BuildState,
    opts: &Options,
    sizes: ChunkSizes,
    pipe: &mut Pipeline<Job, Done>,
    out: &mut BufWriter<File>,
) -> Result<()> {
    let mut file = File::open(disk)?;
    let mut hasher = Sha256::new();
    let mut carry: Vec<u8> = Vec::with_capacity(READ_WINDOW + sizes.max);
    let mut total = 0u64;
    let mut eof = false;

    while !eof {
        let start = carry.len();
        carry.resize(start + READ_WINDOW, 0);
        let mut filled = start;
        while filled < carry.len() {
            let n = file.read(&mut carry[filled..])?;
            if n == 0 {
                eof = true;
                break;
            }
            filled += n;
        }
        carry.truncate(filled);
        hasher.update(&carry[start..]);
        total += (filled - start) as u64;

        // A chunk boundary is decided by looking at most `max` bytes ahead, so
        // while that much data is buffered the decision is final.
        let mut pos = 0;
        while carry.len() - pos >= sizes.max || (eof && pos < carry.len()) {
            let len = chunker::next_chunk(&carry[pos..], sizes);
            let chunk = &carry[pos..pos + len];
            let index = intern_chunk(chunk, state, opts, pipe, out)?;
            entry.chunks.push(index);
            pos += len;
        }
        carry.drain(..pos);
    }

    entry.size = total;
    entry.content_id = hasher.finish()[..16].try_into().unwrap();
    state.stats.raw_bytes += total;
    Ok(())
}

/// Return the chunk's index, appending its bytes only if it is new.
fn intern_chunk(
    chunk: &[u8],
    state: &mut BuildState,
    opts: &Options,
    pipe: &mut Pipeline<Job, Done>,
    out: &mut BufWriter<File>,
) -> Result<u32> {
    let index = u32::try_from(state.meta.chunk_lens.len())
        .map_err(|_| Error::Corrupt("too many chunks for one archive"))?;
    if opts.dedup {
        let id = chunk_id(chunk);
        if let Some(&existing) = state.seen.get(&id) {
            state.stats.deduped_bytes += chunk.len() as u64;
            return Ok(existing);
        }
        state.seen.insert(id, index);
    }
    state.meta.chunk_lens.push(chunk.len() as u32);
    state.block.extend_from_slice(chunk);
    if state.block.len() >= opts.block_size {
        submit_block(state, pipe, out)?;
    }
    Ok(index)
}

fn submit_block(
    state: &mut BuildState,
    pipe: &mut Pipeline<Job, Done>,
    out: &mut BufWriter<File>,
) -> Result<()> {
    let n_chunks = state.meta.chunk_lens.len() as u64 - state.block_first_chunk;
    let job = Job {
        data: std::mem::take(&mut state.block),
        first_chunk: state.block_first_chunk,
        n_chunks,
    };
    state.block_first_chunk += n_chunks;
    state.block.reserve(job.data.capacity());
    for done in pipe.submit(job)? {
        write_block(state, done, out)?;
    }
    Ok(())
}

fn write_block(state: &mut BuildState, done: Done, out: &mut BufWriter<File>) -> Result<()> {
    let (packed, first_chunk, n_chunks) = done;
    out.write_all(&packed.data)?;
    state.meta.blocks.push(BlockRec {
        offset: state.offset,
        comp_len: packed.data.len() as u64,
        raw_len: packed.raw_len as u64,
        first_chunk,
        n_chunks,
        method: packed.method_byte(),
        filter: packed.filter.to_byte(),
        checksum: packed.checksum,
    });
    state.offset += packed.data.len() as u64;
    state.stats.stored_bytes += packed.data.len() as u64;
    state.stats.total_blocks += 1;
    if packed.method == crate::block::Method::Stored {
        state.stats.stored_blocks += 1;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Walking the input
// ---------------------------------------------------------------------------

fn walk(root: &Path, out: &mut Vec<Source>, opts: &Options) -> Result<()> {
    let arc = archive_path(root)?;
    let meta = if opts.follow_symlinks {
        std::fs::metadata(root)?
    } else {
        std::fs::symlink_metadata(root)?
    };
    collect(root, arc, &meta, out, opts)
}

fn collect(
    disk: &Path,
    arc: String,
    meta: &std::fs::Metadata,
    out: &mut Vec<Source>,
    opts: &Options,
) -> Result<()> {
    let kind = if meta.is_dir() {
        Kind::Dir
    } else if meta.is_symlink() {
        Kind::Symlink
    } else if meta.is_file() {
        Kind::File
    } else {
        // Sockets, FIFOs and devices have no portable representation here.
        return Ok(());
    };

    let link_target = if kind == Kind::Symlink {
        std::fs::read_link(disk)?.to_string_lossy().into_owned()
    } else {
        String::new()
    };

    out.push(Source {
        disk: disk.to_path_buf(),
        arc: arc.clone(),
        kind,
        mode: mode_of(meta),
        mtime: mtime_of(meta),
        size: if kind == Kind::File { meta.len() } else { 0 },
        link_target,
    });

    if kind == Kind::Dir {
        let mut children: Vec<_> = std::fs::read_dir(disk)?.collect::<std::io::Result<Vec<_>>>()?;
        // Sort so an archive of the same tree is byte-identical run to run.
        children.sort_by_key(|e| e.file_name());
        for child in children {
            let name = child.file_name().to_string_lossy().into_owned();
            let child_meta = if opts.follow_symlinks {
                std::fs::metadata(child.path())?
            } else {
                child.metadata()?
            };
            collect(&child.path(), format!("{arc}/{name}"), &child_meta, out, opts)?;
        }
    }
    Ok(())
}

/// Turn a filesystem path into a safe, relative, `/`-separated archive path.
fn archive_path(path: &Path) -> Result<String> {
    let mut parts: Vec<String> = Vec::new();
    for part in path.components() {
        use std::path::Component::*;
        match part {
            Normal(s) => parts.push(s.to_string_lossy().into_owned()),
            // Anchors and traversal are dropped: an archive path is always
            // relative to wherever it is later extracted.
            RootDir | Prefix(_) | CurDir => {}
            ParentDir => {
                parts.pop();
            }
        }
    }
    if parts.is_empty() {
        return Err(Error::UnsafePath(path.display().to_string()));
    }
    Ok(parts.join("/"))
}

/// Group files that are likely to share statistics, so a solid block sees
/// similar content back to back. This is worth a few percent on mixed trees
/// and costs nothing.
fn sort_for_solidity(sources: &mut [Source]) {
    sources.sort_by(|a, b| {
        let key = |s: &Source| {
            let ext = s
                .arc
                .rsplit_once('.')
                .map(|(_, e)| e.to_ascii_lowercase())
                .unwrap_or_default();
            // Directories first so extraction can create them in one pass.
            let order = match s.kind {
                Kind::Dir => 0,
                Kind::File => 1,
                Kind::Symlink => 2,
            };
            (order, ext)
        };
        key(a).cmp(&key(b)).then_with(|| a.arc.cmp(&b.arc))
    });
}

#[cfg(unix)]
fn mode_of(meta: &std::fs::Metadata) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    meta.permissions().mode() & 0o7777
}

#[cfg(not(unix))]
fn mode_of(meta: &std::fs::Metadata) -> u32 {
    if meta.permissions().readonly() { 0o444 } else { 0o644 }
}

fn mtime_of(meta: &std::fs::Metadata) -> i64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn archive_paths_are_relative_and_clean() {
        assert_eq!(archive_path(Path::new("/usr/share/doc")).unwrap(), "usr/share/doc");
        assert_eq!(archive_path(Path::new("./a/b")).unwrap(), "a/b");
        assert_eq!(archive_path(Path::new("a/../b/c")).unwrap(), "b/c");
        assert!(archive_path(Path::new("/")).is_err());
    }

    #[test]
    fn sorting_groups_by_extension_with_dirs_first() {
        let mk = |arc: &str, kind: Kind| Source {
            disk: PathBuf::from(arc),
            arc: arc.to_string(),
            kind,
            mode: 0,
            mtime: 0,
            size: 0,
            link_target: String::new(),
        };
        let mut v = vec![
            mk("b.txt", Kind::File),
            mk("z.rs", Kind::File),
            mk("dir", Kind::Dir),
            mk("a.txt", Kind::File),
            mk("a.rs", Kind::File),
        ];
        sort_for_solidity(&mut v);
        let order: Vec<&str> = v.iter().map(|s| s.arc.as_str()).collect();
        assert_eq!(order, ["dir", "a.rs", "z.rs", "a.txt", "b.txt"]);
    }
}
