# HTJ2K conformance codestreams

Two lossless HTJ2K (ISO/IEC 15444-15) codestreams from the ISO/IEC 15444-4 | ITU-T T.803
conformance suite, as redistributed in the OpenHTJ2K repository's `conformance_data/` directory
(commit `dacb452`, the same one vendored under `crates/dcmnorm-jpeg2000/vendor/openhtj2k`). They
contain no patient data.

| File | Image | OpenJPEG 2.5 | Expected decode (FNV-1a 64 of 8-bit interleaved samples) |
|---|---|---|---|
| `ds0_ht_02_b11.j2k` | 64x126, 1 x 8-bit | fails ("Expected EPH marker") | `34065ee060953ded` |
| `ds0_ht_10_b11.j2k` | 64x64, 3 x 8-bit | fails ("Malformed HT codeblock") | `3dcdad4dac4e848e` |

The expected checksums come from decoding with Grok (an independent HTJ2K implementation), not
from dcmnorm itself. OpenHTJ2K's output matches Grok's exactly for both. Used by
`htj2k_conformance_codestreams_decode_where_openjpeg_cannot` in `src/dicom_io/io.rs`.
