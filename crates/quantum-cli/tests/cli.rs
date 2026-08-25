//! Drives the actual binary, the way a user would.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const EXE: &str = env!("CARGO_BIN_EXE_quantum");

struct TempDir(PathBuf);

impl TempDir {
    fn new() -> TempDir {
        let mut base = std::env::temp_dir();
        base.push(format!(
            "quantum-cli-{}-{}",
            std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
        ));
        std::fs::create_dir_all(&base).unwrap();
        TempDir(base)
    }
    fn join(&self, p: &str) -> PathBuf {
        self.0.join(p)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn run(args: &[&str]) -> Output {
    Command::new(EXE).args(args).output().expect("failed to run quantum")
}

fn ok(args: &[&str]) -> String {
    let out = run(args);
    assert!(
        out.status.success(),
        "`quantum {}` failed:\n{}",
        args.join(" "),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn sample_tree(tmp: &TempDir) -> PathBuf {
    let src = tmp.join("src");
    std::fs::create_dir_all(src.join("sub")).unwrap();
    let text = "the quick brown fox jumps over the lazy dog\n".repeat(2000);
    std::fs::write(src.join("a.txt"), &text).unwrap();
    std::fs::write(src.join("sub/b.txt"), text.as_bytes()).unwrap(); // duplicate
    std::fs::write(src.join("sub/c.log"), "log line\n".repeat(5000)).unwrap();
    src
}

#[test]
fn create_list_test_extract_cycle() {
    let tmp = TempDir::new();
    let src = sample_tree(&tmp);
    let archive = tmp.join("out.quantum");

    ok(&["create", "-q", archive.to_str().unwrap(), src.to_str().unwrap()]);
    assert!(archive.exists());

    let listing = ok(&["list", archive.to_str().unwrap()]);
    assert!(listing.contains("a.txt"), "listing was:\n{listing}");
    assert!(listing.contains("c.log"));

    let info = ok(&["info", archive.to_str().unwrap()]);
    assert!(info.contains("format        quantum v"), "info was:\n{info}");
    assert!(info.contains("dedup         on"));

    let tested = ok(&["test", archive.to_str().unwrap()]);
    assert!(tested.starts_with("ok:"), "test said:\n{tested}");

    let dest = tmp.join("dest");
    ok(&["extract", "-q", "-f", archive.to_str().unwrap(), "-C", dest.to_str().unwrap()]);

    let restored = find_dir(&dest, "src").expect("extracted tree not found");
    for name in ["a.txt", "sub/b.txt", "sub/c.log"] {
        assert_eq!(
            std::fs::read(restored.join(name)).unwrap(),
            std::fs::read(src.join(name)).unwrap(),
            "{name} differs after extraction"
        );
    }
}

#[test]
fn stream_compression_round_trips_through_files() {
    let tmp = TempDir::new();
    let plain = tmp.join("plain.txt");
    let packed = tmp.join("packed.quantum");
    let back = tmp.join("back.txt");
    let data = "quantum stream mode\n".repeat(5000);
    std::fs::write(&plain, &data).unwrap();

    ok(&["compress", "-q", plain.to_str().unwrap(), "-o", packed.to_str().unwrap()]);
    let packed_len = std::fs::metadata(&packed).unwrap().len();
    assert!(packed_len < data.len() as u64 / 10, "stream barely compressed: {packed_len}");

    ok(&["decompress", packed.to_str().unwrap(), "-o", back.to_str().unwrap()]);
    assert_eq!(std::fs::read(&back).unwrap(), data.as_bytes());
}

#[test]
fn selective_extraction_from_the_command_line() {
    let tmp = TempDir::new();
    let src = sample_tree(&tmp);
    let archive = tmp.join("sel.quantum");
    ok(&["create", "-q", archive.to_str().unwrap(), src.to_str().unwrap()]);

    // Ask for one file by its archive path.
    let listing = ok(&["list", archive.to_str().unwrap()]);
    let wanted = listing
        .lines()
        .find(|l| l.ends_with("c.log"))
        .expect("c.log should be listed")
        .to_string();

    let dest = tmp.join("one");
    ok(&["extract", "-q", "-f", archive.to_str().unwrap(), &wanted, "-C", dest.to_str().unwrap()]);

    let mut found = Vec::new();
    let mut stack = vec![dest.clone()];
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(&d).unwrap() {
            let e = e.unwrap();
            if e.metadata().unwrap().is_dir() {
                stack.push(e.path());
            } else {
                found.push(e.file_name().to_string_lossy().into_owned());
            }
        }
    }
    assert_eq!(found, ["c.log"]);
}

#[test]
fn levels_change_the_result_and_all_round_trip() {
    let tmp = TempDir::new();
    let src = sample_tree(&tmp);
    let mut sizes = Vec::new();
    for level in ["1", "5", "9"] {
        let archive = tmp.join(&format!("l{level}.quantum"));
        ok(&["create", "-q", "-l", level, archive.to_str().unwrap(), src.to_str().unwrap()]);
        ok(&["test", "-q", archive.to_str().unwrap()]);
        sizes.push(std::fs::metadata(&archive).unwrap().len());
    }
    assert!(sizes[2] <= sizes[0], "level 9 should not be worse than level 1: {sizes:?}");
}

#[test]
fn errors_are_reported_cleanly() {
    let tmp = TempDir::new();

    // Missing archive.
    let out = run(&["list", tmp.join("nope.quantum").to_str().unwrap()]);
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.starts_with("quantum: "), "unexpected error text: {err}");

    // Not an archive at all.
    let junk = tmp.join("junk.bin");
    std::fs::write(&junk, b"this is not a quantum archive").unwrap();
    let out = run(&["list", junk.to_str().unwrap()]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("not a quantum archive"));

    // Refusing to clobber without --force.
    let src = sample_tree(&tmp);
    let archive = tmp.join("x.quantum");
    ok(&["create", "-q", archive.to_str().unwrap(), src.to_str().unwrap()]);
    let out = run(&["create", "-q", archive.to_str().unwrap(), src.to_str().unwrap()]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("already exists"));

    // Bad usage.
    for args in [vec!["frobnicate"], vec!["create"], vec!["create", "-l", "99", "a", "b"]] {
        let out = run(&args);
        assert!(!out.status.success(), "{args:?} should fail");
    }
}

#[test]
fn help_and_version_work() {
    let help = ok(&["--help"]);
    assert!(help.contains("USAGE"));
    assert!(help.contains("create"));
    let version = ok(&["--version"]);
    assert!(version.starts_with("quantum "), "version said {version:?}");
}

fn find_dir(root: &Path, name: &str) -> Option<PathBuf> {
    let mut stack = vec![root.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(&d).ok()? {
            let e = e.ok()?;
            if e.metadata().ok()?.is_dir() {
                if e.file_name() == name {
                    return Some(e.path());
                }
                stack.push(e.path());
            }
        }
    }
    None
}
