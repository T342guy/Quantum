//! End-to-end tests over the whole stack: chunking, dedup, blocks, model,
//! container, extraction.

use quantum::archive::{self, ExtractOptions, Kind, Options};
use quantum::{Config, block};
use std::path::{Path, PathBuf};

/// Deterministic pseudo-random bytes, so failures are reproducible.
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Rng(seed | 1)
    }
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn bytes(&mut self, n: usize) -> Vec<u8> {
        (0..n).map(|_| (self.next() >> 33) as u8).collect()
    }
    /// Text-like data: compressible, but not trivially so.
    fn prose(&mut self, n: usize) -> Vec<u8> {
        const WORDS: &[&str] = &[
            "quantum", "compression", "context", "mixing", "archive", "the", "of", "a", "and",
            "model", "block", "chunk", "entropy", "predict", "bit", "range", "coder",
        ];
        let mut out = Vec::with_capacity(n + 16);
        while out.len() < n {
            out.extend_from_slice(WORDS[(self.next() % WORDS.len() as u64) as usize].as_bytes());
            out.push(if self.next() % 12 == 0 { b'\n' } else { b' ' });
        }
        out.truncate(n);
        out
    }
}

/// A scratch directory that cleans itself up.
struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> TempDir {
        let mut base = std::env::temp_dir();
        let unique = format!(
            "quantum-test-{tag}-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
        );
        base.push(unique);
        std::fs::create_dir_all(&base).unwrap();
        TempDir(base)
    }
    fn path(&self) -> &Path {
        &self.0
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

fn write(path: &Path, data: &[u8]) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, data).unwrap();
}

fn silent() -> impl FnMut(archive::Event<'_>) {
    |_| {}
}

/// Compare two trees byte for byte, including which paths exist.
fn assert_trees_match(a: &Path, b: &Path) {
    let list = |root: &Path| {
        let mut out: Vec<(String, Vec<u8>, bool)> = Vec::new();
        let mut stack = vec![root.to_path_buf()];
        while let Some(dir) = stack.pop() {
            for entry in std::fs::read_dir(&dir).unwrap() {
                let entry = entry.unwrap();
                let path = entry.path();
                let rel = path.strip_prefix(root).unwrap().to_string_lossy().into_owned();
                let meta = std::fs::symlink_metadata(&path).unwrap();
                if meta.is_dir() {
                    out.push((rel, Vec::new(), true));
                    stack.push(path);
                } else if meta.is_symlink() {
                    let target = std::fs::read_link(&path).unwrap();
                    out.push((rel, target.to_string_lossy().as_bytes().to_vec(), false));
                } else {
                    out.push((rel, std::fs::read(&path).unwrap(), false));
                }
            }
        }
        out.sort();
        out
    };
    let (left, right) = (list(a), list(b));
    assert_eq!(left.len(), right.len(), "different number of entries");
    for (l, r) in left.iter().zip(right.iter()) {
        assert_eq!(l.0, r.0, "path mismatch");
        assert_eq!(l.2, r.2, "type mismatch for {}", l.0);
        assert_eq!(l.1, r.1, "content mismatch for {}", l.0);
    }
}

/// Build a tree exercising every content type the codec cares about.
fn build_tree(root: &Path, rng: &mut Rng) {
    write(&root.join("prose/a.txt"), &rng.prose(300_000));
    write(&root.join("prose/b.txt"), &rng.prose(120_000));
    write(&root.join("random.bin"), &rng.bytes(200_000));
    write(&root.join("zeros.bin"), &vec![0u8; 500_000]);
    write(&root.join("empty.txt"), b"");
    write(&root.join("tiny.txt"), b"x");
    // A file whose content repeats, to exercise the match model.
    let unit = rng.prose(4096);
    write(&root.join("repeat.txt"), &unit.repeat(60));
    // Exact duplicates in different directories, to exercise dedup.
    let dup = rng.prose(150_000);
    write(&root.join("dup/one.txt"), &dup);
    write(&root.join("dup/nested/two.txt"), &dup);
    // Something x86-shaped, to exercise the filter.
    let mut code = rng.bytes(150_000);
    for i in (0..code.len() - 8).step_by(11) {
        code[i] = 0xE8;
        code[i + 4] = 0x00;
    }
    write(&root.join("code.bin"), &code);
    std::fs::create_dir_all(root.join("empty_dir")).unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink("prose/a.txt", root.join("link")).unwrap();
}

