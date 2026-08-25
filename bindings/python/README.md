# dcmnorm-python

Native Python bindings for dcmnorm, built with [PyO3](https://pyo3.rs) +
[maturin](https://www.maturin.rs). Calls straight into the `dcmnorm` lib crate in-process (no CLI
subprocess, no stdio/JSON round trip); blocking work releases the GIL (`py.allow_threads`) so it
doesn't stall other Python threads. This is the Python counterpart of `bindings/node` - same
underlying crate, same feature set, adapted to Python conventions (synchronous/blocking calls
instead of Promises, snake_case keyword arguments instead of camelCase options objects).

## Build

```sh
python -m venv .venv && source .venv/bin/activate
pip install maturin
maturin develop           # debug build, installed into the active venv - fast iteration
maturin develop --release # release build, installed into the active venv
./build-in-docker.sh      # release wheel built inside python:3.12-slim-bookworm - see Packaging below
python test/smoke.py         # core / render / MPR / texture-export smoke test
python test/smoke_dimse.py   # DIMSE SCU + SCP smoke test (echo/store/find/move, callbacks)
```

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
- `read_json(file_path, format="flat"|"standard"=None, key_style="name"|"hex"=None, bulk_data="uri"|"inline"=None) -> str`
  - full-file JSON dump. `bulk_data` defaults to `"uri"`, matching the CLI's own default
  (`--bulk-data uri`) - not the Rust library's own default, which is `"inline"`. Getting this
  wrong makes a huge difference: `"inline"` base64-embeds elements like `PixelData` directly,
  ~1000x larger output for a typical image, instead of a small `"?offset=..&length=.."` reference.
- `write_json(json, output_path, format=None, bulk_data_source_path=None) -> None` - writes a
  DICOM file from JSON. `bulk_data_source_path` resolves `BulkDataURI` references against that
  file's bytes.
- `edit_tags(file_path, output_path=None, set: dict[str,str]=None, remove: list[str]=None, remove_private_tags: bool=None) -> None`
  - set/remove attributes; writes back in place unless `output_path` is given.
- `transcode(file_path, output_path, transfer_syntax_uid) -> None`.
- `check_dicom(file_path) -> bool`.

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
  non-parallel/gantry-tilt-inconsistent stack. This is the expensive step - build once per series
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
- see the main [README](../../README.md#export-a-gpu-texture-gputex) and
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
- `move_scu(destination, move_destination_ae, study_instance_uid, calling_ae_title=None, called_ae_title=None, max_pdu_length=None, timeout_ms=None, watch_path=None, stale_data_timeout_ms=None, on_log=None) -> MoveScuHandle`
  - asks `destination` to push `study_instance_uid` to `move_destination_ae` (an AE title
  `destination` already knows how to reach, not a socket address). Unlike the other three, this
  **always** returns a handle immediately (the retrieve runs on its own background thread) rather
  than optionally blocking - call `.result()` to wait for the terminal `MoveScuResult`
  (`{status, completed, failed, warning, remaining, cancelled, cancelled_via}`, regardless of
  success/warning/failure), or `.release()`/`.abort()` to close it early once some other signal
  (e.g. the study already being confirmed fully received via a separate channel) makes further
  waiting pointless. `watch_path` + `stale_data_timeout_ms` watch a directory (typically this
  retrieve's own cache destination) for write activity and abort if it goes stale, independent of
  the overall `timeout_ms` ceiling.

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

Like `bindings/node`, this isn't published to PyPI. Consumers are expected to install the
committed wheel directly (e.g. `pip install /path/to/dcmnorm/bindings/python/dist/*.whl`, or a
`file:` / local-path entry in a `requirements.txt`/`pyproject.toml` pointing at that wheel)
rather than building from source at install time - a consumer's Docker builder stage may have no
Rust toolchain, no `libclang`, and none of the ffmpeg/openjpeg dev headers this crate needs to
compile.

The wheel **is committed** to this repo under `dist/` (see `.gitignore`) rather than built at
Docker-image time, for the same reason `bindings/node` commits its compiled `.node` file.
`build-in-docker.sh` builds it inside a `python:3.12-slim-bookworm` container rather than on the host,
specifically to match that image's glibc (2.36, same Debian bookworm baseline the Node bindings'
own `node:22-slim` container uses) - building on an arbitrary host risks a `GLIBC_X.XX not found`
failure that only surfaces once deployed. `python test/smoke.py` checks this automatically
(`check_glibc_compatibility()`) against whatever wheel is currently in `dist/`, so a host-built
wheel accidentally left in `dist/` fails the test rather than silently shipping. `maturin develop`
(plain host builds, installed straight into the active venv) is for fast local iteration only -
don't commit its output; only `dist/*.whl` from `build-in-docker.sh` is meant to be committed.

Built with `abi3-py39` (PyO3's stable-ABI feature): one compiled wheel
(`dcmnorm_python-*-cp39-abi3-manylinux_2_XX_x86_64.whl` - the exact `manylinux_2_XX` tag is
whatever glibc symbol versions the compiled `.so` actually references, auto-detected by maturin,
not something to hardcode or rely on staying the same across builds) works unmodified against any
CPython >= 3.9, rather than needing one wheel per Python minor version - the Python analog of `bindings/node`
committing a single `.node` binary per platform/arch rather than per Node version.
