# dcmnorm-jpegls

Pure-Rust, `#![forbid(unsafe_code)]` ITU-T T.87 / ISO 14495-1 JPEG-LS lossless
(`.80`) and near-lossless (`.81`) codec for `dcmnorm`, used by
`src/dicom_io/jpeg_ls.rs`.

## Origin & Attribution

Derived from **`pure_jpegls` v2.1.0** by **jpfielding**, originally developed as
part of [`jpfielding/dicos.rs`](https://github.com/jpfielding/dicos.rs/tree/main/crates/jpegls)
and dual-licensed under **MIT OR Apache-2.0**.

`dcmnorm-jpegls` replaces the `charls` / `charls-sys` C++ FFI binding, which
previously was the only dependency in `dcmnorm` requiring `cmake` (and a
dedicated RHEL `manylinux.cmake` toolchain file for Linux wheels).

## Changes from Upstream `pure_jpegls` 2.1.0

### Feature additions
- **Multi-component interleaved scans (`ILV = 1` line-interleaved and `ILV = 2` sample-interleaved)** for both decoding and encoding:
  - `ILV = 1` implements per-component `RUNindex` tracking across shared contexts per ITU-T T.87 A.7.1.
  - `ILV = 2` implements triplet run mode and sample-interleaved run interruption (`RItype = 0`, `Px = Rb`, `sign = sign(Rb - Ra)`) per ITU-T T.87 A.7.3.
- **Direct row-streaming DICOM byte-buffer APIs (no intermediate full-frame `Vec<u16>`)**:
  - `read_frame_info(&[u8])` parses SOF55/LSE/SOS headers without decoding pixel data.
  - `decode_to_bytes(&[u8])` / `decode_to_bytes_with_options(...)` stream each decoded row directly into a single native little-endian `Vec<u8>` (1 byte/sample for `<= 8`-bit, 2 bytes/sample LE for `> 8`-bit; planar for `ILV = 0`, sample-interleaved for `ILV = 1`/`ILV = 2`) with **no intermediate full-frame `Vec<u16>`**.
  - `encode_bytes(...)` streams rows directly from native little-endian byte buffers (`ILV = 0`, `1`, or `2`) with **no intermediate full-frame `Vec<u16>`**.
- **Clinical compatibility**: preserves upstream's T.81 B.1.1.2 `0xFF` fill-byte handling before EOI (`FF FF D9`, emitted by DCMTK and legacy encoders to pad odd-length streams to even length), which CharLS 2.x rejects as `too_much_encoded_data`.

### Performance & memory optimizations (~2.1×–2.6× faster than upstream)
- **Buffered `BitWriter` + single-word Golomb packing** (`src/bitstream.rs`): buffers output in a 4 KiB stack array instead of issuing a `dyn Write` call per byte, and packs unary zeros + stop bit + remainder in a single `u64` shift/OR.
- **Left-justified `u64` `BitReader` with `next_ff` fast refill + pre-unmapped 8-bit Golomb-Rice LUT** (`src/bitstream.rs`): pre-scans the next `0xFF` byte so normal refills pull 4–7 bytes with zero bit-stuffing checks, and decodes ~85–95% of regular samples directly into signed unmapped errors in a single 4 KiB L1 LUT lookup + shift (skipping `unmap_error` and redundant range checks).
- **Zero-allocation `ContextModel` (`[CtxEntry; 367]` + inline `[i8; 1024]` gradient LUT + branchless `compute_k` / `split_qs`)** (`src/context.rs`): replaces 4 separate heap `Vec<i32>` allocations with a single stack/inline context table, eliminates clamps for 8-bit gradients, unifies run-mode detection (`qs == 0`) with context computation, and computes Golomb parameter `k` via `leading_zeros()`.
- **Compact `u16` / `[u16; 3]` line buffers + branchless MED predictor + `const LOSSLESS`/`POW2` specialization** (`src/decode.rs`, `src/encode.rs`, `src/predictor.rs`): halves line-buffer cache footprint (`2 × (W + 2) × 2 B` = 8 KiB for `W = 2048` grayscale), replaces unpredictable modulo wrap branches with `& max_val` for power-of-2 ranges, and keeps error formatting in `#[cold]` helpers.

### Safety & Trimming
- Enforced **`#![forbid(unsafe_code)]`** at the crate root (zero `unsafe` blocks anywhere in the crate).
- **Removed `Profile::LegacyGo` (`src/legacy.rs`)**: upstream's frozen 1.0.0 Go-migration bitstream format (non-standard `0xFF00` stuffing, no run mode) is not valid ITU-T T.87 / DICOM.

## Memory Profile

- **Internal codec working memory:** `ContextModel` (~7 KiB inline, zero heap allocations), static 4 KiB `REGULAR_GOLOMB_LUT` in `.rodata`, `BitWriter` 4 KiB stack buffer, and per-mode `u16` row buffers:
  - **`ILV = 0` (grayscale or planar):** 2 rows (`2 × (W + 2) × 2 B`) on decode (~8 KiB at `W = 2048`); +1 source row (~12 KiB at `W = 2048`) on encode.
  - **`ILV = 1` / `ILV = 2` (3-component interleaved):** 6 component rows (`6 × (W + 2) × 2 B`) on decode (~24 KiB at `W = 2048`); +3 source rows (~36 KiB at `W = 2048`) on encode.
- **Decode peak heap (`decode_to_bytes`):** measured at **1.00×–1.01×** of the output frame size across the benchmark workloads below (**+1–8 KiB** working buffers above the single returned `Vec<u8>` — matching CharLS, with no intermediate full-frame `Vec<u16>`).
- **Encode peak heap (`encode_bytes`):** zero copy of the input pixel buffer; allocates only the compressed output `Vec<u8>` (pre-sized to `raw_bytes / 2 + 64`) plus the row buffers above.

## Benchmark vs C++ CharLS (`charls` 0.4.2 / CharLS 2.4.5)

Apples-to-apples `--release` benchmark on Linux x86_64 comparing `dcmnorm_jpegls::{encode_bytes, decode_to_bytes}` directly against `charls::CharLS::{encode, decode}` on identical `&[u8]` / `Vec<u8>` DICOM buffers (median latency across 9–25 iterations after 3 warmups; cross-checked 100% bit-exact in both directions `charls <-> dcmnorm-jpegls`):

| Workload | `charls` enc | `pure_jpegls` enc | `dcmnorm-jpegls` enc | vs C++ | `charls` dec | `pure_jpegls` dec | `dcmnorm-jpegls` dec | vs C++ | Decode peak heap |
| :--- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| 8-bit gray 512×512 lossless | 3.12 ms | 6.71 ms | **2.60 ms** | **0.83×** | 2.48 ms | 5.58 ms | **2.29 ms** | **0.93×** | 1.01× (+2 KiB) |
| 8-bit gray 333×277 odd | 0.93 ms | 1.97 ms | **0.75 ms** | **0.80×** | 0.73 ms | 1.59 ms | **0.64 ms** | **0.88×** | 1.01× (+1 KiB) |
| 12-bit gray 512×512 lossless | 2.99 ms | 5.65 ms | **2.49 ms** | **0.83×** | 1.97 ms | 4.82 ms | 2.14 ms | 1.08× | 1.00× (+2 KiB) |
| 12-bit gray 512×512 near=3 | 3.64 ms | 6.10 ms | **3.31 ms** | **0.91×** | 1.96 ms | 4.70 ms | 2.12 ms | 1.08× | 1.00× (+2 KiB) |
| 12-bit gray 1024×1024 lossless | 10.88 ms | 22.35 ms | **9.78 ms** | **0.90×** | 7.94 ms | 18.98 ms | 8.36 ms | 1.05× | 1.00× (+4 KiB) |
| 16-bit gray 512×512 lossless | 2.61 ms | 4.85 ms | **2.48 ms** | **0.95×** | 2.03 ms | 4.02 ms | 2.32 ms | 1.15× | 1.00× (+2 KiB) |
| 16-bit gray 1024×1024 lossless | 10.92 ms | 20.05 ms | **9.83 ms** | **0.90×** | 7.64 ms | 16.38 ms | 9.07 ms | 1.19× | 1.00× (+4 KiB) |
| 16-bit gray 2048×2048 lossless | 41.49 ms | 85.57 ms | **38.25 ms** | **0.92×** | 30.21 ms | 70.78 ms | 34.65 ms | 1.15× | 1.00× (+8 KiB) |
| 8-bit RGB 512×512 planar (`ILV=0`) | 8.01 ms | 17.23 ms | **6.85 ms** | **0.86×** | 6.34 ms | 14.43 ms | **6.03 ms** | **0.95×** | 1.00× (+2 KiB) |
| 8-bit RGB 512×512 line (`ILV=1`) | 8.26 ms | N/A | **6.92 ms** | **0.84×** | 6.40 ms | N/A | **6.02 ms** | **0.94×** | 1.01× (+6 KiB) |
| 8-bit RGB 512×512 sample (`ILV=2`) | 7.38 ms | N/A | **6.24 ms** | **0.85×** | 5.20 ms | N/A | 5.36 ms | 1.03× | 1.01× (+6 KiB) |
| 8-bit RGB 512×512 sample `near=3` | 8.79 ms | N/A | **8.48 ms** | **0.96×** | 5.45 ms | N/A | 5.74 ms | 1.05× | 1.01× (+6 KiB) |
| Clinical 600×430 8-bit DICOM | — | — | — | — | 0.92 ms | 2.03 ms | **0.85 ms** | **0.91×** | 1.01× |
| Clinical 512×512 16-bit (`FF FF D9`) | — | — | — | — | Error | 8.28 ms | **3.63 ms** | **2.28× vs upstream** | 1.00× |
