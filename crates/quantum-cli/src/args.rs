//! Argument parsing.
//!
//! Hand-rolled so the tool has no dependencies at all -- the same constraint
//! the library holds itself to.

use std::path::PathBuf;

pub const USAGE: &str = "\
quantum -- compress bulk data into .quantum archives

USAGE
    quantum <command> [options] [arguments]

COMMANDS
    create   <archive> <path>...   Build an archive from files and directories
    extract  <archive> [path]...   Restore an archive (all of it, or just some paths)
    list     <archive>             Show what is inside
    info     <archive>             Show format and sizes (-v: where every byte went)
    test     <archive>             Decode everything and verify every checksum
    compress    [file]             Compress one stream (stdin/stdout by default)
    decompress  [file]             Reverse `compress`
    bench    <file>...             Compare levels, and gzip/bzip2/xz if installed

OPTIONS
    -l, --level <1-9>       Effort. Higher is smaller and slower [default: 5]
    -o, --output <path>     Where to write (create/compress/decompress)
    -C, --directory <dir>   Where to extract [default: .]
    -T, --threads <n>       Worker threads [default: all cores]
    -b, --block-size <size> Bytes per solid block, e.g. 4M, 64M [default: 16M]
        --no-dedup          Skip deduplication (slightly faster, usually bigger)
        --no-sort           Keep input order instead of grouping like files
        --filter <f>        auto | none | x86 | delta:N [default: auto]
        --force-compress    Model every block, even ones that look already
                            compressed (video, images, other archives). Costs
                            minutes per gigabyte and usually gains under 1%
    -k, --keep-going        Report and skip unreadable inputs
    -f, --force             Overwrite existing files
    -v, --verbose           List each entry as it is processed
    -q, --quiet             Print nothing but errors
    -h, --help              Show this message
    -V, --version           Show the version

EXAMPLES
    quantum create backup.quantum ~/projects
    quantum create -l 9 -b 64M archive.quantum ./data
    quantum list backup.quantum
    quantum extract backup.quantum -C /tmp/restore
    quantum extract backup.quantum projects/notes.txt
    tar cf - ./dir | quantum compress -o dir.tar.quantum
";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Command {
    Create,
    Extract,
    List,
    Info,
    Test,
    Compress,
    Decompress,
    Bench,
}

#[derive(Debug)]
pub struct Args {
    pub command: Command,
    pub paths: Vec<PathBuf>,
    pub level: Option<u8>,
    pub output: Option<PathBuf>,
    pub directory: Option<PathBuf>,
    pub threads: Option<usize>,
    pub block_size: Option<usize>,
    pub dedup: bool,
    pub sort: bool,
    pub filter: Option<String>,
    pub force_compress: bool,
    pub keep_going: bool,
    pub force: bool,
    pub verbose: bool,
    pub quiet: bool,
}

impl Default for Args {
    fn default() -> Self {
        Args {
            command: Command::List,
            paths: Vec::new(),
            level: None,
            output: None,
            directory: None,
            threads: None,
            block_size: None,
            dedup: true,
            sort: true,
            filter: None,
            force_compress: false,
            keep_going: false,
            force: false,
            verbose: false,
            quiet: false,
        }
    }
}

/// Either parsed arguments or a message the caller should print and exit with.
pub enum Parsed {
    Run(Box<Args>),
    Message(String),
}

