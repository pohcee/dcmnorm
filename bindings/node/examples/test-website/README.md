# dcmnorm-node test website

A minimal Express app that exercises the `@pohcee/dcmnorm-node` bindings end to end: upload a
`.dcm` file (or pick one of six bundled sample files) in the browser, and it's processed by a
small API that validates it, reads its JSON metadata, renders every frame (multiframe-aware) to
a small JPEG thumbnail, and reports timings for each step. Everything is rendered back on the
page - metadata, frame thumbnails, and a timings table. Click any thumbnail to re-render that
frame at a larger size (up to 1024x1024, aspect ratio preserved, never upscaled past the
source's own resolution) in a lightbox. The header/About tab also report the exact `dcmnorm` and
`dcmnorm-node` versions the deployment is running, and link back to the OSS project at
https://github.com/pohcee/dcmnorm.

Not a production service - it's a manual/demo test harness for the Node bindings.

## Run locally

```sh
npm install   # resolves @pohcee/dcmnorm-node via the file:../.. reference to bindings/node
npm start
```

Then open http://localhost:3000. Sample files are read straight from the repo's own
`test/files/` in this mode (no copy step needed) - see `CURATED_SAMPLES` in `server.js`.

## API

`POST /api/process` - multipart form field `dicomFile` (must have a `.dcm` extension).
`POST /api/samples/:name/process` - same pipeline, run against one of the bundled sample files
listed by `GET /api/samples` (an array of `{ name, description, sizeBytes }`) instead of an
upload. Both return:

```json
{
  "kind": "upload",
  "uploadId": "d7ae56d7-30ad-4c7f-a83f-07960d08ad23",
  "fileName": "mr.dcm",
  "fileSizeBytes": 526260,
  "hasPixelData": true,
  "numberOfFrames": 1,
  "framesRendered": 1,
  "truncated": false,
  "metadata": { "...": "full readJson() output" },
  "frames": [{ "index": 0, "timeMs": 19.5, "width": 320, "height": 320, "mimeType": "image/jpeg", "dataUrl": "data:image/jpeg;base64,..." }],
  "timings": { "validateMs": 0.2, "metadataMs": 1.4, "renderTotalMs": 21.3, "renderAvgMs": 21.3, "totalMs": 23.1 }
}
```

`hasPixelData` is `false` for a non-image SOP class - a Structured Report, encapsulated PDF, or
RTSTRUCT, none of which have `Rows`/`Columns` at all - rather than 500ing (renderFrame would
otherwise reject every frame with "missing required image attribute: Rows"), the response comes
back as a normal 200 with `numberOfFrames`/`framesRendered` both `0`, `frames: []`, and no
`renderTotalMs`/`renderAvgMs` in `timings`; the metadata itself is still the file's full
`readJson()` output. The UI shows this as "Frames (0 of 0)" plus an explanatory note instead of
an empty grid.

Frames in that response are thumbnails, downscaled (never upscaled) to `THUMBNAIL_MAX_DIM`
(320px on the longer side) so a many-frame multiframe file stays light to send. `kind` is
`"upload"` (with `uploadId`) or `"sample"` - it tells the client which of the two "render this
frame larger" endpoints below to call:

- `GET /api/samples/:name/frames/:index?maxDim=1024` - bundled samples never expire.
- `GET /api/uploads/:id/frames/:index?maxDim=1024` - `:id` is the `uploadId` from the initial
  `/api/process` response; the uploaded file (and this endpoint) stops working
  `UPLOAD_RETENTION_MS` (default 10 minutes) after that response, at which point this 404s.

Both return `{ index, timeMs, width, height, mimeType, dataUrl }` for just that one frame,
resized so its longer side is at most `min(maxDim, FULL_MAX_DIM)` - `FULL_MAX_DIM` (1024) is a
hard server-side cap regardless of what `maxDim` a client asks for, and neither route ever
upscales past the source image's own native resolution.

`GET /api/version` - `{ dcmnormVersion, dcmnormNodeVersion }`, the versions shown in the UI.

`MAX_FRAMES_RENDERED` (env var, default 256) caps how many frames a single huge multiframe file
will render, to keep the response/memory bounded; `truncated: true` says when that cap was hit.
`MAX_UPLOAD_BYTES` (default 1GB locally; the Cloud Run deploy script below tightens this to stay
under Cloud Run's own 32MB request-body limit) caps upload size.

## Docker

Build context must be the **dcmnorm repo root**, not this directory or `bindings/node` - the
image needs the node binding, this app, and a curated handful of `test/files/*.dcm` fixtures
(see `CURATED_SAMPLES` in `server.js`), which only a repo-root context can see all of at once:

```sh
cd ../../../..   # dcmnorm repo root, from here
docker build -f bindings/node/examples/test-website/Dockerfile -t dcmnorm-test-website .
docker run --rm -p 3000:3000 dcmnorm-test-website
```

Then open http://localhost:3000.

The image is `node:22-slim` with no Rust toolchain - it copies the addon's already-committed
`.node` binary (see `../../README.md`'s Packaging section for why it's prebuilt rather than
compiled at image build time) rather than rebuilding it. `Dockerfile.dockerignore` (next to the
Dockerfile) overrides the repo's root `.dockerignore` for this build only, since that root file
excludes `test/`/`*.json`/`*.dcm` for the main Rust CLI build - exactly what this Dockerfile
needs to copy.

## Deploy to Cloud Run

`deploy-cloudrun.sh` builds the image above and deploys it as a public
(`--allow-unauthenticated`) Cloud Run service:

```sh
gcloud auth login   # once
./deploy-cloudrun.sh <PROJECT_ID> [REGION] [SERVICE_NAME]
# e.g.
./deploy-cloudrun.sh my-gcp-project us-central1 dcmnorm-test-website
```

It creates an Artifact Registry Docker repo if one doesn't already exist, builds and pushes the
image there, then runs `gcloud run deploy` with `--allow-unauthenticated`. Requires the `gcloud`
CLI (authenticated) and Docker; the script prints the live service URL when done. See the
script's own header comment for the full list of configurable env vars (region, service name,
Artifact Registry repo name, upload size cap).

### Continuous deploy

`.github/workflows/deploy-test-website.yml` redeploys the public instance
(`dcmnorm-test-website` in `safebridge-sandbox`) automatically whenever the Node binding changes
on `main`: any push touching `bindings/node/**`, and every successful Build Bindings run (whose
bot commit can't fire a `push` trigger itself). It can also be run by hand via
`workflow_dispatch`. Auth is keyless Workload Identity Federation - no SA key or repo secret -
and the job finishes by checking that `/api/version` reports the `package.json` version it just
built. The script above remains for manual/other-project deploys.
