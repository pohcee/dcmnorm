# dcmnorm-core

dcmnorm's own fork of the essential DICOM data structures and mechanisms
(`Tag`, `VR`, `PrimitiveValue`, `Length`, `Value`, `DataElement`,
`PixelFragmentSequence`, the `DataDictionary` trait, `VirtualVr::relaxed()`,
`ops::ApplyOp`) that `dcmnorm` builds on.

Forked from [dicom-rs](https://github.com/Enet4/dicom-rs)'s `dicom-core`
0.9.1 to fix bugs in this layer directly instead of waiting on
upstream — see `src/header.rs`'s `VR::from_binary` for the first such fix
(non-standard `xs`/`ox` ambiguous-VR shorthand bytes, seen from dcmjs-family
DICOM writers, that upstream `dicom-core` doesn't recognize).

Originally kept the package/lib name `dicom-core`/`dicom_core` (patched in
via `[patch.crates-io]`) because `dicom-object`/`dicom-encoding`/
`dicom-dictionary-std`/`dicom-transfer-syntax-registry`/`dicom-ul` all still
depended on the real `dicom-core`, and a rename would have broken Cargo's
patch substitution for all of them. Fully renamed to `dcmnorm-core`/
`dcmnorm_core` once every one of those was replaced by its own dcmnorm
crate (`dcmnorm-dictionary`, `dcmnorm-encoding`, `dcmnorm-transcode`,
`dcmnorm-parser`, `dcmnorm-object`, `dcmnorm-dimse`) and nothing in the
dependency graph needed the original name anymore.
