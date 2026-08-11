const assert = require("assert");
const fs = require("fs");
const os = require("os");
const path = require("path");
const { execFileSync } = require("child_process");
const binding = require("../index.js");

const fixture = path.join(__dirname, "..", "..", "..", "test", "files", "us.dcm");
const overlayFixture = path.join(__dirname, "..", "..", "..", "test", "files", "overlay.dcm");
const overlayMultiFixture = path.join(__dirname, "..", "..", "..", "test", "files", "overlay_multi.dcm");
const overlayEmbeddedFixture = path.join(__dirname, "..", "..", "..", "test", "files", "overlay_embedded.dcm");

// The addon must run on the oldest glibc among its consumers' runtime images - node:22-slim
// (edge services), Debian bookworm, GLIBC 2.36 - even though render-server's node:24-trixie-slim
// has a newer one (2.41). build-in-docker.sh builds inside node:22-slim for exactly this reason,
// but nothing stops a plain host `npm run build` (e.g. during dev iteration) from silently
// producing a binary linked against the *host's* (often newer) glibc instead - which then loads
// fine here but dlopen-fails only once actually deployed. Catch that here, since `npm test` runs
// unconditionally as part of every release (see package.json's release-it "before:init" hook),
// regardless of which build script produced the binary.
const MAX_ALLOWED_GLIBC = "2.36";

function compareVersions(a, b) {
  const [aMaj, aMin] = a.split(".").map(Number);
  const [bMaj, bMin] = b.split(".").map(Number);
  return aMaj !== bMaj ? aMaj - bMaj : aMin - bMin;
}

function checkGlibcCompatibility() {
  // Actual filename varies by target (e.g. dcmnorm-node.linux-x64-gnu.node) - glob for whatever
  // .node file is actually present rather than hardcoding one.
  const dir = path.join(__dirname, "..");
  const nodeFiles = fs.readdirSync(dir).filter((f) => f.endsWith(".node"));
  if (nodeFiles.length === 0) return; // nothing built yet (e.g. WASI-only environment) - nothing to check

  for (const file of nodeFiles) {
    if (!file.includes("linux")) continue; // GLIBC only applies to Linux gnu targets
    let output;
    try {
      output = execFileSync("objdump", ["-T", path.join(dir, file)], { encoding: "utf8" });
    } catch (error) {
      console.warn(`Skipping GLIBC compatibility check for ${file}: objdump unavailable (${error.message})`);
      continue;
    }
    const versions = [...output.matchAll(/GLIBC_([0-9]+\.[0-9]+)/g)].map((m) => m[1]);
    const maxRequired = versions.reduce((max, v) => (compareVersions(v, max) > 0 ? v : max), "0.0");
    assert.ok(
      compareVersions(maxRequired, MAX_ALLOWED_GLIBC) <= 0,
      `${file} requires GLIBC_${maxRequired}, but the oldest consumer runtime (node:22-slim) only has ` +
        `GLIBC_${MAX_ALLOWED_GLIBC} - it was likely built directly on the host instead of via ` +
        `'npm run build:docker' (build-in-docker.sh), which links against node:22-slim's own glibc. ` +
        `Rebuild with 'npm run build:docker' before committing.`,
    );
  }
}

