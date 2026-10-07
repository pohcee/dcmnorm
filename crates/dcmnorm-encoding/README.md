# dcmnorm-encoding

dcmnorm's own fork of the DICOM encoding/decoding primitives
(`TransferSyntax`, `Codec`, `PixelDataReader`/`PixelDataWriter`,
`EncodeOptions`, plus the Implicit/Explicit VR Little/Big Endian dataset
encoders and decoders) that `dcmnorm-transcode`, `dcmnorm-parser`, and
`dcmnorm-object` build on.

Forked from [dicom-rs](https://github.com/Enet4/dicom-rs)'s `dicom-encoding`
0.9.1 as a mechanical, byte-for-byte transcription. Still carries the
dataset-tokenizer/encoder machinery (`decode/`, `encode/`, `text.rs`) in
full, not trimmed to just the `adapters.rs`/`transfer_syntax/` surface
`dcmnorm`'s own code calls directly — `dcmnorm-parser`/`dcmnorm-object`
depend on that full surface. Trimming it to a thin, `dcmnorm`-scoped API is
a deliberate, not-yet-done follow-up.

Originally kept the package/lib name `dicom-encoding`/`dicom_encoding`
(patched in via `[patch.crates-io]`) because `dicom-object` and `dicom-ul`
both still depended on it. Fully renamed to `dcmnorm-encoding`/
`dcmnorm_encoding` once both were replaced by `dcmnorm-object` and
`dcmnorm-dimse` and nothing in the dependency graph needed the original name
anymore.

## Upstream changes ported since the fork

Selectively ported from dicom-rs after 0.9.1 (each commit message names the
upstream commit):

- `encode/explicit_{le,be}.rs`: reject short-form VR values over 65535 bytes
  (upstream bff112e6) instead of truncating the length field.
- `adapters.rs`: a correct, cumulative Basic Offset Table from the default
  `PixelDataWriter::encode` (upstream 59ab4edb), which here also counts
  odd-length fragments' padding byte.
- `decode/adaptive_le.rs`: `AdaptiveVRLittleEndianDecoder` (upstream
  e5e2a99a), for files that declare Explicit VR Little Endian but write
  Implicit VR. dcmnorm decides a probed long-form VR that disagrees with the
  dictionary from its reserved bytes, so explicit files written as all-`UN`
  aren't misdetected.
