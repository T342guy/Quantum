//! Human-readable output.

use quantum::archive::{Archive, Kind, Stats};
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

pub fn creation_summary(stats: &Stats, elapsed: Duration) {
    let raw = stats.raw_bytes;
    let archive = stats.archive_bytes;
    eprintln!();
    eprintln!("  input      {:>12}  ({} files, {} dirs, {} links)",
        format_bytes(raw), stats.files, stats.dirs, stats.symlinks);
    if stats.deduped_bytes > 0 {
        let unique = raw.saturating_sub(stats.deduped_bytes);
        eprintln!(
            "  deduped    {:>12}  ({:.1}% removed before compression, {} left)",
            format_bytes(stats.deduped_bytes),
            stats.deduped_bytes as f64 * 100.0 / raw.max(1) as f64,
            format_bytes(unique)
        );
    }
    eprintln!("  archive    {:>12}  (index {})",
        format_bytes(archive), format_bytes(stats.metadata_bytes));
    if raw > 0 {
        eprintln!(
            "  ratio      {:>12}  ({:.3} bits/byte, saved {:.1}%)",
            format!("{:.2}x", raw as f64 / archive.max(1) as f64),
            archive as f64 * 8.0 / raw as f64,
            100.0 - archive as f64 * 100.0 / raw as f64
        );
    }
    eprintln!("  took       {:>12}  ({})", format_duration(elapsed), rate(raw, elapsed));
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

pub fn info(archive: &Archive) {
    let total = archive.total_size();
    let stored = archive.archive_bytes();
    let deduped = archive.deduplicated_bytes();
    let files = archive.entries().iter().filter(|e| e.kind == Kind::File).count();
    let dirs = archive.entries().iter().filter(|e| e.kind == Kind::Dir).count();
    let links = archive.entries().iter().filter(|e| e.kind == Kind::Symlink).count();

    println!("format        quantum v1");
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