#[test]
fn tree_round_trips_at_every_level() {
    let tmp = TempDir::new("levels");
    let src = tmp.join("src");
    let mut rng = Rng::new(0xC0FFEE);
    build_tree(&src, &mut rng);

    for level in [1u8, 5, 9] {
        let archive_path = tmp.join(&format!("l{level}.quantum"));
        let opts = Options { level, block_size: 1 << 20, ..Default::default() };
        let stats =
            archive::create(&archive_path, &[src.clone()], &opts, &mut silent()).unwrap();
        assert!(stats.files >= 9, "level {level}: only saw {} files", stats.files);
        assert!(stats.deduped_bytes > 100_000, "level {level}: dedup did not fire");

        let dest = tmp.join(&format!("out{level}"));
        let mut a = archive::open(&archive_path).unwrap();
        a.extract(&dest, &ExtractOptions::default(), 4, &mut silent()).unwrap();

        let extracted = dest.join(src.strip_prefix("/").unwrap_or(&src));
        assert_trees_match(&src, &extracted);
    }
}

#[test]
fn higher_levels_do_not_produce_bigger_archives() {
    let tmp = TempDir::new("monotone");
    let src = tmp.join("src");
    let mut rng = Rng::new(7);
    write(&src.join("text.txt"), &rng.prose(400_000));

    let mut sizes = Vec::new();
    for level in [1u8, 3, 5, 7, 9] {
        let path = tmp.join(&format!("{level}.quantum"));
        let opts = Options { level, ..Default::default() };
        archive::create(&path, &[src.clone()], &opts, &mut silent()).unwrap();
        sizes.push(std::fs::metadata(&path).unwrap().len());
    }
    for pair in sizes.windows(2) {
        // Allow a hair of slack: more models can occasionally cost a few bytes
        // on small inputs before they pay off.
        assert!(
            pair[1] <= pair[0] + pair[0] / 100,
            "a higher level got notably worse: {sizes:?}"
        );
    }
    assert!(sizes[4] < sizes[0], "level 9 should beat level 1: {sizes:?}");
}

#[test]
fn thread_count_does_not_change_the_output() {
    let tmp = TempDir::new("threads");
    let src = tmp.join("src");
    let mut rng = Rng::new(42);
    build_tree(&src, &mut rng);

    let mut digests = Vec::new();
    for threads in [1usize, 2, 8] {
        let path = tmp.join(&format!("t{threads}.quantum"));
        let opts = Options { threads, block_size: 1 << 19, ..Default::default() };
        archive::create(&path, &[src.clone()], &opts, &mut silent()).unwrap();
        digests.push(std::fs::read(&path).unwrap());
    }
    assert_eq!(digests[0], digests[1], "1 and 2 threads disagree");
    assert_eq!(digests[0], digests[2], "1 and 8 threads disagree");
}

#[test]
fn deduplication_actually_saves_space() {
    let tmp = TempDir::new("dedup");
    let src = tmp.join("src");
    let mut rng = Rng::new(99);
    // Twelve copies of the same large file: with dedup the archive should be
    // barely larger than one copy.
    let payload = rng.prose(400_000);
    for i in 0..12 {
        write(&src.join(&format!("copy{i}.txt")), &payload);
    }

    let with = tmp.join("with.quantum");
    let without = tmp.join("without.quantum");
    archive::create(&with, &[src.clone()], &Options { block_size: 1 << 19, ..Default::default() }, &mut silent()).unwrap();
    archive::create(
        &without,
        &[src.clone()],
        &Options { dedup: false, block_size: 1 << 19, ..Default::default() },
        &mut silent(),
    )
    .unwrap();

    let with_len = std::fs::metadata(&with).unwrap().len();
    let without_len = std::fs::metadata(&without).unwrap().len();
    assert!(with_len * 3 < without_len, "dedup saved too little: {with_len} vs {without_len}");

    // Both must still extract correctly.
    for path in [&with, &without] {
        let dest = tmp.join(&format!("out{}", path.file_name().unwrap().to_string_lossy()));
        let mut a = archive::open(path).unwrap();
        a.extract(&dest, &ExtractOptions::default(), 2, &mut silent()).unwrap();
        let extracted = dest.join(src.strip_prefix("/").unwrap_or(&src));
        assert_trees_match(&src, &extracted);
    }
}

