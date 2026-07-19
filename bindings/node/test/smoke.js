const assert = require("assert");
const fs = require("fs");
const os = require("os");
const path = require("path");
const binding = require("../index.js");

const fixture = path.join(__dirname, "..", "..", "..", "test", "files", "us.dcm");

async function main() {
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

  fs.rmSync(tmpDir, { recursive: true, force: true });
  console.log("smoke test passed");
}

main().catch((error) => {
  console.error(error);
  process.exit(1);
});
