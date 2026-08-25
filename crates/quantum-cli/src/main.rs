//! The `quantum` command-line tool.

mod args;
mod bench;
mod report;

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use args::{Args, Command, Parsed};
use quantum::archive::{self, ExtractOptions, Options};
use quantum::filters::Filter;
use quantum::{Config, block};
use report::{format_bytes, format_duration};

fn main() -> ExitCode {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let parsed = match args::parse(argv.into_iter()) {
        Ok(p) => p,
        Err(msg) => {
            eprintln!("quantum: {msg}");
            return ExitCode::FAILURE;
        }
    };
    let args = match parsed {
        Parsed::Message(msg) => {
            println!("{}", msg.trim_end());
            return ExitCode::SUCCESS;
        }
        Parsed::Run(args) => *args,
    };

    match run(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("quantum: {e}");
            ExitCode::FAILURE
        }
    }
}

fn run(args: &Args) -> Result<(), Box<dyn std::error::Error>> {
    match args.command {
        Command::Create => create(args),
        Command::Extract => extract(args),
        Command::List => list(args),
        Command::Info => info(args),
        Command::Test => test(args),
        Command::Compress => compress_stream(args),
        Command::Decompress => decompress_stream(args),
        Command::Bench => bench::run(args),
    }
}

fn parse_filter(spec: &str) -> Result<Option<Filter>, String> {
    match spec {
        "auto" => Ok(None),
        "none" => Ok(Some(Filter::None)),
        "x86" => Ok(Some(Filter::X86)),
        other => match other.split_once(':') {
            Some(("delta", n)) => {
                let n: u8 = n.parse().map_err(|_| format!("bad delta stride in {spec:?}"))?;
                if !(1..=63).contains(&n) {
                    return Err("delta stride must be 1-63".into());
                }
                Ok(Some(Filter::Delta(n)))
            }
            _ => Err(format!("unknown filter {spec:?} (try auto, none, x86 or delta:N)")),
        },
    }
}

fn build_options(args: &Args) -> Result<Options, Box<dyn std::error::Error>> {
    let mut opts = Options {
        dedup: args.dedup,
        sort: args.sort,
        ..Default::default()
    };
    if let Some(l) = args.level {
        opts.level = l;
    }
    if let Some(t) = args.threads {
        opts.threads = t;
    }
    if let Some(b) = args.block_size {
        opts.block_size = b.max(64 * 1024);
    }
    if let Some(f) = &args.filter {
        opts.filter = parse_filter(f)?;
    }
    Ok(opts)
}

fn archive_and_inputs(args: &Args) -> Result<(PathBuf, Vec<PathBuf>), String> {
    let mut it = args.paths.iter();
    let archive = it.next().ok_or("expected an archive path")?.clone();
    Ok((archive, it.cloned().collect()))
}

fn create(args: &Args) -> Result<(), Box<dyn std::error::Error>> {
    let (dest, inputs) = archive_and_inputs(args)?;
    if inputs.is_empty() {
        return Err("expected at least one file or directory to archive".into());
    }
    let mut existing: Vec<PathBuf> = Vec::new();
    for p in &inputs {
        match std::fs::symlink_metadata(p) {
            Ok(_) => existing.push(p.clone()),
            Err(e) if args.keep_going => eprintln!("quantum: skipping {}: {e}", p.display()),
            Err(e) => return Err(format!("{}: {e}", p.display()).into()),
        }
    }
    if existing.is_empty() {
        return Err("nothing to archive".into());
    }
    if dest.exists() && !args.force {
        return Err(format!(
            "{} already exists (use --force to overwrite)",
            dest.display()
        )
        .into());
    }

    let opts = build_options(args)?;
    if !args.quiet {
        eprintln!(
            "creating {} at level {} ({} threads, {} blocks{})",
            dest.display(),
            opts.level,
            opts.threads,
            format_bytes(opts.block_size as u64),
            if opts.dedup { ", deduplicated" } else { "" }
        );
    }

    let started = std::time::Instant::now();
    let verbose = args.verbose && !args.quiet;
    let stats = archive::create(&dest, &existing, &opts, &mut |ev| {
        if verbose {
            let archive::Event::Entry { path, size } = ev;
            eprintln!("  {:>10}  {}", format_bytes(size), path);
        }
    })?;
    let elapsed = started.elapsed();

    if !args.quiet {
        report::creation_summary(&stats, elapsed);
    }
    Ok(())
}

