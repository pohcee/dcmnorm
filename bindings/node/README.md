# dcmnorm (Node.js binding)

Native Node.js bindings for dcmnorm, built with [napi-rs](https://napi.rs). Calls
straight into the `dcmnorm` lib crate in-process (no CLI subprocess, no stdio/JSON
round trip) with the blocking work offloaded to libuv's threadpool so it doesn't
block the JS event loop.

## Demo / test website

**Live demo: https://dcmnorm-test-website-spzcbrdjlq-uc.a.run.app**

`examples/test-website` is a small Express app that exercises these bindings end to end through
a browser UI: upload a `.dcm` file (or pick one of five bundled samples) and it renders the
file's JSON metadata, a JPEG of every frame, and per-step timings back on the page. See
[`examples/test-website/README.md`](examples/test-website/README.md) for how to run it locally,
build its Docker image, and deploy it to Cloud Run.

## Install

```sh
npm i dcmnorm
```

```js
const dcmnorm = require("dcmnorm");
const json = await dcmnorm.readJson("image.dcm");
```

Prebuilt for Linux x64 and arm64 (glibc 2.31+: Ubuntu 20.04+, Debian 11+), macOS arm64 (11+) and
x64 (10.15+), and Windows x64, with every codec linked in (FFmpeg included, so the video transfer
syntaxes need no `ffmpeg` command). npm installs only your platform's addon; see Packaging below.
Node.js 18 or newer.

## Build

```sh
npm install
npm run build        # release build -> dcmnorm-node.<platform>.node + index.js/index.d.ts
npm run build:debug  # debug build, faster to iterate
npm run build:docker # Linux release build inside node:22-bullseye-slim (glibc 2.31) - see Packaging below
npm test             # runs test/smoke.js against the fixtures in ../../test/files
```

## API

- `readTags(filePath, tags: string[]): Promise<string>` — JSON (flat, hex-keyed,
  bulk data as a URI reference) containing only the requested tags. Stops
  parsing right after the highest requested tag, same fast path as `dcmnorm
  --filter`. Filtering for a bulk-data-eligible tag (e.g. `PixelData`) falls
  back to inlining it rather than a URI reference, unlike `readJson` below -
  see the comment on `ReadTagsTask` in `src/lib.rs`.
- `readJson(filePath, options?: { format?: 'flat'|'standard', keyStyle?: 'name'|'hex', bulkData?: 'uri'|'inline', omitKeywordVm?: boolean }): Promise<string>`
  — full-file JSON dump. `bulkData` defaults to `'uri'`, matching the CLI's own
  default (`--bulk-data uri`) - not the Rust library's own default, which is
  `'inline'`. Getting this wrong makes a huge difference: `'inline'`
  base64-embeds elements like `PixelData` directly, ~1000x larger output for a
  typical image, instead of a small `"?offset=..&length=.."` reference.
  `omitKeywordVm: true` drops the non-standard `Keyword` and `VM` members from
  `format: 'standard'` output, leaving only what PS3.18 Annex F defines.
- `editTags(filePath, options?: { outputPath?, set?: Record<string,string>, remove?: string[], removePrivateTags?: boolean }): Promise<void>`
  — set/remove attributes; writes back in place unless `outputPath` is given.
- `transcode(filePath, outputPath, transferSyntaxUid): Promise<void>`.
- `checkDicom(filePath): Promise<boolean>`.
- `writeDicomdir(entries: { fileId: string[], json?: string, format?: 'flat'|'standard', path?: string, transferSyntaxUid?: string }[], outputPath, options?: { fileSetId?, sopInstanceUid? }): Promise<{ records, recordTypes: { recordType, count }[], synthesized: { recordType, entity, attribute, value, source? }[], warnings: string[] }>`
  — writes a DICOMDIR (Media Storage Directory, Explicit VR Little Endian) for a File-set, the
  in-process form of `dcmnorm dicomdir ROOT`. Each entry is one file of the File-set: its File ID
  components (1-8 of 1-8 characters from `A-Z 0-9 _`, PS3.10 8.2) and its header, either as the
  DICOM JSON `readJson` returns (`json`; BulkDataURI elements are ignored, so the default `'uri'`
  output is fine) or a file to read it from (`path`; pixel data isn't loaded). With `json`, the
  referenced file's transfer syntax is `transferSyntaxUid`, else the JSON's TransferSyntaxUID,
  else Explicit VR Little Endian. Records are PATIENT > STUDY > SERIES > one per entry, typed by
  SOP Class (IMAGE, SR DOCUMENT, KEY OBJECT DOC, PRESENTATION, ENCAP DOC, ...; PS3.3 F.5).
  `synthesized` lists the Type 1 keys the instances lacked and the values written (IHE PDI
  Table 4.47.4-1: Patient ID, Study ID, Study Date/Time, Modality, Series Number, Instance
  Number), with `source` naming the attribute a value was copied from, if any. Rejects a bad File
  ID or File-set ID (at most 16 of `A-Z 0-9 _`), a Patient ID shared by two patients, a study
  under two patients, a series in two studies, and duplicate files or SOP Instance UIDs.
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
  result to one frame instead of every frame in the instance (and, like `renderFrame`, reads only
  that frame's bytes of a native multi-frame file, and decodes only that frame of a compressed
  one); `minValue`/`maxValue` (must be set
  together) pin the bin range instead of defaulting to each frame's own observed min/max.
  `FrameHistogram` is `{ frameIndex, channel: 'red'|'green'|'blue'|null, binCount, rangeMin,
  rangeMax, binWidth, counts: number[], pixelCount, minValue, maxValue, mean, stdDev }`.
  `channel` is `null` for a grayscale frame; an RGB/color frame (YBR variants converted to RGB
  first) instead resolves to **three** entries per frame index, one per channel, each over that
  channel's own 8-bit (0-255) samples with the bin range defaulted to the full 0-255 domain
  rather than each channel's own min/max.

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

Published to npm by the [`Build and release`](../../.github/workflows/release.yml) workflow on
every `v*.*.*` release tag, with the release version, in the per-platform layout esbuild and the
napi-rs projects use:

| Package | Contents |
| --- | --- |
| `dcmnorm` | `index.js` (the loader), `index.d.ts`, README, LICENSE; `optionalDependencies` on the five below |
| `@dcmnorm/linux-x64-gnu` | `dcmnorm-node.linux-x64-gnu.node` only (`os: linux`, `cpu: x64`, `libc: glibc`) |
| `@dcmnorm/linux-arm64-gnu` | `dcmnorm-node.linux-arm64-gnu.node` only (`os: linux`, `cpu: arm64`, `libc: glibc`) |
| `@dcmnorm/darwin-arm64` | `dcmnorm-node.darwin-arm64.node` only (`os: darwin`, `cpu: arm64`) |
| `@dcmnorm/darwin-x64` | `dcmnorm-node.darwin-x64.node` only (`os: darwin`, `cpu: x64`) |
| `@dcmnorm/win32-x64-msvc` | `dcmnorm-node.win32-x64-msvc.node` only (`os: win32`, `cpu: x64`) |

npm skips the optional dependencies whose `os`/`cpu`/`libc` don't match, so an install downloads
one addon (about 12 MB packed, 31 MB unpacked on Linux x64). `index.js` (generated by napi-rs)
first tries a `dcmnorm-node.<platform>.node` next to itself, which is what a local build produces,
then `@dcmnorm/<platform>`. napi-rs names platform packages `<name>-<platform>`, so
`npm run build` (and `build-in-docker.sh`) runs `node prepare-npm.js --fix-loader` to rewrite the
generated requires to the `@dcmnorm` scope; CI fails if a committed `index.js` misses that.

`prepare-npm.js` lays the packages out for a release (`--artifacts <dir>` with every platform's
`.node`) and checks them with `npm pack --dry-run`: exact file lists, versions and
`optionalDependencies` in step. Pull requests run it with `--placeholder` (empty stand-in addons),
so the package metadata can't drift between releases. No `.node` file is committed; `npm/`,
`artifacts/` and the copied `LICENSE` are build outputs (`.gitignore`).

How each addon is built (all with FFmpeg linked in statically and the same codec set; a codec that
fails to build fails the release, there is no fallback to a smaller feature set):

- **Linux** (x64 on `ubuntu-24.04`, arm64 on `ubuntu-24.04-arm`): `build-in-docker.sh` builds in a
  `node:22-bullseye-slim` container (glibc 2.31, GCC 10), so the addon needs at most GLIBC 2.31 /
  GLIBCXX 3.4.28; the script fails if a newer symbol version or a dynamically linked FFmpeg sneaks
  in, then runs `test/smoke.js` and `load-check.js` inside the container. A plain `npm run build`
  links the host's glibc and is for local iteration only.
- **macOS** (arm64 on `macos-15`, deployment target 11.0; x64 on `macos-15-intel`, 10.15):
  `napi build` with `packaging/macos-link.py` as the linker (ffmpeg-sys-next emits GNU-only link
  flags there) and `packaging/portable-cc` on `PATH`, then `load-check.js`.
- **Windows** (`windows-latest`, MSVC): MSVC can't run FFmpeg's `./configure` + `make`, so the
  workflow installs a pinned vcpkg `ffmpeg` (`x64-windows-static-md`: static, LGPL-only, native
  codecs; `.github/actions/ffmpeg-vcpkg`) and builds with `ffmpeg-codec-linked`. The job fails if
  the `.node` imports any FFmpeg DLL, then runs `load-check.js`. By hand: install that vcpkg
  package, set `VCPKG_ROOT` and `VCPKGRS_TRIPLET=x64-windows-static-md`, then

  ```sh
  npx napi build --platform --release --target x86_64-pc-windows-msvc --no-default-features --features "ffmpeg-codec-linked,jpeg-ls-codec,jpeg-xl-codec,jpeg2000-openjpeg-encode"
  ```

Every Linux and macOS build compiles FFmpeg through `packaging/portable-cc`, which drops the
`-march=native` ffmpeg-sys-next would otherwise configure (it tunes FFmpeg's C code for the build
machine's CPU, which then crashes on older CPUs).

Publishing setup (npm org, trusted publisher, the `NPM_PUBLISH` variable):
[`packaging/README.md`](../../packaging/README.md).
