# dcmnorm-parser

dcmnorm's own fork of the DICOM dataset streaming reader/writer
(`DataSetReader`, `DataToken`, the stateful value decoder/encoder) that
`crates/dcmnorm-object` builds its `InMemDicomObject` tree on top of.

Forked from [dicom-rs](https://github.com/Enet4/dicom-rs)'s `dicom-parser`
0.9.1 as a mechanical, byte-for-byte transcription of `src/` — this is the
proven, spec-compliant engine that turns a byte stream into a token stream
(`DataToken::{ElementHeader, SequenceStart, ItemStart, PrimitiveValue, ...}`)
for any of the three base transfer syntaxes. `crates/dcmnorm-object` (new,
dcmnorm-authored code) folds that token stream into an in-memory tree; this
crate's own byte-level value decoding (every VR's exact wire format,
including edge cases accumulated from years of real-world DICOM files) was
deliberately kept as a proven port rather than rewritten, given the stakes of
getting per-VR value parsing subtly wrong in a medical-imaging library.

## Naming history

Originally kept the package/lib name `dicom-parser`/`dicom_parser` even
though, unlike `dicom-core`/`dicom-dictionary-std`/`dicom-encoding`/
`dicom-transfer-syntax-registry`, nothing else in the dependency graph
(`dicom-ul` never depended on it) actually forced that — done purely for
consistency with the other forks, to keep the Phase 6 rename batch uniform.
Fully renamed to `dcmnorm-parser`/`dcmnorm_parser` in that same batch, once
every crate it depends on (`dcmnorm-core`, `dcmnorm-dictionary`,
`dcmnorm-encoding`) had also dropped its original name.
