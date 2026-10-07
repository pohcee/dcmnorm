# dcmnorm-java

Native Java bindings for dcmnorm, built with the [jni](https://docs.rs/jni) crate. Calls
straight into the `dcmnorm` lib crate in-process (no CLI subprocess, no stdio/JSON round trip).
This is the Java counterpart of `bindings/node` (napi-rs) and `bindings/python` (PyO3) - same
underlying crate, same feature set, adapted to Java/JNI conventions.

## Build

```sh
./build.sh              # debug build -> target/debug/libdcmnorm_java.so + target/dcmnorm-java-0.1.0.jar
./build.sh --release     # release build, slower to compile but faster to run
./build.sh test           # also builds exec/dcmtalk and runs the smoke test against it
./build.sh --release test # release build + smoke test
./build-in-docker.sh      # release build inside debian:bookworm-slim - see Packaging below
```

Unlike `bindings/node` (napi-rs's own build tool) and `bindings/python` (maturin), there is no
single Java-ecosystem tool that drives Cargo for you, so `build.sh`/`build-in-docker.sh` just run
Cargo (for `src/lib.rs`, the native library) and Maven (for `src/main/java`, the Java API)
side by side - see the comment at the top of `pom.xml`.

## API

Every method on `Dcmnorm` blocks the calling thread until it completes - there is no JS event
loop or Python GIL to avoid blocking here, so (unlike the Node bindings' `Promise`-returning
calls) there is no separate async API; run a call on your own background thread/
`ExecutorService` if you want one. Every method that can fail declares `throws
DcmnormException`; a panic inside the native library (e.g. a severely malformed DICOM file
hitting an edge case the parser does not expect) is also surfaced as `DcmnormException` rather
than crashing the JVM - every entry point runs the actual dcmnorm call through Rust's
`catch_unwind` first, mirroring `guarded()` in the Node/Python bindings.

- `Dcmnorm.readTags(filePath, List<String> tags) -> String` - JSON (flat, hex-keyed, bulk data as
  a URI reference) containing only the requested tags. Stops parsing right after the highest
  requested tag, same fast path as `dcmnorm --filter`. Filtering for a bulk-data-eligible tag
  (e.g. `PixelData`) falls back to inlining it rather than a URI reference, unlike `readJson`
  below - see the comment on `nativeReadTags` in `src/lib.rs`.
- `Dcmnorm.readJson(filePath[, ReadJsonOptions]) -> String` - full-file JSON dump.
  `ReadJsonOptions.bulkData` defaults to `"uri"`, matching the CLI's own default
  (`--bulk-data uri`) - not the Rust library's own default, which is `"inline"`. Getting this
  wrong makes a huge difference: `"inline"` base64-embeds elements like `PixelData` directly,
  ~1000x larger output for a typical image, instead of a small `"?offset=..&length=.."`
  reference.
- `Dcmnorm.writeJson(json, outputPath[, WriteJsonOptions])` - writes a DICOM file from JSON.
  `WriteJsonOptions.bulkDataSourcePath` resolves `BulkDataURI` references against that file's
  bytes.
- `Dcmnorm.editTags(filePath[, EditTagsOptions])` - set/remove attributes; writes back in place
  unless `EditTagsOptions.outputPath` is given.
- `Dcmnorm.transcode(filePath, outputPath, transferSyntaxUid)`.
- `Dcmnorm.checkDicom(filePath) -> boolean`.

All return values that carry structured data are plain JSON strings (`readTags`/`readJson`/
`findScu`) - parse them with whatever JSON library your application already uses. Every other
structured result (`RenderedFrame`, `TextureExportResult`, `StoreScuResult`, `MoveScuResult`,
`DicomVolumeHandle`'s getters, ...) is a real typed Java object/getter - see "Why JSON for
options, not results" below for why only the options side of this binding leans on JSON text.

### Rendering

- `Dcmnorm.renderFrame(filePath[, RenderFrameOptions]) -> RenderedFrame` - renders a single frame
  to JPEG or PNG. If the instance has one or more DICOM overlay planes (group `60xx`), the first
  available overlay composites onto the image by default; `overlayIndex` selects a different one
  (0-based, by `OverlaySummary.index`, matching the CLI's `--overlay-index`), `showOverlays(false)`
  disables overlay rendering, and `overlayColor` (`"R,G,B"` or `"#RRGGBB"`, default green) sets
  the fill color. `RenderedFrame.getOverlays()` always lists every overlay present on the
  instance (even when none was rendered), so a caller can offer overlay selection without a
  separate metadata call; `getSelectedOverlayIndex()` says which one (if any) is actually in
  `getData()`.
- `Dcmnorm.renderMovie(filePath[, RenderMovieOptions]) -> RenderedMovie` - renders every frame of
  a multiframe instance to an MP4 (requires `ffmpeg` on `PATH`). Does not currently support the
  overlay options `renderFrame` does.

### MPR (Multiplanar Reformation)

- `Dcmnorm.buildVolume(List<String> filePaths) -> DicomVolumeHandle` - reads and decodes every
  slice of a parallel stack (e.g. one CT/MR/PT series), spatially re-sorted by
  `ImagePositionPatient` regardless of input order. Raises for fewer than 2 files, mismatched
  Rows/Columns, or a non-parallel/gantry-tilt-inconsistent stack. This is the expensive step -
  build once per series and keep the returned handle resident (e.g. in a volume cache) so every
  subsequent `reformat()` call is cheap.
- `DicomVolumeHandle` - an opaque, read-only handle around the built volume. **Must be closed**
  (`close()`, ideally via try-with-resources) exactly once to free its native memory - see "Why
  `DicomVolumeHandle` needs `close()`" below.
  - getters: `getRows()`, `getCols()`, `getNumSlices()`, `getNativeBasis()` (`double[6]`,
    `[rowDir(3), colDir(3)]` - the volume's own acquisition-native orientation, a reasonable seed
    for an "axial" reformat), `getCenter()` (`double[3]`, `[x,y,z]` LPS mm, the volume's physical
    center), `getMinSpacingMm()` (its smallest voxel dimension, a reasonable default output
    spacing)
  - `reformat(ReformatParams) -> RenderedFrame` - resamples one plane through the volume and
    encodes it exactly like `renderFrame`'s output shape, so callers reuse their existing
    image-display code path. `ReformatParams`'s constructor takes `origin`/`rowDir`/`colDir`
    (each `double[3]`, mm/unit-vector components), `outputWidth`, `outputHeight`, `spacingMm`;
    its fluent setters cover the rest. `interpolation` defaults to `"trilinear"`; use `"nearest"`
    (faster) for a live-drag preview frame. `slabThicknessMm` (default 0, an infinitely-thin
    plane) turns on a thick-slab reformat centered on `origin`, combined per `slabProjection`
    (default `"mip"`).
  - `exportTexture([TextureExportOptions]) -> TextureExportResult` - see
    [Texture export](#texture-export) below.

### Texture export

Packs a volume, a single frame, or several independent frames as a lossless, GPU-upload-ready
payload (16-bit samples, row-major, optionally gzip-compressed) instead of an 8-bit windowed
render - the client does its own window/level and oblique reslicing in a GPU shader instead of
round-tripping to the server per interaction. Mirrors the CLI's `--output-type texture`/`.gputex`
- see the main [README](../../README.md#export-a-gpu-texture-gputex) and
`dcmnorm::dicom_io::texture_export`'s own module doc for the full format contract.

- `DicomVolumeHandle.exportTexture([TextureExportOptions]) -> TextureExportResult` - packs the
  volume's own NATIVE voxel lattice (not a resampled oblique plane - that's `reformat()`).
  `targetMaxDim` caps the longest of width/height/depth, proportionally downsampling (trilinear)
  if the native volume exceeds it; omitted means full native resolution. `compression` defaults
  to `"gzip"`. `windowCenter`/`windowWidth` are purely informational, carried through to the
  result for the client's initial render - the exported samples are never windowed.
- `Dcmnorm.exportFrameTexture(filePath[, TextureExportOptions]) -> TextureExportResult` - packs
  one decoded 2D frame as a depth-1 "1-slice volume" texture, so a large diagnostic 2D image
  (DX/CR/mammography) can reuse the same client GPU texture/shader pipeline as an MPR volume.
  `frameIndex` defaults to 0.
- `Dcmnorm.exportFrameStackTexture(List<FrameStackSource>[, TextureExportOptions]) ->
  TextureExportResult` - packs several independent original frames (no resampling, no
  cross-layer interpolation, no physical geometry) as one texture-array upload: a
  cine/multiframe instance supplies one `FrameStackSource` with several frame indices (its file
  is parsed once), a multi-image series supplies one source per instance file (frame indices
  defaulting to `[0]`). The result's layer order is the flattened source order followed by each
  source's own frame-index order - callers must supply sources in the exact order the client's
  own frame/instance index expects. This has no CLI equivalent - like the Node bindings' own copy
  of this call, it's binding-only.
- `TextureExportResult` getters: `getContentKind()` (`"volume"`/`"image2d"`/`"framestack"`),
  `getSampleFormat()` (`"int16"`/`"uint16"`), `getCompression()` (`"none"`/`"gzip"`),
  `isLossless()`, `getWidth()`/`getHeight()`/`getDepth()`, `getRescaleSlope()`/
  `getRescaleIntercept()`, spacing/origin/direction getters, `getDefaultWindowCenter()`/
  `getDefaultWindowWidth()`, `getNativeWidth()`/`getNativeHeight()`/`getNativeDepth()`,
  `isDownsampled()`, `getPayloadBytesRaw()`/`getPayloadBytesStored()`, `getData()`. `texel *
  getRescaleSlope() + getRescaleIntercept()` recovers the physical value (e.g. HU). Geometry
  getters carry no meaning for `contentKind: "framestack"` - only `"volume"` makes a real
  spatial claim.

### DIMSE (network)

Every `*Scu` method accepts an `Options` object with `callingAeTitle` (default `"DCMNORM"`),
`calledAeTitle`, and `onLog` (a `DimseLogger` - a plain `void log(String message)` functional
interface, invoked synchronously, on the calling thread, for each notable DIMSE event -
association open/close, each request/response, release/abort). `destination` is always
`"host:port"`.

- `Dcmnorm.echoScu(destination[, EchoScuOptions]) -> int` - performs a C-ECHO, blocking until it
  completes. Returns the response Status code (0 = success); raises `DcmnormException` only if
  the association itself couldn't be established.
- `Dcmnorm.storeScu(destination, List<String> files[, StoreScuOptions]) -> List<StoreScuResult>`
  - sends each file via C-STORE, blocking until every file has been sent. Returns one
  `StoreScuResult` (`sopInstanceUid`, `status`) per file that could be read and sent - a non-zero
  status is just data in the result (the peer rejected that instance), not a raised error.
  `neverTranscode(true)` means only each file's own transfer syntax is proposed.
- `Dcmnorm.findScu(destination, Map<String,String> query[, FindScuOptions]) -> List<String>` -
  performs a C-FIND (Study Root Query/Retrieve), blocking until it completes. `query` values: an
  empty string is a universal-match "return key" (mirrors findscu's bare `-k TAG`), non-empty
  constrains the match (mirrors `-k TAG=value`); `QueryRetrieveLevel` defaults to `"STUDY"` if
  not given. Returns one flat/hex-keyed DICOM JSON string per match (parse each with your own
  JSON library), same shape as `readJson(path, new
  ReadJsonOptions().format("flat").keyStyle("hex"))`.
- `Dcmnorm.moveScu(destination, moveDestinationAe, studyInstanceUid[, MoveScuOptions]) ->
  MoveScuResult` - asks `destination` to push `studyInstanceUid` to `moveDestinationAe` (an AE
  title `destination` already knows how to reach, not a socket address), blocking until the
  retrieve reaches a terminal status. Returns `completed`/`failed`/`warning`/`remaining`
  sub-operation counts regardless of success/warning/failure. `watchPath` + `staleDataTimeoutMs`
  watch a directory (typically this retrieve's own cache destination) for write activity and
  abort if it goes stale, independent of the overall `timeoutMs` ceiling.

**Scope note - what's intentionally not here yet:** the Python bindings additionally expose
handle-based `start_echo_scu`/`start_store_scu`/`start_find_scu` (returning a handle a caller can
`abort()`/`release()` early instead of just blocking) and `move_scu` itself is handle-based
there; `Dcmnorm.moveScu` here always blocks to the terminal result instead. Neither binding's
`start_dicom_server`/DICOM SCP equivalent (a server with `onFind`/`onMove`/
`onAssociationComplete` Java callback interfaces) is implemented here either. All of this is
addable later with the same JNI patterns already in `src/lib.rs` (`JavaDimseLogger` is already a
working example of calling back into Java from a blocking native call) - it was left out of this
first pass to keep the amount of new JNI surface area reviewable and heavily testable in one
change, not because of any technical blocker. `exec/dcmtalk`'s own `storescu`/`findscu`/`movescu`
CLI subcommands, or the Node/Python bindings, remain the options for an early-cancel SCU call or
an in-process DICOM SCP today.

## Design notes

### Why JSON for options, not results

Every optional/structured *argument* (an `Options` object, a `List<String>` of tags/files, a
`Map<String,String>` query, ...) crosses the JNI boundary as a single JSON string, built by a
small internal encoder (`com.pohcee.dcmnorm.internal.Json`) and parsed Rust-side with
`serde_json`. This is not how the Node or Python bindings do it (they pass structured options as
native JS objects/Python keyword arguments, via napi-rs's/PyO3's own codegen) - but Java/JNI has
no such codegen: every field of every options shape would otherwise need its own hand-written
`GetFieldID`/`GetObjectField` call pair, with the method/field signature encoded as a string
(`"(Ljava/lang/String;)Ljava/lang/Object;"` and friends) that the compiler cannot check. A typo
there doesn't fail to compile - it returns a null id, and calling through a null id crashes the
JVM rather than raising a catchable exception. A single, small, well-tested JSON codec bounds
that failure mode to one reviewable place instead of spreading it across a dozen options
structs - and this project already "talks JSON everywhere" (see the main README and both other
bindings' own docs), so this is not a foreign idiom here, just an unusually direct application
of it.

*Results* don't need this: a result without a binary payload (`StoreScuResult`, `MoveScuResult`,
the plain `readTags`/`readJson`/`findScu` JSON-text returns) is either trivial to hand-construct
from native code or is already meant to be JSON text for the caller to parse themselves; a result
*with* a binary payload (`RenderedFrame`, `RenderedMovie`, `TextureExportResult`) still crosses as
one real `NewObject` JNI call each, with exactly one constructor signature
(`(String metadataJson, byte[] data)`) to get right - the metadata half happens to be JSON-encoded
too (reusing `TextureMeta::to_json()` outright for `TextureExportResult`, the single source of
truth that payload shape already has), but the *mechanism* for getting the result back into Java
is a plain, ordinary JNI object construction, not this binding's JSON convenience.

### Why `DicomVolumeHandle` needs `close()`

`Dcmnorm.buildVolume` returns a handle wrapping a native `Arc<Volume>` (a Rust-heap-allocated
value, leaked into a raw pointer boxed as a Java `long`) rather than a Java object whose lifetime
the JVM manages for you. napi-rs ties a Node-side class instance's native data to V8's own GC;
PyO3 ties a `#[pyclass]` instance to Python's own refcounting - both runtimes call back into the
native library's destructor logic as part of collecting the host-language object. The JVM has no
equivalent hook a native library can rely on running *promptly* (a plain `finalize()`/
`Cleaner`-based approach would eventually free it, but "eventually, if ever, under GC pressure
nobody controls" is not an acceptable way to bound how much decoded volume data - potentially
hundreds of MB per series - stays resident), so `DicomVolumeHandle` is a plain `AutoCloseable`
instead: call `close()` (ideally via try-with-resources) exactly once when done with a volume.
Every method after `close()` throws `IllegalStateException` rather than silently reading freed
native memory.

## Packaging

Like `bindings/node` and `bindings/python`, this isn't published to Maven Central. Consumers are
expected to depend on the jar `build-in-docker.sh` produces (e.g. via a local
`<systemPath>`/`mvn install:install-file` into their own local repository, or a Gradle
`files(...)` dependency) rather than building from source - a consumer's Docker builder stage may
have no Rust toolchain of its own.

The jar bundles the compiled `libdcmnorm_java.so` at
`src/main/resources/native/linux-x64/libdcmnorm_java.so`, extracted to a temp file and
`System.load()`-ed at class-init time (`com.pohcee.dcmnorm.internal.NativeLoader`) - falling back
to a plain `System.loadLibrary("dcmnorm_java")` lookup on `java.library.path` for local
development, where the `.so` was just built straight into a directory already on that path (see
`build.sh`). **Only linux-x64 is currently packaged** - a consumer on another OS/architecture
needs their own native build on `java.library.path`.

`build-in-docker.sh` builds inside a `debian:bookworm-slim` container rather than on the host,
specifically to match that image's glibc (2.36) - the same baseline `bindings/node`'s
`node:22-slim` and `bindings/python`'s `python:3.12-slim-bookworm` build containers already pin
to, so all three bindings' native code stays safe on the same deploy targets. Building on an
arbitrary host risks a `GLIBC_X.XX not found` failure that only surfaces once deployed. `./build.sh`
(plain host build) is for fast local iteration only - don't commit its output; only the
`.so`/jar `build-in-docker.sh` produces is meant to be committed.

The [`Build Bindings`](../../.github/workflows/build-bindings.yml) GitHub Actions workflow runs
`build-in-docker.sh` (for all three bindings) and commits whatever changed - triggered the same
way [`release.yml`](../../.github/workflows/release.yml) is, by a pushed `v*.*.*` tag (i.e.
whenever [`semver-tag.yml`](../../.github/workflows/semver-tag.yml) cuts a new version), as well
as on manual dispatch - this is what actually produces and commits
`src/main/resources/native/linux-x64/libdcmnorm_java.so` in practice, rather than a maintainer
needing to run the script by hand from a machine with Docker.

**Note on the state of this packaging step:** `build-in-docker.sh` has been written to the same
pattern as the other two bindings' own scripts but, unlike theirs, had not been run against a
real Docker daemon as of this binding's own first version (the environment it was developed in
had no Docker daemon available) - `build.sh` (the plain host build) and the full smoke test *did*
run and pass there. The first real run of the `Build Bindings` workflow (the next time a version
is tagged, or a manual `workflow_dispatch` run) is effectively that validation - if it fails, the
`build-java` job's log is the place to start (most likely culprit: an apt package name that's
moved since this was written, or a step that assumed something about the container beyond what
`debian:bookworm-slim` actually provides).