pub fn parse<I: Iterator<Item = String>>(argv: I) -> Result<Parsed, String> {
    let argv: Vec<String> = argv.collect();
    if argv.is_empty() {
        return Ok(Parsed::Message(USAGE.to_string()));
    }

    let mut args = Args::default();
    let mut rest = &argv[..];

    // A leading -h/-V works without a command.
    match argv[0].as_str() {
        "-h" | "--help" | "help" => return Ok(Parsed::Message(USAGE.to_string())),
        "-V" | "--version" => {
            return Ok(Parsed::Message(format!("quantum {}", env!("CARGO_PKG_VERSION"))));
        }
        _ => {}
    }

    args.command = match argv[0].as_str() {
        "create" | "c" | "a" | "add" => Command::Create,
        "extract" | "x" => Command::Extract,
        "list" | "l" | "ls" => Command::List,
        "info" => Command::Info,
        "test" | "verify" | "t" => Command::Test,
        "compress" => Command::Compress,
        "decompress" | "d" => Command::Decompress,
        "bench" => Command::Bench,
        other => {
            return Err(format!("unknown command {other:?}\n\nRun `quantum --help` for usage."));
        }
    };
    rest = &rest[1..];

    let mut i = 0;
    let mut only_positional = false;
    while i < rest.len() {
        let arg = rest[i].as_str();
        if only_positional || !arg.starts_with('-') || arg == "-" {
            args.paths.push(PathBuf::from(arg));
            i += 1;
            continue;
        }
        // A value can be attached (`--level=9`) or separate (`--level 9`).
        let (name, inline) = match arg.split_once('=') {
            Some((n, v)) => (n, Some(v.to_string())),
            None => (arg, None),
        };
        let mut take_value = |what: &str| -> Result<String, String> {
            if let Some(v) = inline.clone() {
                return Ok(v);
            }
            i += 1;
            rest.get(i)
                .cloned()
                .ok_or_else(|| format!("{what} needs a value"))
        };
        match name {
            "--" => only_positional = true,
            "-h" | "--help" => return Ok(Parsed::Message(USAGE.to_string())),
            "-V" | "--version" => {
                return Ok(Parsed::Message(format!("quantum {}", env!("CARGO_PKG_VERSION"))));
            }
            "-l" | "--level" => {
                let v = take_value("--level")?;
                let n: u8 = v.parse().map_err(|_| format!("bad level {v:?}"))?;
                if !(1..=9).contains(&n) {
                    return Err(format!("level must be 1-9, got {n}"));
                }
                args.level = Some(n);
            }
            "-o" | "--output" => args.output = Some(PathBuf::from(take_value("--output")?)),
            "-C" | "--directory" => {
                args.directory = Some(PathBuf::from(take_value("--directory")?))
            }
            "-T" | "--threads" => {
                let v = take_value("--threads")?;
                let n: usize = v.parse().map_err(|_| format!("bad thread count {v:?}"))?;
                args.threads = Some(n.max(1));
            }
            "-b" | "--block-size" => {
                args.block_size = Some(parse_size(&take_value("--block-size")?)?)
            }
            "--filter" => args.filter = Some(take_value("--filter")?),
            "--force-compress" => args.force_compress = true,
            "--no-dedup" => args.dedup = false,
            "--no-sort" => args.sort = false,
            "-k" | "--keep-going" => args.keep_going = true,
            "-f" | "--force" => args.force = true,
            "-v" | "--verbose" => args.verbose = true,
            "-q" | "--quiet" => args.quiet = true,
            other if other.starts_with("--") => return Err(format!("unknown option {other:?}")),
            // Allow bundled short flags such as `-vf`.
            other => {
                let mut ok = true;
                for c in other.chars().skip(1) {
                    match c {
                        'v' => args.verbose = true,
                        'q' => args.quiet = true,
                        'f' => args.force = true,
                        'k' => args.keep_going = true,
                        _ => ok = false,
                    }
                }
                if !ok {
                    return Err(format!("unknown option {other:?}"));
                }
            }
        }
        i += 1;
    }

    Ok(Parsed::Run(Box::new(args)))
}

