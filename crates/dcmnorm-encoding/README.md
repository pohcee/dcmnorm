# dcmnorm-encoding

This project's own fork of the DICOM encoding/decoding primitives
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
