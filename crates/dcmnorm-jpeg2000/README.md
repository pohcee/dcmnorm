# dcmnorm-jpeg2000

JPEG 2000 codecs for dcmnorm:

- `htj2k`: High-Throughput JPEG 2000 (Part 15) decode and lossless encode, through a vendored copy
  of [OpenHTJ2K](https://github.com/osamu620/OpenHTJ2K) and a small C bridge
  (`src/htj2k_bridge.cpp`).
- `openjpeg`: classic JPEG 2000 decode through `openjpeg-sys`, with an explicit thread count.
  dcmnorm's classic *encoder* is `src/dicom_io/jpeg2000_openjpeg.rs` in the root crate.
- `threads`: the shared threading policy (`DCMNORM_JPEG2000_THREADS`).

See [docs/jpeg2000-codec-evaluation.md](../../docs/jpeg2000-codec-evaluation.md) for why these were
chosen and for the integration details. In short:

- **Two variants.** `build.rs` compiles OpenHTJ2K twice on x86-64 Linux (portable and
  `x86-64-v3`/AVX2) and keeps the copies apart by localizing every symbol except the bridge's
  entry points. The variant is chosen at runtime from CPU features.
- **Concurrent calls.** OpenHTJ2K supports multiple concurrent decoder/encoder instances across
  threads natively (no process-wide lock). Calls from inside Rayon workers pass `num_threads = 1`
  via `per_call_threads()` so they run on the calling thread without contending on the shared pool.
- **Truncation rejection.** OpenHTJ2K 0.27.0+ decodes truncated codestreams partially and flags
  them via `is_truncated()`. The bridge checks `is_truncated()` and returns an error so truncated
  DICOM frames are never accepted silently.

## Vendored OpenHTJ2K

`vendor/openhtj2k` contains `source/core/{common,codestream,coding,transform,interface,jph}` and
`LICENSE` (BSD-3-Clause) from release `v0.28.1` (`fe40d158db03bcfe2e80443347c3e606acd06424`,
2026-11-16), with no modifications. `*_wasm.*` sources, `CMakeLists.txt`, JPIP, the apps and the
third-party directories are left out.

To update it:
1. Copy the same directories from a newer release/commit and update the version and commit hash above.
2. Run `cargo test -p dcmnorm-jpeg2000 --release` and dcmnorm's `htj2k` tests.
3. Re-check `src/htj2k_bridge.cpp` (especially `is_truncated()` handling and decomposition-level
   clamping for small images).

## Build knobs

- `DCMNORM_HTJ2K_SINGLE_VARIANT=1` (build time): build only the portable variant.
- `DCMNORM_HTJ2K_SIMD=base` (run time): use the portable variant even on an AVX2 CPU.
- `OBJCOPY` / `AR` (build time): override the binutils used for the dual-variant build.
