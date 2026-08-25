# The `.quantum` format, version 1

All integers are little-endian. `varint` means LEB128: seven bits per byte,
high bit set to continue. `zigzag` means a signed value mapped to unsigned as
`(n << 1) ^ (n >> 63)` before LEB128 encoding.

## File layout

```
+---------------------------------------------------------------+
| header        16 bytes                                        |
| data blocks   concatenated payloads, in order, no framing     |
| metadata      one compressed block: the index                 |
| footer        40 bytes                                        |
+---------------------------------------------------------------+
```

Data blocks carry no headers of their own. Everything needed to locate and
decode them lives in the index, which is written last so that the writer can
stream blocks out as they are produced without knowing the final layout. A
reader finds the index with one seek to the end.

## Header (16 bytes, at offset 0)

| Offset | Size | Field |
|---|---|---|
| 0 | 4 | Magic, `"QNTM"` |
| 4 | 1 | Format version, currently `1` |
| 5 | 1 | Compression level, 1–9 |
| 6 | 2 | Flags |
| 8 | 4 | Target uncompressed bytes per block |
| 12 | 4 | Reserved, zero |

Flags: bit 0 set means the archive is deduplicated.

The level matters to the decoder, not just as a record: it determines which
models exist and how large their tables are. Both sides derive those from the
level and the block length, so nothing about the model is stored.

## Footer (40 bytes, at the end of the file)

| Offset | Size | Field |
|---|---|---|
| 0 | 8 | Offset of the metadata block |
| 8 | 8 | Compressed length of the metadata block |
| 16 | 8 | Uncompressed length of the metadata |
| 24 | 8 | Checksum of the uncompressed metadata |
| 32 | 1 | Metadata compression method |
| 33 | 1 | Metadata filter |
| 34 | 2 | Reserved, zero |
| 36 | 4 | Magic, `"MTNQ"` |

## Metadata

The metadata is compressed exactly like a data block and holds three tables in
this order.

### Chunk table

```
varint  chunk_count
varint  length             x chunk_count
```

Chunks are numbered from zero in the order they were first stored.

### Block table

```
varint  block_count
  varint  gap from the previous block's end   (almost always 0)
  varint  compressed length
  varint  uncompressed length
  varint  first chunk index, delta from the previous block
  varint  chunk count
  u8      method            0 = stored, 1 = context-mixed
  u8      filter            0 = none, 1 = x86, 0x40|N = delta stride N
  u64     checksum of the uncompressed block
x block_count
```

A chunk always lies entirely within one block. A chunk's offset inside its
block is the sum of the lengths of the chunks before it in that block, so it
does not need to be stored.

### Entry table

```
varint  entry_count
  varint  path length, then that many UTF-8 bytes
  u8      kind              0 = file, 1 = directory, 2 = symlink
  varint  mode              unix permission bits, 0 if unknown
  zigzag  mtime             seconds since the unix epoch
  if file:
    varint  size
    16      truncated SHA-256 of the contents
    varint  chunk reference count
    zigzag  chunk index, delta from the previous reference   x count
  if symlink:
    varint  target length, then that many UTF-8 bytes
x entry_count
```

Paths are relative, `/`-separated, and contain no `.` or `..` component. A
reader must reject anything else before writing to disk.

Chunk references are delta-coded because a file's chunks are usually
consecutive, which makes the common delta `1` and costs one byte per chunk —
about 0.003% of the data at the default chunk size. A deduplicated file points
backwards, so deltas can be negative; that is why they are zigzagged.

## Blocks

A block is the unit of independent compression. Its payload is either the raw
bytes (method 0) or the output of the context-mixing coder (method 1), applied
after the filter named in the block record.

*When* an encoder chooses method 0 is entirely up to it — the decoder only
reads what it was told. This implementation stores a block verbatim if
modelling it came out no smaller, and also if a sample of it says it is
already compressed, but neither rule is part of the format.

Decoding a block is:

1. Read `compressed length` bytes at `offset`.
2. If the method is context-mixed, decode exactly `uncompressed length` bytes.
3. Reverse the filter.
4. Check the checksum.

A reader must reject an uncompressed length that the coder could not have
produced from that many compressed bytes. Probabilities are clamped to
`1..=65535`, so one compressed byte can stand for at most about 45,000
original bytes; the implementation uses 65,536 as a safe bound.

## Filters

Both filters are applied to the whole block before compression and reversed
after decompression.

**x86** (`1`) scans forward for `E8` or `E9`. At each one it reads the next
four bytes as a little-endian displacement and replaces them with
`displacement + position + 5`, then skips to `position + 5`. Reversal
subtracts the same quantity, scanning identically. This is exactly reversible:
the opcode byte itself is never modified, and the four bytes that are modified
are skipped by both directions, so encoder and decoder always agree on where
the instructions are — even when a rewritten displacement happens to contain
`E8`.

**delta** (`0x40 | stride`, stride 1–63) replaces each byte from `stride`
onward with `byte - byte[i - stride]`, working backwards so each subtraction
sees the original predecessor. Reversal adds, working forwards.

## Chunking

Files are split with a Gear rolling hash. The hash is
`h = (h << 1) + GEAR[byte]`, where `GEAR` is 256 fixed 64-bit constants
generated by SplitMix64 from the seed `0x2545F4914F6CDD1D`.

A boundary is declared when `h & MASK == 0`. Two masks are used, as in
FastCDC: a strict one (18 bits set) before the average size is reached, and a
lax one (14 bits set) after it, which tightens the size distribution
considerably. Each mask places its bits at positions
`16 + (i * 32) / bits` for `i` in `0..bits`, giving the hash an effective
window of about 48 bytes.

Defaults are 8 KiB minimum, 32 KiB average, 128 KiB maximum. Chunk boundaries
never cross a file boundary.

Deduplication keys chunks by SHA-256 truncated to 16 bytes. With 2^32 distinct
chunks — around 256 TB at the default size — the chance of any collision is
about 2^-64.

## Integrity

Three layers, all verified by `quantum test`:

- Every data block carries a 64-bit checksum of its uncompressed bytes.
- Every file carries a truncated SHA-256, checked after extraction.
- The metadata block carries its own checksum in the footer.

## Compatibility

Version 1 is the only version. A reader must refuse any other value rather
than guess. Reserved fields are zero and must be ignored, not rejected, so
that a later version can use them without invalidating the layout.
