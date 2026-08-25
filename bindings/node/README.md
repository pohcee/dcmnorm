# @pohcee/dcmnorm-node

Native Node.js bindings for dcmnorm, built with [napi-rs](https://napi.rs). Calls
straight into the `dcmnorm` lib crate in-process (no CLI subprocess, no stdio/JSON
round trip) with the blocking work offloaded to libuv's threadpool so it doesn't
block the JS event loop.

## Build

```sh
npm install
npm run build        # release build -> dcmnorm-node.<platform>.node + index.js/index.d.ts
npm run build:debug  # debug build, faster to iterate
npm run build:docker # release build inside node:22-slim - see Packaging below
npm test             # runs test/smoke.js against the fixtures in ../../test/files
```

## API

- `readTags(filePath, tags: string[]): Promise<string>` — JSON (flat, hex-keyed,
  bulk data as a URI reference) containing only the requested tags. Stops
  parsing right after the highest requested tag, same fast path as `dcmnorm
  --filter`. Filtering for a bulk-data-eligible tag (e.g. `PixelData`) falls
  back to inlining it rather than a URI reference, unlike `readJson` below -
  see the comment on `ReadTagsTask` in `src/lib.rs`.
- `readJson(filePath, options?: { format?: 'flat'|'standard', keyStyle?: 'name'|'hex', bulkData?: 'uri'|'inline' }): Promise<string>`
  — full-file JSON dump. `bulkData` defaults to `'uri'`, matching the CLI's own
  default (`--bulk-data uri`) - not the Rust library's own default, which is
  `'inline'`. Getting this wrong makes a huge difference: `'inline'`
  base64-embeds elements like `PixelData` directly, ~1000x larger output for a
  typical image, instead of a small `"?offset=..&length=.."` reference.
- `editTags(filePath, options?: { outputPath?, set?: Record<string,string>, remove?: string[], removePrivateTags?: boolean }): Promise<void>`
  — set/remove attributes; writes back in place unless `outputPath` is given.
- `transcode(filePath, outputPath, transferSyntaxUid): Promise<void>`.
- `checkDicom(filePath): Promise<boolean>`.
- `renderFrame(filePath, options?: { format?: 'jpeg'|'png', outputWidth?, outputHeight?, windowCenter?, windowWidth?, frameIndex?, jpegQuality?, showOverlays?: boolean, overlayIndex?: number, overlayColor?: string }): Promise<{ mimeType, width, height, data: Buffer, overlays: OverlaySummary[], selectedOverlayIndex?: number }>`
  — renders a single frame to JPEG or PNG. If the instance has one or more DICOM overlay planes
  (group `60xx`), the first available overlay composites onto the image by default; `overlayIndex`
  selects a different one (0-based, by `OverlaySummary.index`, matching the CLI's
  `--overlay-index`), `showOverlays: false` disables overlay rendering, and `overlayColor` (`"R,G,B"`
  or `"#RRGGBB"`, default green) sets the fill color. `overlays` in the result always lists every
  overlay present on the instance (even when none was rendered), so a caller can offer overlay
  selection without a separate metadata call; `selectedOverlayIndex` says which one (if any) is
  actually in `data`. `OverlaySummary` is `{ index, group, rows, columns, overlayType?, label? }`.
- `renderMovie(filePath, options?: { outputWidth?, outputHeight?, windowCenter?, windowWidth?, fps? }): Promise<{ mimeType, data: Buffer }>`
  — renders every frame of a multiframe instance to an MP4 (requires `ffmpeg` on `PATH`). Does not
  currently support the overlay options `renderFrame` does.
- `computeHistogram(filePath, options?: { binCount?, frameIndex?, minValue?, maxValue? }): Promise<FrameHistogram[]>`
  — computes a pixel-value histogram per frame, over the same modality-LUT-applied grayscale
  values `renderFrame` decodes from (so e.g. a CT frame's bins are in Hounsfield units). Mirrors
  the CLI's `--histogram`/`--histogram-bins`/`--histogram-frame`/`--histogram-min`/`--histogram-max`
  — see the main [README](../../README.md#compute-a-pixel-histogram-with---histogram) for the
  field-by-field output shape. `binCount` defaults to 256; `frameIndex` (0-based) restricts the
  result to one frame instead of every frame in the instance; `minValue`/`maxValue` (must be set
  together) pin the bin range instead of defaulting to each frame's own observed min/max.
  `FrameHistogram` is `{ frameIndex, binCount, rangeMin, rangeMax, binWidth, counts: number[],
  pixelCount, minValue, maxValue, mean, stdDev }`.

### MPR (Multiplanar Reformation)

- `buildVolume(filePaths: string[]): Promise<DicomVolumeHandle>` — reads and decodes every slice
  of a parallel stack (e.g. one CT/MR/PT series), spatially re-sorted by `ImagePositionPatient`
  regardless of input order. Rejects fewer than 2 files, mismatched Rows/Columns, or a
  non-parallel/gantry-tilt-inconsistent stack. This is the expensive step — build once per series
  and keep the returned handle resident (e.g. in a volume cache) so every subsequent `reformat()`
  call is cheap.
- `DicomVolumeHandle` — an opaque, read-only handle around the built volume:
  - getters: `rows`, `cols`, `numSlices`, `nativeBasis` (`[rowDir(3), colDir(3)]`, the volume's own
    acquisition-native orientation — a reasonable seed for an "axial" reformat), `center`
    (`[x,y,z]` LPS mm, the volume's physical center), `minSpacingMm` (its smallest voxel
    dimension, a reasonable default output spacing)
  - `reformat(options): Promise<RenderedFrame>` — resamples one plane through the volume and
    encodes it exactly like `renderFrame`'s output shape, so callers reuse their existing
    image-display code path. `options`: `{ origin: number[3], rowDir: number[3], colDir: number[3],
    outputWidth, outputHeight, spacingMm, windowCenter?, windowWidth?, format?: 'jpeg'|'png',
    jpegQuality?, interpolation?: 'trilinear'|'nearest', slabThicknessMm?, slabProjection?:
    'mip'|'minip'|'average' }`. `interpolation` defaults to `'trilinear'`; use `'nearest'` (faster)
    for a live-drag preview frame. `slabThicknessMm` (default 0, an infinitely-thin plane) turns on
    a thick-slab reformat centered on `origin`, combined per `slabProjection` (default `'mip'`).
  - `exportTexture(options?): Promise<TextureExportResult>` — see
    [Texture export](#texture-export) below.

### Texture export

Packs a volume, a single frame, or several independent frames as a lossless, GPU-upload-ready
payload (16-bit samples, row-major, optionally gzip-compressed) instead of an 8-bit windowed
render — the client does its own window/level and oblique reslicing in a GPU shader instead of
round-tripping to the server per interaction. Mirrors the CLI's `--output-type texture`/`.gputex`
(`exportTexture`/`exportFrameTexture`) — see the main [README](../../README.md#export-a-gpu-texture-gputex)
and `dcmnorm::dicom_io::texture_export`'s own module doc for the full format contract.

- `DicomVolumeHandle.exportTexture(options?: { targetMaxDim?, compression?: 'gzip'|'none', windowCenter?, windowWidth? }): Promise<TextureExportResult>`
  — packs the volume's own NATIVE voxel lattice (not a resampled oblique plane — that's
  `reformat()`). `targetMaxDim` caps the longest of width/height/depth, proportionally
  downsampling (trilinear) if the native volume exceeds it; omitted means full native resolution.
  `compression` defaults to `'gzip'`. `windowCenter`/`windowWidth` are purely informational,
  carried through to the result for the client's initial render — the exported samples are never
  windowed.
- `exportFrameTexture(filePath, options?: { frameIndex?, targetMaxDim?, compression?, windowCenter?, windowWidth? }): Promise<TextureExportResult>`
  — packs one decoded 2D frame as a depth-1 "1-slice volume" texture, so a large diagnostic 2D
  image (DX/CR/mammography) can reuse the same client GPU texture/shader pipeline as an MPR
  volume. `frameIndex` defaults to 0.
- `exportFrameStackTexture(sources: FrameStackSource[], options?: { compression?, windowCenter?, windowWidth? }): Promise<TextureExportResult>`
  — packs several independent original frames (no resampling, no cross-layer interpolation, no
  physical geometry) as one texture-array upload: a cine/multiframe instance supplies one source
  with several `frameIndices` (its file is parsed once), a multi-image series supplies one source
  per instance file (`frameIndices` defaulting to `[0]`). `FrameStackSource` is `{ filePath,
  frameIndices?: number[] }`. The result's layer order is the flattened source order followed by
  each source's own `frameIndices` order — callers must supply sources in the exact order the
  client's own frame/instance index expects. This has no CLI equivalent — it's Node-bindings-only.
- `TextureExportResult`: `{ contentKind: 'volume'|'image2d'|'framestack', sampleFormat:
  'int16'|'uint16', compression: 'none'|'gzip', lossless, width, height, depth, rescaleSlope,
  rescaleIntercept, rowSpacingMm, colSpacingMm, sliceSpacingMm, origin: number[3], rowDir:
  number[3], colDir: number[3], normalDir: number[3], defaultWindowCenter?, defaultWindowWidth?,
  nativeWidth, nativeHeight, nativeDepth, downsampled, payloadBytesRaw, payloadBytesStored, data:
  Buffer }`. `texel * rescaleSlope + rescaleIntercept` recovers the physical value (e.g. HU).
  Geometry fields (`rowSpacingMm`/`origin`/`rowDir`/etc.) carry no meaning for `contentKind:
  'framestack'` — only `'volume'` makes a real spatial claim.

All return values that carry data are JSON strings — parse them JS-side. This
sidesteps a napi-rs constraint (`Task::JsValue` requires `TypeName`, which
`serde_json::Value` doesn't implement) and matches how the CLI already talks
JSON everywhere else in this project.

Every entry point runs through `catch_unwind` (see `guarded()` in `src/lib.rs`):
DICOM files come from arbitrary, sometimes-malformed vendor equipment, and unlike
a subprocess call, a panic in an in-process addon takes the whole host process
down unless it's caught at the FFI boundary.

## Packaging

This isn't published to any npm registry, and isn't set up with the
cross-platform prebuild + `optionalDependencies` scheme napi-rs projects
typically use for standalone/public distribution (sharp, esbuild, swc).
Consumers are expected to pull it in via a `file:` reference to this
directory (e.g. a submodule checked out at a fixed relative path) rather than
installing from a registry.

**Watch out for `npm install --install-links`:** it hard-copies `file:`
dependencies instead of symlinking them, and it does not correctly re-resolve
a `file:` dependency's *own* relative `file:` dependency after hard-copying
it — it can end up trying to read this package's `package.json` relative to
the copy's new location instead of the real path. A plain (non-`--install-links`)
`npm install` resolves a nested `file:` reference fine via symlinks - the bug
is specific to `--install-links`. If a consumer's install uses that flag and
only depends on this package transitively (through another local package),
list it as a direct `file:` dependency too so npm resolves it directly
instead of transitively; if a future npm version fixes the underlying bug,
the direct dependency becomes harmless-but-unnecessary duplication rather
than something required for correctness.

The compiled `dcmnorm-node.linux-x64-gnu.node` binary **is committed** to this
repo (see `.gitignore`) rather than built at Docker-image time, since a
consumer's Docker builder stage may be plain `node:22-slim` with no Rust
toolchain. `npm run build:docker` (`build-in-docker.sh`) builds it inside a
`node:22-slim` container rather than on the host, specifically to match that
image's glibc — building on an arbitrary host risks a `GLIBC_X.XX not found`
failure that only surfaces once deployed. `npm run release`'s `before:init`
hook runs `build:docker` (needs Docker available wherever a release is cut)
followed by `npm test`, so cutting a release always ships a binary that's
actually safe for the deploy target — you don't need to remember to do this
by hand. `build`/`build:debug` (plain host builds) are for fast local
iteration only; don't commit their output.
