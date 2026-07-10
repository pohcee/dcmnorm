---
name: dcmnorm
description: Read, inspect, edit, anonymize, convert, transcode, and render DICOM medical imaging files using the dcmnorm CLI. Use this skill whenever the user mentions DICOM, .dcm files, medical images (CT/MR/US/DX/SR), DICOM tags/metadata (PatientName, StudyInstanceUID, transfer syntax, SOP class), DICOM JSON, de-identifying/redacting medical images, or wants to view/export DICOM pixel data as PNG/JPEG/MP4 — even if they don't name the dcmnorm tool. Also use it to check whether files are valid DICOM or to batch-process directories of medical images.
---

# dcmnorm — DICOM read / edit / write / render CLI

`dcmnorm` is an installed CLI that converts between DICOM and JSON, edits DICOM elements, transcodes transfer syntaxes, validates files, and renders pixel data to PNG/JPEG/raw/MPEG4. Verify it's available with `dcmnorm --version`; run `dcmnorm --help` for the full option list.

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

When extensions are missing or misleading, override detection with `--input-type dicom|json` and `--output-type dicom|json|raw|png|jpeg|mpeg4`.

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

## Tips

- After creating or editing a file, verify with `dcmnorm --check-dicom out.dcm` and spot-check tags with `--filter`.
- To view a DICOM image yourself (e.g. to answer "what does this scan show?" or verify a redaction), render it to PNG and read the image file.
- CT images often need explicit windowing to be readable: brain ≈ C40/W80, soft tissue ≈ C40/W400, lung ≈ C-600/W1500, bone ≈ C300/W1500.
- `--verbose` prints conversion/rendering diagnostics when a command fails or produces unexpected output.
- Detection handles files missing the standard preamble/meta group; for files with wrong or missing extensions, force it with `--input-type dicom`.
