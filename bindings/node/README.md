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
