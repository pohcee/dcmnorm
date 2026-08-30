# dcmnorm-transcode

dcmnorm's own fork of the DICOM transfer-syntax registry (`TransferSyntaxRegistry`,
the table of known transfer syntax UIDs, and the codec adapters for JPEG
Baseline/Extended/Lossless, JPEG 2000, RLE Lossless, and Deflated pixel data)
that `dcmnorm` builds on.

Forked from [dicom-rs](https://github.com/Enet4/dicom-rs)'s
`dicom-transfer-syntax-registry` 0.9.1 as a mechanical, byte-for-byte
transcription of `src/`, then trimmed and given in-house JPEG decode
ownership (see below) once nothing else in the dependency graph needed its
original full surface.

Enabled features mirror what `dcmnorm`'s `Cargo.toml` actually requests:
`native` (JPEG + RLE), `deflate`, `openjpeg-sys`. Not enabled: `openjp2`
(alternate JPEG2000 backend, `openjpeg-sys` is used instead), `simd`
(available for `jpeg-encoder` but not currently requested).

## Codec ownership

- **JPEG decode** (baseline/extended/lossless, transfer syntaxes `.4.50/.51/.57/.70`)
  is now backed by `crates/dcmnorm-jpeg`, an in-house crate forked from
  `jpeg-decoder` 0.3.2 — see that crate's own README for why. JPEG *encode*
  (baseline lossy re-encode only) still uses the external `jpeg-encoder`
  crate: it's unmodified and works fine, so per the ownership criterion
  ("own it unless it's a genuinely decoupled codec that already works with
  no modifications") it stays external.
- **RLE Lossless** (`.2.5`, `adapters/rle_lossless.rs`) has always been
  dcmnorm's own code with zero external dependency — nothing to change here,
  it was already in-house.
- **Deflate** (dataset-level Deflated Explicit VR LE, the rare "Deflated
  Image Frame Compression" pixel adapter, and an unrelated internal use in
  `volume_export.rs`/`texture_export.rs` for GPU-cache compression) stays on
  `flate2` — generic, standard DEFLATE compression with zero DICOM-specific
  value in re-implementing it.
- **JPEG 2000/HTJ2K** (`adapters/jpeg2k.rs`, via the `jpeg2k` crate wrapping
  `openjpeg-sys`) stays external and is the real, live default JPEG2000
  decode path (`src/dicom_io/io.rs`'s Kakadu-then-OpenJPEG fallback) —
  correctly out of scope per the original dicom-rs-removal plan: hand-writing
  a compliant JPEG2000 codec is far riskier than the value it'd add.
- **JPEG-LS** and **JPEG XL** have no adapters in this crate at all anymore
  (see "Trimmed to dcmnorm's actual surface" below) — dcmnorm's own JPEG-LS
  support calls `charls` directly, bypassing this registry entirely, and
  nothing in the wild has ever required JPEG XL decode.

## Trimmed to dcmnorm's actual surface

Originally kept as a full byte-identical port because `dicom-object` and
`dicom-ul` (both since fully replaced) needed the complete upstream API
surface, not just what `dcmnorm` itself calls. Once both were gone, this was
trimmed:

- Deleted `adapters/jpegls.rs` and `adapters/jpegxl.rs` — both gated behind
  Cargo features (`charls`, `jxl-oxide`/`zune-jpegxl`) that were never
  requested by any real build in this workspace (confirmed via
  `cargo tree -e features`). Their transfer syntax UIDs remain registered as
  registry-completeness stubs in `entries.rs` (`Codec::None` — metadata only,
  no codec code), so lookups/negotiation against real-world senders still
  resolve the UID correctly.
- Deleted `adapters/encapsulated.rs` — it wasn't even wired into
  `adapters/mod.rs` and didn't compile as-is (leftover cruft from the fork);
  the real, working implementation of its transfer syntax
  (Encapsulated Uncompressed Explicit VR LE) lives in `adapters/uncompressed.rs`
  + `entries.rs` and was untouched by this deletion.
- Removed the `native_windows` (deprecated alias for `native`),
  `openjpeg-sys-threads`, `charls`, `charls-vcpkg`, `jxl-oxide`,
  `zune-jpegxl`, `zune-jpegxl-threads`, `rayon`, and `inventory-registry`
  Cargo features, and their corresponding optional dependencies — none were
  ever requested by any build target in this workspace.
- `default = []` (was `default = ["rayon", "simd"]`) so a direct
  `cargo build --workspace`/`cargo test --workspace` doesn't silently pull in
  features the real `dcmnorm`/`dcmtalk`/binding builds never request.

Left alone (confirmed live or confirmed cheap-and-harmless, not trim
candidates): `decode::`/`encode::`/`text::`/`adapters.rs`/`transfer_syntax/`
in `dcmnorm-encoding` (all reachable via the live `TransferSyntaxRegistry`
dispatch table); the MPEG/JPIP/SMPTE/HTJ2K stub registry entries in
`entries.rs` (metadata-only `Codec::None` rows, negligible cost, useful for
negotiation completeness against real-world senders); JPEG
Extended/Lossless variants and Deflated-Explicit-VR-LE/JPIP-Deflate/
Deflated-Image-Frame-Compression/JPEG2000-Part-2-multi-component (all share
live, tested codec code paths — untested by name in `tests.rs`, but not
separable dead code).

## What was left out

Upstream's own `tests/` directory (~1500 lines, its dev-dependencies were
`dicom-test-files` — a crate that fetches real sample DICOM files — and
`dicom-object`) was deliberately not ported. dcmnorm's own test suite plus
its `test/files/*.dcm` fixtures are the acceptance bar for this fork, and
pulling in a network-fetched sample-file dependency was more scope than that
needed.

## Naming history

Originally kept the package/lib name `dicom-transfer-syntax-registry`/
`dicom_transfer_syntax_registry` (patched in via `[patch.crates-io]`)
because `dicom-object` and `dicom-ul` both still depended on it. Fully
renamed to `dcmnorm-transcode`/`dcmnorm_transcode` once both were replaced by
`dcmnorm-object` and `dcmnorm-dimse` and nothing in the dependency graph
needed the original name anymore.
