//! Reading a `.quantum` archive.

use std::collections::VecDeque;
use std::fs::File;
use std::io::{BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use super::format::{
    Entry, FOOTER_LEN, Footer, HEADER_LEN, Header, Kind, Metadata, validate_path,
};
use super::{Event, ExtractOptions, Stats};
use crate::block::{self, Packed};
use crate::codec::Config;
use crate::error::{Error, Result};
use crate::filters::Filter;
use crate::hash::Sha256;
use crate::parallel::Pipeline;

pub struct Archive {
    path: PathBuf,
    file: File,
    header: Header,
    cfg: Config,
    meta: Metadata,
    /// Which block holds each chunk, and where inside it.
    chunk_block: Vec<u32>,
    chunk_offset: Vec<u64>,
    archive_bytes: u64,
}

impl Archive {
    pub fn open(path: &Path) -> Result<Archive> {
        let mut file = File::open(path)?;
        let archive_bytes = file.metadata()?.len();
        if archive_bytes < (HEADER_LEN + FOOTER_LEN) as u64 {
            return Err(Error::BadMagic);
        }

        let mut head = [0u8; HEADER_LEN];
        file.read_exact(&mut head)?;
        let header = Header::parse(&head)?;
        let cfg = Config::new(header.level);

        let mut foot = [0u8; FOOTER_LEN];
        file.seek(SeekFrom::End(-(FOOTER_LEN as i64)))?;
        file.read_exact(&mut foot)?;
        let footer = Footer::parse(&foot)?;

        let meta_end = footer
            .meta_offset
            .checked_add(footer.meta_comp_len)
            .ok_or(Error::Corrupt("metadata extends past the end of the file"))?;
        if meta_end > archive_bytes - FOOTER_LEN as u64 {
            return Err(Error::Corrupt("metadata extends past the end of the file"));
        }
        let raw_len = usize::try_from(footer.meta_raw_len)
            .map_err(|_| Error::Corrupt("metadata is implausibly large"))?;

        let mut buf = vec![0u8; footer.meta_comp_len as usize];
        file.seek(SeekFrom::Start(footer.meta_offset))?;
        file.read_exact(&mut buf)?;
        let packed = Packed {
            method: Packed::method_from_byte(footer.meta_method)?,
            filter: Filter::from_byte(footer.meta_filter)?,
            raw_len,
            checksum: footer.meta_checksum,
            data: buf,
        };
        let meta = Metadata::decode(&block::unpack(&packed, &cfg)?)?;

        let (chunk_block, chunk_offset) = index_chunks(&meta, archive_bytes)?;
        Ok(Archive {
            path: path.to_path_buf(),
            file,
            header,
            cfg,
            meta,
            chunk_block,
            chunk_offset,
            archive_bytes,
        })
    }

    pub fn entries(&self) -> &[Entry] {
        &self.meta.entries
    }

    pub fn level(&self) -> u8 {
        self.header.level
    }

    pub fn block_size(&self) -> u32 {
        self.header.block_size
    }

    pub fn deduplicated(&self) -> bool {
        self.header.dedup()
    }

    pub fn block_count(&self) -> usize {
        self.meta.blocks.len()
    }

    pub fn chunk_count(&self) -> usize {
        self.meta.chunk_lens.len()
    }

    pub fn archive_bytes(&self) -> u64 {
        self.archive_bytes
    }

    /// Total size of the archived files, before compression.
    pub fn total_size(&self) -> u64 {
        self.meta.entries.iter().map(|e| e.size).sum()
    }

    /// Bytes that deduplication removed before compression.
    pub fn deduplicated_bytes(&self) -> u64 {
        let unique: u64 = self.meta.chunk_lens.iter().map(|&l| l as u64).sum();
        self.total_size().saturating_sub(unique)
    }

    /// Decode every block and verify all checksums and file hashes, without
    /// writing anything.
    pub fn verify(
        &mut self,
        threads: usize,
        listener: &mut dyn FnMut(Event<'_>),
    ) -> Result<Stats> {
        self.run(threads, None, &ExtractOptions::default(), listener)
    }

    /// Extract into `dest`.
    pub fn extract(
        &mut self,
        dest: &Path,
        opts: &ExtractOptions,
        threads: usize,
        listener: &mut dyn FnMut(Event<'_>),
    ) -> Result<Stats> {
        self.run(threads, Some(dest), opts, listener)
    }

    fn run(
        &mut self,
        threads: usize,
        dest: Option<&Path>,
        opts: &ExtractOptions,
        listener: &mut dyn FnMut(Event<'_>),
    ) -> Result<Stats> {
        // Validate every path before touching the filesystem, so a hostile
        // archive cannot half-extract before being rejected.
        for entry in &self.meta.entries {
            validate_path(&entry.path)?;
        }

        let selected: Vec<usize> = (0..self.meta.entries.len())
            .filter(|&i| opts.selects(&self.meta.entries[i].path))
            .collect();

        if let Some(dest) = dest {
            std::fs::create_dir_all(dest)?;
            for &i in &selected {
                let entry = &self.meta.entries[i];
                if entry.kind == Kind::Dir {
                    std::fs::create_dir_all(dest.join(&entry.path))?;
                }
            }
        }

        let mut source = BlockSource::new(&self.path, &self.meta, self.cfg.clone(), threads)?;
        let mut stats = Stats::default();
        stats.archive_bytes = self.archive_bytes;

        for &i in &selected {
            let entry = &self.meta.entries[i];
            match entry.kind {
                Kind::Dir => stats.dirs += 1,
                Kind::Symlink => stats.symlinks += 1,
                Kind::File => {
                    listener(Event::Entry { path: &entry.path, size: entry.size });
                    let target = dest.map(|d| d.join(&entry.path));
                    write_file(entry, target.as_deref(), &mut source, self, opts)?;
                    stats.files += 1;
                    stats.raw_bytes += entry.size;
                }
            }
        }

        // Symlinks go in last. Creating them earlier would let an archive
        // point a link at an outside directory and then write "through" it.
        if let Some(dest) = dest {
            for &i in &selected {
                let entry = &self.meta.entries[i];
                if entry.kind == Kind::Symlink {
                    listener(Event::Entry { path: &entry.path, size: 0 });
                    create_symlink(entry, &dest.join(&entry.path), opts)?;
                }
            }
            // Directory times are restored after their contents, which would
            // otherwise bump them back to now.
            for &i in selected.iter().rev() {
                let entry = &self.meta.entries[i];
                if entry.kind == Kind::Dir {
                    apply_metadata(&dest.join(&entry.path), entry, opts);
                }
            }
        }
        Ok(stats)
    }
}

/// Work out which block each chunk lives in and its offset there, validating
/// the tables against each other on the way.
fn index_chunks(meta: &Metadata, archive_bytes: u64) -> Result<(Vec<u32>, Vec<u64>)> {
    let n = meta.chunk_lens.len();
    let mut chunk_block = vec![u32::MAX; n];
    let mut chunk_offset = vec![0u64; n];
    for (bi, b) in meta.blocks.iter().enumerate() {
        if b.offset + b.comp_len > archive_bytes {
            return Err(Error::Corrupt("a block extends past the end of the file"));
        }
        let first = usize::try_from(b.first_chunk)
            .map_err(|_| Error::Corrupt("block references an impossible chunk"))?;
        let count = usize::try_from(b.n_chunks)
            .map_err(|_| Error::Corrupt("block references an impossible chunk"))?;
        let end = first
            .checked_add(count)
            .ok_or(Error::Corrupt("block chunk range overflows"))?;
        if end > n {
            return Err(Error::Corrupt("block references chunks that do not exist"));
        }
        let mut offset = 0u64;
        for c in first..end {
            if chunk_block[c] != u32::MAX {
                return Err(Error::Corrupt("a chunk is claimed by two blocks"));
            }
            chunk_block[c] = bi as u32;
            chunk_offset[c] = offset;
            offset += meta.chunk_lens[c] as u64;
        }
        if offset != b.raw_len {
            return Err(Error::Corrupt("block length disagrees with its chunk table"));
        }
    }
    if chunk_block.iter().any(|&b| b == u32::MAX) {
        return Err(Error::Corrupt("some chunks belong to no block"));
    }
    Ok((chunk_block, chunk_offset))
}

fn write_file(
    entry: &Entry,
    target: Option<&Path>,
    source: &mut BlockSource,
    archive: &Archive,
    opts: &ExtractOptions,
) -> Result<()> {
    let mut sink: Option<BufWriter<File>> = match target {
        Some(path) => {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            // Never write through a symlink that is already sitting here.
            if let Ok(existing) = std::fs::symlink_metadata(path) {
                if existing.is_symlink() {
                    std::fs::remove_file(path)?;
                } else if !opts.overwrite {
                    return Err(Error::Io(std::io::Error::new(
                        std::io::ErrorKind::AlreadyExists,
                        format!("{} already exists", path.display()),
                    )));
                }
            }
            Some(BufWriter::new(File::create(path)?))
        }
        None => None,
    };

    let mut hasher = Sha256::new();
    let mut written = 0u64;
    for &c in &entry.chunks {
        let c = c as usize;
        let block = source.get(archive.chunk_block[c] as usize)?;
        let start = archive.chunk_offset[c] as usize;
        let len = archive.meta.chunk_lens[c] as usize;
        let data = block
            .get(start..start + len)
            .ok_or(Error::Corrupt("chunk lies outside its block"))?;
        hasher.update(data);
        written += len as u64;
        if let Some(w) = sink.as_mut() {
            w.write_all(data)?;
        }
    }

    if written != entry.size {
        return Err(Error::IntegrityFailure(entry.path.clone()));
    }
    let digest: [u8; 16] = hasher.finish()[..16].try_into().unwrap();
    if digest != entry.content_id {
        return Err(Error::IntegrityFailure(entry.path.clone()));
    }

    if let (Some(mut w), Some(path)) = (sink, target) {
        w.flush()?;
        drop(w);
        apply_metadata(path, entry, opts);
    }
    Ok(())
}

/// Restore permissions and modification time. Failures here are cosmetic --
/// the file's contents are already correct and verified -- so they do not
/// abort an extraction.
fn apply_metadata(path: &Path, entry: &Entry, opts: &ExtractOptions) {
    if opts.restore_mtime && entry.mtime != 0 {
        if let Ok(file) = File::options().write(entry.kind != Kind::Dir).read(true).open(path) {
            let when = if entry.mtime >= 0 {
                std::time::UNIX_EPOCH.checked_add(std::time::Duration::from_secs(entry.mtime as u64))
            } else {
                std::time::UNIX_EPOCH
                    .checked_sub(std::time::Duration::from_secs(entry.mtime.unsigned_abs()))
            };
            if let Some(when) = when {
                let _ = file.set_times(std::fs::FileTimes::new().set_modified(when));
            }
        }
    }
    #[cfg(unix)]
    if opts.restore_mode && entry.mode != 0 {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(entry.mode));
    }
}

fn create_symlink(entry: &Entry, path: &Path, opts: &ExtractOptions) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    if std::fs::symlink_metadata(path).is_ok() {
        if !opts.overwrite {
            return Ok(());
        }
        let _ = std::fs::remove_file(path);
    }
    #[cfg(unix)]
    std::os::unix::fs::symlink(&entry.link_target, path)?;
    #[cfg(not(unix))]
    {
        // Windows needs a privilege for symlinks; fall back to a copy of the
        // target's name so the extraction still completes.
        std::fs::write(path, entry.link_target.as_bytes())?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Block access
// ---------------------------------------------------------------------------

/// Supplies decompressed blocks, decoding ahead on worker threads and keeping
/// recent blocks around so that deduplicated chunks pointing backwards do not
/// force a re-decode.
struct BlockSource<'a> {
    file: File,
    meta: &'a Metadata,
    cfg: Config,
    pipe: Pipeline<(usize, Packed), (usize, Result<Vec<u8>>)>,
    submitted: usize,
    lookahead: usize,
    cache: VecDeque<(usize, Arc<Vec<u8>>)>,
    capacity: usize,
}

impl<'a> BlockSource<'a> {
    fn new(path: &Path, meta: &'a Metadata, cfg: Config, threads: usize) -> Result<Self> {
        let threads = threads.max(1);
        let worker_cfg = cfg.clone();
        Ok(BlockSource {
            file: File::open(path)?,
            meta,
            cfg,
            pipe: Pipeline::new(threads, move |(i, packed): (usize, Packed)| {
                (i, block::unpack(&packed, &worker_cfg))
            }),
            submitted: 0,
            lookahead: threads,
            // Enough recent blocks that backward chunk references from
            // deduplication are usually served from memory.
            capacity: (threads * 2).max(4),
            cache: VecDeque::new(),
        })
    }

    fn get(&mut self, index: usize) -> Result<Arc<Vec<u8>>> {
        if let Some(pos) = self.cache.iter().position(|(i, _)| *i == index) {
            let hit = self.cache.remove(pos).unwrap();
            let data = Arc::clone(&hit.1);
            self.cache.push_back(hit);
            return Ok(data);
        }
        if index >= self.submitted {
            // Decode forward through the block we need, plus a little more to
            // keep every worker busy.
            let target = (index + self.lookahead + 1).min(self.meta.blocks.len());
            while self.submitted < target {
                let i = self.submitted;
                let packed = self.read_packed(i)?;
                self.submitted += 1;
                for done in self.pipe.submit((i, packed))? {
                    self.insert(done.0, done.1?);
                }
            }
            // Drain until the block we asked for has actually arrived.
            while !self.cache.iter().any(|(i, _)| *i == index) {
                let ready = self.pipe.drain_one()?;
                match ready {
                    Some((i, data)) => self.insert(i, data?),
                    None => break,
                }
            }
            if let Some(pos) = self.cache.iter().position(|(i, _)| *i == index) {
                return Ok(Arc::clone(&self.cache[pos].1));
            }
        }
        // Evicted and behind the read-ahead window: decode it on the spot.
        let packed = self.read_packed(index)?;
        let data = block::unpack(&packed, &self.cfg)?;
        self.insert(index, data);
        Ok(Arc::clone(&self.cache.back().unwrap().1))
    }

    fn insert(&mut self, index: usize, data: Vec<u8>) {
        if self.cache.iter().any(|(i, _)| *i == index) {
            return;
        }
        while self.cache.len() >= self.capacity {
            self.cache.pop_front();
        }
        self.cache.push_back((index, Arc::new(data)));
    }

    fn read_packed(&mut self, index: usize) -> Result<Packed> {
        let rec = self
            .meta
            .blocks
            .get(index)
            .ok_or(Error::Corrupt("reference to a block that does not exist"))?;
        let len = usize::try_from(rec.comp_len)
            .map_err(|_| Error::Corrupt("block is implausibly large"))?;
        let raw_len = usize::try_from(rec.raw_len)
            .map_err(|_| Error::Corrupt("block is implausibly large"))?;
        let mut data = vec![0u8; len];
        self.file.seek(SeekFrom::Start(rec.offset))?;
        self.file.read_exact(&mut data)?;
        Ok(Packed {
            method: Packed::method_from_byte(rec.method)?,
            filter: Filter::from_byte(rec.filter)?,
            raw_len,
            checksum: rec.checksum,
            data,
        })
    }
}
