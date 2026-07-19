# @pohcee/dcmnorm-node

Native Node.js bindings for dcmnorm, built with [napi-rs](https://napi.rs). Calls
straight into the `dcmnorm` lib crate in-process (no CLI subprocess, no stdio/JSON
round trip) with the blocking work offloaded to libuv's threadpool so it doesn't
block the JS event loop.

## Build

```sh
npm install
npm run build       # release build -> dcmnorm-node.<platform>.node + index.js/index.d.ts
npm run build:debug # debug build, faster to iterate
npm test            # runs test/smoke.js against the fixtures in ../../test/files
```

## API

- `readTags(filePath, tags: string[]): Promise<string>` — JSON (flat, hex-keyed)
  containing only the requested tags. Stops parsing right after the highest
  requested tag, same fast path as `dcmnorm --filter`.
- `readJson(filePath, options?: { format?: 'flat'|'standard', keyStyle?: 'name'|'hex' }): Promise<string>`
  — full-file JSON dump.
- `editTags(filePath, options?: { outputPath?, set?: Record<string,string>, remove?: string[], removePrivateTags?: boolean }): Promise<void>`
  — set/remove attributes; writes back in place unless `outputPath` is given.
- `transcode(filePath, outputPath, transferSyntaxUid): Promise<void>`.
- `checkDicom(filePath): Promise<boolean>`.

All return values that carry data are JSON strings — parse them JS-side. This
sidesteps a napi-rs constraint (`Task::JsValue` requires `TypeName`, which
`serde_json::Value` doesn't implement) and matches how the CLI already talks
JSON everywhere else in this project.

Every entry point runs through `catch_unwind` (see `guarded()` in `src/lib.rs`):
DICOM files come from arbitrary, sometimes-malformed vendor equipment, and unlike
a subprocess call, a panic in an in-process addon takes the whole host process
down unless it's caught at the FFI boundary.

## Packaging status

This crate is wired up for local/in-monorepo use only so far: `napi build`
produces a single-platform `.node` binary you build yourself. It is **not**
yet set up for cross-platform prebuild + `optionalDependencies` publishing
(the standard napi-rs pattern used by sharp/esbuild/swc) — that's the
remaining work before a package that embeds this can be `npm install`ed
outside this monorepo checkout on an arbitrary platform.
