// Windows CI load check (see .github/workflows/build-bindings.yml, build-node-windows).
const assert = require("assert");
const os = require("os");
const path = require("path");
const b = require("./index.js");

const fixture = path.join(__dirname, "..", "..", "test", "files", "us.dcm");
const MPEG2_MP_ML = "1.2.840.10008.1.2.4.100";
const EXPLICIT_VR_LE = "1.2.840.10008.1.2.1";

(async () => {
  assert.ok(JSON.parse(await b.readJson(fixture)), "readJson returned nothing");
  const mpeg = path.join(os.tmpdir(), "load-check-mpeg2.dcm");
  const back = path.join(os.tmpdir(), "load-check-mpeg2-back.dcm");
  await b.transcode(fixture, mpeg, MPEG2_MP_ML);
  await b.transcode(mpeg, back, EXPLICIT_VR_LE);
  const j = JSON.parse(await b.readJson(back));
  assert.strictEqual(j.TransferSyntaxUID, EXPLICIT_VR_LE);
  console.log("ok: parse + MPEG-2 encode/decode round trip");
})().catch((e) => {
  console.error(e);
  process.exit(1);
});
