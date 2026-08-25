//! `quantum bench`: measure this build against whatever else is installed.

use crate::args::Args;
use crate::report::format_bytes;
use quantum::{Config, block};
use std::process::{Command, Stdio};
use std::time::Instant;

/// Reference compressors to compare against, if they are on PATH.
const REFERENCES: &[(&str, &[&str])] = &[
    ("gzip -9", &["gzip", "-9", "-c"]),
    ("bzip2 -9", &["bzip2", "-9", "-c"]),
    ("xz -9e", &["xz", "-9e", "-c"]),
    ("zstd -19", &["zstd", "-19", "-c", "-q"]),
    ("brotli -q11", &["brotli", "-q", "11", "-c"]),
];

pub fn run(args: &Args) -> Result<(), Box<dyn std::error::Error>> {
    if args.paths.is_empty() {
        return Err("bench needs at least one file".into());
    }
    let levels: Vec<u8> = match args.level {
        Some(l) => vec![l],
        None => vec![1, 5, 9],
    };

    for path in &args.paths {
        let data = std::fs::read(path)?;
        if data.is_empty() {
            continue;
        }
        println!("\n{}  ({})", path.display(), format_bytes(data.len() as u64));
        println!("{:<14} {:>12} {:>8} {:>9} {:>11}", "compressor", "size", "bits/B", "ratio", "speed");

        for &level in &levels {
            let cfg = Config::new(level);
            let started = Instant::now();
            let packed = block::pack(&data, &cfg, None);
            let elapsed = started.elapsed().as_secs_f64();
            let size = packed.data.len() + 24;
            row(&format!("quantum -{level}"), data.len(), size, elapsed);
        }

        for (name, argv) in REFERENCES {
            if let Some((size, elapsed)) = run_reference(argv, &data) {
                row(name, data.len(), size, elapsed);
            }
        }
    }
    Ok(())
}

fn row(name: &str, raw: usize, size: usize, elapsed: f64) {
    println!(
        "{:<14} {:>12} {:>8.3} {:>8.2}x {:>9.2} MB/s",
        name,
        format_bytes(size as u64),
        size as f64 * 8.0 / raw as f64,
        raw as f64 / size as f64,
        raw as f64 / 1e6 / elapsed.max(1e-9),
    );
}

/// Run an external compressor over stdin, returning its output size and how
/// long it took. `None` if the tool is not installed.
fn run_reference(argv: &[&str], data: &[u8]) -> Option<(usize, f64)> {
    use std::io::Write;
    let started = Instant::now();
    let mut child = Command::new(argv[0])
        .args(&argv[1..])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let mut stdin = child.stdin.take()?;
    let payload = data.to_vec();
    // Feed on another thread: a large input will fill the pipe buffer long
    // before the child finishes writing its output.
    let feeder = std::thread::spawn(move || {
        let _ = stdin.write_all(&payload);
    });
    let out = child.wait_with_output().ok()?;
    let _ = feeder.join();
    if !out.status.success() {
        return None;
    }
    Some((out.stdout.len(), started.elapsed().as_secs_f64()))
}
