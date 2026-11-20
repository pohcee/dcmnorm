// Registration through the addon, end to end on synthetic DICOM written here (no PHI): build two
// CT series of one analytic phantom, the second seen through a known rigid transform, then
// buildVolume -> estimateTranslation -> registerVolumes -> writeSpatialRegistration ->
// readSpatialRegistration, and the cancel token. Matrices are row-major, moving -> fixed.
const assert = require("assert");
const fs = require("fs");
const os = require("os");
const path = require("path");
const d = require("../dcmnorm.js");

const CT_IMAGE = "1.2.840.10008.5.1.4.1.1.2";

function rigid(rx, ry, rz, t, c) {
  const [sx, cx, sy, cy, sz, cz] = [Math.sin(rx), Math.cos(rx), Math.sin(ry), Math.cos(ry), Math.sin(rz), Math.cos(rz)];
  const r = [
    cz * cy, cz * sy * sx - sz * cx, cz * sy * cx + sz * sx,
    sz * cy, sz * sy * sx + cz * cx, sz * sy * cx - cz * sx,
    -sy, cy * sx, cy * cx,
  ];
  const m = [r[0], r[1], r[2], 0, r[3], r[4], r[5], 0, r[6], r[7], r[8], 0, 0, 0, 0, 1];
  for (let i = 0; i < 3; i++) {
    m[i * 4 + 3] = c[i] - (r[i * 3] * c[0] + r[i * 3 + 1] * c[1] + r[i * 3 + 2] * c[2]) + t[i];
  }
  return m;
}

function apply(m, p) {
  return [0, 1, 2].map((i) => m[i * 4] * p[0] + m[i * 4 + 1] * p[1] + m[i * 4 + 2] * p[2] + m[i * 4 + 3]);
}

function soft(q, scale) {
  return Math.min(1, Math.max(0, 0.5 - (Math.sqrt(q) - 1) * scale / 2));
}

// A torso: body ellipse, two unequal lungs, spine, liver, a table below the patient (HU).
function phantom([x, y, z]) {
  const body = soft((x / 150) ** 2 + (y / 100) ** 2, 100);
  const lungL = soft(((x - 75) / 45) ** 2 + (y / 55) ** 2 + ((z - 40) / 80) ** 2, 45);
  const lungR = soft(((x + 70) / 52) ** 2 + ((y + 5) / 60) ** 2 + ((z - 50) / 90) ** 2, 52);
  const spine = soft((x / 16) ** 2 + ((y - 70) / 14) ** 2, 14) * (0.6 + 0.4 * Math.cos(z / 4.5));
  const liver = soft(((x + 60) / 70) ** 2 + ((y - 5) / 55) ** 2 + ((z + 50) / 50) ** 2, 50);
  const table = Math.abs(x) < 200 && y > 115 && y < 127 ? 1 : 0;
  let v = -1000 + body * 1040 - (lungL + lungR) * body * 880 + spine * 700 + liver * 25 * (1 - lungR);
  if (table && body < 0.5) v = 300;
  return v;
}