fn extract(args: &Args) -> Result<(), Box<dyn std::error::Error>> {
    let (path, selected) = archive_and_inputs(args)?;
    let mut archive = archive::open(&path)?;
    let dest = args.directory.clone().unwrap_or_else(|| PathBuf::from("."));
    let opts = ExtractOptions {
        select: selected.iter().map(|p| p.to_string_lossy().replace('\\', "/")).collect(),
        overwrite: args.force,
        ..Default::default()
    };
    let threads = args.threads.unwrap_or_else(default_threads);

    let started = std::time::Instant::now();
    let verbose = args.verbose && !args.quiet;
    let stats = archive.extract(&dest, &opts, threads, &mut |ev| {
        if verbose {
            let archive::Event::Entry { path, size } = ev;
            eprintln!("  {:>10}  {}", format_bytes(size), path);
        }
    })?;
    let elapsed = started.elapsed();

    if !args.quiet {
        eprintln!(
            "extracted {} file(s), {} dir(s), {} into {} in {}",
            stats.files,
            stats.dirs,
            format_bytes(stats.raw_bytes),
            dest.display(),
            format_duration(elapsed)
        );
    }
    Ok(())
}

fn list(args: &Args) -> Result<(), Box<dyn std::error::Error>> {
    let (path, _) = archive_and_inputs(args)?;
    let archive = archive::open(&path)?;
    report::listing(&archive, args.verbose);
    Ok(())
}

fn info(args: &Args) -> Result<(), Box<dyn std::error::Error>> {
    let (path, _) = archive_and_inputs(args)?;
    let archive = archive::open(&path)?;
    report::info(&archive);
    Ok(())
}

fn test(args: &Args) -> Result<(), Box<dyn std::error::Error>> {
    let (path, _) = archive_and_inputs(args)?;
    let mut archive = archive::open(&path)?;
    let threads = args.threads.unwrap_or_else(default_threads);
    let started = std::time::Instant::now();
    let verbose = args.verbose && !args.quiet;
    let stats = archive.verify(threads, &mut |ev| {
        if verbose {
            let archive::Event::Entry { path, .. } = ev;
            eprintln!("  {path}");
        }
    })?;
    if !args.quiet {
        println!(
            "ok: {} file(s), {} verified in {}",
            stats.files,
            format_bytes(stats.raw_bytes),
            format_duration(started.elapsed())
        );
    }
    Ok(())
}

fn read_input(args: &Args) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    match args.paths.first() {
        Some(p) if p != Path::new("-") => Ok(std::fs::read(p)?),
        _ => {
            let mut buf = Vec::new();
            std::io::stdin().lock().read_to_end(&mut buf)?;
            Ok(buf)
        }
    }
}

fn write_output(args: &Args, data: &[u8]) -> Result<(), Box<dyn std::error::Error>> {
    match &args.output {
        Some(p) if p != Path::new("-") => {
            if p.exists() && !args.force {
                return Err(format!("{} already exists (use --force)", p.display()).into());
            }
            std::fs::write(p, data)?;
        }
        _ => {
            let mut out = std::io::stdout().lock();
            out.write_all(data)?;
            out.flush()?;
        }
    }
    Ok(())
}

fn compress_stream(args: &Args) -> Result<(), Box<dyn std::error::Error>> {
    let data = read_input(args)?;
    let level = args.level.unwrap_or(quantum::DEFAULT_LEVEL);
    let cfg = Config::new(level);
    let filter = args.filter.as_deref().map(parse_filter).transpose()?.flatten();
    let packed = block::pack(&data, &cfg, filter);
    let framed = block::write_raw(&packed, level);
    if !args.quiet {
        eprintln!(
            "{} -> {} ({:.2}x, {})",
            format_bytes(data.len() as u64),
            format_bytes(framed.len() as u64),
            data.len() as f64 / framed.len().max(1) as f64,
            packed.filter.name()
        );
    }
    write_output(args, &framed)
}

fn decompress_stream(args: &Args) -> Result<(), Box<dyn std::error::Error>> {
    let data = read_input(args)?;
    let (packed, level) = block::read_raw(&data)?;
    let out = block::unpack(&packed, &Config::new(level))?;
    write_output(args, &out)
}

fn default_threads() -> usize {
    std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1)
}
