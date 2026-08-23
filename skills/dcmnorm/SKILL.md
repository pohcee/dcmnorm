---
name: dcmnorm
description: Read, inspect, edit, anonymize, convert, transcode, and render DICOM medical imaging files using the dcmnorm CLI, and send/receive/query them over a DICOM network using the dcmtalk CLI. Use this skill whenever the user mentions DICOM, .dcm files, medical images (CT/MR/US/DX/SR), DICOM tags/metadata (PatientName, StudyInstanceUID, transfer syntax, SOP class), DICOM JSON, de-identifying/redacting medical images, wants to view/export DICOM pixel data as PNG/JPEG/MP4, or wants to talk to a PACS/DICOM node — connectivity checks (C-ECHO), sending files (C-STORE), querying (C-FIND), retrieving (C-MOVE), or receiving files (a storage SCP) — even if they don't name the dcmnorm or dcmtalk tools. Also use it to check whether files are valid DICOM or to batch-process directories of medical images.
---

# dcmnorm — DICOM read / edit / write / render CLI

`dcmnorm` is an installed CLI that converts between DICOM and JSON, edits DICOM elements, transcodes transfer syntaxes, validates files, and renders pixel data to PNG/JPEG/raw/MPEG4. Verify it's available with `dcmnorm --version`; run `dcmnorm --help` for the full option list.