function writeSeries(dir, { cols, rows, slices, spacing, sliceSpacing, center, transform, frameOfReferenceUid, studyUid, seriesUid }) {
  fs.mkdirSync(dir, { recursive: true });
  const files = [];
  const instances = [];
  const origin = [center[0] - ((cols - 1) * spacing) / 2, center[1] - ((rows - 1) * spacing) / 2, center[2] - ((slices - 1) * sliceSpacing) / 2];
  for (let k = 0; k < slices; k++) {
    const pixels = Buffer.alloc(cols * rows * 2);
    for (let r = 0; r < rows; r++) {
      for (let c = 0; c < cols; c++) {
        const p = [origin[0] + c * spacing, origin[1] + r * spacing, origin[2] + k * sliceSpacing];
        pixels.writeInt16LE(Math.round(phantom(apply(transform, p))) + 1024, (r * cols + c) * 2);
      }
    }
    const sopInstanceUid = `${seriesUid}.${k + 1}`;
    const json = {
      "00080016": { vr: "UI", Value: [CT_IMAGE] },
      "00080018": { vr: "UI", Value: [sopInstanceUid] },
      "00080060": { vr: "CS", Value: ["CT"] },
      "00100010": { vr: "PN", Value: [{ Alphabetic: "SYNTHETIC^NODE" }] },
      "00100020": { vr: "LO", Value: ["SYN-NODE-1"] },
      "0020000D": { vr: "UI", Value: [studyUid] },
      "0020000E": { vr: "UI", Value: [seriesUid] },
      "00200013": { vr: "IS", Value: [k + 1] },
      "00200032": { vr: "DS", Value: [origin[0], origin[1], origin[2] + k * sliceSpacing] },
      "00200037": { vr: "DS", Value: [1, 0, 0, 0, 1, 0] },
      "00200052": { vr: "UI", Value: [frameOfReferenceUid] },
      "00280002": { vr: "US", Value: [1] },
      "00280004": { vr: "CS", Value: ["MONOCHROME2"] },
      "00280010": { vr: "US", Value: [rows] },
      "00280011": { vr: "US", Value: [cols] },
      "00280030": { vr: "DS", Value: [spacing, spacing] },
      "00280100": { vr: "US", Value: [16] },
      "00280101": { vr: "US", Value: [16] },
      "00280102": { vr: "US", Value: [15] },
      "00280103": { vr: "US", Value: [1] },
      "00281052": { vr: "DS", Value: [-1024] },
      "00281053": { vr: "DS", Value: [1] },
      "7FE00010": { vr: "OW", InlineBinary: pixels.toString("base64") },
    };
    const file = path.join(dir, `slice-${k}.dcm`);
    files.push({ file, json: JSON.stringify(json) });
    instances.push({ sopClassUid: CT_IMAGE, sopInstanceUid });
  }
  return { files, instances };
}

function tre(estimated, truth) {
  // Landmarks in the fixed frame; map them back through the truth, then forward through the estimate.
  const landmarks = [[0, 0, 0], [75, 0, 40], [-70, -5, 50], [-60, 5, -50], [0, 70, 20], [120, -40, -30]];
  const inverse = invertRigid(truth);
  return Math.max(...landmarks.map((p) => {
    const e = apply(estimated, apply(inverse, p));
    return Math.hypot(e[0] - p[0], e[1] - p[1], e[2] - p[2]);
  }));
}

function invertRigid(m) {
  const r = [m[0], m[4], m[8], m[1], m[5], m[9], m[2], m[6], m[10]];
  const t = [m[3], m[7], m[11]];
  const out = [r[0], r[1], r[2], 0, r[3], r[4], r[5], 0, r[6], r[7], r[8], 0, 0, 0, 0, 1];
  for (let i = 0; i < 3; i++) out[i * 4 + 3] = -(r[i * 3] * t[0] + r[i * 3 + 1] * t[1] + r[i * 3 + 2] * t[2]);
  return out;
}