/// Parse `4096`, `64K`, `16M`, `2G` (case-insensitive, optional `B`).
pub fn parse_size(text: &str) -> Result<usize, String> {
    let t = text.trim();
    let bad = || format!("bad size {text:?}");
    let t = t.strip_suffix(['b', 'B']).unwrap_or(t);
    let (digits, scale) = match t.chars().last().ok_or_else(bad)? {
        'k' | 'K' => (&t[..t.len() - 1], 1024usize),
        'm' | 'M' => (&t[..t.len() - 1], 1024 * 1024),
        'g' | 'G' => (&t[..t.len() - 1], 1024 * 1024 * 1024),
        _ => (t, 1),
    };
    let n: usize = digits.trim().parse().map_err(|_| bad())?;
    n.checked_mul(scale).ok_or_else(bad)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_ok(argv: &[&str]) -> Args {
        match parse(argv.iter().map(|s| s.to_string())).unwrap() {
            Parsed::Run(a) => *a,
            Parsed::Message(m) => panic!("expected args, got message: {m}"),
        }
    }

    #[test]
    fn parses_a_typical_create() {
        let a = parse_ok(&["create", "-l", "9", "out.quantum", "src", "docs"]);
        assert_eq!(a.command, Command::Create);
        assert_eq!(a.level, Some(9));
        assert_eq!(a.paths, [PathBuf::from("out.quantum"), "src".into(), "docs".into()]);
    }

    #[test]
    fn accepts_inline_values_and_bundled_flags() {
        let a = parse_ok(&["create", "--level=7", "--block-size=32M", "-vf", "a.quantum", "x"]);
        assert_eq!(a.level, Some(7));
        assert_eq!(a.block_size, Some(32 * 1024 * 1024));
        assert!(a.verbose && a.force);
    }

    #[test]
    fn force_compress_is_distinct_from_force() {
        let a = parse_ok(&["create", "--force-compress", "-f", "a.quantum", "x"]);
        assert!(a.force_compress, "--force-compress should set its own flag");
        assert!(a.force, "-f should still mean overwrite");
        let b = parse_ok(&["create", "-f", "a.quantum", "x"]);
        assert!(!b.force_compress, "-f alone must not force compression");
    }

    #[test]
    fn double_dash_stops_option_parsing() {
        let a = parse_ok(&["extract", "a.quantum", "--", "-weird-name"]);
        assert_eq!(a.paths, [PathBuf::from("a.quantum"), "-weird-name".into()]);
    }

    #[test]
    fn command_aliases_work() {
        for (alias, want) in [
            ("c", Command::Create),
            ("a", Command::Create),
            ("x", Command::Extract),
            ("l", Command::List),
            ("t", Command::Test),
            ("d", Command::Decompress),
        ] {
            assert_eq!(parse_ok(&[alias, "a.quantum"]).command, want, "alias {alias}");
        }
    }

    #[test]
    fn help_and_version_short_circuit() {
        for argv in [vec!["--help"], vec!["-V"], vec!["create", "--help"], vec![]] {
            match parse(argv.iter().map(|s| s.to_string())).unwrap() {
                Parsed::Message(_) => {}
                Parsed::Run(_) => panic!("{argv:?} should print a message"),
            }
        }
    }

    #[test]
    fn bad_input_is_reported_not_panicked() {
        for argv in [
            vec!["frobnicate", "x"],
            vec!["create", "--level", "12", "a"],
            vec!["create", "--level"],
            vec!["create", "--nonsense"],
            vec!["create", "--block-size", "12Q"],
        ] {
            assert!(
                parse(argv.iter().map(|s| s.to_string())).is_err(),
                "{argv:?} should be an error"
            );
        }
    }

    #[test]
    fn sizes_parse() {
        assert_eq!(parse_size("4096").unwrap(), 4096);
        assert_eq!(parse_size("64K").unwrap(), 65536);
        assert_eq!(parse_size("16M").unwrap(), 16 * 1024 * 1024);
        assert_eq!(parse_size("2G").unwrap(), 2 * 1024 * 1024 * 1024);
        assert_eq!(parse_size("8MB").unwrap(), 8 * 1024 * 1024);
        assert!(parse_size("").is_err());
        assert!(parse_size("abc").is_err());
    }
}
