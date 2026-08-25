//! Development benchmark: `cargo run --profile quick --example bench -- <level> <files...>`
use quantum::{Config, block, filters::Filter};
use std::time::Instant;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let level: u8 = args[0].parse().unwrap();
    let cfg = Config::new(level);
    let force = std::env::var("QT_FILTER").ok().map(|v| match v.as_str() {
        "none" => Filter::None,
        "x86" => Filter::X86,
        other => Filter::Delta(other.parse().unwrap()),
    });
    println!("level {level}");
    println!("{:<16} {:>10} {:>10} {:>7} {:>8} {:>8} {:>9} {:>9}", "file", "raw", "packed", "bpc", "ratio", "filter", "comp MB/s", "dec MB/s");
    let mut total_raw = 0usize;
    let mut total_packed = 0usize;
    for path in &args[1..] {
        let data = std::fs::read(path).unwrap();
        let t0 = Instant::now();
        let packed = block::pack(&data, &cfg, force, block::Effort::Always);
        let ct = t0.elapsed().as_secs_f64();
        let t1 = Instant::now();
        let back = block::unpack(&packed, &cfg).unwrap();
        let dt = t1.elapsed().as_secs_f64();
        assert!(back == data, "ROUND TRIP FAILED for {path}");
        let size = packed.data.len();
        total_raw += data.len();
        total_packed += size;
        let name = std::path::Path::new(path).file_name().unwrap().to_string_lossy();
        println!(
            "{:<16} {:>10} {:>10} {:>7.3} {:>7.2}x {:>8} {:>9.2} {:>9.2}",
            name,
            data.len(),
            size,
            size as f64 * 8.0 / data.len() as f64,
            data.len() as f64 / size as f64,
            packed.filter.name(),
            data.len() as f64 / 1e6 / ct,
            data.len() as f64 / 1e6 / dt,
        );
    }
    println!(
        "{:<16} {:>10} {:>10} {:>7.3} {:>7.2}x",
        "TOTAL", total_raw, total_packed,
        total_packed as f64 * 8.0 / total_raw as f64,
        total_raw as f64 / total_packed as f64
    );
}
