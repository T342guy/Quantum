# Quantum

A compressor and archive format for making bulk data as small as practical.

Quantum spends CPU to buy size. Where ZIP stores every file separately with a
1993-era LZ77 coder, and even `tar.xz` looks at the data through a single
sliding window, Quantum does three things in sequence:

1. **Deduplicates** content-defined chunks across the entire input, so
   repeated data — anywhere, at any alignment, in any file — is stored once.
2. **Packs what remains into large solid blocks**, so the model builds
   statistics across many files instead of restarting at each one.
3. **Codes each block with a context-mixing model**: a dozen predictors, each
   looking at a different view of the recent past, combined by a small neural
   network and written out by a binary range coder at almost exactly their
   information content.

The result is typically **20–30% smaller than `xz -9e`** on text, source code
and structured data, and **5% smaller than `tar.xz`** on a mixed directory
tree — at roughly 1 MB/s per core.

There is no dependency of any kind, in either crate. Everything — the range
coder, SHA-256, the chunker, the thread pool, the argument parser — is in the
tree and tested.

## Results

Measured on this machine, single core, against the best settings of each
reference compressor. Lower is better.

### Single streams

| Data | `gzip -9` | `bzip2 -9` | `xz -9e` | **`quantum -5`** | **`quantum -9`** |
|---|---:|---:|---:|---:|---:|
| Man pages, 2.9 MiB | 690 KiB | 509 KiB | 483 KiB | **377 KiB** | **367 KiB** |
| Python source, 2.9 MiB | 628 KiB | 506 KiB | 478 KiB | **383 KiB** | **375 KiB** |
| JSON/XML/config, 1.4 MiB | 188 KiB | 155 KiB | 136 KiB | **97.1 KiB** | **92.9 KiB** |
| ELF executables, 2.9 MiB | 1.5 MiB | 1.4 MiB | 1.2 MiB | **1.0 MiB** | **1011 KiB** |
| Random bytes, 1.9 MiB | +339 B | +8.8 KiB | +160 B | **+0 B** | **+0 B** |

Against `xz -9e`, level 9 is 24% smaller on text, 22% on source, 32% on
structured data and 18% on executables. On incompressible input Quantum stores
the block verbatim, so it is the only one of the four that never grows.

### A directory tree

56.5 MB: the Python 3 standard library, `/usr/share/doc`, and seven large ELF
binaries (four of them different builds of CPython, so there is real
cross-file redundancy to find).

| | Size | vs `tar.xz` | Time |
|---|---:|---:|---:|
| `tar` (no compression) | 57,692,160 | | |
| `tar.gz` (`gzip -9`) | 21,651,878 | +60% | 7 s |
| `tar.bz2` (`bzip2 -9`) | 20,226,398 | +49% | 5 s |
| `tar.xz` (`xz -9e`) | 13,563,608 | — | 34 s |
| **`quantum -1`** | 14,879,504 | +10% | 38 s |
| **`quantum -5`** | 13,641,088 | +0.6% | 64 s |
| **`quantum -9`** | **12,861,262** | **−5.2%** | 132 s |

Deduplication alone removed 50% of `/usr/share/doc` before the compressor ran.

### The honest part

Quantum is **slow**: about 1 MB/s per core at level 5, against 2 MB/s for
`xz -9e` and 20 MB/s for `gzip`. Decompression costs the same as compression —
context mixing runs the identical model on both sides, so there is no fast
path back. It is also **memory hungry**: level 5 wants ~120 MB per worker
thread and level 9 wants ~700 MB (the CLI reduces the thread count on its own
rather than let that turn into an OOM).

That trade is the whole point. Use Quantum where the data is written once and
read rarely and the size is what costs you: backups, archival, artifacts,
anything shipped over a slow or metered link. For everyday interactive use,
`zstd` is a better tool and it is not close.

## Install

```
cargo build --release
./target/release/quantum --help
```

Requires Rust 1.85 or newer (edition 2024).

## Using the CLI

```
quantum create backup.quantum ~/projects       # build an archive
quantum list backup.quantum                    # see what is inside
quantum info backup.quantum                    # format, sizes, where space went
quantum test backup.quantum                    # decode everything, verify every hash
quantum extract backup.quantum -C /tmp/restore # restore it
quantum extract backup.quantum projects/notes.txt   # or just one path
```

Single streams, for piping:

```
tar cf - ./dir | quantum compress -o dir.tar.quantum
quantum decompress dir.tar.quantum | tar xf -
```

Useful options:

| Option | Meaning |
|---|---|
| `-l, --level <1-9>` | Effort. 1 is ~2.5× faster than 5; 9 is ~2× slower and ~5% smaller |
| `-b, --block-size <size>` | Solid block size, default 16M. Larger is smaller but less parallel |
| `-T, --threads <n>` | Workers. Defaults to your core count, trimmed to fit in memory |
| `--no-dedup` | Skip deduplication |
| `--filter <f>` | `auto` (default), `none`, `x86`, `delta:N` |
| `-v, --verbose` | List entries as they are processed |

