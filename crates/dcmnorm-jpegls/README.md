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
- **Direct DICOM byte-buffer APIs**:
  - `read_frame_info(&[u8])` parses SOF55/LSE/SOS headers without decoding pixel data.
  - `decode_to_bytes(&[u8])` decodes directly into native little-endian bytes (1 byte/sample for `<= 8`-bit, 2 bytes/sample LE for `> 8`-bit; planar for `ILV = 0`, sample-interleaved for `ILV = 1`/`ILV = 2`), matching DICOM `PixelData` and CharLS conventions without an intermediate `Vec<u16>` conversion.
  - `encode_bytes(...)` encodes directly from native little-endian byte buffers (`ILV = 0`, `1`, or `2`).
- **Clinical compatibility**: preserves upstream's T.81 B.1.1.2 `0xFF` fill-byte handling before EOI (`FF FF D9`, emitted by DCMTK and legacy encoders to pad odd-length streams to even length), which CharLS 2.x rejects as `too_much_encoded_data`.

### Performance optimizations (~2.0×–2.4× faster than upstream)
- **Buffered `BitWriter` + single-word Golomb packing** (`src/bitstream.rs`): buffers output in a 4 KiB stack array instead of issuing a `dyn Write` call per byte, and packs unary zeros + stop bit + remainder in a single `u64` shift/OR.
- **Left-justified `u64` `BitReader` with SWAR 8-byte `0xFF`-free fast refill + compile-time 8-bit Golomb-Rice LUT** (`src/bitstream.rs`): decodes ~85–95% of regular samples in a single L1 table lookup + shift without `leading_zeros` or escape branches.
- **AoS context array + precomputed gradient quantization LUT + branchless `compute_k`** (`src/context.rs`): replaces 4 separate heap `Vec<i32>` allocations with a single cache-local `[CtxEntry; 367]`, replaces 9-branch gradient quantization with a 1D LUT, and computes Golomb parameter `k` in 2 ALU instructions via `leading_zeros()`.
- **Register-carried neighbors + `const LOSSLESS: bool` specialization + triplet `ILV = 2` buffers** (`src/decode.rs`, `src/encode.rs`): eliminates near-lossless branches in the lossless hot loop, carries `(Ra, Rb, Rc)` in registers across each row, and uses triplet `[i32; 3]` line buffers for `ILV = 2`.

### Safety & Trimming
- Enforced **`#![forbid(unsafe_code)]`** at the crate root (zero `unsafe` blocks anywhere in the crate).
- **Removed `Profile::LegacyGo` (`src/legacy.rs`)**: upstream's frozen 1.0.0 Go-migration bitstream format (non-standard `0xFF00` stuffing, no run mode) is not valid ITU-T T.87 / DICOM.

## Benchmark vs C++ CharLS (`charls` 0.4.2 / CharLS 2.4.5)

Measured in `--release` mode on Linux x86_64 (median latency across 9–25 iterations after 3 warmups; medical-style synthetic images with flat background for run mode, smooth gradients, and high-frequency texture; cross-checked 100% bit-exact in both directions `charls <-> dcmnorm-jpegls`):

| Workload | `charls` enc | `pure_jpegls` enc | `dcmnorm-jpegls` enc | vs C++ | `charls` dec | `pure_jpegls` dec | `dcmnorm-jpegls` dec | vs C++ |
| :--- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| 8-bit gray 512×512 lossless | 3.22 ms | 6.62 ms | **2.36 ms** | **0.73×** | 2.48 ms | 4.79 ms | **2.39 ms** | **0.97×** |
| 8-bit gray 333×277 odd | 0.83 ms | 1.79 ms | **0.72 ms** | **0.86×** | 0.64 ms | 1.43 ms | 0.72 ms | 1.14× |
| 12-bit gray 512×512 lossless | 2.70 ms | 5.82 ms | **2.36 ms** | **0.87×** | 1.99 ms | 4.70 ms | 2.09 ms | 1.05× |
| 12-bit gray 512×512 near=3 | 3.51 ms | 6.20 ms | **3.35 ms** | **0.95×** | 1.96 ms | 4.66 ms | 2.33 ms | 1.19× |
| 12-bit gray 1024×1024 lossless | 10.47 ms | 23.08 ms | 10.68 ms | 1.02× | 7.93 ms | 18.54 ms | 9.42 ms | 1.19× |
| 16-bit gray 512×512 lossless | 3.00 ms | 5.83 ms | **2.86 ms** | **0.96×** | 2.36 ms | 4.54 ms | 2.67 ms | 1.13× |
| 16-bit gray 1024×1024 lossless | 11.96 ms | 24.15 ms | **9.84 ms** | **0.82×** | 8.93 ms | 16.47 ms | **8.81 ms** | **0.99×** |
| 16-bit gray 2048×2048 lossless | 41.16 ms | 89.70 ms | **38.10 ms** | **0.93×** | 30.83 ms | 69.42 ms | 33.94 ms | 1.10× |
| 8-bit RGB 512×512 planar (`ILV=0`) | 8.16 ms | 17.97 ms | **7.20 ms** | **0.88×** | 6.32 ms | 14.33 ms | 7.32 ms | 1.16× |
| 8-bit RGB 512×512 line (`ILV=1`) | 8.27 ms | N/A | **6.91 ms** | **0.84×** | 6.40 ms | N/A | 7.40 ms | 1.16× |
| 8-bit RGB 512×512 sample (`ILV=2`) | 7.37 ms | N/A | **7.16 ms** | **0.97×** | 5.20 ms | N/A | 6.48 ms | 1.25× |
| 8-bit RGB 512×512 sample `near=3` | 8.78 ms | N/A | 9.42 ms | 1.07× | 5.34 ms | N/A | 7.24 ms | 1.35× |
| Clinical 600×430 8-bit DICOM | — | — | — | — | 0.92 ms | 2.01 ms | **0.89 ms** | **0.96×** |
| Clinical 512×512 16-bit (`FF FF D9`) | — | — | — | — | Error | 8.06 ms | **3.56 ms** | **2.27× vs upstream** |

*(Note: table above benchmarks `decode_image` returning `Vec<u16>` planar; `decode_to_bytes` eliminates the `u16 -> u8` narrowing copy and 3-plane scatter for 8-bit / interleaved DICOM frames.)*
