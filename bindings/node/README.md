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

## Packaging

This is consumed entirely within the parent monorepo via `file:` references
(`shared-client` depends on `file:../dcmnorm/bindings/node`, the same pattern
edge services use for `shared-client` itself) — not published to any npm
registry, and not set up with the cross-platform prebuild + `optionalDependencies`
scheme napi-rs projects typically use for standalone/public distribution
(sharp, esbuild, swc). There's no need for that machinery here: every consumer
lives in the same repo at a fixed relative path, exactly like every other local
`file:` dependency in this project.

**Any service that needs this transitively through `shared-client` (i.e. via
`shared-client/dicom`) must also list `@pohcee/dcmnorm-node` as its own direct
`file:` dependency** (see edge/insert or edge/find-dicom's package.json) — this
looks redundant with shared-client's own dependency on it, but it isn't: the
edge services' Docker builder stage installs with `npm install --install-links`
(see Docker.tmpl) so the final image doesn't need symlinks back into the
source tree, and `--install-links` does not correctly re-resolve a `file:`
dependency's *own* relative `file:` dependency after hard-copying it — it was
observed trying to read `node_modules/dcmnorm/bindings/node/package.json`
(relative to the copy's new location) instead of the real path. A plain
(non-`--install-links`) `npm install`, like every local dev/test flow in this
repo uses, resolves the nested reference fine via symlinks - the bug is
specific to `--install-links`. Verified end-to-end against the actual
Docker.tmpl builder stage; if a future npm version fixes this, the direct
dependency becomes harmless-but-unnecessary duplication rather than something
required for correctness.

The compiled `dcmnorm-node.linux-x64-gnu.node` binary **is committed** to this
repo (see `.gitignore`) rather than built at Docker-image time, since the
service Dockerfiles' builder stage is plain `node:22-slim` with no Rust
toolchain. `npm run build:docker` (`build-in-docker.sh`) builds it inside a
`node:22-slim` container rather than on the host, specifically to match that
image's glibc — building on an arbitrary host risks a `GLIBC_X.XX not found`
failure that only surfaces once deployed. `npm run release`'s `before:init`
hook runs `build:docker` (needs Docker available wherever a release is cut)
followed by `npm test`, so cutting a release always ships a binary that's
actually safe for the deploy target — you don't need to remember to do this
by hand. `build`/`build:debug` (plain host builds) are for fast local
iteration only; don't commit their output.