#[test]
fn a_file_spanning_many_blocks_round_trips() {
    let tmp = TempDir::new("bigfile");
    let src = tmp.join("src");
    let mut rng = Rng::new(1234);
    // Larger than several blocks, with a mix of compressible and not.
    let mut data = rng.prose(3_000_000);
    data.extend_from_slice(&rng.bytes(500_000));
    data.extend_from_slice(&vec![7u8; 400_000]);
    write(&src.join("big.dat"), &data);

    let path = tmp.join("big.quantum");
    let opts = Options { level: 2, block_size: 512 * 1024, ..Default::default() };
    archive::create(&path, &[src.clone()], &opts, &mut silent()).unwrap();

    let mut a = archive::open(&path).unwrap();
    assert!(a.block_count() > 4, "expected several blocks, got {}", a.block_count());
    let dest = tmp.join("out");
    a.extract(&dest, &ExtractOptions::default(), 4, &mut silent()).unwrap();
    let extracted = dest.join(src.strip_prefix("/").unwrap_or(&src)).join("big.dat");
    assert_eq!(std::fs::read(extracted).unwrap(), data);
}

#[test]
fn selective_extraction_takes_only_what_was_asked_for() {
    let tmp = TempDir::new("select");
    let src = tmp.join("src");
    let mut rng = Rng::new(5);
    write(&src.join("keep/a.txt"), &rng.prose(50_000));
    write(&src.join("keep/deep/b.txt"), &rng.prose(20_000));
    write(&src.join("skip/c.txt"), &rng.prose(50_000));

    let path = tmp.join("s.quantum");
    archive::create(&path, &[src.clone()], &Options::default(), &mut silent()).unwrap();
    let mut a = archive::open(&path).unwrap();

    let prefix = a
        .entries()
        .iter()
        .find(|e| e.path.ends_with("/keep"))
        .expect("keep directory should be present")
        .path
        .clone();
    let dest = tmp.join("out");
    let opts = ExtractOptions { select: vec![prefix], ..Default::default() };
    let stats = a.extract(&dest, &opts, 2, &mut silent()).unwrap();
    assert_eq!(stats.files, 2, "should have extracted exactly the two kept files");

    let mut found: Vec<String> = Vec::new();
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
    found.sort();
    assert_eq!(found, ["a.txt", "b.txt"]);
}

#[test]
fn verify_detects_damaged_archives() {
    let tmp = TempDir::new("damage");
    let src = tmp.join("src");
    let mut rng = Rng::new(31337);
    write(&src.join("a.txt"), &rng.prose(200_000));
    write(&src.join("b.bin"), &rng.bytes(60_000));

    let path = tmp.join("d.quantum");
    archive::create(&path, &[src.clone()], &Options::default(), &mut silent()).unwrap();
    let good = std::fs::read(&path).unwrap();
    archive::open(&path).unwrap().verify(2, &mut silent()).unwrap();

    // Flip bits across the whole file; every corruption must be reported,
    // never silently accepted and never a panic.
    let probes: Vec<usize> = (0..40).map(|i| i * good.len() / 40).collect();
    let mut caught = 0;
    for &at in &probes {
        let mut damaged = good.clone();
        damaged[at] ^= 0x40;
        if damaged == good {
            continue;
        }
        std::fs::write(&path, &damaged).unwrap();
        let failed = match archive::open(&path) {
            Err(_) => true,
            Ok(mut a) => a.verify(2, &mut silent()).is_err(),
        };
        assert!(failed, "corruption at byte {at} went undetected");
        caught += 1;
    }
    assert!(caught > 30, "not enough probes ran");

    // Truncation must be rejected too.
    for cut in [1usize, 16, good.len() / 2, good.len() - 1] {
        std::fs::write(&path, &good[..cut]).unwrap();
        let failed = match archive::open(&path) {
            Err(_) => true,
            Ok(mut a) => a.verify(2, &mut silent()).is_err(),
        };
        assert!(failed, "truncation to {cut} bytes went undetected");
    }
}

