//! Human-readable output.

use quantum::archive::{Archive, Kind, Stats};
use std::collections::HashMap;
use std::time::Duration;

pub fn format_bytes(n: u64) -> String {
    const UNITS: [&str; 7] = ["B", "KiB", "MiB", "GiB", "TiB", "PiB", "EiB"];
    if n < 1024 {
        return format!("{n} B");
    }
    let mut value = n as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if value >= 100.0 {
        format!("{value:.0} {}", UNITS[unit])
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

pub fn format_duration(d: Duration) -> String {
    let s = d.as_secs_f64();
    if s < 1.0 {
        format!("{:.0} ms", s * 1000.0)
    } else if s < 60.0 {
        format!("{s:.1} s")
    } else {
        format!("{} m {:.0} s", (s / 60.0) as u64, s % 60.0)
    }
}

fn rate(bytes: u64, d: Duration) -> String {
    let s = d.as_secs_f64();
    if s <= 0.0 {
        return "-".into();
    }
    format!("{}/s", format_bytes((bytes as f64 / s) as u64))
}

/// Point out when the input produced fewer blocks than there are cores, so
/// most of the machine sat idle. Blocks are the unit of parallelism, and
/// their size is the one setting that controls how many there are.
fn parallelism_hint(stats: &Stats, block_size: usize, available: usize) {
    if stats.total_blocks == 0 || available <= 1 {
        return;
    }
    let blocks = stats.total_blocks as usize;
    if blocks * 3 >= available * 2 {
        return; // Already using most of the machine.
    }
    // Halve until there would be roughly one block per core, but never below
    // 2 MiB, where the ratio cost stops being worth it.
    let mut suggested = block_size;
    let mut count = blocks;
    while count * 2 <= available && suggested > 2 * 1024 * 1024 {
        suggested /= 2;
        count *= 2;
    }
    if suggested == block_size {
        return;
    }
    eprintln!();
    eprintln!(
        "  note: {blocks} block(s) over {available} core(s) left most of the machine idle.",
    );
    eprintln!(
        "  `-b {}` would make about {count} and run several times faster, for roughly 1-2% more size.",
        if suggested >= 1024 * 1024 {
            format!("{}M", suggested / (1024 * 1024))
        } else {
            format!("{}K", suggested / 1024)
        }
    );
}

pub fn creation_summary(stats: &Stats, elapsed: Duration, block_size: usize, available: usize) {
    let raw = stats.raw_bytes;
    let archive = stats.archive_bytes;
    eprintln!();
    eprintln!(
        "  input      {:>12}  ({} files, {} dirs, {} links)",
        format_bytes(raw),
        stats.files,
        stats.dirs,
        stats.symlinks
    );
    if stats.deduped_bytes > 0 {
        let unique = raw.saturating_sub(stats.deduped_bytes);
        eprintln!(
            "  deduped    {:>12}  ({:.1}% removed before compression, {} left)",
            format_bytes(stats.deduped_bytes),
            stats.deduped_bytes as f64 * 100.0 / raw.max(1) as f64,
            format_bytes(unique)
        );
    }
    eprintln!(
        "  archive    {:>12}  (index {})",
        format_bytes(archive),
        format_bytes(stats.metadata_bytes)
    );
    if raw > 0 {
        eprintln!(
            "  ratio      {:>12}  ({:.3} bits/byte, saved {:.1}%)",
            format!("{:.2}x", raw as f64 / archive.max(1) as f64),
            archive as f64 * 8.0 / raw as f64,
            100.0 - archive as f64 * 100.0 / raw as f64
        );
    }
    eprintln!(
        "  took       {:>12}  ({}, {} thread(s), {} memory)",
        format_duration(elapsed),
        rate(raw, elapsed),
        stats.threads,
        format_bytes(stats.memory_bytes)
    );
    parallelism_hint(stats, block_size, available);
    if stats.mostly_incompressible() {
        eprintln!();
        eprintln!(
            "  {} of {} blocks were already compressed and were stored as-is.",
            stats.stored_blocks, stats.total_blocks
        );
        eprintln!(
            "  Video, audio, images and existing archives carry their own\n  \
             compression; no general-purpose compressor can shrink them\n  \
             meaningfully. A near-1.00x ratio here is the data, not a fault."
        );
    }
}

pub fn listing(archive: &Archive, verbose: bool) {
    let mut files = 0u64;
    let mut total = 0u64;
    if verbose {
        println!("{:<6} {:>12}  {:<10}  {}", "MODE", "SIZE", "MTIME", "PATH");
    }
    for e in archive.entries() {
        let marker = match e.kind {
            Kind::Dir => "d",
            Kind::Symlink => "l",
            Kind::File => "-",
        };
        if e.kind == Kind::File {
            files += 1;
            total += e.size;
        }
        if verbose {
            println!(
                "{}{:<5o} {:>12}  {:<10}  {}{}",
                marker,
                e.mode,
                format_bytes(e.size),
                e.mtime,
                e.path,
                if e.kind == Kind::Symlink { format!(" -> {}", e.link_target) } else { String::new() }
            );
        } else {
            println!("{}", e.path);
        }
    }
    if verbose {
        println!();
        println!("{files} file(s), {} uncompressed, archive is {} ({:.2}x)",
            format_bytes(total),
            format_bytes(archive.archive_bytes()),
            total as f64 / archive.archive_bytes().max(1) as f64);
    }
}

/// The detailed view: where the bytes actually went.
///
/// This exists to answer "why is my archive this big?" without guesswork --
/// it shows which blocks refused to compress and which files are responsible.
pub fn info_verbose(archive: &Archive) {
    let blocks = archive.block_report();
    println!();
    println!("BLOCKS");
    println!("{:>5} {:>12} {:>12} {:>8} {:>8}  {}", "#", "raw", "stored", "ratio", "filter", "method");
    let mut stored_raw = 0u64;
    for b in &blocks {
        println!(
            "{:>5} {:>12} {:>12} {:>7.2}x {:>8}  {}",
            b.index,
            format_bytes(b.raw_len),
            format_bytes(b.comp_len),
            b.raw_len as f64 / b.comp_len.max(1) as f64,
            b.filter,
            if b.stored { "stored (would not compress)" } else { "context-mixed" }
        );
        if b.stored {
            stored_raw += b.raw_len;
        }
    }
    if stored_raw > 0 {
        let total: u64 = blocks.iter().map(|b| b.raw_len).sum();
        println!();
        println!(
            "  {} of {} ({:.0}%) would not compress and was stored verbatim.",
            format_bytes(stored_raw),
            format_bytes(total),
            stored_raw as f64 * 100.0 / total.max(1) as f64
        );
    }

    // Which file types are costing the space.
    let sizes = archive.size_attribution();
    let mut by_ext: HashMap<String, (u64, u64, u64)> = HashMap::new();
    for (entry, &packed) in archive.entries().iter().zip(&sizes) {
        if entry.kind != Kind::File {
            continue;
        }
        let ext = entry
            .path
            .rsplit_once('.')
            .map(|(_, e)| e.to_ascii_lowercase())
            // A version suffix such as `python3.13` is not a file type.
            .filter(|e| {
                e.len() <= 8
                    && e.chars().all(|c| c.is_ascii_alphanumeric())
                    && !e.chars().all(|c| c.is_ascii_digit())
            })
            .unwrap_or_else(|| "(none)".into());
        let slot = by_ext.entry(ext).or_default();
        slot.0 += entry.size;
        slot.1 += packed;
        slot.2 += 1;
    }
    let mut rows: Vec<_> = by_ext.into_iter().collect();
    rows.sort_by_key(|(_, v)| std::cmp::Reverse(v.1));
    println!();
    println!("BY EXTENSION");
    println!("{:<12} {:>7} {:>12} {:>12} {:>8}", "ext", "files", "raw", "stored", "ratio");
    for (ext, (raw, packed, count)) in rows.iter().take(15) {
        println!(
            "{:<12} {:>7} {:>12} {:>12} {:>7.2}x",
            ext,
            count,
            format_bytes(*raw),
            format_bytes(*packed),
            *raw as f64 / (*packed).max(1) as f64
        );
    }

    // And the individual files, worst first.
    let mut worst: Vec<_> = archive
        .entries()
        .iter()
        .zip(&sizes)
        .filter(|(e, _)| e.kind == Kind::File && e.size > 0)
        .collect();
    worst.sort_by_key(|(_, packed)| std::cmp::Reverse(**packed));
    println!();
    println!("LARGEST IN THE ARCHIVE");
    println!("{:>12} {:>12} {:>8}  {}", "raw", "stored", "ratio", "path");
    for (entry, packed) in worst.iter().take(15) {
        println!(
            "{:>12} {:>12} {:>7.2}x  {}",
            format_bytes(entry.size),
            format_bytes(**packed),
            entry.size as f64 / (**packed).max(1) as f64,
            entry.path
        );
    }
}

pub fn info(archive: &Archive) {
    let total = archive.total_size();
    let stored = archive.archive_bytes();
    let deduped = archive.deduplicated_bytes();
    let files = archive.entries().iter().filter(|e| e.kind == Kind::File).count();
    let dirs = archive.entries().iter().filter(|e| e.kind == Kind::Dir).count();
    let links = archive.entries().iter().filter(|e| e.kind == Kind::Symlink).count();

    println!("format        quantum v{}", archive.version());
    println!("level         {}", archive.level());
    println!("block size    {}", format_bytes(archive.block_size() as u64));
    println!("dedup         {}", if archive.deduplicated() { "on" } else { "off" });
    println!("blocks        {}", archive.block_count());
    println!("chunks        {}", archive.chunk_count());
    println!("entries       {files} file(s), {dirs} dir(s), {links} link(s)");
    println!("uncompressed  {}", format_bytes(total));
    if deduped > 0 {
        println!(
            "deduplicated  {} ({:.1}% of input)",
            format_bytes(deduped),
            deduped as f64 * 100.0 / total.max(1) as f64
        );
    }
    println!("archive       {}", format_bytes(stored));
    if total > 0 {
        println!(
            "ratio         {:.2}x  ({:.3} bits/byte)",
            total as f64 / stored.max(1) as f64,
            stored as f64 * 8.0 / total as f64
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn byte_formatting_is_readable() {
        assert_eq!(format_bytes(0), "0 B");
        assert_eq!(format_bytes(999), "999 B");
        assert_eq!(format_bytes(1024), "1.0 KiB");
        assert_eq!(format_bytes(1536), "1.5 KiB");
        assert_eq!(format_bytes(200 * 1024), "200 KiB");
        assert_eq!(format_bytes(1024 * 1024 * 3), "3.0 MiB");
        assert_eq!(format_bytes(u64::MAX), "16.0 EiB");
    }

    #[test]
    fn duration_formatting_picks_sensible_units() {
        assert_eq!(format_duration(Duration::from_millis(250)), "250 ms");
        assert_eq!(format_duration(Duration::from_secs_f64(3.25)), "3.2 s");
        assert!(format_duration(Duration::from_secs(125)).starts_with("2 m"));
    }
}
