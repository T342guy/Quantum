//! Extraction is the one place an untrusted archive gets to influence the
//! filesystem. These tests build deliberately hostile archives and check that
//! they are refused.

use quantum::archive::format::{FOOTER_LEN, Footer, HEADER_LEN, Metadata};
use quantum::archive::{self, ExtractOptions, Options};
use quantum::{Config, Error, block};
use std::path::{Path, PathBuf};

struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> TempDir {
        let mut base = std::env::temp_dir();
        base.push(format!(
            "quantum-sec-{tag}-{}-{}",
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

/// Build a normal archive, then rewrite its index so that the single file
/// entry claims `evil_path`.
fn forge_archive(tmp: &TempDir, evil_path: &str) -> PathBuf {
    let src = tmp.join("src");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(src.join("harmless.txt"), b"payload".repeat(500)).unwrap();

    let path = tmp.join("forged.quantum");
    let opts = Options { level: 1, ..Default::default() };
    archive::create(&path, &[src], &opts, &mut |_| {}).unwrap();

    let bytes = std::fs::read(&path).unwrap();
    let footer = Footer::parse(&bytes[bytes.len() - FOOTER_LEN..]).unwrap();
    let level = bytes[5];
    let cfg = Config::new(level);

    // Decode the index, rewrite the path, and put it back.
    let meta_bytes = &bytes[footer.meta_offset as usize..][..footer.meta_comp_len as usize];
    let packed = block::Packed {
        method: block::Packed::method_from_byte(footer.meta_method).unwrap(),
        filter: quantum::filters::Filter::from_byte(footer.meta_filter).unwrap(),
        raw_len: footer.meta_raw_len as usize,
        checksum: footer.meta_checksum,
        data: meta_bytes.to_vec(),
    };
    let mut meta = Metadata::decode(&block::unpack(&packed, &cfg).unwrap()).unwrap();
    let victim = meta
        .entries
        .iter_mut()
        .find(|e| e.path.ends_with("harmless.txt"))
        .unwrap();
    victim.path = evil_path.to_string();

    let raw = meta.encode();
    let repacked = block::pack(&raw, &cfg, None, block::Effort::Always);
    let mut out = bytes[..footer.meta_offset as usize].to_vec();
    out.extend_from_slice(&repacked.data);
    out.extend_from_slice(
        &Footer {
            meta_offset: footer.meta_offset,
            meta_comp_len: repacked.data.len() as u64,
            meta_raw_len: repacked.raw_len as u64,
            meta_checksum: repacked.checksum,
            meta_method: repacked.method_byte(),
            meta_filter: repacked.filter.to_byte(),
        }
        .write(),
    );
    std::fs::write(&path, &out).unwrap();
    path
}

#[test]
fn traversal_paths_are_refused() {
    for evil in [
        "../escaped.txt",
        "a/../../escaped.txt",
        "/tmp/escaped.txt",
        "..",
        "a/./b",
        "C:/escaped.txt",
        "back\\slash.txt",
    ] {
        let tmp = TempDir::new("traverse");
        let path = forge_archive(&tmp, evil);
        let dest = tmp.join("dest");

        let mut archive = archive::open(&path).unwrap();
        let result = archive.extract(&dest, &ExtractOptions::default(), 2, &mut |_| {});
        assert!(
            matches!(result, Err(Error::UnsafePath(_))),
            "{evil:?} was not rejected: {result:?}"
        );

        // Nothing may have been written outside the destination -- and since
        // paths are checked before any I/O, nothing inside it either.
        assert!(!tmp.join("escaped.txt").exists(), "{evil:?} escaped to the parent");
        assert!(!Path::new("/tmp/escaped.txt").exists(), "{evil:?} escaped to an absolute path");
        let leaked = std::fs::read_dir(&dest).map(|d| d.count()).unwrap_or(0);
        assert_eq!(leaked, 0, "{evil:?} wrote something before being rejected");
    }
}

#[test]
fn a_symlink_cannot_be_used_to_write_outside_the_destination() {
    // The attack: an archive containing a symlink pointing outside, followed
    // by a file "inside" that symlink. Creating links only after every file is
    // written is what defeats it.
    let tmp = TempDir::new("symlink");
    let src = tmp.join("src");
    let outside = tmp.join("outside");
    std::fs::create_dir_all(src.join("d")).unwrap();
    std::fs::create_dir_all(&outside).unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(&outside, src.join("d/link")).unwrap();
    std::fs::write(src.join("d/real.txt"), b"data".repeat(100)).unwrap();

    let path = tmp.join("s.quantum");
    archive::create(&path, &[src], &Options { level: 1, ..Default::default() }, &mut |_| {})
        .unwrap();
    let dest = tmp.join("dest");
    archive::open(&path)
        .unwrap()
        .extract(&dest, &ExtractOptions::default(), 2, &mut |_| {})
        .unwrap();

    // The link is restored as a link; nothing was written through it.
    assert_eq!(std::fs::read_dir(&outside).unwrap().count(), 0);
}

#[test]
fn existing_symlinks_at_the_destination_are_replaced_not_followed() {
    let tmp = TempDir::new("replace");
    let src = tmp.join("src");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(src.join("a.txt"), b"new contents".repeat(50)).unwrap();

    let path = tmp.join("r.quantum");
    archive::create(&path, &[src.clone()], &Options { level: 1, ..Default::default() }, &mut |_| {})
        .unwrap();

    // Pre-plant a symlink where the file will land, pointing at a file we must
    // not clobber.
    let dest = tmp.join("dest");
    let target = dest.join(src.strip_prefix("/").unwrap_or(&src)).join("a.txt");
    std::fs::create_dir_all(target.parent().unwrap()).unwrap();
    let sensitive = tmp.join("sensitive.txt");
    std::fs::write(&sensitive, b"do not overwrite").unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(&sensitive, &target).unwrap();

    archive::open(&path)
        .unwrap()
        .extract(&dest, &ExtractOptions::default(), 2, &mut |_| {})
        .unwrap();

    assert_eq!(std::fs::read(&sensitive).unwrap(), b"do not overwrite");
    assert!(!std::fs::symlink_metadata(&target).unwrap().is_symlink());
}

#[test]
fn garbage_input_is_rejected_without_panicking() {
    let tmp = TempDir::new("garbage");
    let path = tmp.join("junk.quantum");
    let mut x = 0x9E37_79B9u64;

    for len in [0usize, 1, 16, 56, 57, 1000, 40_000] {
        let junk: Vec<u8> = (0..len)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                (x >> 40) as u8
            })
            .collect();
        std::fs::write(&path, &junk).unwrap();
        assert!(archive::open(&path).is_err(), "{len} bytes of junk was accepted");

        // Now with a valid-looking header, so parsing gets further in.
        let mut framed = vec![0u8; junk.len().max(HEADER_LEN + FOOTER_LEN)];
        framed[..4].copy_from_slice(b"QNTM");
        framed[4] = 1;
        framed[5] = 5;
        let n = junk.len().min(framed.len() - HEADER_LEN);
        framed[HEADER_LEN..HEADER_LEN + n].copy_from_slice(&junk[..n]);
        std::fs::write(&path, &framed).unwrap();
        let opened = archive::open(&path);
        if let Ok(mut a) = opened {
            let _ = a.verify(2, &mut |_| {});
        }
    }
}

#[test]
fn a_block_claiming_absurd_sizes_is_rejected() {
    let tmp = TempDir::new("absurd");
    let path = forge_archive(&tmp, "fine.txt");
    let mut bytes = std::fs::read(&path).unwrap();

    // Point the footer at a metadata block that runs past the end of the file.
    let start = bytes.len() - FOOTER_LEN;
    bytes[start + 8..start + 16].copy_from_slice(&u64::MAX.to_le_bytes());
    std::fs::write(&path, &bytes).unwrap();
    assert!(archive::open(&path).is_err(), "an over-long metadata block was accepted");

    // And at one with an absurd decompressed size.
    let mut bytes = std::fs::read(&forge_archive(&tmp, "fine.txt")).unwrap();
    let start = bytes.len() - FOOTER_LEN;
    bytes[start + 16..start + 24].copy_from_slice(&(1u64 << 60).to_le_bytes());
    std::fs::write(&path, &bytes).unwrap();
    assert!(archive::open(&path).is_err(), "an absurd metadata length was accepted");
}
