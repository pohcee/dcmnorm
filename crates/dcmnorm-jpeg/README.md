# dcmnorm-jpeg

This project's own JPEG baseline/extended/lossless decoder, used by
`crates/dcmnorm-transcode`'s `jpeg` adapter to decode DICOM pixel data in the
JPEG Baseline, JPEG Extended, and JPEG Lossless (Process 14, including
Selection Value 1 / first-order prediction) transfer syntaxes.

Forked from [image-rs/jpeg-decoder](https://github.com/image-rs/jpeg-decoder)
0.3.2. Originally kept as `vendor/jpeg-decoder`, patched to fix a real
production bug (see below); fully absorbed as an in-house, dcmnorm-owned
crate once the ownership criterion became "own it unless it's a genuinely
decoupled codec that already works with no modifications" — this decoder
*was* modified (and modified for a customer-impacting bug, not a cosmetic
reason), so it doesn't qualify for staying external the way, say, the
unmodified `jpeg-encoder` or `flate2` do.

JPEG *encoding* (baseline lossy re-encode) is deliberately **not** part of
this crate — that's the separate, unmodified, external `jpeg-encoder` crate,
used directly by `dcmnorm-transcode`'s `jpeg` adapter alongside this decoder.

## The restart-marker fix

The `Predictor::Ra` (JPEG Lossless, Selection Value 1) scan decoder in
`src/decoder/lossless.rs` originally ignored restart-interval boundaries when
reconstructing predicted pixel values, and a related fast path checked a
stale scalar instead of a per-pixel restart flag. Both produced corrupted
output (periodic banding) on any lossless JPEG using restart markers — common
in mammography (Hologic C-View/tomosynthesis) DICOM, which is how this was
found. Fixed by tracking, per pixel index, whether that sample is the first
one decoded after a restart marker (`restart_starts` in
`decode_scan_lossless`), so the reconstruction pass can correctly reset
prediction at every restart boundary regardless of where it falls relative to
line boundaries (PS3.5/T.81 Annex H.1.2.3). This fix is now just... the code;
there's no separate patch/diff to carry forward anymore.

## Trimmed from upstream

- **Removed the `rayon` worker backend** (`src/worker/rayon.rs`, the `rayon`
  optional dependency and Cargo feature). dcmnorm already parallelizes JPEG
  decode at the *frame* level (`src/dicom_io/io.rs`'s
  `decode_pixel_data_parallel_frames`, used for multi-frame series); having
  each of those frame-level workers *also* spin up rayon's own row-level
  parallel color-conversion pipeline inside a single frame's decode is
  redundant nested parallelism with real thread-pool contention risk and no
  demonstrated benefit, for a real dependency-graph cost. The crate still has
  genuine parallelism available via the OS-thread-based `multithreaded`
  worker (`src/worker/multithreaded.rs`) for the single-frame case (e.g. a 2D
  mammography image, where dcmnorm's own frame-level parallelism has nothing
  to parallelize across) — only the rayon-specific *duplicate* of that
  capability was removed.
- **Removed `tests/`, `benches/`, `examples/`** — none of it actually worked
  in the vendored copy (`tests/lib.rs`'s `mod common; mod crashtest;
  mod reftest;` referenced files that were never included when this crate was
  first vendored — `exclude = ["/tests/*", "!/tests/*.rs"]` upstream kept only
  the `.rs` files, not their fixture images), and the benches/examples
  pulled in a real sample-image test corpus this workspace doesn't otherwise
  need. dcmnorm's own test suite (plus a dedicated regression test for the
  restart-marker fix above) is the acceptance bar for this crate now.
- Left untouched: the SIMD IDCT fast paths (`src/arch/{ssse3,neon,wasm}.rs`)
  and the rest of the decode pipeline (Huffman decoding, IDCT, upsampling,
  color conversion) — these are unrelated to the worker-threading question
  above, algorithmically stable, and not something to touch without strong
  reason given how easy it is to introduce a subtle correctness bug in
  exactly this kind of numeric code (which is how the restart-marker bug
  happened in the first place).

## Naming

Named `dcmnorm-jpeg`/`dcmnorm_jpeg` from the start — unlike the five
dicom-rs-derived forks (`dcmnorm-core` etc.), nothing else in the dependency
graph ever depended on the name `jpeg-decoder`, so there was no need for a
deferred-rename step.