async function main() {
  checkGlibcCompatibility();
  assert.strictEqual(await binding.checkDicom(fixture), true, "checkDicom should be true for a real DICOM file");
  assert.strictEqual(await binding.checkDicom(__filename), false, "checkDicom should be false for a non-DICOM file");

const tagsJson = await binding.readTags(fixture, ["StudyInstanceUID", "SOPInstanceUID"]);
  const tags = JSON.parse(tagsJson);
  assert.ok(tags["0020000D"], "expected StudyInstanceUID (0020000D) in filtered readTags output");
  assert.ok(tags["00080018"], "expected SOPInstanceUID (00080018) in filtered readTags output");
  // File meta header elements (0002,eeee) always ride along regardless of
  // --filter (they live outside the dataset apply_filter_to_object prunes),
  // so assert absence of an unrequested *dataset* tag rather than an exact
  // key set.
  assert.ok(!("00080060" in tags), "Modality (00080060) was not requested and should be filtered out");

  const fullJson = await binding.readJson(fixture);
  const full = JSON.parse(fullJson);
  assert.ok(Object.keys(full).length > Object.keys(tags).length, "full readJson should have more keys than the filtered readTags");
  assert.ok(full.PixelData && typeof full.PixelData.BulkDataURI === "string" && !("InlineBinary" in full.PixelData),
    "readJson's default bulkData mode should be 'uri' (matching the CLI), not inline-embed PixelData");
  assert.ok(fullJson.length < 5000,
    `readJson output for a filtered non-bulk fixture should stay small (was ${fullJson.length} bytes) - a huge payload means PixelData got inlined instead of referenced`);

  const inlineJson = await binding.readJson(fixture, { bulkData: "inline" });
  const inline = JSON.parse(inlineJson);
  assert.ok(typeof inline.PixelData.InlineBinary === "string" && inline.PixelData.InlineBinary.length > 1000,
    "readJson with bulkData: 'inline' should base64-embed PixelData");

  const tmpDir = fs.mkdtempSync(path.join(os.tmpdir(), "dcmnorm-node-smoke-"));
  const edited = path.join(tmpDir, "edited.dcm");
  await binding.editTags(fixture, {
    outputPath: edited,
    set: { PatientName: "SMOKE^TEST" },
    removePrivateTags: true,
  });
  const editedTags = JSON.parse(await binding.readTags(edited, ["PatientName"]));
  assert.strictEqual(editedTags["00100010"], "SMOKE^TEST", "editTags should have set PatientName");

  const transcoded = path.join(tmpDir, "transcoded.dcm");
  await binding.transcode(fixture, transcoded, "1.2.840.10008.1.2.1");
  assert.strictEqual(await binding.checkDicom(transcoded), true, "transcoded output should still be valid DICOM");

  let panicked = false;
  try {
    await binding.readTags(fixture, ["NotARealKeyword"]);
  } catch (error) {
    panicked = true;
    assert.ok(error.message.includes("invalid --filter key"), `unexpected error message: ${error.message}`);
  }
  assert.ok(panicked, "readTags with an invalid tag keyword should reject, not throw synchronously or crash");

  const withOverlay = await binding.renderFrame(overlayFixture, { format: "png" });
  assert.strictEqual(withOverlay.overlays.length, 1, "overlay.dcm should report exactly one overlay plane");
  assert.strictEqual(withOverlay.selectedOverlayIndex, 0, "the first overlay should render by default");

  const withoutOverlay = await binding.renderFrame(overlayFixture, { format: "png", showOverlays: false });
  assert.strictEqual(withoutOverlay.selectedOverlayIndex, undefined, "showOverlays: false should render no overlay");
  assert.ok(
    !withOverlay.data.equals(withoutOverlay.data),
    "rendering with and without the overlay should produce different image bytes",
  );

  const redOverlay = await binding.renderFrame(overlayFixture, { format: "png", overlayColor: "255,0,0" });
  assert.ok(
    !withOverlay.data.equals(redOverlay.data),
    "a different overlayColor should change the rendered bytes",
  );

  let overlayIndexRejected = false;
  try {
    await binding.renderFrame(overlayFixture, { format: "png", overlayIndex: 5 });
  } catch (error) {
    overlayIndexRejected = true;
    assert.ok(error.message.includes("overlay index"), `unexpected error message: ${error.message}`);
  }
  assert.ok(overlayIndexRejected, "an out-of-range overlayIndex should reject");

  const multiOverlayFirst = await binding.renderFrame(overlayMultiFixture, { format: "png" });
  assert.strictEqual(multiOverlayFirst.overlays.length, 2, "overlay_multi.dcm should report two overlay planes");
  assert.strictEqual(multiOverlayFirst.selectedOverlayIndex, 0);
  const multiOverlaySecond = await binding.renderFrame(overlayMultiFixture, { format: "png", overlayIndex: 1 });
  assert.strictEqual(multiOverlaySecond.selectedOverlayIndex, 1);
  assert.ok(
    !multiOverlayFirst.data.equals(multiOverlaySecond.data),
    "selecting a different overlay index should change the rendered bytes",
  );

  const embeddedOverlay = await binding.renderFrame(overlayEmbeddedFixture, { format: "png" });
  assert.strictEqual(embeddedOverlay.overlays.length, 1, "overlay_embedded.dcm should report one overlay plane");
  assert.strictEqual(embeddedOverlay.selectedOverlayIndex, 0);

  fs.rmSync(tmpDir, { recursive: true, force: true });
  console.log("smoke test passed");
}

main().catch((error) => {
  console.error(error);
  process.exit(1);
});
