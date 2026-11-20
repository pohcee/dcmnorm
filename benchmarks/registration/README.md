# Registration benchmark (public TCIA data)

Accuracy of `estimate_translation` and `register_rigid` on real CT, PT and MR pairs, plus versions of
them with known moves, cropped ends and a smaller reconstruction field of view. The data is not in
the repository; this folder has the case list, the reference matrices and the scripts to rebuild
the derived series.

## Data

Six public series from [The Cancer Imaging Archive](https://www.cancerimagingarchive.net/):

| Dir | Collection (licence) | SeriesInstanceUID | What |
|---|---|---|---|
| `A` | RIDER Lung CT (CC BY 4.0) | `1.3.6.1.4.1.9328.50.1.48441840081578419409180519840073808100` | chest CT, 213 x 1.25 mm |
| `B` | RIDER Lung CT (CC BY 4.0) | `1.3.6.1.4.1.9328.50.1.83304264089411327530730818890072724533` | the same patient's repeat CT, minutes later, 236 x 1.25 mm |
| `P1` | RIDER Lung PET-CT (CC BY 3.0) | `1.3.6.1.4.1.9328.50.17.10308613002188064065304746188291350954` | FDG PET, whole body |
| `P2` | RIDER Lung PET-CT (CC BY 3.0) | `1.3.6.1.4.1.9328.50.17.71370567559995220330016138139725857573` | the same patient 6 months later |
| `M1` | QIN-PROSTATE-Repeatability (CC BY 4.0) | `1.3.6.1.4.1.14519.5.2.1.3671.4754.609837242670719860929372097937` | T2 axial prostate slab, oblique, 30 x 3 mm |
| `M2` | QIN-PROSTATE-Repeatability (CC BY 4.0) | `1.3.6.1.4.1.14519.5.2.1.3671.4754.983460207615355998147518323000` | the same patient's repeat exam |

Data from: RIDER Lung CT, doi:10.7937/K9/TCIA.2015.U1X8A5NR; RIDER Lung PET-CT,
doi:10.7937/K9/TCIA.2015.OFIP7TVM; QIN-PROSTATE-Repeatability, doi:10.7937/K9/TCIA.2018.MR1CKGND;
Clark K, et al. The Cancer Imaging Archive (TCIA), J Digit Imaging 26(6), 2013.

Download them (NBIA Data Retriever, or the TCIA REST API `getImage?SeriesInstanceUID=...`) into
`<root>/A`, `<root>/B`, ... and build the derived series:

```bash
benchmarks/registration/make-cases.sh <root>      # needs python3 with numpy and pydicom
```

`perturb.py` makes each derived series from a source one: a known rigid move written into
ImagePositionPatient / ImageOrientationPatient (the pixels are untouched), slices dropped from the
top or bottom, and pixels outside a circular reconstruction field of view set to -1024 HU. It
writes the true moving -> fixed matrix next to the series (`<name>.truth.json`).

## Run

```bash
cargo run --release --example registration_benchmark -- <root>              # the table
cargo run --release --example registration_benchmark -- <root> --json       # machine-readable
cargo run --release --example registration_benchmark -- <root> --diverge    # far-off seeds
BENCH_VERBOSE=1 cargo run --release --example registration_benchmark -- <root> --only ct-fov250
```

Errors are the mean displacement, in mm, of 27 points spread over the fixed volume's central 60%.
`[floor]` is the best a pure translation can do: the truth's rotation alone. The real pairs'
truths (`ref-*.json`) are rigid registrations that every start agreed on (dcmnorm 0.16.0), not
independent ground truth, so a millimetre or two against them, or more for the PET pair 6 months
apart, does not prove an error. The derived series' truths are exact relative to them. `--diverge`
also starts single and multi-start registrations 120 or 180 mm off in S-I, 60 mm off in-plane,
rotated 25 degrees, or upside down, to see what a divergent result scores.

## Cases

| Case | Fixed | Moving | What it tests |
|---|---|---|---|
| `ct-repeat` | A | B | the real repeat pair, full overlap |
| `ct-fov250` | A | B, 250 mm FOV | different FOV truncation |
| `ct-crop-top60` | A | B, top 60 mm removed | a cropped prior |
| `ct-shift` | A | B, moved 15/-20/40 mm | an S-I and table offset |
| `ct-rot-shift` | A | B, rotated 3/2/4 degrees and moved | rotation |
| `ct-crop-rot-shift-fov` | A | B, all of the above | everything at once |
| `ct-both-cropped` | A, bottom 40 mm removed | B, all of the above | a partial overlap at both ends |
| `ct-self-*` | A or A cropped | A, cropped, rotated, moved | exact truth |
| `pt-repeat-6mo` | P1 | P2 | the real pair, 6 months apart |
| `pt-self-crop150-rot` | P1 | P1, top 150 mm removed, rotated, moved | exact truth |
| `pt-6mo-crop150-rot` | P1 | P2, the same | a cropped prior 6 months apart |
| `pt-both-cropped` | P1, bottom 200 mm removed | P2, the same | a partial overlap |
| `mr-repeat` | M1 | M2 | 80-90 mm oblique slabs |
| `mr-self-rot`, `mr-repeat-rot` | M1 | M1 / M2 rotated, moved, bottom 18 mm removed | short slabs |
| `x-unrelated-*` | | | different patients and modalities, no truth: confidence and quality only |

## Results (2026-11-20, 22-core x86-64)

The estimate, the default rigid registration, and the multi-start rigid registration
(`Initialization::Multi`). Errors are in mm. The estimate shows confidence; the rigid shows the
quality label.

| Case | Translation floor | Estimate 0.16.0 | Estimate 0.17.0 | Rigid 0.16.0 | Rigid 0.17.0 | Multi-start 0.17.0 |
|---|---|---|---|---|---|---|
| ct-repeat | 5.8 | 6.3 / 0.99 | 6.2 / 1.00 | 0.1 good | 0.4 good | 0.3 good |
| ct-fov250 | 5.8 | **191.7** / 0.06 | 6.7 / 0.50 | 1.5 **poor** | 1.4 good | 1.4 good |
| ct-crop-top60 | 5.8 | 6.2 / 0.64 | 6.3 / 0.99 | 0.9 good | 0.8 good | 0.8 good |
| ct-shift | 5.8 | 6.3 / 0.99 | 6.2 / 1.00 | 0.1 good | 0.2 good | 0.2 good |
| ct-rot-shift | 7.9 | 9.2 / 0.99 | 9.4 / 0.73 | 0.3 good | 0.4 good | 0.2 good |
| ct-crop-rot-shift-fov | 7.9 | **185.4** / 0.01 | 10.4 / 0.21 | 1.7 **poor** | 1.7 good | 1.7 good |
| ct-both-cropped | 7.2 | **186.3** / 0.01 | **34.1** / 0.00 | 1.3 **poor** | 1.3 good | 1.4 good |
| ct-self-crop-rot | 9.3 | 10.3 / 0.97 | 10.6 / 1.00 | 0.1 good | 0.1 good | 0.1 good |
| ct-self-both-cropped | 9.0 | 10.2 / 0.77 | 10.6 / 0.90 | 0.2 good | 0.1 good | 0.1 good |
| pt-repeat-6mo | 34.8 | **635.0** / 0.23 | 48.3 / 0.76 | 0.1 fair | 4.6 good | 4.6 good |
| pt-self-crop150-rot | 20.6 | 21.0 / 0.75 | 20.8 / 0.54 | 0.3 good | 0.1 good | 0.1 good |
| pt-6mo-crop150-rot | 28.6 | **509.5** / 0.13 | 30.9 / 0.26 | 4.9 fair | 5.1 good | 5.2 good |
| pt-both-cropped | 27.1 | **144.9 / 0.48** | 30.0 / 0.22 | **358.0** poor | 32.8 fair | 32.9 fair |
| mr-repeat | 5.0 | no overlap | 8.2 / 0.63 | 0.1 fair | 0.1 good | 0.0 good |
| mr-self-rot | 4.2 | no overlap | 6.0 / 0.50 | 0.1 good | 0.0 good | 0.0 good |
| mr-repeat-rot | 7.7 | no overlap | 10.2 / 0.64 | 2.6 fair | 2.3 good | 2.3 good |
| x-unrelated-ct-pt | - | - | 0.50 (chest matched to chest) | - | poor | poor |
| x-unrelated-pt-ct | - | - | 0.02 | - | poor | poor |
| x-unrelated-mr-ct | - | - | 0.00 | - | poor | poor |

Times on this machine: the estimate 110-280 ms for CT/PT and 15-70 ms for MR (0.16.0: 110-460 ms);
the default rigid registration 0.5-1.3 s; the multi-start one 0.8-1.9 s.

`pt-both-cropped` is a genuinely ambiguous case: a PET 6 months apart with a different posture,
compared over a partial overlap. Every start converges to the same transform 8 degrees from the
reference, and it is labelled `fair`, not `good`.

With `--diverge`, the 80 single-start runs from far-off seeds gave 28 correct results, all `good`,
and 43 divergent ones, all `poor`. The one `fair` was `pt-both-cropped` again. The other 8 had no
overlap. The multi-start registration from the same seeds was correct in every case except
`pt-both-cropped`, and consistent in all of them.
