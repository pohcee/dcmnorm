# JPEG 2000 codec evaluation (October 2026)

Why dcmnorm decodes and encodes **HTJ2K with OpenHTJ2K**, keeps **OpenJPEG** for classic JPEG
2000 (now multi-threaded), and keeps **Kakadu** only as an optional override for classic JPEG
2000. This records the evaluation behind that choice, so it can be repeated when the candidates
change.

## Background

A production mammography study contained a CAD secondary capture (1250x1500, 8-bit) stored as
High-Throughput JPEG 2000 (`1.2.840.10008.1.2.4.201`) that dcmnorm could not decode. The
codestream is valid HTJ2K with five quality layers, which HTJ2K implements with *placeholder
passes* (more than three coding passes per HT code-block). OpenJPEG 2.5 only implements a single
HT set per code-block and rejects it. The study's full-field mammograms (4096x3328, 12-bit, also
HTJ2K) decoded fine because they are single-layer. Any device that writes multi-layer HTJ2K hits
the same wall.

At the time, `--list-transfer-syntaxes` reported HTJ2K decode as supported because an OpenJPEG
adapter was registered for it. That was only partly true.

## Candidates

| Decoder | Type, license | HTJ2K decode of the failing image |
|---|---|---|
| OpenJPEG 2.5.x (dcmnorm's previous decoder) | C, BSD-2 | fails: more than 3 coding passes / segment too long |
| OpenJPH (`main`, 0.32) | C++, BSD-2 | fails: "supports 1 quality layer only" |
| `openjph-core` 0.1 (Rust port of OpenJPH) | Rust, BSD-2 | same limits as OpenJPH |
| `hayro-jpeg2000` 0.4.1 | Rust, MIT/Apache | fails: no HTJ2K support at all |
| **OpenHTJ2K** (`dacb452`) | C++, BSD-3 | **decodes** |
| **Grok** 20.4 | C++, **AGPL-3** | **decodes** |
| **`j2k`** 0.11.3 | Rust, MIT/Apache | **decodes** |
| `oxideav-jpeg2000` 0.0.17 | Rust | decodes (2.3 s) |
| Kakadu 7.8 (the licensed version) | C++, commercial | no HTJ2K (added in Kakadu 8.0); hangs on HT input |

OpenHTJ2K, Grok, `j2k` and `oxideav-jpeg2000` produced byte-identical pixels for the failing
image.

### ISO/IEC 15444-4 HTJ2K conformance codestreams

These are the 22 `ds*_ht_*` codestreams shipped in OpenHTJ2K's `conformance_data/`.

| Decoder | Decoded |
|---|---|
| OpenJPEG 2.5 | 4 / 22. Failures include "more than 3 coding passes", "segment too long", "Expected EPH marker", "ROI not supported". |
| OpenHTJ2K | 22 / 22 |

Lossless outputs are identical to Grok's. Lossy (9/7) outputs are within ±2 of Grok's, which is
normal rounding between conformant decoders. Two small lossless codestreams that OpenJPEG cannot
decode are dcmnorm regression fixtures; see `test/files/htj2k_conformance/`.

## Benchmarks

### Method

- **Images:** four production full-field mammograms (4096x3328, 12-bit), a 2457x1890 10-bit
  mammogram, a 3000x1388 15-bit CR, and a 512x512 CT slice.
- **Encoders tested:**
  - production OpenJPEG settings (5 decomposition levels)
  - `j2k` at the same level count. Its default for LRCP is a single level, which doubles HTJ2K
    output size.
  - OpenHTJ2K reversible
  - the original vendor HTJ2K files
- **Correctness:** every lossless encode was decoded by every decoder and compared bit for bit with
  the source pixels. All matched.
- **Timing:** median of several runs, tools pinned with `taskset` to 1 core (production at the
  time) or 8 cores (the render service's allocation).
- **Units:** times are milliseconds per full-field mammogram. Command-line reference tools include
  process start and output-file writing.

### Classic JPEG 2000 lossless decode

| Decoder | 1 core | 8 cores |
|---|---|---|
| OpenJPEG, as built in production before this change (no threads) | 475-615 | 475-615 |
| OpenJPEG with threads | n/a | **135-170** |
| Kakadu 7.8 (`kdu_expand`) | n/a | 94-123 |
| Grok | n/a | 162-207 |
| OpenHTJ2K (Part 1 path) | n/a | 186-258 |
| `j2k` | 610-760 | 300-345 |

### HTJ2K lossless decode (vendor files)

| Decoder | 1 core | 8 cores |
|---|---|---|
| OpenJPEG (production, single-layer files only) | 106-131 | 65-70 (threaded) |
| `j2k` | 217-229 | ~200 (barely scales) |
| Grok | n/a | 61-63 |
| **OpenHTJ2K** | n/a | **27-30 CLI; 14 in-process (streaming API)** |

### Lossless encode (8 cores)

| Encoder | Time | Output size (one mammogram) |
|---|---|---|
| OpenJPEG classic, production (single-threaded) | ~560 | 3.94 MB |
| OpenJPEG classic with threads | 145-205 | 3.94 MB |
| `j2k` HTJ2K | 345-375 | 4.09 MB |
| **OpenHTJ2K HTJ2K** | **~37** | 4.09 MB |

At equal decomposition levels, HTJ2K files are about 3.5% larger than classic lossless.

`j2k` lossy encode took 15-17 s per mammogram, against about 0.6 s for OpenJPEG, and its HTJ2K
lossy output had much lower PSNR at the same ratio (31-36 dB vs 50-62 dB). Lossy encoding stays
on OpenJPEG.

### End to end through the `dcmnorm` CLI (8 cores, one full-field mammogram)

| Operation | dcmnorm 0.3.2 | After this change |
|---|---|---|
| Explicit VR LE -> `.90` (encode) | 641 | 181 |
| `.90` -> Explicit VR LE (decode) | 614 | 258 |
| vendor `.201` -> Explicit VR LE | 260 | 124 |
| Render vendor `.201` to PNG | 290 | 175 |
| Render the multi-layer HTJ2K CAD image | **fails** | 25 |

## Decision

1. **HTJ2K (`.201`-`.203`) decode: OpenHTJ2K**, with OpenJPEG as an automatic fallback if
   OpenHTJ2K ever rejects a codestream. It is the only permissively licensed decoder that handles
   all of the conformance suite, and the fastest by a wide margin.
2. **HTJ2K encode (`.201`, `.203`): OpenHTJ2K**, lossless (reversible 5/3, one layer, no colour
   transform).
   - `.202` (RPCL Options) stays decode-only: its DICOM profile requires PLT/TLM marker segments,
     which OpenHTJ2K's encoder doesn't write.
   - `.203` is encoded losslessly, which the transfer syntax allows.
3. **Classic JPEG 2000: OpenJPEG, now multi-threaded** for both decode and encode.
   - Threading alone closes most of the gap to Kakadu: 3.5-4.4x faster decode and about 3x faster
     encode on 8 cores.
   - Production builds previously compiled OpenJPEG without thread support.
4. **Kakadu: an optional override (`kakadu-ffi` + `--jpeg2000-codec kakadu`) for classic JPEG 2000
   only.** It is never used for HTJ2K: the licensed 7.8 SDK predates HTJ2K and can hang on it.
5. **Not adopted:**
   - `j2k`: correct and interoperable, but slower than OpenJPEG at every multi-threaded workload
     measured. It barely scales across cores, its lossy encode is 20-25x slower, and it is a young
     single-maintainer crate whose 0.12 release already needed a newer compiler than ours.
   - Grok: AGPL-3.
   - OpenJPH and `hayro-jpeg2000`: can't decode the files.

## Integration notes

The `dcmnorm-jpeg2000` crate (`crates/dcmnorm-jpeg2000`) holds the vendored OpenHTJ2K core, its C
bridge, and the OpenJPEG decoder.

**CPU variants.**
- OpenHTJ2K picks its SIMD code at compile time; its own CMake build uses `-march=native`.
- dcmnorm's binaries are built in one place and run in another: committed Node, Python and Java
  bindings, and the CLI `.deb`. Some edge VMs may not expose AVX2.
- `build.rs` therefore compiles OpenHTJ2K twice on x86-64 Linux, portable and `x86-64-v3`
  (AVX2), and the library picks one at runtime from the CPU's features.
- The portable variant is about 1.7-2x slower.
- The two copies are kept apart by partially linking each into one object and localizing every
  symbol except the bridge's variant-named entry points. OpenHTJ2K exports hundreds of
  un-namespaced symbols, so they would otherwise collide.
- `DCMNORM_HTJ2K_SINGLE_VARIANT=1` at build time skips the dual build.
- `DCMNORM_HTJ2K_SIMD=base` at run time forces the portable variant.

**Concurrency.**
- OpenHTJ2K is not safe for concurrent calls in one process: threads blocked in one call's barrier
  run other calls' pool tasks on shared per-thread scratch, and concurrent decodes segfault.
- dcmnorm serializes OpenHTJ2K calls with a process-wide lock.
- Each call already spreads one image across OpenHTJ2K's own pool, so this costs little
  throughput.

**API paths.**
- The buffered `invoke_line_based()` encoder path dereferences an unallocated buffer in this
  OpenHTJ2K version, for any image size. dcmnorm uses the row-streaming encode path that the
  reference CLI uses.
- Decode streams rows straight into the DICOM sample layout, which is about 2x faster than
  decoding to full `int32` planes.

**Small images.** The library doesn't clamp decomposition levels: a 16x16 image at 5 levels
crashes the reference encoder. The bridge clamps levels to what the image supports.

**Threads.**
- `DCMNORM_JPEG2000_THREADS` sets threads per codec call; the default is min(CPUs, 8).
- A call made from inside rayon's frame-parallel multi-frame decode uses one OpenJPEG thread, so
  the machine isn't oversubscribed.
- OpenHTJ2K's pool is sized once, at first use.

## Reproducing

- **Benchmark harness:** a small Rust program that loads raw 16-bit pixels plus JSON geometry. It
  encodes with production OpenJPEG settings (the same FFI code as
  `src/dicom_io/jpeg2000_openjpeg.rs`) and with `j2k`, decodes every codestream with each decoder,
  and checks results against the raw pixels.
- **Reference tools:** command-line builds of OpenHTJ2K, Grok, OpenJPH and Kakadu, timed with
  `taskset -c 0-7`.
- **Before you start:** extract multi-fragment DICOM pixel data by reassembling fragments, not by
  slicing from the PixelData offset. Leftover item tags inside the codestream produce misleading
  decoder errors.
