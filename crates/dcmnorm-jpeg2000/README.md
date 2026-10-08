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
- **No concurrent calls.** OpenHTJ2K calls are serialized with a process-wide lock, because the
  library isn't safe for concurrent decodes or encodes.

## Vendored OpenHTJ2K

`vendor/openhtj2k` contains `source/core/{common,codestream,coding,transform,interface,jph}` and
`LICENSE` (BSD-3-Clause) from commit `dacb452ecb21f50e88a74bf860c60f4aafa8a602` (2026-09-16), with
no modifications. `*_wasm.cpp` sources, JPIP, the apps and the third-party directories are left
out.

To update it:
1. Copy the same directories from a newer commit and update the commit hash above.
2. Run `cargo test -p dcmnorm-jpeg2000 --release` and dcmnorm's `htj2k` tests.
3. Re-check the two workarounds in `src/htj2k_bridge.cpp`: the streaming encode path, and
   decomposition-level clamping for small images.

## Build knobs

- `DCMNORM_HTJ2K_SINGLE_VARIANT=1` (build time): build only the portable variant.
- `DCMNORM_HTJ2K_SIMD=base` (run time): use the portable variant even on an AVX2 CPU.
- `OBJCOPY` / `AR` (build time): override the binutils used for the dual-variant build.
