const express = require("express");
const multer = require("multer");
const fs = require("fs/promises");
const fsSync = require("fs");
const os = require("os");
const path = require("path");
const crypto = require("crypto");
const { performance } = require("perf_hooks");
const dcmnorm = require("@pohcee/dcmnorm-node");

const PORT = process.env.PORT || 3000;
const MAX_UPLOAD_BYTES = Number(process.env.MAX_UPLOAD_BYTES) || 1024 * 1024 * 1024; // 1GB
const MAX_FRAMES_RENDERED = Number(process.env.MAX_FRAMES_RENDERED) || 256;
const UPLOAD_DIR = path.join(os.tmpdir(), "dcmnorm-test-website-uploads");

// How long an uploaded file is kept on disk after its initial /api/process response, so a
// later "click a thumbnail to enlarge" request can still re-render that same frame at a higher
// resolution. Bundled samples never expire (they're a fixed part of the image), so this only
// applies to /api/uploads/:id/frames/:index.
const UPLOAD_RETENTION_MS = Number(process.env.UPLOAD_RETENTION_MS) || 10 * 60 * 1000;

// Thumbnails in the frame grid are capped small (fast to render, light to send, especially for
// a many-frame multiframe file); clicking one re-renders just that frame up to FULL_MAX_DIM.
// Neither ever upscales past the source image's own native resolution.
const THUMBNAIL_MAX_DIM = 320;
const FULL_MAX_DIM = 1024;

// Sample .dcm fixtures to offer alongside "upload your own file". `samples/` (populated by
// the Dockerfile, see its own comment) wins when present; falling back to the dcmnorm repo's
// own test/files lets this run straight from a checkout with no copy/build step.
//
// dcmnorm's test/files directory has 50+ fixtures, but most are deliberately-malformed edge
// cases for exercising the parser's error handling (truncated files, bad VRs, missing transfer
// syntax, ...) - useful for dcmnorm's own test suite, not for a first look at the library. This
// is a small, hand-picked set instead: real, valid files, each demonstrating a different shape
// (grayscale vs. color, single-frame vs. multiframe, a different modality), kept light enough
// to process quickly, with a description of what makes it worth trying.
const DOCKER_SAMPLES_DIR = path.join(__dirname, "samples");
const REPO_SAMPLES_DIR = path.join(__dirname, "..", "..", "..", "..", "test", "files");
const SAMPLES_DIR =
  process.env.SAMPLES_DIR || (fsSync.existsSync(DOCKER_SAMPLES_DIR) ? DOCKER_SAMPLES_DIR : REPO_SAMPLES_DIR);

const CURATED_SAMPLES = [
  {
    name: "ct.dcm",
    description: "CT slice - single-frame grayscale, 512x512, the most common baseline case.",
  },
  {
    name: "mr.dcm",
    description: "MR image - single-frame grayscale, 512x512.",
  },
  {
    name: "wsi.dcm",
    description: "Whole-slide-imaging tile stack - 96-frame color multiframe (digital pathology).",
  },
  {
    name: "us.dcm",
    description: "Secondary capture - single-frame color RGB, 600x430, a screenshot-style capture.",
  },
  {
    name: "dx2.dcm",
    description: "Digital X-ray - single-frame grayscale, high-resolution (1736x2022, 16-bit).",
  },
  {
    name: "sr.dcm",
    description: "Structured Report - no pixel data at all; shows the metadata-only path.",
  },
];

// The @pohcee/dcmnorm-node npm package version - always resolvable, since it's the very
// package we just required above.
const dcmnormNodeVersion = require("@pohcee/dcmnorm-node/package.json").version;

// The underlying dcmnorm Rust library version (root Cargo.toml's [package] version - the first
// `version = "..."` line in the file). The Dockerfile copies just that one file in alongside
// server.js (see its own comment) since a full Cargo.toml/workspace checkout isn't otherwise
// part of this image; local dev instead reads the real repo file directly.
const DOCKER_CORE_CARGO_TOML = path.join(__dirname, "dcmnorm-core-cargo.toml");
const REPO_CORE_CARGO_TOML = path.join(__dirname, "..", "..", "..", "..", "Cargo.toml");
function readDcmnormCoreVersion() {
  const cargoTomlPath = fsSync.existsSync(DOCKER_CORE_CARGO_TOML) ? DOCKER_CORE_CARGO_TOML : REPO_CORE_CARGO_TOML;
  try {
    const match = fsSync.readFileSync(cargoTomlPath, "utf8").match(/^version\s*=\s*"([^"]+)"/m);
    return match ? match[1] : null;
  } catch {
    return null;
  }
}
const dcmnormCoreVersion = readDcmnormCoreVersion();

