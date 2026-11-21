# dcmnorm CLI

## dcmnorm CLI Usage

Examples use the sample fixtures in [`test/files/`](../test/files/). When working from a source checkout without installing, substitute `cargo run -p dcmnorm-cli --` for `dcmnorm`.

Get the full option reference from either help form:

```bash
dcmnorm -h
dcmnorm --help
```

Command shape:

```text
dcmnorm [OPTIONS] [INPUT]... [OUTPUT]
```

`dcmnorm` infers the conversion direction from the input and output file types:

- DICOM input + JSON output, or no output, runs DICOM to JSON
- DICOM input + DICOM output with `--transfer-syntax <UID>` runs DICOM to DICOM transcoding
- DICOM input + `.png` / `.jpg` / `.jpeg` / `.raw` output runs DICOM frame rendering
- JSON input + DICOM output runs JSON to DICOM (requires an output path)

### Options reference

Positional arguments - a single trailing list, split by convention rather than as two separate
flags (shell globs work as usual, since the shell expands them before `dcmnorm` ever sees them):

- 0 or 1 path: `[INPUT]`, with OUTPUT defaulting to stdout JSON - e.g. `dcmnorm in.dcm`
- 2 paths: `[INPUT] [OUTPUT]` - e.g. `dcmnorm in.dcm out.png`
- 3+ paths, no `--mpr`: every path is an independent `[INPUT]`, each processed on its own (same
  as piping via `-I`/`--stdin-paths` - see [Batch mode](#batch-mode-with--i----stdin-paths-or-a-file-list) below) -
  e.g. `dcmnorm *.dcm --set SOPClassUID=... --overwrite`
- 2+ paths with `--mpr`: every path but the last is an `[INPUT]` slice combined into one volume,
  the last is `[OUTPUT]` - see [Render a Multiplanar Reformation (MPR)](#render-a-multiplanar-reformation-mpr)

General:

- `-h`, `--help`
- `-V`, `--version`
- `--list-transfer-syntaxes`
- `--check-dicom`
- `--check-dicom-logic` / `--strict`
- `--jpeg2000-codec <auto|openjpeg|kakadu>`
- `--verbose`
- `-I`, `--stdin-paths`
- `--filter <KEY>`
- `--overwrite`
- `--input-type <dicom|json>`
- `--output-type <dicom|json|raw|png|jpeg|mpeg4|texture>`

DICOMDIR (a command of its own, see [Write a DICOMDIR](#write-a-dicomdir)):

- `dcmnorm dicomdir <ROOT> [--file-set-id <ID>] [-o <PATH>] [--json]`

Registration (see [Register two volumes](#register-two-volumes)):

- `--register` (the paths are `FIXED MOVING [OUTPUT]`)
- `--register-metric <auto|ncc|mattes-mi>`
- `--register-translation-only`
- `--register-multi`
- `--register-time-limit-ms <MS>`
- `--register-levels <MM,...>`

DICOM editing:

- `--set <KEY=VALUE>`
- `--remove <KEY>`
- `--remove-private-tags`

JSON conversion:

- `--format <flat|standard>`
- `--keys <name|hex>`
- `--bulk-data <inline|uri>`
- `--bulk-data-source [<SOURCE>]`

DICOM transcoding:

- `--transfer-syntax <UID>`

Rendering:

- `--render-frame <N>`
- `--render-all-frames`
- `--render-fps <FPS>`
- `--no-modality-lut`
- `--no-voi-lut`
- `--no-icc-profile`
- `--window-center <FLOAT>`
- `--window-width <FLOAT>`
- `--jpeg-quality <1-100>`
- `--output-width <PIXELS>`
- `--output-height <PIXELS>`
- `--scale-max-size <PIXELS>`
- `--redact-box <X,Y,W,H>`
- `--redact-color <R,G,B|#RRGGBB>`
- `--pad`
- `--pad-color <R,G,B|#RRGGBB>`
- `--no-overlays`
- `--overlay-index <N>`
- `--overlay-color <R,G,B|#RRGGBB>`

MPR (Multiplanar Reformation):

- `--mpr <axial|coronal|sagittal|YAW,PITCH,ROLL>`
- `--mpr-origin <X,Y,Z>`
- `--mpr-depth <MM>`
- `--mpr-spacing <MM>`
- `--mpr-thickness <MM>`
- `--mpr-projection <mip|minip|average>`

Texture Export:

- `--texture-max-dim <N>`
- `--texture-compression <none|gzip>`

Histogram:

- `--histogram`
- `--histogram-bins <N>`
- `--histogram-frame <N>`
- `--histogram-min <FLOAT>`
- `--histogram-max <FLOAT>`

### JSON conversion defaults

DICOM to JSON defaults to:

- flattened JSON output
- named lookup keys where possible
- relative `BulkDataURI` bulk data output (`?offset=...&length=...`)
- `file://` `BulkDataURI` output when `--bulk-data-source` is passed without a value
- automatic `InlineBinary` fallback for bulk values of 32 bytes or less

JSON to DICOM defaults to:

- flattened JSON input
- optional `--bulk-data-source` when resolving `BulkDataURI`

### Runtime environment variables

- `DCMNORM_PERF` — enables scoped performance timing logs to stderr. Truthy values: `1`, `true`, `yes`, `on`.
- `DCMNORM_JPEG2000_CODEC` — classic JPEG 2000 decoder preference: `auto`, `openjpeg`, or `kakadu`. The CLI always sets this from `--jpeg2000-codec` (default `auto`). HTJ2K always uses OpenHTJ2K regardless.
- `DCMNORM_JPEG2000_THREADS` — threads per JPEG 2000 decode/encode call (OpenJPEG and OpenHTJ2K). Default: min(available CPUs, 8). See [JPEG 2000 codec selection](#jpeg-2000-codec-selection).
- `DCMNORM_HTJ2K_SIMD` — set to `base` to force the portable (non-AVX2) OpenHTJ2K build on an x86-64-v3 CPU. Diagnostic use only.
- `DCMNORM_JPEG2000_DEBUG` — enables JPEG 2000 debug logging when truthy. `--verbose` sets this to `1`.
- `LD_LIBRARY_PATH` — used to discover Kakadu shared libraries (`libkdu*.so`) at runtime.

(Build-time Kakadu variables are covered under [Kakadu FFI](building.md#kakadu-ffi-jpeg-2000).)

### Convert DICOM ⇄ JSON

Convert a DICOM file to flattened JSON using named keys:

```bash
dcmnorm test/files/dx.dcm
```

Convert a DICOM file to standard JSON with hex keys and write to a file:

```bash
dcmnorm test/files/dx.dcm out.json --format standard --keys hex
```

Convert JSON back to a DICOM file:

```bash
dcmnorm out.json out.dcm
```

Convert JSON with `BulkDataURI` references back to DICOM using a source file:

```bash
dcmnorm out.json out.dcm --bulk-data-source test/files/dx.dcm
```

`--format standard` adds non-standard `Keyword` and `VM` members to each top-level attribute. Pass
`--omit-keyword-vm` for output with only what PS3.18 Annex F defines (`vr` and the value). JSON
converts back to DICOM with or without them:

```bash
dcmnorm test/files/dx.dcm out.json --format standard --keys hex --omit-keyword-vm
```

### Filter attributes

Filter DICOM attributes before conversion (only filtered tags are parsed and emitted):

```bash
dcmnorm test/files/dx.dcm --filter StudyInstanceUID
```

Use multiple filters (repeat `--filter` or comma-separate values):

```bash
dcmnorm test/files/dx.dcm out.json --filter StudyInstanceUID,PatientID
```

`--filter` applies only to DICOM input. The parser reads until the requested attributes are
available, drops non-filtered attributes, and then continues with the normal conversion
pipeline (for example, DICOM to JSON output).

### Bulk data / `BulkDataURI`

By default, bulk data is emitted as relative `BulkDataURI` values (`?offset=...&length=...`)
when converting DICOM to JSON, and values of 32 bytes or less are automatically emitted as
`InlineBinary`.

To embed absolute `file://` URIs in `BulkDataURI`, pass `--bulk-data-source` without a value:

```bash
dcmnorm test/files/dx.dcm --bulk-data uri --bulk-data-source
```

A `BulkDataURI` points at the value's bytes in the file, in the byte order of the file's transfer
syntax, so they are big-endian for Explicit VR Big Endian. `InlineBinary` is always little-endian.
Converting JSON back to DICOM handles both, going by the document's `TransferSyntaxUID`, so keep
that attribute (for example in `--filter`) when `BulkDataURI`s of a big-endian file are to be read
back. Files whose values can't be addressed (deflated, or big-endian without a file meta group)
inline them.

### Validate files with `--check-dicom`

Checks for a Part 10 header first, then falls back to dataset parsing up to `SOPClassUID`
for streams without file meta.

Single file:

```bash
dcmnorm --check-dicom test/files/dx.dcm
```

Read paths from stdin (`-I` / `--stdin-paths`) and print only valid DICOM paths:

```bash
find . -type f | dcmnorm -I --check-dicom
```

Behavior:

- prints only successful (valid DICOM) paths to stdout
- suppresses per-file failure messages
- returns exit code `0` when all inputs are valid
- returns exit code `1` if any input is invalid, unreadable, or not a regular file

### Validate DICOM semantics with `--check-dicom-logic`

Checks that a *parseable* DICOM file's metadata is internally consistent - as opposed to
`--check-dicom`, which only checks that the bytes parse as DICOM at all. Catches the kind of
mismatch that will break downstream decompress/render/routing but doesn't stop the file from
parsing: `BitsStored`/`HighBit`/`BitsAllocated` relationships, `SamplesPerPixel` vs
`PhotometricInterpretation`, native `PixelData` length vs `Rows`/`Columns`/`NumberOfFrames`,
an encapsulated frame's first bytes not matching what its transfer syntax claims (e.g. a file
labeled JPEG 2000 whose fragment doesn't start with a JPEG 2000 marker), malformed or
mismatched UIDs, and (checked but non-fatal by default) suspect geometry/window/CT-rescale
metadata. No pixel codec ever runs, so it stays cheap enough for a full-archive batch scan.

```bash
dcmnorm --check-dicom-logic --verbose test/files/dx.dcm
```

Add `--output-type json` for a structured report (NDJSON, one object per line, when combined
with `-I`/`--stdin-paths`) instead of the one-line-per-file text summary:

```bash
dcmnorm --check-dicom-logic --output-type json test/files/dx.dcm
```

Behavior:

- prints `OK <path>` / `WARN <path>: N warning(s)` / `FAIL <path>: N error(s), M warning(s)`
  per file; `--verbose` also prints each finding's rule ID and message
- findings are `error` (will plausibly break decompress/render/routing) or `warning`
  (suspicious/non-conformant but usually survivable) severity
- returns exit code `0` unless any file has an `error`-level finding (or fails to parse);
  `--strict` also fails on `warning`-only files
- supports `-I`/`--stdin-paths` for batch scans, same as `--check-dicom`

### Compute a pixel histogram with `--histogram`

Computes a value histogram per frame (Hounsfield units for CT, the rescaled physical value
otherwise - the same modality-LUT-applied values rendering and `--render-frame` use) and prints
it as JSON to `OUTPUT` or stdout, instead of the default DICOM/JSON conversion. Bin range defaults
to each frame's own observed min/max; `--histogram-min`/`--histogram-max` (must be given together)
pin it to a fixed range instead, e.g. to compare several frames or instances on the same axis.

For an RGB/color frame (`SamplesPerPixel` 3 - YBR variants are converted to RGB first, same as 2D
rendering), each frame produces **three** entries instead of one - one per `"red"`/`"green"`/
`"blue"` `channel` - each over that channel's own decoded 8-bit (0-255) samples, defaulting the
bin range to the full 0-255 domain (not each channel's own min/max) so all three stay directly
comparable. A grayscale frame's entry has `channel: null`.

Every frame in the instance:

```bash
dcmnorm --histogram test/files/ct.dcm
```

One frame, with a custom bin count, written to a file:

```bash
dcmnorm --histogram --histogram-frame 0 --histogram-bins 64 test/files/ct.dcm histogram.json
```

Output shape (`{"frames": [...]}`, one entry per computed frame - or three, for an RGB/color
frame; `counts` has `binCount` entries):

```json
{
  "frames": [
    {
      "frameIndex": 0,
      "channel": null,
      "binCount": 8,
      "rangeMin": -1000.0,
      "rangeMax": 2540.0,
      "binWidth": 442.5,
      "counts": [130793, 6601, 116719, 7134, 883, 4, 3, 7],
      "pixelCount": 262144,
      "minValue": -1000.0,
      "maxValue": 2540.0,
      "mean": -469.71,
      "stdDev": 530.11
    }
  ]
}
```

### Override type detection with `--input-type` / `--output-type`

Useful for files with no extension or a misleading one. Supported `--output-type` values are
`dicom`, `json`, `raw`, `png`, `jpeg`, `mpeg4`, `texture`.

```bash
# Convert a DICOM file with no extension to JSON
dcmnorm dicom_data --input-type dicom

# Write DICOM output without an extension
dcmnorm input.json output --output-type dicom

# Render a DICOM file to an arbitrary extension as PNG
dcmnorm test/files/dx.dcm frame.img --output-type png

# Render a DICOM file as MPEG4 without a recognized extension
dcmnorm test/files/ct.dcm output.video --output-type mpeg4 --render-fps 24
```

### Edit DICOM elements with `--set`

Set one or more DICOM element values while converting by repeating `--set KEY=VALUE`. `KEY`
can be a DICOM keyword (for example, `SOPClassUID`) or a tag expression (for example,
`(0008,0016)`):

```bash
dcmnorm test/files/dx.dcm out.dcm --transfer-syntax 1.2.840.10008.1.2.1 --set SOPClassUID=1.2.840.10008.5.1.4.1.1.2 --set StudyDescription=Normalized
```

Use `--overwrite` to write DICOM output back to the input path — useful for in-place edits:

```bash
dcmnorm test/files/dx.dcm --set SOPClassUID=1.2.840.10008.5.1.4.1.1.2 --overwrite
```

### Render frames

Render the first frame of a DICOM file to PNG:

```bash
dcmnorm test/files/dx.dcm out.png
```

Render frame 2 to JPEG with explicit quality:

```bash
dcmnorm test/files/ct.dcm out.jpg --render-frame 1 --jpeg-quality 95
```

Render to raw 8-bit frame bytes:

```bash
dcmnorm test/files/dx.dcm out.raw
```

Render all frames from a multiframe dataset to numbered PNG files (`out_000001.png`, `out_000002.png`, ...):

```bash
dcmnorm test/files/ct.dcm out.png --render-all-frames
```

Render all frames from a multiframe dataset to a single `.mp4` video:

```bash
dcmnorm test/files/ct.dcm out.mp4 --render-fps 24
```

If `--render-fps` is omitted for `.mp4` output, `dcmnorm` uses frame-rate metadata from the
DICOM instance when available (`RecommendedDisplayFrameRate`, `CineRate`, `FrameTime`, or
`FrameTimeVector`) and falls back to 24 FPS otherwise. `.mp4` output requires `ffmpeg`
installed and available on `PATH`.

Rendering supports 1-bit, 8-bit, and 16-bit monochrome pixel data, as well as RGB data. The
render pipeline includes decompression when needed and applies modality LUT and VOI
LUT/windowing by default. Use `--no-modality-lut` and/or `--no-voi-lut` to disable those
steps, and `--window-center` / `--window-width` to override VOI windowing.

Photometric interpretations supported by rendering:

- `MONOCHROME1`
- `MONOCHROME2`
- `PALETTE COLOR`
- `RGB`

Both planar configurations are supported for RGB rendering (`PlanarConfiguration` 0 and 1).

### Export a GPU texture (`.gputex`)

`--output-type texture` (or a `.gputex` output extension) packs a frame or volume as a lossless,
GPU-upload-ready payload instead of an 8-bit windowed render: the raw `int16`/`uint16` sample
lattice (row-major, little-endian, gzip-compressed by default), plus a `rescaleSlope`/
`rescaleIntercept` pair for recovering physical values (e.g. HU). This lets a client do its own
window/level and oblique reslicing in a GPU shader instead of round-tripping to the server per
interaction - see `dicom_io::texture_export`'s module doc for the full rationale.

Every export writes two files: `OUTPUT` (the payload bytes) and `OUTPUT.json` (a `TextureMeta`
sidecar - dimensions, physical spacing/origin/orientation, rescale slope/intercept, a default
window/level, and whether/how the payload is compressed). Always read `compression` from the
sidecar rather than assuming gzip - `--texture-compression none` disables it per export.

Export a single frame as a depth-1 texture:

```bash
dcmnorm test/files/dx.dcm frame.gputex
```

Export a specific frame with an explicit default window, capping the longest axis at 1024 samples:

```bash
dcmnorm test/files/ct.dcm frame.gputex --render-frame 1 --window-center 40 --window-width 400 --texture-max-dim 1024
```

Export a whole CT/MR series as one volume texture (its own native voxel lattice, not a reformatted
plane - `--mpr`'s plane/depth/spacing flags don't apply here, only the volume-building/plane-value
is needed to select `--mpr`'s multi-file mode):

```bash
dcmnorm --mpr axial series_dir/*.dcm volume.gputex
```

Skip gzip compression (e.g. when the transport already compresses, such as a websocket permessage-deflate connection):

```bash
dcmnorm --mpr axial series_dir/*.dcm volume.gputex --texture-compression none
```

A third texture kind, `ContentKind::FrameStack` (`"framestack"` in the sidecar), packs several
independent original frames — a cine instance's own frames, or one file per instance in a
non-MPR-eligible multi-image series — as one texture-array upload with no resampling and no
physical geometry (`rowSpacing`/`origin`/etc. are meaningless for this kind). It has no CLI
invocation today; it's exposed only via each binding's own `exportFrameStackTexture`/
`export_frame_stack_texture` — see [`bindings/node`](../bindings/node/)'s,
[`bindings/python`](../bindings/python/)'s, or [`bindings/java`](../bindings/java/)'s own README.

### Render a Multiplanar Reformation (MPR)

MPR builds one 3D volume from multiple DICOM slice files (sharing a parallel stack - e.g. one
CT/MR series) and reformats it into either one 2D plane or a STACK of slices, honoring each
slice's real `ImagePositionPatient`/`ImageOrientationPatient`/`PixelSpacing`/`SliceThickness`
rather than just stacking images. Depending on `OUTPUT`'s extension, the result is a rendered
2D image (or a numbered series of them), a proper multi-instance DICOM series, or a single
whole-volume NIfTI/NRRD file - see [Reformatting a STACK of
slices](#reformatting-a-stack-of-slices-----mpr-depth-ranges) below. This is the same
`dicom_io::volume` code the Node bindings use for the interactive viewer, exposed directly for
scripting/testing. The whole file set is always read and combined within this single `dcmnorm`
process - one `build_volume` call sees every slice given - so a series can never end up silently
split across separate volumes.

`--mpr` supplies its input files the same way as any other multi-file operation (see
[Batch mode](#batch-mode-with--i----stdin-paths-or-a-file-list) above) - directly as positional
arguments (shell globs work as usual) or piped via `-I`/`--stdin-paths`. Either way, `--mpr` is
what says "combine these into one volume, with the last path as OUTPUT" instead of the default
"process each independently" - its value is either a canonical view or an oblique rotation, not
both combined:

```bash
dcmnorm --mpr axial series_dir/*.dcm out.png
find series_dir -name "*.dcm" | dcmnorm -I --mpr axial out.png
```

The three canonical, patient-anatomy-aligned views:

```bash
dcmnorm --mpr coronal series_dir/*.dcm coronal.png
dcmnorm --mpr sagittal series_dir/*.dcm sagittal.png
```

An arbitrary oblique camera angle - `YAW,PITCH,ROLL` (degrees, about the patient's Z/X/Y axes
respectively), applied to the volume's own native acquisition basis:

```bash
dcmnorm --mpr 15,30,0 series_dir/*.dcm oblique.png
```

`--mpr-origin X,Y,Z` (patient/LPS millimeters) recenters the reformat; it defaults to the volume's
own physical center. For the common case of stepping along the CURRENT view's own depth axis,
`--mpr-depth MM` is more convenient than recomputing a 3D point - it offsets `--mpr-origin` (or
the default center) along the resolved plane's own normal:

```bash
dcmnorm --mpr coronal --mpr-depth -15 series_dir/*.dcm coronal-anterior.png
```

By default MPR reformats an infinitely-thin plane (one voxel thick). `--mpr-thickness MM`
reformats a thick slab instead - multiple depths spanning that many millimeters, centered on the
plane, combined per `--mpr-projection` (`mip`, the default - maximum intensity projection, the
radiology-standard way to make a thin vessel or bright structure visible across a slab even if it
only crosses the exact center plane at one point; `minip` - minimum intensity projection; or
`average`):

```bash
dcmnorm --mpr coronal --mpr-thickness 20 --mpr-projection mip series_dir/*.dcm coronal-mip.png
```

`--mpr-spacing MM` sets the physical size of one output pixel (the same in both axes, so the
reformat is never distorted even when slice spacing differs from in-plane pixel spacing); it
defaults to the volume's own smallest voxel dimension. `--output-width`/`--output-height` (shared
with normal rendering) size the output image, defaulting to the volume's own row/column count.
`--window-center`/`--window-width` apply VOI windowing exactly as they do for a normal 2D render
(for `.png`/`.jpg`/`.dcm` output - see below, VOI windowing never applies to `.nii`/`.nii.gz`/
`.nrrd`, which always carry true rescaled values).

`--mpr-origin`/`--mpr-depth`/`--mpr-spacing`/`--mpr-thickness`/`--mpr-projection` all require
`--mpr` itself (they carry no "which plane" information on their own, so using them without it is
a clear error rather than a silently-ignored flag); `--mpr-projection` additionally requires a
positive `--mpr-thickness`. MPR mode is incompatible with `--filter`/`--transfer-syntax`/`--set`/
`--remove`/`--render-all-frames`/`--render-fps`/`--scale-max-size`, and fails with a clear error
(rather than a silently wrong reformat) if the given files don't share a consistent
`ImageOrientationPatient` - e.g. an accidentally mixed set of series. A gantry-tilted stack
(parallel slices whose origins drift within the slice plane) is fine: it is desheared onto an
orthogonal grid. One Enhanced CT/MR/PET multi-frame file is a whole volume on its own.

#### Reformatting a STACK of slices - `--mpr-depth` ranges

A bare `--mpr-depth MM` (or omitting it, default `0`) offsets a single plane, as above - exactly
one output slice. `--mpr-depth` also accepts a RANGE, which switches the whole `--mpr` invocation
from "one reformatted plane" to "a stack of slices spanning that depth range":

```bash
--mpr-depth START:END          # e.g. -20:20
--mpr-depth START:END:STEP     # explicit step, e.g. -20:20:2
--mpr-depth all                # the volume's own full extent along the plane's normal
--mpr-depth all:STEP           # full extent, explicit step
```

The step defaults to `--mpr-thickness` (contiguous, non-overlapping slabs - the natural default
when you're already asking for a thick reformat) if it's set, else `--mpr-spacing`. `all` computes
the volume's own physical extent along the resolved plane's normal by projecting its bounding box
onto that normal - correct even for an oblique (rotated) plane whose normal isn't the volume's own
acquisition axis.

What a multi-slice stack produces depends entirely on `OUTPUT`'s extension:

- **`.png`/`.jpg`/`.jpeg`**: one numbered file per slice - `OUTPUT_000001.png`,
  `OUTPUT_000002.png`, ... (the same `{stem}_{NNNNNN}.{ext}` convention `--render-all-frames`
  already uses) - each one an independent, VOI-windowed, 8-bit render exactly like a single-plane
  `--mpr` output.
- **`.dcm`/`.dicom`**: one numbered, spatially-valid DICOM file per slice (see below) sharing a
  single `SeriesInstanceUID`, so any PACS/viewer groups them as one loadable series.
- **`.nii`/`.nii.gz`/`.nrrd`**: the entire stack as ONE whole-volume file (see below).

A single depth (the default, or an explicit non-range `--mpr-depth MM`) still writes exactly one
file at `OUTPUT` for every extension, including `.dcm`/`.nii`/`.nrrd`.

```bash
# 41 coronal slices, 2mm apart, as a numbered PNG stack:
dcmnorm --mpr coronal --mpr-depth -40:40:2 series_dir/*.dcm coronal.png

# The same slices as a proper multi-instance DICOM series:
dcmnorm --mpr coronal --mpr-depth -40:40:2 series_dir/*.dcm coronal.dcm

# The whole volume, reformatted to a 1mm-isotropic coronal-oriented NIfTI:
dcmnorm --mpr coronal --mpr-depth all --mpr-spacing 1 series_dir/*.dcm coronal.nii.gz
```

`--output-type` is not valid with `.nii`/`.nii.gz`/`.nrrd`/`.dcm` output - the format is
determined entirely by `OUTPUT`'s extension for these.

#### Whole-volume export - NIfTI and NRRD

`.nii`/`.nii.gz` (gzip-compressed) and `.nrrd` write the ENTIRE reformatted stack as a single
volumetric file - float32 voxels carrying true rescaled values (e.g. Hounsfield units for CT),
never VOI-windowed or 8-bit-encoded, so the export is suitable for real volumetric analysis (3D
Slicer, ITK-SNAP, FSL, etc.), not just visual inspection. Neither format is available via an
existing well-maintained Rust crate, so both are written directly by `dcmnorm`:

- **NIfTI-1** (`.nii`/`.nii.gz`) uses `sform` for orientation. NIfTI's coordinate convention is
  RAS+ (Right/Anterior/Superior), while DICOM (and `dcmnorm`'s own geometry) is LPS+
  (Left/Posterior/Superior) - every direction vector and the origin get their X/Y components
  negated on export, the same convention `dcm2niix`/`nibabel` use for DICOM-derived NIfTI files.
- **NRRD** (`.nrrd`) names its space directly (`space: left-posterior-superior`), so no axis flip
  is needed - arguably the more direct/less error-prone choice for a DICOM-derived export.

#### Multi-file DICOM series output

`.dcm`/`.dicom` output wraps each reformatted slice in a standalone, spatially-valid DICOM object
using **Multi-frame Grayscale Word Secondary Capture Image Storage**
(`1.2.840.10008.5.1.4.1.1.7.3`, `NumberOfFrames=1` per file) - not plain "Secondary Capture Image
Storage", which is 8-bit-only per its IOD and can't carry signed 16-bit rescaled values. Each
slice's `ImagePositionPatient`/`ImageOrientationPatient`/`PixelSpacing`/`SliceThickness` reflect
its actual reformatted geometry (not copied from the source series), so the output is a genuinely
reconstructable spatial series, not just a flat picture with DICOM headers bolted on. `PixelData`
is stored as signed 16-bit with `RescaleSlope=1`/`RescaleIntercept=0` - the stored value IS the
physical value - so any DICOM viewer can window it however it likes, rather than getting a
pre-baked 8-bit render. Patient/study-identifying attributes (`PatientName`, `PatientID`,
`StudyInstanceUID`, `Modality`, etc.) are copied best-effort from the first input file, so the
derived series stays associated with its source study; `SeriesDescription` is fixed to
`"MPR Reformat"` and `SeriesNumber` to `9901`, deliberately unlikely to collide with a real
acquired series. `SeriesInstanceUID` is generated once per `--mpr` invocation and shared across
every file in the output stack; `SOPInstanceUID`/`StudyInstanceUID` (when not copied from the
source) use the UUID-derived DICOM UID scheme (PS3.5 Annex B, `2.25.<uuid>`), needing no
registered organization root.

#### Overlay planes

DICOM overlay planes (group `60xx`, up to 16 per instance: `6000,eeee` .. `601E,eeee`) are
composited onto the rendered image. Both encodings defined by the standard are supported:

- **Distinct `OverlayData`** (`60xx,3000`, current standard): `OverlayBitsAllocated`=1, a
  separately-stored 1-bit-per-pixel bitmap, packed LSB-first.
- **Embedded in `PixelData`** (legacy CR/DX): `OverlayBitsAllocated` equals the image's own
  `BitsAllocated`, and `OverlayBitPosition` names a specific high bit unused by `BitsStored`.

If an instance has one or more overlays, the first available overlay (ascending by DICOM group)
renders by default:

```bash
dcmnorm test/files/overlay.dcm out.png
```

Select a different overlay by its 0-based index (ordinal among the overlays present, not the raw
DICOM group), or disable overlay rendering entirely:

```bash
dcmnorm test/files/overlay_multi.dcm out.png --overlay-index 1
dcmnorm test/files/overlay.dcm out.png --no-overlays
```

Overlay pixels render in a fill color, `R,G,B` (0-255 each) or `#RRGGBB` hex, defaulting to
green (`0,255,0`):

```bash
dcmnorm test/files/overlay.dcm out.png --overlay-color 255,0,0
```

`--overlay-index`/`--overlay-color` require overlays to be enabled (they conflict with
`--no-overlays`), and an `--overlay-index` beyond the number of overlays present is an error
rather than being silently clamped.

Three small synthetic (no PHI, not derived from any real study) fixtures exercise overlay
rendering: `test/files/overlay.dcm` (one overlay, distinct `OverlayData`), `test/files/
overlay_multi.dcm` (two overlays, distinct `OverlayData`), and `test/files/overlay_embedded.dcm`
(one overlay, legacy embedded-in-`PixelData` encoding).

Use `--verbose` to print render/conversion diagnostics — without it, external tool output
such as `ffmpeg` is suppressed unless an error occurs. For stage-by-stage performance timing,
set `DCMNORM_PERF=1` (or `true`/`yes`/`on`):

```bash
DCMNORM_PERF=1 dcmnorm test/files/mr.dcm out.jpg --output-width 920 --output-height 758

# or with an explicit render format for a file without a recognized extension
DCMNORM_PERF=1 dcmnorm test/files/mr.dcm output.img --output-type jpeg --output-width 920 --output-height 758
```

### Batch mode with `-I` / `--stdin-paths` or a file list

`dcmnorm` has two interchangeable ways to run the same options across multiple input files
instead of just one - pick whichever fits how the file list is produced. Both apply the same
options to every path; errors for individual files are printed to stderr with the filename, and
`dcmnorm` exits non-zero if any file fails. (`--mpr` repurposes the same file list to mean
something different - combine every file into one volume instead of processing each
independently - see [Render a Multiplanar Reformation (MPR)](#render-a-multiplanar-reformation-mpr).)

Give 3 or more files directly as positional arguments - no special flag needed, shell globs work
as usual since the shell expands them into separate arguments before `dcmnorm` ever sees them:

```bash
dcmnorm *.dcm
```

(2 or fewer positional arguments are always `[INPUT] [OUTPUT]`, per the single-file convention
above - batch mode only kicks in at 3+, since that shape was otherwise always a "too many
arguments" error.)

Or pipe input paths from stdin, one path per line, via `-I`/`--stdin-paths` - best for a file list
produced by another command (`find`, a database query, ...) or too large for a shell command line:

```bash
find . -name "*.dcm" | dcmnorm -I
```

`--set` also applies in batch mode, and combines with `--overwrite` to update each file in place:

```bash
find . -name "*.dcm" | dcmnorm -I --set SOPClassUID=1.2.840.10008.5.1.4.1.1.2
find . -name "*.dcm" | dcmnorm -I --set SOPClassUID=1.2.840.10008.5.1.4.1.1.2 --overwrite
```

To emit `file://` `BulkDataURI` values in batch mode, also pass `--bulk-data-source` without a value:

```bash
find . -name "*.dcm" | dcmnorm -I --bulk-data uri --bulk-data-source
```

### Transcode and inspect transfer syntaxes

Transcode a DICOM file to Explicit VR Big Endian:

```bash
dcmnorm test/files/dx.dcm out.dcm --transfer-syntax 1.2.840.10008.1.2.2
```

List the transfer syntaxes known to the current build and whether dataset read/write and
pixel decode/encode are available:

```bash
dcmnorm --list-transfer-syntaxes
```

Transfer-syntax support is build-specific. The default build in this repository enables the
JPEG-LS, JPEG XL and JPEG 2000 encode codec features in addition to the DICOM library support that
is available without extra native imaging libraries:

- native uncompressed syntaxes
- deflated dataset syntaxes
- encapsulated uncompressed pixel data
- MPEG-2, MPEG-4 AVC/H.264 and HEVC decode/encode through the `ffmpeg` command on `PATH` (see
  [Video transfer syntaxes](#video-transfer-syntaxes))
- JPEG baseline decode/encode
- JPEG extended and JPEG lossless decode-only
- JPEG-LS transfer syntax support via CharLS-backed build integration
- JPEG 2000 decode, plus encode to `.90`/`.91` (OpenJPEG)
- High-Throughput JPEG 2000 (HTJ2K) decode for `.201`-`.203`, plus lossless encode to `.201`/`.203` (OpenHTJ2K)
- JPEG XL decode for `.110`-`.112` (jxl-oxide), plus lossless encode to `.110` (zune-jpegxl,
  pure Rust; unsigned 8/16-bit samples, 1 or 3 samples per pixel, at least 2x2 pixels)
- RLE lossless decode-only

Transfer syntaxes which the current build cannot encode or decode are reported explicitly by
`--list-transfer-syntaxes` and by transcoding errors.

### Video transfer syntaxes

MPEG-2 (`.100`/`.101`), MPEG-4 AVC/H.264 (`.102`-`.106`) and HEVC/H.265 (`.107`/`.108`) pixel
data is decoded and encoded by the `ffmpeg` command on `PATH`: the elementary stream is piped
through it, so nothing FFmpeg-related is compiled into `dcmnorm` or `dcmtalk`. Install FFmpeg to
use them (`apt install ffmpeg`, `brew install ffmpeg`, `winget install ffmpeg`).
`--list-transfer-syntaxes` shows `ffmpeg` as their engine when it is found and `no` when it is
not, and a transcode that needs it fails with an error saying how to install it. Every other
transfer syntax works without FFmpeg.

Encoding uses the installed FFmpeg's default encoder for each format: `mpeg2video` for MPEG-2,
and libx264 / libx265 for H.264 / HEVC, which most packaged FFmpeg builds include. `.mp4`/`.mov`
movie output also uses the `ffmpeg` command (with libx264), see [Render frames](#render-frames).

To link FFmpeg in instead (in-process, no `ffmpeg` command needed at run time), build with
`--features dcmnorm/ffmpeg-codec`, which compiles FFmpeg from source, or
`--features dcmnorm/ffmpeg-codec-linked` against an FFmpeg the build machine provides. The
bindings do this; `--list-transfer-syntaxes` then shows `builtin`. The linked FFmpeg has only
FFmpeg's own codecs, so it decodes all of these but encodes only MPEG-2 (H.264 and HEVC encoding
need libx264 / libx265, which the `ffmpeg` command usually has).

### JPEG 2000 codec selection

| Transfer syntaxes | Decode | Encode |
|---|---|---|
| Classic JPEG 2000 (`.90`-`.93`) | OpenJPEG (or Kakadu, see below) | OpenJPEG (`.90` lossless, `.91` lossy 10:1) |
| HTJ2K (`.201`-`.203`) | OpenHTJ2K, with OpenJPEG as an automatic fallback | OpenHTJ2K, lossless (`.201`, `.203`); `.202` is decode-only |

Both libraries are linked statically through the `crates/dcmnorm-jpeg2000` crate, which vendors
OpenHTJ2K. Unlike OpenJPEG, OpenHTJ2K decodes HTJ2K that uses multiple quality layers. On
x86-64 Linux it is compiled twice, portable and AVX2 (`x86-64-v3`), and the faster variant is
picked at runtime from the CPU's features. `--verbose` prints the variant in use:
`classic=openjpeg htj2k=openhtj2k(x86-64-v3) threads=8`.

A single large image is decoded and encoded across several threads.
`DCMNORM_JPEG2000_THREADS` sets the per-call thread count; the default is min(CPUs, 8). Frames
of a multi-frame object that are already decoded in parallel use one OpenJPEG thread each.

Kakadu is an optional override for **classic** JPEG 2000 only.
- It requires the `kakadu-ffi` build feature (see [Kakadu FFI](building.md#kakadu-ffi-jpeg-2000)).
  Kakadu use is FFI-only (Rust → C++ interop), not CLI-based.
- At runtime `dcmnorm` looks for its libraries (`libkdu*.so`) on `LD_LIBRARY_PATH`.
- `--jpeg2000-codec` / `DCMNORM_JPEG2000_CODEC` select between `auto`, `openjpeg` and `kakadu`
  for classic codestreams.
- HTJ2K never goes to Kakadu: the licensed Kakadu 7.8 predates HTJ2K and can hang on it.

The evaluation behind these choices covers OpenJPEG, OpenHTJ2K, OpenJPH, Grok, Kakadu and several
Rust decoders, with correctness checks and benchmarks on full-field mammograms. See
[docs/jpeg2000-codec-evaluation.md](jpeg2000-codec-evaluation.md).

### Write a DICOMDIR

`dcmnorm dicomdir ROOT` writes the DICOMDIR of a File-set (DICOM media: CD, DVD, USB): a Basic
Directory (Media Storage Directory Storage, `1.2.840.10008.1.3.10`, Explicit VR Little Endian)
with PATIENT, STUDY and SERIES records and one record per file below them, like dcmtk's
`dcmmkdir`.

```bash
dcmnorm dicomdir /media/disc --file-set-id DISC_0001           # writes /media/disc/DICOMDIR
dcmnorm dicomdir /media/disc -o /tmp/DICOMDIR --json           # elsewhere; the report as JSON
```

- Every file under `ROOT` that starts with the `DICM` prefix is a member of the File-set; other
  files (a `README.TXT`, say) are skipped. Only headers are read, never pixel data.
- A file's path below `ROOT` is its File ID, so it must follow PS3.10 8.2: at most 8 levels, each
  name 1-8 characters of `A-Z`, `0-9` and `_`, no lower case and no extension (for example
  `DICOM/ST000001/SE000001/IM000001`). A DICOM file with any other name is an error.
- The record type of each file comes from its SOP Class (PS3.3 F.5): IMAGE, SR DOCUMENT, KEY
  OBJECT DOC, PRESENTATION, ENCAP DOC, WAVEFORM, RT DOSE/STRUCTURE SET/PLAN/TREAT RECORD,
  SPECTROSCOPY, REGISTRATION, MEASUREMENT, RADIOTHERAPY and the rest; a SOP Class with no record
  type that isn't an image is an error. Hanging protocols, color palettes, implant templates and
  inventories go in the root, beside the patients.
- Records carry the PS3.3 F.5 keys plus the STD-GEN-CD and STD-GEN-DVD additional keys (PS3.11
  Tables D.3-2 and H.3-2) when the instances have them. Records with non-ASCII text are written
  in UTF-8 (`ISO_IR 192`).
- Type 1 keys the instances lack (Patient ID, Study ID, Study Date/Time, Modality, Series
  Number, Instance Number, as IHE PDI's Table 4.47.4-1 lists) are synthesized, distinct per
  patient/study/series, and listed in the output. A Patient ID shared by two differently named
  patients, a study under two patients, a series in two studies, or a SOP Instance in two files
  is an error.

The bindings do the same in-process from a list of entries, each a File ID plus the file's header
as DICOM JSON or a path to read it from: Node's `writeDicomdir(entries, outputPath, options)`
(`{ fileId, json }` / `{ fileId, path }`, see `bindings/node/README.md`), Python's
`write_dicomdir(entries, output_path, file_set_id=..., sop_instance_uid=...)`
(`DicomdirEntry(file_id, json=...)` / `path=...`, or dicts; see `bindings/python/README.md`) and
Java's `Dcmnorm.writeDicomdir(entries, outputPath, options)` (`DicomdirEntry.fromJson` /
`fromFile`, see `bindings/java/README.md`).

### Register two volumes

`dcmnorm --register FIXED MOVING [OUTPUT]` aligns the MOVING series to the FIXED one with a rigid
(6 degrees of freedom) registration. Each of FIXED and MOVING is a directory holding one series'
files, a single Enhanced CT/MR/PET file, or a comma-separated list of files. Without OUTPUT the
result is printed as JSON. An OUTPUT ending in `.json` receives that JSON; any other OUTPUT is
written as a DICOM Spatial Registration object and the JSON still goes to stdout
(`--output-type json|dicom` overrides the extension).

```bash
dcmnorm --register current/ prior/                                 # the result as JSON
dcmnorm --register current/ prior/ result.json                     # the result in a file
dcmnorm --register ct/ pet/ pet-to-ct.dcm                          # also write a DICOM REG
dcmnorm --register current/ prior/ --register-translation-only     # the fast estimate only
dcmnorm --register current/ prior/ --register-multi                # several starts, the best kept
```

Before 0.18 this was `dcmnorm register FIXED MOVING [--reg PATH]` with the options unprefixed
(`--metric`, `--multi`, ...). That form still works and prints a deprecation note.

- `matrix` is a row-major 4x4 that maps a point in the MOVING patient coordinates (LPS mm) to the
  FIXED ones. This is the direction a DICOM Spatial Registration item stores, with the item's
  Frame of Reference the moving one and the REG's own the fixed one.
- Any pairing of CT, MR and PT works. The metric is NCC when both series have the same modality
  and Mattes mutual information otherwise (`--register-metric` overrides it). An MR-MR pair that NCC can't
  align (a T1 against a T2) is retried with mutual information.
- Both volumes are resampled onto an 8, 4 and 2 mm pyramid, so anisotropic voxels, different
  voxel sizes and different fields of view are fine. Body masks depend on the modality: a CT mask
  drops the table, and MR and PT masks need no HU or SUV values. Intensities are normalized by
  percentile, so MR scaling and PET counts versus SUV don't matter.
- The result carries `metric`, `metricValue`, `normalizedMetric` (the NCC correlation, or MI's
  entropy correlation coefficient), `overlapFraction`, `quality` and `qualityLabel`
  (`good`/`fair`/`poor`), the per-level iterations, `initialization`, `stopReason` and `timingMs`.
- `quality` comes from `bodyMetric`: the metric over only the fixed samples that land inside the
  moving body, so anatomy one scan never saw (a smaller reconstruction field of view, a cropped
  end) does not count against a correct result, while a misalignment still compares anatomy that
  doesn't match. It is scaled down when the moving body covers under 30% of the fixed one
  (`bodyOverlapFraction`) or a transform 2 voxels away scores better (not converged). The scale
  is calibrated per modality on public CT, PT and MR pairs (`benchmarks/registration`): correct
  results score `good`, divergent ones `poor`. A rotation above 30 degrees (`rotationAngleDeg`)
  is always `poor` with `plausible: false`; a current and a prior on a table are not rotated that
  much.
- `--register-multi` tries several starts, the translation estimate, the body-mask centroids and the
  scanner coordinates (plus the initial matrix and extra `starts` in the bindings), each through
  the coarse levels, refines the best plausible one, and reports `consistent` (another start
  converged to within 3 mm of it) and every start under `starts`. The working images are built
  once for all of them, so it costs about 1.5 times one start. It recovers from seeds 120-180 mm
  off in S-I, 60 mm off in-plane or upside down, where a single start ends in a wrong minimum.
- The result is deterministic: the same inputs give the same matrix bit for bit, whatever the
  thread count. `--register-time-limit-ms` (default 30000) returns the best result so far when it runs out.
- `--register-translation-only` runs just the fast estimate, which also seeds the rigid registration.
  The S-I shift comes from cross-correlating per-height profiles (body area, plus lung and bone
  area for CT and integrated intensity for MR/PT) over the in-plane region both scans cover, so a
  smaller reconstruction field of view doesn't skew them. A shift with a short overlap is
  down-weighted, so a chance fit at the ends can't beat a long overlap, and the smallest overlap
  allowed is 40% of the shorter scan (20 to 60 mm), so short MR slabs still match. The in-plane
  shift comes from a masked correlation of the two scans' axial projections over the matched
  overlap, which truncation doesn't bias the way body centroids are biased. `confidence` combines
  the peak, its margin over the next one, whether each profile on its own agrees, and the
  in-plane fit. It is calibrated so that no wrong answer on the benchmark scores 0.3 or more.
- A DICOM OUTPUT is written as a Spatial Registration object
  (`1.2.840.10008.5.1.4.1.1.66.1`) in the fixed study. The fixed and moving series and instances
  are referenced, and the patient and study modules are copied from the first fixed file.
  `dcmnorm PATH` prints any REG object as JSON.

The bindings expose the same functions: Node's `registerVolumes`, `estimateTranslation`,
`readSpatialRegistration` and `writeSpatialRegistration` (see `bindings/node/README.md`), and
`dcmnorm::dicom_io::{register_rigid, estimate_translation, read_spatial_registration,
build_spatial_registration, validate_spatial_registration}` in Rust.