#[test]
fn empty_and_degenerate_inputs() {
    let tmp = TempDir::new("empty");
    let src = tmp.join("src");
    std::fs::create_dir_all(&src).unwrap();

    // A directory with nothing in it.
    let path = tmp.join("e.quantum");
    archive::create(&path, &[src.clone()], &Options::default(), &mut silent()).unwrap();
    let mut a = archive::open(&path).unwrap();
    assert_eq!(a.entries().len(), 1);
    assert_eq!(a.entries()[0].kind, Kind::Dir);
    a.verify(2, &mut silent()).unwrap();
    a.extract(&tmp.join("out1"), &ExtractOptions::default(), 2, &mut silent()).unwrap();

    // Only empty files.
    write(&src.join("a"), b"");
    write(&src.join("b"), b"");
    let path2 = tmp.join("e2.quantum");
    archive::create(&path2, &[src.clone()], &Options::default(), &mut silent()).unwrap();
    let mut a = archive::open(&path2).unwrap();
    a.verify(2, &mut silent()).unwrap();
    let dest = tmp.join("out2");
    a.extract(&dest, &ExtractOptions::default(), 2, &mut silent()).unwrap();
    assert_trees_match(&src, &dest.join(src.strip_prefix("/").unwrap_or(&src)));
}

#[test]
fn raw_stream_matches_the_archive_codec() {
    let mut rng = Rng::new(2024);
    for len in [0usize, 1, 100, 70_000] {
        let data = rng.prose(len);
        for level in [1u8, 5, 9] {
            let cfg = Config::new(level);
            let packed = block::pack(&data, &cfg, None, block::Effort::Adaptive);
            let framed = block::write_raw(&packed, level);
            let (back, got_level) = block::read_raw(&framed).unwrap();
            assert_eq!(got_level, level);
            assert_eq!(block::unpack(&back, &Config::new(got_level)).unwrap(), data);
        }
    }
}

#[test]
fn accounting_adds_up() {
    // The diagnostic has to be trustworthy: what it charges to files must
    // match what the blocks actually cost.
    let tmp = TempDir::new("accounting");
    let src = tmp.join("src");
    let mut rng = Rng::new(4242);
    build_tree(&src, &mut rng);

    let path = tmp.join("a.quantum");
    let opts = Options { level: 2, block_size: 1 << 19, ..Default::default() };
    archive::create(&path, &[src.clone()], &opts, &mut silent()).unwrap();
    let a = archive::open(&path).unwrap();

    let blocks = a.block_report();
    assert_eq!(blocks.len(), a.block_count());
    let block_raw: u64 = blocks.iter().map(|b| b.raw_len).sum();
    let block_comp: u64 = blocks.iter().map(|b| b.comp_len).sum();
    assert!(block_comp < a.archive_bytes());

    // Every chunk is charged exactly once, so the attribution should sum to
    // the compressed size of the data blocks.
    let charged: u64 = a.size_attribution().iter().sum();
    let slack = block_comp / 50 + 1024;
    assert!(
        charged.abs_diff(block_comp) <= slack,
        "attribution {charged} does not match block total {block_comp}"
    );

    // And the raw side must account for the deduplicated total, not the
    // input total: shared chunks are stored once.
    let unique: u64 = a.total_size() - a.deduplicated_bytes();
    assert_eq!(block_raw, unique, "blocks should hold exactly the unique bytes");
}

#[test]
fn archives_are_reproducible() {
    let tmp = TempDir::new("repro");
    let src = tmp.join("src");
    let mut rng = Rng::new(808);
    build_tree(&src, &mut rng);

    let a = tmp.join("a.quantum");
    let b = tmp.join("b.quantum");
    let opts = Options { block_size: 1 << 19, ..Default::default() };
    archive::create(&a, &[src.clone()], &opts, &mut silent()).unwrap();
    archive::create(&b, &[src.clone()], &opts, &mut silent()).unwrap();
    assert_eq!(
        std::fs::read(&a).unwrap(),
        std::fs::read(&b).unwrap(),
        "the same input should always produce the same archive"
    );
}