const app = express();

const storage = multer.diskStorage({
  destination: async (_req, _file, cb) => {
    try {
      await fs.mkdir(UPLOAD_DIR, { recursive: true });
      cb(null, UPLOAD_DIR);
    } catch (err) {
      cb(err);
    }
  },
  filename: (_req, file, cb) => {
    cb(null, `${crypto.randomUUID()}-${path.basename(file.originalname)}`);
  },
});

const upload = multer({
  storage,
  limits: { fileSize: MAX_UPLOAD_BYTES },
  fileFilter: (_req, file, cb) => {
    if (path.extname(file.originalname).toLowerCase() !== ".dcm") {
      cb(new Error("Only .dcm files are accepted"));
      return;
    }
    cb(null, true);
  },
});

// Uploaded files kept around past their initial /api/process response, so a "click to enlarge"
// request can re-render a specific frame later without re-uploading. Each entry self-deletes
// (both the map entry and the on-disk file) after UPLOAD_RETENTION_MS.
const activeUploads = new Map();

function registerUpload(id, filePath) {
  const timeout = setTimeout(() => {
    activeUploads.delete(id);
    fs.unlink(filePath).catch(() => {});
  }, UPLOAD_RETENTION_MS);
  timeout.unref();
  activeUploads.set(id, { filePath, timeout });
}

function clampMaxDim(rawValue) {
  const parsed = parseInt(rawValue, 10);
  if (!Number.isFinite(parsed) || parsed <= 0) return FULL_MAX_DIM;
  return Math.min(parsed, FULL_MAX_DIM);
}

async function listSampleFiles() {
  const found = [];
  for (const sample of CURATED_SAMPLES) {
    try {
      const stat = await fs.stat(path.join(SAMPLES_DIR, sample.name));
      found.push({ ...sample, sizeBytes: stat.size });
    } catch {
      // Curated sample missing from SAMPLES_DIR (e.g. a stale list entry) - just skip it
      // rather than failing the whole listing.
    }
  }
  return found;
}

// Renders one frame, downscaled (never upscaled) so its longer side is at most maxDim - both
// output dimensions are computed here from the DICOM's own Rows/Columns and passed explicitly,
// rather than relying on renderFrame's own single-dimension aspect-ratio behavior, so callers
// get an exact, predictable cap regardless of orientation.
async function renderFrameCapped(filePath, frameIndex, nativeWidth, nativeHeight, maxDim) {
  const longestNativeSide = Math.max(nativeWidth || 0, nativeHeight || 0);
  const options = { frameIndex, format: "jpeg" };
  if (longestNativeSide > maxDim) {
    const scale = maxDim / longestNativeSide;
    options.outputWidth = Math.max(1, Math.round(nativeWidth * scale));
    options.outputHeight = Math.max(1, Math.round(nativeHeight * scale));
  }

  const renderStart = performance.now();
  const rendered = await dcmnorm.renderFrame(filePath, options);
  const timeMs = performance.now() - renderStart;

  return {
    index: frameIndex,
    timeMs,
    width: rendered.width,
    height: rendered.height,
    mimeType: rendered.mimeType,
    dataUrl: `data:${rendered.mimeType};base64,${Buffer.from(rendered.data).toString("base64")}`,
  };
}

// Runs the readJson -> renderFrame (thumbnail-sized) pipeline against an already-on-disk DICOM
// file, shared by both the upload route and the bundled-sample route.
async function processDicomFile(filePath) {
  const totalStart = performance.now();

  const validateStart = performance.now();
  const isDicom = await dcmnorm.checkDicom(filePath);
  const validateMs = performance.now() - validateStart;

  if (!isDicom) {
    const err = new Error("File does not look like valid DICOM");
    err.statusCode = 400;
    err.timings = { validateMs };
    throw err;
  }

  const metadataStart = performance.now();
  const metadataJson = await dcmnorm.readJson(filePath);
  const metadataMs = performance.now() - metadataStart;
  const metadata = JSON.parse(metadataJson);

  const nativeWidth = Number(metadata.Columns) || undefined;
  const nativeHeight = Number(metadata.Rows) || undefined;
  // Rows/Columns are absent for non-image SOP classes (SR, encapsulated PDF, RTSTRUCT, ...) -
  // renderFrame would just reject every frame with "missing required image attribute: Rows" in
  // that case, so skip rendering entirely rather than failing the whole request: the metadata
  // is still valid and worth showing, there just isn't a picture to go with it.
  const hasPixelData = nativeWidth !== undefined && nativeHeight !== undefined;

  const numberOfFrames = hasPixelData ? Math.max(1, parseInt(metadata.NumberOfFrames, 10) || 1) : 0;
  const framesToRender = Math.min(numberOfFrames, MAX_FRAMES_RENDERED);
  const truncated = framesToRender < numberOfFrames;

  const frames = [];
  const renderStart = performance.now();
  for (let frameIndex = 0; frameIndex < framesToRender; frameIndex++) {
    frames.push(await renderFrameCapped(filePath, frameIndex, nativeWidth, nativeHeight, THUMBNAIL_MAX_DIM));
  }
  const renderTotalMs = performance.now() - renderStart;
  const totalMs = performance.now() - totalStart;

  return {
    hasPixelData,
    numberOfFrames,
    framesRendered: framesToRender,
    truncated,
    metadata,
    frames,
    timings: {
      validateMs,
      metadataMs,
      renderTotalMs,
      renderAvgMs: framesToRender > 0 ? renderTotalMs / framesToRender : 0,
      totalMs,
    },
  };
}