`quantum bench <files>` compares this build against whichever of gzip, bzip2,
xz, zstd and brotli are installed.

## Using the library

```rust
use quantum::archive::{self, Options, ExtractOptions};
use std::path::PathBuf;

// Build an archive.
let opts = Options { level: 9, ..Default::default() };
archive::create(
    "backup.quantum".as_ref(),
    &[PathBuf::from("data")],
    &opts,
    &mut |_event| {},
)?;

// Read it back.
let mut a = archive::open("backup.quantum".as_ref())?;
for entry in a.entries() {
    println!("{} ({} bytes)", entry.path, entry.size);
}
a.extract("restored".as_ref(), &ExtractOptions::default(), 4, &mut |_| {})?;
# Ok::<(), quantum::Error>(())
```

Or use the codec directly on a buffer:

```rust
use quantum::{Config, block};

let cfg = Config::new(5);
let packed = block::pack(b"data to compress", &cfg, None);
let original = block::unpack(&packed, &cfg)?;
# Ok::<(), quantum::Error>(())
```

## How it works

### The codec

Data is coded one **bit** at a time. Before each bit, every model predicts its
value:

- **Order-0 through order-8 context models.** Each hashes the last *n* bytes,
  looks up a bit-history byte in its own table, and maps that history to a
  probability through a state map it learns as it goes. The histories are
  *non-stationary*: seeing a bit discounts the opposite count, so a context
  that changes behaviour adapts in a few bits rather than being anchored by
  ancient statistics.
- **A word model**, keyed on the word currently being typed, which is what
  makes natural-language text compress as well as it does.
- **Sparse models** that skip bytes, for data with alternating or
  record-shaped structure.
- **A match model**, which indexes the block by a rolling hash and, when the
  current position continues something seen before, predicts the byte that
  followed last time. This is the job an LZ77 matcher does in a dictionary
  compressor, except it produces a probability instead of a token — so a
  *likely* repeat still helps, where an LZ77 match has to be exact.

Each prediction is converted to the logistic domain, where mixing linearly is
the right thing to do. A single-layer network — one weight vector per mixing
context, trained online by gradient descent on coding loss — combines them.
Two adaptive probability maps then correct for residual bias, and a
carry-safe binary range coder writes the bit.

Nothing about the model is ever stored. The decoder rebuilds it from the bits
it has already decoded, which is where the compression comes from: the
dictionary is reconstructed on both sides instead of transmitted.

### Before the codec

Each block is checked for two reversible transforms. The **x86 filter**
rewrites `CALL`/`JMP` displacements as absolute addresses, so that every call
to a given function becomes the same byte string — worth about 8% on
executables. The **delta filter** subtracts a fixed stride, for sampled data
and fixed-width records. Delta is chosen by compressing a sample both ways and
keeping the winner; x86 is chosen by detection, because its payoff is
long-range and a sample too small to probe cheaply cannot see it.

If a block still comes out no smaller, it is stored verbatim. That is why
random data costs exactly zero extra bytes.

### The container

Files are deduplicated with content-defined chunking (a Gear/FastCDC variant,
32 KiB average). Splitting on content rather than at fixed offsets is what
makes deduplication survive insertions: adding a byte to the middle of a file
disturbs one chunk instead of renumbering every later one.

Unique chunks are concatenated into solid blocks and compressed independently,
so both directions parallelise and any single file can be extracted without
decoding the whole archive. Files are sorted by extension first, so a solid
block tends to see similar content back to back.

The index — paths, permissions, times, the chunk table, the block table — is
compressed with the same codec and written at the end, so the writer can
stream blocks out without knowing the final layout.

Every block carries a checksum and every file a truncated SHA-256, both
verified on extraction. `quantum test` checks them without writing anything.

See [FORMAT.md](FORMAT.md) for the byte layout.

### Determinism

The same input always produces a byte-identical archive, whatever the thread
count. Directory listings are sorted, chunk boundaries are content-derived,
and every table is sized from values both sides can compute.

## Safety

Extraction is where an untrusted archive gets to touch your filesystem, so:

- Every path is validated before any I/O — no absolute paths, no `..`, no
  drive letters, no backslashes, no NUL. One bad path aborts the whole
  extraction before a single byte is written.
- Symlinks are created only after every file, so an archive cannot point a
  link outside the destination and then write through it.
- An existing symlink at a destination path is removed, never followed.
- Declared block sizes are checked against what the coder could actually have
  produced, so a forged length cannot force a huge allocation.
- Every file is verified against its hash after extraction.

There are tests for each of these that build deliberately hostile archives.

## Tests

```
cargo test                # ~90 tests
cargo test --release      # slower, worth it before trusting a change
```

The suite covers the coder at extreme probabilities, model components in
isolation, chunker resynchronisation after insertions, metadata round-trips
under every truncation and single-bit corruption, whole-tree round-trips at
every level, thread-count invariance, deduplication, selective extraction,
damage detection, the hostile archives above, and the CLI itself.

## Name

There is a historical "Quantum" compressor (Cinemaware, later Microsoft CAB).
This is unrelated to it, and to quantum computing.

## License

MIT.