async function main() {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), "dcmnorm-registration-"));
  try {
    const truth = rigid(0.04, -0.03, 0.09, [11, -9, 27], [5, 15, 0]);
    const identity = [1, 0, 0, 0, 0, 1, 0, 0, 0, 0, 1, 0, 0, 0, 0, 1];
    const fixedSeries = writeSeries(path.join(root, "fixed"), {
      cols: 128, rows: 112, slices: 60, spacing: 3, sliceSpacing: 4, center: [0, 10, 0], transform: identity,
      frameOfReferenceUid: "2.25.1001", studyUid: "2.25.1002", seriesUid: "2.25.1003",
    });
    const movingSeries = writeSeries(path.join(root, "moving"), {
      cols: 120, rows: 120, slices: 70, spacing: 3.2, sliceSpacing: 3.5, center: [5, 20, 15], transform: truth,
      frameOfReferenceUid: "2.25.2001", studyUid: "2.25.2002", seriesUid: "2.25.2003",
    });
    for (const { file, json } of [...fixedSeries.files, ...movingSeries.files]) {
      await d.writeJson(json, file, { format: "standard" });
    }

    const fixed = await d.buildVolume(fixedSeries.files.map((f) => f.file));
    const moving = await d.buildVolume(movingSeries.files.map((f) => f.file));
    assert.strictEqual(fixed.modality, "CT");
    assert.strictEqual(fixed.frameOfReferenceUid, "2.25.1001");
    assert.strictEqual(moving.seriesInstanceUid, "2.25.2003");
    assert.strictEqual(fixed.studyInstanceUid, "2.25.1002");
    assert.strictEqual(fixed.gantryTiltDeg, 0);
    assert.strictEqual(fixed.desheared, false);
    assert.strictEqual(fixed.irregularSpacing, false);
    const v2p = fixed.voxelToPatient;
    assert.strictEqual(v2p.length, 16);
    assert.ok(Math.abs(v2p[0] - 3) < 1e-9 && Math.abs(v2p[10] - 4) < 1e-9, `voxelToPatient ${v2p}`);

    const estimate = await d.estimateTranslation(fixed, moving);
    assert.strictEqual(estimate.matrix.length, 16);
    assert.ok(Math.abs(estimate.translationMm[2] - truth[11]) < 8, `estimate dz ${estimate.translationMm[2]} vs ${truth[11]}`);
    assert.ok(estimate.confidence >= 0 && estimate.confidence <= 1);
    assert.deepStrictEqual(estimate.profiles, ["body", "lung", "bone"]);
    assert.ok(estimate.channelAgreement >= 0 && estimate.channelAgreement <= 1);
    assert.ok(Number.isFinite(estimate.xyCorrelation) && estimate.minOverlapMm > 0 && estimate.minOverlapMm <= 60);
    // An explicit minimum alone is used as is; the fraction can be given too.
    assert.strictEqual((await d.estimateTranslation(fixed, moving, { minOverlapMm: 30 })).minOverlapMm, 30);
    assert.ok((await d.estimateTranslation(fixed, moving, { minOverlapFraction: 0.2 })).minOverlapMm <= 60);

    const result = await d.registerVolumes(fixed, moving, { timeLimitMs: 60000 });
    const error = tre(result.matrix, truth);
    console.log(`registerVolumes: TRE max ${error.toFixed(2)} mm, ${result.metric} ${result.metricValue.toFixed(4)}, quality ${result.quality.toFixed(2)} ${result.qualityLabel}, ${result.timingMs.toFixed(0)} ms, init ${result.initialization}`);
    assert.ok(error < 2.5, `TRE ${error}`);
    assert.strictEqual(result.metric, "ncc");
    assert.strictEqual(result.stopReason, "converged");
    assert.ok(["good", "fair"].includes(result.qualityLabel));
    assert.strictEqual(result.levels.length, 3);
    assert.strictEqual(result.consistent, undefined);
    assert.deepStrictEqual(result.starts, []);
    assert.ok(result.plausible && result.rotationAngleDeg < 30 && result.bodyMetric > 0.5 && result.bodyOverlapFraction > 0.3);

    // Multi-start from a seed 150 mm off: the other starts find the answer, and agree on it.
    const wrongSeed = truth.slice();
    wrongSeed[11] += 150;
    const multi = await d.registerVolumes(fixed, moving, { initialize: "multi", initialMatrix: wrongSeed, timeLimitMs: 60000 });
    console.log(`registerVolumes multi: TRE max ${tre(multi.matrix, truth).toFixed(2)} mm, ${multi.qualityLabel}, consistent ${multi.consistent}, starts ${multi.starts.map((s) => `${s.start}:${s.qualityLabel}${s.agrees ? "*" : ""}`).join(" ")}, ${multi.timingMs.toFixed(0)} ms`);
    assert.ok(tre(multi.matrix, truth) < 2.5);
    assert.strictEqual(multi.consistent, true);
    assert.ok(multi.starts.length >= 3 && multi.starts[0].refined && multi.starts.some((s) => s.start === "matrix"));
    const extra = await d.registerVolumes(fixed, moving, { starts: [wrongSeed], levelsMm: [8, 4] });
    assert.ok(extra.starts.some((s) => s.start === "start-1"));
    assert.throws(() => d.registerVolumes(fixed, moving, { starts: [[1, 2, 3]] }), /starts\[0\]/);
    assert.throws(() => d.registerVolumes(fixed, moving, { initialize: "sideways" }), /'multi'/);

    // Initial matrix and option validation.
    const seeded = await d.registerVolumes(fixed, moving, { initialMatrix: estimate.matrix, levelsMm: [8, 4] });
    assert.strictEqual(seeded.initialization, "matrix");
    assert.throws(() => d.registerVolumes(fixed, moving, { metric: "nope" }), /invalid metric/);
    await assert.rejects(d.registerVolumes(fixed, moving, { levelsMm: [] }), /levelsMm/);

    // Cancel: a token cancelled up front rejects at the first check.
    const token = new d.RegistrationCancelToken();
    assert.strictEqual(token.cancelled, false);
    token.cancel();
    assert.strictEqual(token.cancelled, true);
    await assert.rejects(d.registerVolumes(fixed, moving, {}, token), /registration cancelled/);

    // REG: write the result, read it back.
    const spec = {
      matrix: result.matrix,
      fixed: { frameOfReferenceUid: "2.25.1001", studyInstanceUid: "2.25.1002", seriesInstanceUid: "2.25.1003", instances: fixedSeries.instances },
      moving: { frameOfReferenceUid: "2.25.2001", studyInstanceUid: "2.25.2002", seriesInstanceUid: "2.25.2003", instances: movingSeries.instances },
      copyPatientStudyFrom: fixedSeries.files[0].file,
      contentDescription: `quality ${result.quality.toFixed(2)}`,
    };
    const regPath = path.join(root, "reg.dcm");
    const written = await d.writeSpatialRegistration(JSON.stringify(spec), regPath);
    assert.ok(written.sopInstanceUid.startsWith("2.25."));
    assert.strictEqual(written.data, undefined);
    const dataset = JSON.parse(written.dataset);
    assert.strictEqual(dataset["00080016"].Value[0], "1.2.840.10008.5.1.4.1.1.66.1");
    assert.strictEqual(dataset["00080060"].Value[0], "REG");
    const reg = JSON.parse(await d.readSpatialRegistration(regPath));
    assert.deepStrictEqual(reg.warnings, []);
    assert.strictEqual(reg.frameOfReferenceUid, "2.25.1001");
    assert.strictEqual(reg.patientId, "SYN-NODE-1");
    assert.strictEqual(reg.registrations[1].frameOfReferenceUid, "2.25.2001");
    reg.registrations[1].matrix.forEach((v, i) => assert.ok(Math.abs(v - result.matrix[i]) < 1e-6, `matrix[${i}]`));
    assert.strictEqual(reg.referencedSeries.length, 2);
    assert.deepStrictEqual(JSON.parse(written.json).registrations, reg.registrations);

    // Without outputPath the bytes come back, and writeJson of `dataset` makes an equivalent file.
    const inMemory = await d.writeSpatialRegistration(JSON.stringify(spec));
    assert.ok(Buffer.isBuffer(inMemory.data) && inMemory.data.length > 500);
    const fromDataset = path.join(root, "reg-from-dataset.dcm");
    await d.writeJson(inMemory.dataset, fromDataset, { format: "standard" });
    const reread = JSON.parse(await d.readSpatialRegistration(fromDataset));
    assert.deepStrictEqual(reread.registrations, JSON.parse(inMemory.json).registrations);

    await assert.rejects(d.writeSpatialRegistration(JSON.stringify({ ...spec, matrix: [1, 2, 3] })), /matrix must be 16 numbers/);
    await assert.rejects(d.readSpatialRegistration(fixedSeries.files[0].file), /not a Spatial Registration/);

    fixed.release();
    await assert.rejects(Promise.resolve().then(() => d.registerVolumes(fixed, moving)), /released/);
    moving.release();
    console.log("registration tests passed");
  } finally {
    fs.rmSync(root, { recursive: true, force: true });
  }
}

main().catch((error) => {
  console.error(error);
  process.exit(1);
});