For talking to a DICOM network peer (PACS, modality, another node) instead of local files, use the companion `dcmtalk` CLI — see [Network operations with `dcmtalk`](#network-operations-with-dcmtalk) below.

The core invocation is:

```
dcmnorm [OPTIONS] [INPUT] [OUTPUT]
```

The operation is **inferred from the file extensions** of INPUT and OUTPUT:

| INPUT | OUTPUT | Operation |
|-------|--------|-----------|
| `.dcm` | *(none)* | Print DICOM as JSON to stdout |
| `.dcm` | `.json` | DICOM → JSON file |
| `.json` | `.dcm` | JSON → DICOM file |
| `.dcm` | `.dcm` | DICOM → DICOM (edit / transcode) |
| `.dcm` | `.png` / `.jpg` / `.raw` | Render a frame to an image |
| `.dcm` | `.mp4` / `.mov` | Render frames to MPEG4 video |
| `.dcm` (+ `--mpr`) | `.png` / `.dcm` / `.nii(.gz)` / `.nrrd` | Multiplanar reformation — see [MPR](#multiplanar-reformation-mpr) below |
| `.dcm` | `.gputex` | Lossless GPU texture export — see [GPU texture export](#gpu-texture-export-gputex) below |

When extensions are missing or misleading, override detection with `--input-type dicom|json` and `--output-type dicom|json|raw|png|jpeg|mpeg4|texture`.

## Reading / inspecting DICOM

Print the whole dataset as flat JSON to stdout (bulk data like PixelData becomes a `BulkDataURI` reference, so output stays small):

```bash
dcmnorm file.dcm
```

Read only specific elements — much faster on big files since only filtered elements are parsed. Keys are DICOM keywords or `GGGG,EEEE` tags, repeated or comma-separated:

```bash
dcmnorm --filter PatientName,Modality,StudyInstanceUID file.dcm
# → {"Modality":"CT","PatientName":"Anonymized","StudyInstanceUID":"1.2..."}
```

JSON shape options (defaults: `--format flat --keys name`):
- `--format standard` — PS3.18 DICOM JSON (`{"00080060":{"vr":"CS","Value":["CT"]}}`)
- `--keys hex` — hex tags instead of keywords
- `--bulk-data inline` — base64-embed binary values instead of URI references (large output; avoid unless a full self-contained JSON is required)

### Validating files

`--check-dicom` prints the path if the file is valid DICOM and exits non-zero otherwise:

```bash
dcmnorm --check-dicom file.dcm
```

Batch mode — pipe paths via `-I` / `--stdin-paths`; only valid DICOM paths are printed (great for finding DICOM files without relying on extensions):

```bash
find /data -type f | dcmnorm -I --check-dicom
```

`-I` also works for batch conversion/edit operations on many files.

## Editing / updating DICOM

Set or remove elements (keywords or `GGGG,EEEE` tags, repeatable):

```bash
dcmnorm --set "PatientName=DOE^JOHN" --set "PatientID=12345" in.dcm out.dcm
dcmnorm --remove PatientBirthDate --remove "0010,1000" in.dcm out.dcm
dcmnorm --remove-private-tags in.dcm out.dcm   # strip all private tags
```

Edit in place with `--overwrite` (no OUTPUT argument):

```bash
dcmnorm --overwrite --set PatientName=ANON file.dcm
```

Editing flags combine freely with each other and with conversion/transcoding in one pass. A typical de-identification:

```bash
dcmnorm --set PatientName=ANON --set PatientID=ANON \
        --remove PatientBirthDate --remove-private-tags in.dcm out.dcm
```

## Writing DICOM from JSON

JSON (flat or standard format, auto-detected) converts back to DICOM:

```bash
dcmnorm dataset.json out.dcm
```

For a lossless DICOM → JSON → DICOM round trip, export with a bulk-data source so binary values (PixelData etc.) can be resolved on the way back:

```bash
dcmnorm in.dcm dataset.json --bulk-data-source   # embeds file:// URIs pointing at in.dcm
# ...edit dataset.json...
dcmnorm dataset.json out.dcm                     # URIs resolved automatically
```

**Gotcha:** `--bulk-data-source` takes an *optional* value. If a positional path follows it, the flag swallows that path as its value. When using the no-value form, put the flag **after** INPUT/OUTPUT (as above). With a value (`--bulk-data-source /path/to/source.dcm`, used on JSON→DICOM to resolve relative URIs), position doesn't matter.

## Transcoding transfer syntaxes

```bash
dcmnorm --list-transfer-syntaxes                 # table of supported UIDs + decode/encode capability
dcmnorm --transfer-syntax 1.2.840.10008.1.2.1 in.dcm out.dcm
```

Common targets: `1.2.840.10008.1.2.1` Explicit VR Little Endian (uncompressed, maximally compatible), `1.2.840.10008.1.2.4.90` JPEG 2000 lossless, `1.2.840.10008.1.2.4.50` JPEG baseline. Check the PIXEL_ENCODE column in the list before targeting a compressed syntax. `--jpeg2000-codec openjpeg|kakadu` forces a specific JPEG 2000 decoder if needed.

## Visualizing / rendering

Render to PNG or JPEG (modality LUT, VOI windowing, and ICC profile applied automatically):

```bash
dcmnorm file.dcm out.png
dcmnorm --jpeg-quality 95 file.dcm out.jpg
```

Frame selection and video:

```bash
dcmnorm --render-frame 5 multi.dcm frame5.png    # zero-based index
dcmnorm --render-all-frames multi.dcm frame.png  # writes frame_000001.png, frame_000002.png, ...
dcmnorm --render-fps 30 cine.dcm loop.mp4        # fps defaults to DICOM metadata, else 24
```

Windowing and sizing:

```bash
dcmnorm --window-center 40 --window-width 400 ct.dcm soft-tissue.png
dcmnorm --scale-max-size 512 file.dcm thumb.png            # longer side = 512, aspect preserved
dcmnorm --output-width 256 file.dcm out.png                # height auto from aspect ratio
dcmnorm --pad --pad-color '#000000' file.dcm square.png    # pad to square canvas
```

`--no-voi-lut` / `--no-modality-lut` / `--no-icc-profile` disable those pipeline stages when raw-er values are wanted.

Burn redaction boxes over the pixels (e.g. to mask burned-in PHI). Coordinates are in output pixels; negative X/Y anchor from right/bottom; W/H accept pixels or `%`; repeatable:

```bash
dcmnorm --redact-box 10,10,200,40 --redact-box -110,-60,100,50 \
        --redact-color '#000000' us.dcm redacted.png
```

## Multiplanar Reformation (MPR)

`--mpr` combines multiple slice files from one parallel stack (e.g. a CT/MR/PT series) into a 3D volume — honoring real `ImagePositionPatient`/`ImageOrientationPatient`/spacing, not just stacking images — and reformats a plane or a whole stack of planes out of it. Give it every slice file as INPUT with the last path as OUTPUT (shell globs work as usual):

```bash
dcmnorm --mpr axial series_dir/*.dcm axial.png
dcmnorm --mpr coronal series_dir/*.dcm coronal.png
dcmnorm --mpr 15,30,0 series_dir/*.dcm oblique.png   # YAW,PITCH,ROLL degrees, oblique camera
```

Useful modifiers (all require `--mpr`): `--mpr-origin X,Y,Z` (mm, defaults to volume center), `--mpr-depth MM` (offset along the plane's own normal), `--mpr-spacing MM` (output pixel size), `--mpr-thickness MM` + `--mpr-projection mip|minip|average` (thick-slab projection instead of an infinitely-thin plane).

`--mpr-depth` also accepts a range (`START:END`, `START:END:STEP`, or `all`/`all:STEP`) to reformat a whole stack of slices instead of one plane — output extension decides the result shape: numbered PNGs, a proper multi-instance DICOM series (`.dcm`), or one whole-volume file (`.nii`/`.nii.gz`/`.nrrd`, float32 physical values, never 8-bit/windowed):

```bash
dcmnorm --mpr coronal --mpr-depth -40:40:2 series_dir/*.dcm coronal.dcm       # multi-instance DICOM series
dcmnorm --mpr coronal --mpr-depth all --mpr-spacing 1 series_dir/*.dcm coronal.nii.gz
```

MPR mode is incompatible with `--filter`/`--transfer-syntax`/`--set`/`--remove`/`--render-all-frames`/`--render-fps`/`--scale-max-size`, and errors clearly (rather than silently mis-rendering) on an inconsistent/gantry-tilted stack.

## GPU texture export (`.gputex`)

`--output-type texture` (or a `.gputex` output extension) packs a frame or volume as a lossless, GPU-upload-ready payload — the raw `int16`/`uint16` sample lattice plus a `rescaleSlope`/`rescaleIntercept` pair for recovering physical values — instead of an 8-bit windowed render. Every export writes `OUTPUT` (payload bytes) plus `OUTPUT.json` (a metadata sidecar: dimensions, physical geometry, rescale slope/intercept, default window/level, compression):

```bash
dcmnorm frame.dcm frame.gputex                                       # single frame, depth-1 texture
dcmnorm --mpr axial series_dir/*.dcm volume.gputex                   # whole series as one volume texture
dcmnorm --mpr axial series_dir/*.dcm volume.gputex --texture-compression none  # skip gzip
```

`--texture-max-dim <N>` caps the longest axis (proportional downsample), for both a single-frame export and a `--mpr` volume export. A frame-stack variant (independent frames packed as a texture array, e.g. a cine loop's frames or one file per series instance, no resampling/physical geometry) exists only via the Node bindings' `exportFrameStackTexture` — no CLI flag for it yet.

## Tips

- After creating or editing a file, verify with `dcmnorm --check-dicom out.dcm` and spot-check tags with `--filter`.
- To view a DICOM image yourself (e.g. to answer "what does this scan show?" or verify a redaction), render it to PNG and read the image file.
- CT images often need explicit windowing to be readable: brain ≈ C40/W80, soft tissue ≈ C40/W400, lung ≈ C-600/W1500, bone ≈ C300/W1500.
- `--verbose` prints conversion/rendering diagnostics when a command fails or produces unexpected output.
- Detection handles files missing the standard preamble/meta group; for files with wrong or missing extensions, force it with `--input-type dicom`.

## Network operations with `dcmtalk`

`dcmtalk` is an installed CLI for DICOM network (DIMSE) operations — the same ground as dcmtk's `echoscu`/`storescu`/`findscu`/`movescu`/`storescp`. Verify it's available with `dcmtalk --version`; run `dcmtalk --help` or `dcmtalk <subcommand> --help` for the full option list. Every subcommand takes a peer address as `HOST:PORT` and accepts `-v`/`--verbose` to log association negotiation, presentation contexts, and each DIMSE command/response to stderr — reach for `--verbose` first whenever a connection to a real PACS behaves unexpectedly.

Check connectivity to a peer (C-ECHO):

```bash
dcmtalk echoscu pacs.example.com:11112
```

Send DICOM file(s) or a whole directory (recursive) to a peer (C-STORE):

```bash
dcmtalk storescu pacs.example.com:11112 study.dcm
dcmtalk storescu pacs.example.com:11112 ./study_dir/
```

Query a peer's studies (C-FIND) — keys are DICOM keywords, `KEY=VALUE` to match or bare `KEY` as a return key; matches print as one DICOM JSON line per study:

```bash
dcmtalk findscu pacs.example.com:11112 -k PatientID=12345 -k StudyDate
```

Ask a peer to push a study to another AE title it already knows (C-MOVE):

```bash
dcmtalk movescu pacs.example.com:11112 MY_STORE_AE 1.2.840.113619.2.55.3.604688119.971.1600000000.123
```

Run a temporary receiver to capture what a peer sends (a storescp), e.g. to inspect what a modality/PACS is actually transmitting — instances land under `--cache-path` as `S_<StudyInstanceUID>/<Modality>_<SOPInstanceUID>.dcm`, ready for `dcmnorm` to inspect:

```bash
dcmtalk storescp 11112 --ae-title MY_STORE_AE --cache-path ./received --verbose
```

`dcmtalk storescp` is receive-only: it answers C-FIND/C-MOVE requests with "unable to process" rather than serving a real query index or retrieve queue.

Most `dcmtalk` peers require the calling/called AE titles to match what they've been configured to accept — set them with `-a`/`--calling-aet` and `-c`/`--called-aet` if the default calling AE title (`DCMTALK`) or an unset called AE title gets an association rejected.