app.use(express.static(path.join(__dirname, "public")));

app.get("/api/samples", async (_req, res) => {
  res.json(await listSampleFiles());
});

app.get("/api/version", (_req, res) => {
  res.json({ dcmnormVersion: dcmnormCoreVersion, dcmnormNodeVersion });
});

app.post("/api/process", (req, res) => {
  upload.single("dicomFile")(req, res, async (uploadErr) => {
    if (uploadErr) {
      res.status(400).json({ error: uploadErr.message });
      return;
    }
    if (!req.file) {
      res.status(400).json({ error: "No file uploaded" });
      return;
    }

    const filePath = req.file.path;
    try {
      const result = await processDicomFile(filePath);
      const uploadId = crypto.randomUUID();
      registerUpload(uploadId, filePath);
      res.json({ kind: "upload", uploadId, fileName: req.file.originalname, fileSizeBytes: req.file.size, ...result });
    } catch (err) {
      fs.unlink(filePath).catch(() => {});
      res.status(err.statusCode || 500).json({ error: err.message || String(err), timings: err.timings });
    }
  });
});

app.get("/api/uploads/:id/frames/:index", async (req, res) => {
  const entry = activeUploads.get(req.params.id);
  if (!entry) {
    res.status(404).json({ error: "This upload has expired - please re-upload the file." });
    return;
  }

  const frameIndex = parseInt(req.params.index, 10);
  if (!Number.isInteger(frameIndex) || frameIndex < 0) {
    res.status(400).json({ error: "Invalid frame index" });
    return;
  }

  try {
    const metadata = JSON.parse(await dcmnorm.readJson(entry.filePath));
    const nativeWidth = Number(metadata.Columns) || undefined;
    const nativeHeight = Number(metadata.Rows) || undefined;
    const maxDim = clampMaxDim(req.query.maxDim);
    res.json(await renderFrameCapped(entry.filePath, frameIndex, nativeWidth, nativeHeight, maxDim));
  } catch (err) {
    res.status(500).json({ error: err.message || String(err) });
  }
});

app.post("/api/samples/:name/process", async (req, res) => {
  const samples = await listSampleFiles();
  const sample = samples.find((s) => s.name === req.params.name);
  if (!sample) {
    res.status(404).json({ error: "Unknown sample file" });
    return;
  }

  try {
    const result = await processDicomFile(path.join(SAMPLES_DIR, sample.name));
    res.json({ kind: "sample", fileName: sample.name, fileSizeBytes: sample.sizeBytes, ...result });
  } catch (err) {
    res.status(err.statusCode || 500).json({ error: err.message || String(err), timings: err.timings });
  }
});

app.get("/api/samples/:name/frames/:index", async (req, res) => {
  const samples = await listSampleFiles();
  const sample = samples.find((s) => s.name === req.params.name);
  if (!sample) {
    res.status(404).json({ error: "Unknown sample file" });
    return;
  }

  const frameIndex = parseInt(req.params.index, 10);
  if (!Number.isInteger(frameIndex) || frameIndex < 0) {
    res.status(400).json({ error: "Invalid frame index" });
    return;
  }

  const filePath = path.join(SAMPLES_DIR, sample.name);
  try {
    const metadata = JSON.parse(await dcmnorm.readJson(filePath));
    const nativeWidth = Number(metadata.Columns) || undefined;
    const nativeHeight = Number(metadata.Rows) || undefined;
    const maxDim = clampMaxDim(req.query.maxDim);
    res.json(await renderFrameCapped(filePath, frameIndex, nativeWidth, nativeHeight, maxDim));
  } catch (err) {
    res.status(500).json({ error: err.message || String(err) });
  }
});

app.listen(PORT, () => {
  console.log(`dcmnorm-node test website listening on http://localhost:${PORT}`);
});
