# dcmnorm (Python binding)

Native Python bindings for dcmnorm, built with [PyO3](https://pyo3.rs) +
[maturin](https://www.maturin.rs). Calls straight into the `dcmnorm` lib crate in-process (no CLI
subprocess, no stdio/JSON round trip); blocking work releases the GIL (`py.allow_threads`) so it
doesn't stall other Python threads. This is the Python counterpart of `bindings/node` - same
underlying crate, same feature set, adapted to Python conventions (synchronous/blocking calls
instead of Promises, snake_case keyword arguments instead of camelCase options objects).

## Install

```sh
pip install dcmnorm        # or: uv add dcmnorm / uv tool install dcmnorm / pipx install dcmnorm
```

```python
import dcmnorm
print(dcmnorm.read_json("image.dcm"))
```

The `dcmnorm` package on PyPI is both this binding and the `dcmnorm`/`dcmtalk` command-line tools:
one wheel per platform (Linux x86_64 and aarch64 with glibc 2.28+, macOS arm64 11+ and x86_64
10.15+, Windows x64) holds the `import dcmnorm` extension module for CPython 3.9+ (stable ABI) and
both CLIs, which installers put on `PATH`. FFmpeg is linked into the module, so the video transfer
syntaxes need no `ffmpeg` command in process. See Packaging below.

## Build

```sh
python -m venv .venv && source .venv/bin/activate
pip install maturin
maturin develop           # debug build, installed into the active venv - fast iteration
maturin develop --release # release build, installed into the active venv
./build-in-docker.sh      # Linux release wheel (module + CLIs) in manylinux_2_28 - see Packaging below
python test/smoke.py         # core / render / MPR / texture-export smoke test
python test/smoke_dimse.py   # DIMSE SCU + SCP smoke test (echo/store/find/move, callbacks)
```

## Typed datasets

A typed dataset API (#6) alongside the calls below. A source is a path, bytes-like, or a binary
file object read front to back with `read(n)` (e.g. an object-storage download).

```python
import dcmnorm
from dcmnorm import Tag, VR, TransferSyntax

ds = dcmnorm.read("in.dcm", until_pixel_data=True)          # DicomDataset, plain Python values
ds.get_string(Tag.StudyInstanceUID)
ds.get_int(Tag.NumberOfFrames, 1)
ds.get_sequence(Tag.ReferencedSeriesSequence)                # [DicomDataset]
ds.get_person_name(Tag.PatientName).family_name
ds.get_bulk_data_ref(Tag.PixelData)                          # BulkDataRef(tag, vr, offset, length)

json_text = dcmnorm.to_dicom_json("in.dcm", bulk_data_uri_builder=lambda ref: f"{base}/{ref.offset}")

edited = dcmnorm.read("in.dcm").to_builder().set(Tag.PatientName, VR.PN, "Doe^Jane").build()
dcmnorm.write(edited, "out.dcm", transfer_syntax=TransferSyntax.ExplicitVRLittleEndian)

frame = dcmnorm.render_pixels("in.dcm", 0, window_center=40, window_width=400)
frame.width, frame.height, frame.channels, frame.shape     # frame.data: bytes, no image encoding
frame.memoryview()                                         # shaped view; frame.to_numpy() if numpy is installed
```

- `read(source, until_pixel_data=False, load_bulk_data=False)`: `until_pixel_data` stops at the
  PixelData header (a file object isn't read past it); bulk values stay in the source as
  `BulkDataRef`s unless `load_bulk_data`. Accessors: `get_string`, `get_strings`, `get_int`,
  `get_float`, `get_numbers`, `get_bytes`, `get_sequence`, `get_nested_dataset`,
  `get_person_name`, `get_bulk_data_ref`, `vr`, `tags`, `file_meta`, `transfer_syntax_uid`,
  `number_of_frames`, `frame_time_ms`, `pixel_aspect_ratio`; `tag in ds`, `ds[tag]`, iteration.
- `to_dicom_json(source | dataset, include_tags=, exclude_tags=, bulk_data="uri" | "inline",
  bulk_data_uri_base=, bulk_data_uri_builder=, include_file_meta=, keyword_and_vm=)`: PS3.18
  Annex F JSON without reading PixelData.
- `to_builder()` → `set(tag, vr, *values)` / `set(tag, [values])`, `set_int`, `set_float`,
  `set_tags`, `set_bytes`, `set_sequence`, `add_sequence_item`, `edit_sequence_item`, `remove`,
  `remove_private_tags`, `build()`. `write(dataset, path | file | None, transfer_syntax=,
  bulk_data_source=)` creates the File Meta Information and reads references natively from the
  source.
- `render_pixels(source, frame_index, **params)`: `window_center`/`window_width`, `voi_lut`,
  `modality_lut`, `icc_profile`, `output_width`, `output_height`, `show_overlays`,
  `overlay_index`, `overlay_color`; pipeline as in [the Java guide](../java/GUIDE.md#3-frame-rendering).
  `RenderedPixels.data` is the one copy of the pixels; `memoryview()` / `to_numpy()` view it.
- `transcode_to(source, uid, output=None)`, `transfer_syntax_support()`, `keyword_of` /
  `vr_of` / `tag_for_keyword`.
- Generated constants: `Tag` (int constants), `VR`, `TransferSyntax` (`.lossy`,
  `.encapsulated`, `.explicit_vr`, `.from_uid()`), `SOPClassUID` (`.description`).
- Errors: `DcmnormError` subclasses `InvalidDicomError`, `UnsupportedTransferSyntaxError`,
  `DecodeError`, `EncodeError`, `RenderError`, `DimseError`, with `kind`,
  `source_transfer_syntax_uid`, `target_transfer_syntax_uid`, `frame_index`. numpy is optional;
  there are no type stubs yet.

## API

Every function is synchronous and releases the GIL while it runs; every entry point runs through
`catch_unwind` (see `guarded()` in `src/lib.rs`) and raises `dcmnorm.DcmnormError` instead of
crashing the process - DICOM files come from arbitrary, sometimes-malformed vendor equipment, and
unlike a subprocess call, a panic in an in-process extension takes the whole host process down
unless it's caught at the FFI boundary.

- `read_tags(file_path, tags: list[str]) -> str` - JSON (flat, hex-keyed, bulk data as a URI
  reference) containing only the requested tags. Stops parsing right after the highest requested
  tag, same fast path as `dcmnorm --filter`. Filtering for a bulk-data-eligible tag (e.g.
  `PixelData`) falls back to inlining it rather than a URI reference, unlike `read_json` below.
- `read_json(file_path, format="flat"|"standard"=None, key_style="name"|"hex"=None, bulk_data="uri"|"inline"=None, omit_keyword_vm=False) -> str`
  - full-file JSON dump. `bulk_data` defaults to `"uri"`, matching the CLI's own default
  (`--bulk-data uri`) - not the Rust library's own default, which is `"inline"`. Getting this
  wrong makes a huge difference: `"inline"` base64-embeds elements like `PixelData` directly,
  ~1000x larger output for a typical image, instead of a small `"?offset=..&length=.."` reference.
  `omit_keyword_vm=True` drops the non-standard `Keyword` and `VM` members from `format="standard"`
  output, leaving only what PS3.18 Annex F defines.
- `write_json(json, output_path, format=None, bulk_data_source_path=None) -> None` - writes a
  DICOM file from JSON. `bulk_data_source_path` resolves `BulkDataURI` references against that
  file's bytes.
- `edit_tags(file_path, output_path=None, set: dict[str,str]=None, remove: list[str]=None, remove_private_tags: bool=None) -> None`
  - set/remove attributes; writes back in place unless `output_path` is given.
- `transcode(file_path, output_path, transfer_syntax_uid) -> None`.
- `check_dicom(file_path) -> bool`.
- `write_dicomdir(entries: list[DicomdirEntry | dict], output_path, file_set_id=None, sop_instance_uid=None) -> WriteDicomdirResult`
  - writes a DICOMDIR (Media Storage Directory, Explicit VR Little Endian) for a File-set, the
  in-process form of `dcmnorm dicomdir ROOT` (same as the Node binding's `writeDicomdir`). Each
  entry is `DicomdirEntry(file_id, json=None, format=None, path=None, transfer_syntax_uid=None)`,
  or a dict with those keys (an unknown key raises): the File ID components (1-8 of 1-8
  characters from `A-Z 0-9 _`, PS3.10 8.2) and the file's header, either as the DICOM JSON
  `read_json` returns (`json`, `format` `"flat"` by default or `"standard"`; BulkDataURI elements
  are ignored, so the default `"uri"` output is fine) or a file to read it from (`path`; pixel
  data isn't loaded). Give exactly one of `json` and `path`. With `json`, the referenced file's
  transfer syntax is `transfer_syntax_uid`, else the JSON's TransferSyntaxUID, else Explicit VR
  Little Endian. `sop_instance_uid` is the DICOMDIR's own (generated when not given). The result
  has `records` (total), `record_types: list[DicomdirRecordCount]` (`record_type`, `count`, in
  first-seen order; `record_counts()` gives them as a dict),
  `synthesized: list[DicomdirSynthesized]` (`record_type`, `entity`, `attribute`, `value`,
  `source`) and `warnings: list[str]`.
  Records are PATIENT > STUDY > SERIES > one per entry, typed by SOP Class (IMAGE, SR DOCUMENT,
  KEY OBJECT DOC, PRESENTATION, ENCAP DOC, ...; PS3.3 F.5), written depth-first.
  `synthesized` lists the Type 1 keys the instances lacked and the values written (IHE PDI Table
  4.47.4-1: Patient ID, Study ID, Study Date/Time, Modality, Series Number, Instance Number), with
  `source` naming the attribute a value was copied from, if any. Raises `DcmnormError` for a bad File ID
  or File-set ID (at most 16 of `A-Z 0-9 _`), a Patient ID shared by two patients, a study under
  two patients, a series in two studies, and duplicate files or SOP Instance UIDs.

  ```python
  result = dcmnorm.write_dicomdir(
      [
          dcmnorm.DicomdirEntry(["DICOM", "ST000001", "SE000001", "IM000001"], path="/media/disc/DICOM/ST000001/SE000001/IM000001"),
          {"file_id": ["DICOM", "ST000001", "SE000001", "IM000002"], "json": dcmnorm.read_json(other_path)},
      ],
      "/media/disc/DICOMDIR",
      file_set_id="DISC_0001",
  )
  result.record_counts()  # {'PATIENT': 1, 'STUDY': 1, 'SERIES': 1, 'IMAGE': 2}
  ```

All return values that carry structured data are JSON strings - `json.loads()` them Python-side.
This matches how the CLI and the Node bindings both already talk JSON everywhere else in this
project, rather than inventing a third Python-specific dataset representation.

### Rendering

- `render_frame(file_path, format="jpeg"|"png"=None, output_width=None, output_height=None, window_center=None, window_width=None, frame_index=None, jpeg_quality=None, show_overlays: bool=None, overlay_index=None, overlay_color=None) -> RenderedFrame`
  - renders a single frame to JPEG or PNG. If the instance has one or more DICOM overlay planes
  (group `60xx`), the first available overlay composites onto the image by default;
  `overlay_index` selects a different one (0-based, by `OverlaySummary.index`, matching the CLI's
  `--overlay-index`), `show_overlays=False` disables overlay rendering, and `overlay_color`
  (`"R,G,B"` or `"#RRGGBB"`, default green) sets the fill color. `RenderedFrame.overlays` always
  lists every overlay present on the instance (even when none was rendered), so a caller can offer
  overlay selection without a separate metadata call; `selected_overlay_index` says which one (if
  any) is actually in `data`. `RenderedFrame` is `{mime_type, width, height, data: bytes,
  overlays: list[OverlaySummary], selected_overlay_index}`; `OverlaySummary` is `{index, group,
  rows, columns, overlay_type, label}`.
- `render_movie(file_path, output_width=None, output_height=None, window_center=None, window_width=None, fps=None) -> RenderedMovie`
  - renders every frame of a multiframe instance to an MP4 (requires `ffmpeg` on `PATH`). Returns
  `{mime_type, data: bytes}`. Does not currently support the overlay options `render_frame` does.

### MPR (Multiplanar Reformation)

- `build_volume(file_paths: list[str]) -> DicomVolumeHandle` - reads and decodes every slice of a
  parallel stack (e.g. one CT/MR/PT series), spatially re-sorted by `ImagePositionPatient`
  regardless of input order. Raises for fewer than 2 files, mismatched Rows/Columns, or a
  non-parallel stack (one Enhanced multi-frame file is a whole volume; a gantry-tilted stack is
  desheared onto an orthogonal grid). This is the expensive step - build once per series
  and keep the returned handle resident (e.g. in a volume cache) so every subsequent `reformat()`
  call is cheap.
- `DicomVolumeHandle` - an opaque, read-only handle around the built volume:
  - properties: `rows`, `cols`, `num_slices`, `native_basis` (`[row_dir(3), col_dir(3)]`, the
    volume's own acquisition-native orientation - a reasonable seed for an "axial" reformat),
    `center` (`[x,y,z]` LPS mm, the volume's physical center), `min_spacing_mm` (its smallest
    voxel dimension, a reasonable default output spacing)
  - `.reformat(origin, row_dir, col_dir, output_width, output_height, spacing_mm, window_center=None, window_width=None, format=None, jpeg_quality=None, interpolation="trilinear"|"nearest"=None, slab_thickness_mm=None, slab_projection="mip"|"minip"|"average"=None) -> RenderedFrame`
    - resamples one plane through the volume and encodes it exactly like `render_frame`'s output
    shape, so callers reuse their existing image-display code path. `origin`/`row_dir`/`col_dir`
    are 3-element lists of mm/unit-vector components. `interpolation` defaults to `"trilinear"`;
    use `"nearest"` (faster) for a live-drag preview frame. `slab_thickness_mm` (default 0, an
    infinitely-thin plane) turns on a thick-slab reformat centered on `origin`, combined per
    `slab_projection` (default `"mip"`).
  - `.export_texture(...) -> TextureExportResult` - see [Texture export](#texture-export) below.

### Texture export

Packs a volume, a single frame, or several independent frames as a lossless, GPU-upload-ready
payload (16-bit samples, row-major, optionally gzip-compressed) instead of an 8-bit windowed
render - the client does its own window/level and oblique reslicing in a GPU shader instead of
round-tripping to the server per interaction. Mirrors the CLI's `--output-type texture`/`.gputex`
- see the [CLI docs](../../docs/cli.md#export-a-gpu-texture-gputex) and
`dcmnorm::dicom_io::texture_export`'s own module doc for the full format contract.

- `DicomVolumeHandle.export_texture(target_max_dim=None, compression="gzip"|"none"=None, window_center=None, window_width=None) -> TextureExportResult`
  - packs the volume's own NATIVE voxel lattice (not a resampled oblique plane - that's
  `reformat()`). `target_max_dim` caps the longest of width/height/depth, proportionally
  downsampling (trilinear) if the native volume exceeds it; omitted means full native resolution.
  `compression` defaults to `"gzip"`. `window_center`/`window_width` are purely informational,
  carried through to the result for the client's initial render - the exported samples are never
  windowed.
- `export_frame_texture(file_path, frame_index=None, target_max_dim=None, compression=None, window_center=None, window_width=None) -> TextureExportResult`
  - packs one decoded 2D frame as a depth-1 "1-slice volume" texture, so a large diagnostic 2D
  image (DX/CR/mammography) can reuse the same client GPU texture/shader pipeline as an MPR
  volume. `frame_index` defaults to 0.
- `export_frame_stack_texture(sources: list[FrameStackSource], compression=None, window_center=None, window_width=None) -> TextureExportResult`
  - packs several independent original frames (no resampling, no cross-layer interpolation, no
  physical geometry) as one texture-array upload: a cine/multiframe instance supplies one source
  with several `frame_indices` (its file is parsed once), a multi-image series supplies one source
  per instance file (`frame_indices` defaulting to `[0]`). `FrameStackSource(file_path,
  frame_indices=None)` is a small class - construct one per source. The result's layer order is
  the flattened source order followed by each source's own `frame_indices` order - callers must
  supply sources in the exact order the client's own frame/instance index expects.
- `TextureExportResult`: `content_kind` (`"volume"|"image2d"|"framestack"`), `sample_format`
  (`"int16"|"uint16"`), `compression` (`"none"|"gzip"`), `lossless`, `width`, `height`, `depth`,
  `rescale_slope`, `rescale_intercept`, `row_spacing_mm`, `col_spacing_mm`, `slice_spacing_mm`,
  `origin` (3-element list), `row_dir`, `col_dir`, `normal_dir`, `default_window_center`,
  `default_window_width`, `native_width`, `native_height`, `native_depth`, `downsampled`,
  `payload_bytes_raw`, `payload_bytes_stored`, `data: bytes`. `texel * rescale_slope +
  rescale_intercept` recovers the physical value (e.g. HU). Geometry fields
  (`row_spacing_mm`/`origin`/`row_dir`/etc.) carry no meaning for `content_kind: "framestack"` -
  only `"volume"` makes a real spatial claim.

### DIMSE (network)

Every `*_scu` function accepts `calling_ae_title` (default `"DCMNORM"`), `called_ae_title`, and
`on_log` (a plain callable taking one `str` argument, invoked synchronously for each notable DIMSE
event - association open/close, each request/response, release/abort). `destination` is always
`"host:port"`.

- `echo_scu(destination, calling_ae_title=None, called_ae_title=None, timeout_ms=None, on_log=None) -> int`
  - performs a C-ECHO, blocking until it completes. Returns the response Status code (0 =
  success); raises `DcmnormError` only if the association itself couldn't be established.
- `start_echo_scu(...) -> EchoScuHandle` - same arguments, returns immediately with a handle
  instead of blocking. `EchoScuHandle.result()` blocks for the eventual status;
  `.release()`/`.abort()` request the DICOM UL teardown verb and block until the association has
  genuinely closed (real acknowledgement, not fire-and-forget) - both idempotent.
- `store_scu(destination, files: list[str], calling_ae_title=None, called_ae_title=None, max_pdu_length=None, never_transcode: bool=None, timeout_ms=None, on_log=None) -> list[StoreScuResult]`
  - sends each of `files` via C-STORE, blocking until every file has been sent. Returns one
  `StoreScuResult` (`{sop_instance_uid, status}`) per file that could be read and sent - a
  non-zero status is just data in the result (the peer rejected that instance), not a raised
  error. `never_transcode=True` means only each file's own transfer syntax is proposed.
- `start_store_scu(...) -> StoreScuHandle` - same shape as `start_echo_scu`, for `store_scu`.
- `find_scu(destination, query: dict[str,str], calling_ae_title=None, called_ae_title=None, max_pdu_length=None, timeout_ms=None, on_log=None) -> list[str]`
  - performs a C-FIND (Study Root Query/Retrieve), blocking until it completes. `query` values: an
  empty string is a universal-match "return key" (mirrors findscu's bare `-k TAG`), non-empty
  constrains the match (mirrors `-k TAG=value`); `QueryRetrieveLevel` defaults to `"STUDY"` if not
  given. Returns one flat/hex-keyed DICOM JSON string per match (`json.loads()` it, same shape as
  `read_json(format="flat", key_style="hex")`).
- `start_find_scu(...) -> FindScuHandle` - same shape as `start_echo_scu`, for `find_scu`.
- `move_scu(destination, move_destination_ae, study_instance_uid, calling_ae_title=None, called_ae_title=None, max_pdu_length=None, timeout_ms=None, watch_path=None, stale_data_timeout_ms=None, on_log=None, query_retrieve_level=None, series_instance_uid=None, sop_instance_uids=None) -> MoveScuHandle`
  - asks `destination` to push `study_instance_uid` to `move_destination_ae` (an AE title
  `destination` already knows how to reach, not a socket address). Unlike the other three, this
  **always** returns a handle immediately (the retrieve runs on its own background thread) rather
  than optionally blocking - call `.result()` to wait for the terminal `MoveScuResult`
  (`{status, completed, failed, warning, remaining, cancelled, cancelled_via}`, regardless of
  success/warning/failure), or `.release()`/`.abort()` to close it early once some other signal
  (e.g. the study already being confirmed fully received via a separate channel) makes further
  waiting pointless. `watch_path` + `stale_data_timeout_ms` watch a directory (typically this
  retrieve's own cache destination) for write activity and abort if it goes stale, independent of
  the overall `timeout_ms` ceiling. `query_retrieve_level="SERIES"`/`"IMAGE"` with
  `series_instance_uid` (+ `sop_instance_uids` for IMAGE) retrieves one series or a list of its
  instances instead of the whole study.

Every `*ScuHandle`'s background call runs on its own OS thread (not a shared pool) so a
long-running `move` can't starve an unrelated concurrent `abort()`/`release()`; `.result()` /
`.release()` / `.abort()` release the GIL while they block, so other Python threads keep running.

### DICOM SCP (server)

- `start_dicom_server(port, cache_path, ae_title, on_find, on_move, on_association_complete, max_pdu_length=None, idle_timeout_ms=None, on_log=None) -> DicomServerHandle`
  - starts a DICOM SCP (association acceptor) on `port` (`0` picks an ephemeral port - see the
  returned handle's `local_port`), answering C-ECHO, C-STORE, C-FIND, and C-MOVE. C-ECHO and
  C-STORE are handled entirely natively (C-STORE writes each received instance under `cache_path`
  as `S_<StudyInstanceUID>/<Modality>_<SOPInstanceUID>.dcm`); C-FIND, C-MOVE, and "association
  complete" (fired once per association that stored at least one instance) are delegated to plain
  Python callables, called synchronously from whichever connection thread is handling that
  association - **must be thread-safe** if `on_find`/`on_move` share mutable state, since
  multiple associations can be in flight at once:
  - `on_find(filter: dict[str, str]) -> list[dict[str, str]]` - `filter` has whichever of
    StudyInstanceUID/StudyDate/PatientName/PatientID keys the request's Identifier actually had
    values for. Must return matched studies, each a dict with whichever of
    StudyInstanceUID/AccessionNumber/PatientID/PatientName/PatientBirthDate/StudyDate/StudyTime/
    StudyDescription/ModalitiesInStudy/NumberOfStudyRelatedSeries/NumberOfStudyRelatedInstances it
    has values for.
  - `on_move(study_instance_uid: str, move_destination_ae: str) -> bool` - whether the move was
    successfully queued (mirrors a "fire and forget" C-MOVE that delegates the actual transfer
    elsewhere and only reports whether it was accepted for processing).
  - `on_association_complete(stored_instances_by_study: dict[str, list[str]])` - maps each
    StudyInstanceUID to the on-disk paths of every instance C-STORE'd for it during that
    association. Return value is ignored.

  Unlike the Node bindings (whose JS callbacks are inherently async and need a
  `ThreadsafeFunction`/Promise/oneshot-channel bridge - see that crate's own comment on
  `NapiScpHandlers`), dcmnorm's underlying `ScpHandlers` trait is itself synchronous, so a Python
  callable's return value is used directly - no bridging needed.

  Accepts every proposed presentation context regardless of abstract syntax - this is a
  permissive "accept anything a real sender proposes" SCP, not a curated allow-list.
- `DicomServerHandle.local_port` (property) - the actual bound port (useful when `port=0`).
- `DicomServerHandle.close()` - stops accepting new associations and waits for the listener to
  shut down. Associations already in progress finish independently (or hit their own idle
  timeout) - this does not interrupt them. Safe to call more than once.

## Packaging

Published to PyPI as `dcmnorm` by the [`Build and release`](../../.github/workflows/release.yml)
workflow on every `v*.*.*` release tag, with the release version (PEP 440 form: `v0.12.0-rc.1` is
`0.12.0rc1`). Each platform gets two wheels of the same name and version:

| Wheel | Contents | Picked by |
| --- | --- | --- |
| `dcmnorm-<v>-cp39-abi3-<plat>.whl` (this directory, maturin) | `dcmnorm/` (the extension module), `dcmnorm-<v>.data/scripts/{dcmnorm,dcmtalk}` | CPython 3.9+ (non-free-threaded) |
| `dcmnorm-<v>-py3-none-<plat>.whl` (`packaging/package.py`) | `dcmnorm-<v>.data/scripts/{dcmnorm,dcmtalk}` only | everything else: CPython 3.8, free-threaded 3.13t/3.14t, other implementations |

`<plat>` is `manylinux_2_28_x86_64`, `manylinux_2_28_aarch64`, `macosx_11_0_arm64`,
`macosx_10_15_x86_64` or `win_amd64`. Installers prefer the abi3 wheel wherever it fits, so
`pip install dcmnorm`, `uv tool install dcmnorm` and `pipx install dcmnorm` put both CLIs on `PATH`
on every Python, and `import dcmnorm` works on CPython 3.9+. The CLI binaries in both wheels are
the same files, byte for byte: the release builds them once (the CLI matrix, `packaging/build-linux.sh`
on Linux), and `packaging/stage_cli.py` copies them out of the CLI-only wheel into `data/scripts/`
(`[tool.maturin] data = "data"`) before maturin builds this one; `packaging/smoke_test.py
--same-cli-as` checks it. `data/`, `dist/` and the copied `LICENSE` are build outputs
(`.gitignore`); no wheel is committed. No sdist is published: building from source needs Rust,
libclang, git and nasm and compiles FFmpeg.

Built with `abi3-py39` (PyO3's stable ABI): one extension per platform works unmodified on every
CPython >= 3.9.

How each wheel is built (FFmpeg linked in statically with the same codec set everywhere, no
fallback to a smaller one):

- **Linux**: `build-in-docker.sh` (`packaging/build-binding-linux.sh python`) builds in
  `quay.io/pypa/manylinux_2_28_<arch>` on a runner of the same architecture with
  `maturin build --compatibility manylinux_2_28`, and fails if the module needs a glibc newer than
  2.28 or links anything outside the manylinux policy (FFmpeg included).
- **macOS**: `maturin build` on `macos-15` / `macos-15-intel` (deployment targets 11.0 / 10.15),
  with `packaging/macos-link.py` as the linker and `packaging/portable-cc` on `PATH`; fails if the
  module links an FFmpeg dylib.
- **Windows**: `maturin build` with `--no-default-features --features
  pyo3/extension-module,ffmpeg-codec-linked,jpeg-ls-codec,jpeg-xl-codec,jpeg2000-openjpeg-encode`
  against vcpkg's static FFmpeg (`.github/actions/ffmpeg-vcpkg`); fails if the `.pyd` imports an
  FFmpeg DLL.

Every wheel then goes through `packaging/smoke_test.py` in a fresh venv: both CLIs on `PATH` and
working, `import dcmnorm`, a parse and an in-process MPEG-2 round trip. The local equivalent, for
Linux x86_64:

```sh
packaging/build-linux.sh
python3 packaging/package.py --platform linux-x86_64 --bin-dir target/manylinux_2_28-x86_64/release --out-dir dist
python3 packaging/stage_cli.py dist/dcmnorm-*-py3-none-manylinux_2_28_x86_64.whl
bindings/python/build-in-docker.sh
python3 packaging/smoke_test.py bindings/python/dist/*.whl --same-cli-as dist/dcmnorm-*-py3-none-manylinux_2_28_x86_64.whl
```

`maturin develop` (a host build installed into the active venv, without the CLIs) is for local
iteration only. `python test/smoke.py` checks the glibc floor of whatever wheel is in `dist/`.

Publishing setup (trusted publisher, the `PYPI_PUBLISH` variable):
[`packaging/README.md`](../../packaging/README.md).
