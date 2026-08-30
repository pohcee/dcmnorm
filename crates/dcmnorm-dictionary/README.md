# dcmnorm-dictionary

dcmnorm's own fork of the standard DICOM data dictionary (`tags::*`,
`uids::*`, `StandardDataDictionary`/`.parse_tag()`/`.by_tag()`) that
`dcmnorm` and `dcmnorm-core` build on.

Forked from [dicom-rs](https://github.com/Enet4/dicom-rs)'s
`dicom-dictionary-std` 0.9.0 as a mechanical, byte-for-byte transcription
(`tags.rs`/`uids.rs`/`data_element.rs`/`sop_class.rs`/`lib.rs` are unmodified
from upstream) — this is ~4000 entries of DICOM Part 6/7 data, not
hand-written logic, so the port deliberately does not touch the generated
content. Long-term direction: regenerate this from NEMA's own machine-readable
PS3.6/PS3.7 registry so new tag revisions don't wait on any upstream refresh;
not done yet.

Originally kept the package/lib name `dicom-dictionary-std`/
`dicom_dictionary_std` (patched in via `[patch.crates-io]`) because
`dicom-object`/`dicom-encoding`/`dicom-parser` all still depended on the real
`dicom-dictionary-std`. Fully renamed to `dcmnorm-dictionary`/
`dcmnorm_dictionary` once all of those were replaced by their own dcmnorm
crates and nothing in the dependency graph needed the original name anymore.
