// Tests of the typed dataset API (#6) through the package entry point (dcmnorm.js): read,
// accessors, sequences, PN, multi-value, bulk data references, DICOM JSON options, typed errors,
// mutation round trips, rendering and the generated constants. Run by `npm test`.
const assert = require("assert");
const fs = require("fs");
const os = require("os");
const path = require("path");
const { Readable } = require("stream");
const d = require("../dcmnorm.js");
const { Tag, VR, TransferSyntax, SOPClassUID, TRANSFER_SYNTAX_INFO } = d;

const files = path.join(__dirname, "..", "..", "..", "test", "files");
const fixture = (name) => path.join(files, name);

async function main() {
  // constants
  assert.strictEqual(Tag.StudyInstanceUID, 0x0020000d);
  assert.strictEqual(Tag.PixelData, 0x7fe00010);
  assert.strictEqual(VR.PN, "PN");
  assert.strictEqual(TransferSyntax.ExplicitVRLittleEndian, "1.2.840.10008.1.2.1");
  assert.ok(TRANSFER_SYNTAX_INFO[TransferSyntax.JPEGBaseline8Bit].lossy);
  assert.strictEqual(d.sopClassName(SOPClassUID.CTImageStorage), "CT Image Storage");
  assert.strictEqual(d.keywordOf(Tag.PatientName), "PatientName");
  assert.strictEqual(d.vrOf(Tag.PatientName), "PN");
  assert.strictEqual(d.tagForKeyword("StudyInstanceUID"), Tag.StudyInstanceUID);
  assert.ok(typeof d.readJson === "function", "the existing API is still exported");

  // accessors
  const ds = await d.read(fixture("mr_small.dcm"), { untilPixelData: true });
  assert.strictEqual(ds.transferSyntaxUid, "1.2.840.10008.1.2.1");
  assert.strictEqual(ds.transferSyntax.keyword, "ExplicitVRLittleEndian");
  assert.strictEqual(ds.getInt(Tag.Rows), 64);
  assert.strictEqual(ds.getString(Tag.Columns), "64");
  assert.strictEqual(ds.vr(Tag.PatientName), "PN");
  assert.strictEqual(ds.getString(Tag.PatientName), "CompressedSamples^MR1");
  assert.strictEqual(ds.getString(0x00091001, "none"), "none");
  assert.deepStrictEqual(ds.getStrings(Tag.ImageType), ["DERIVED", "SECONDARY", "OTHER"]);
  assert.strictEqual(ds.getString(Tag.ImageType, null, 1), "SECONDARY");
  assert.deepStrictEqual(ds.getNumbers(Tag.PixelSpacing), [0.3125, 0.3125]);
  assert.strictEqual(ds.numberOfFrames, 1);
  assert.strictEqual(ds.pixelAspectRatio, 1);
  const pixels = ds.getBulkDataRef(Tag.PixelData);
  assert.deepStrictEqual({ offset: pixels.offset, length: pixels.length }, { offset: 1500, length: 8192 });
  assert.strictEqual(ds.tags().at(-1), Tag.PixelData);
  assert.strictEqual(ds.fileMeta.getString(Tag.TransferSyntaxUID), ds.transferSyntaxUid);
  const loaded = await d.read(fixture("mr_small.dcm"), { loadBulkData: true });
  const raw = loaded.getBytes(Tag.PixelData);
  assert.ok(raw.equals(fs.readFileSync(fixture("mr_small.dcm")).subarray(1500, 1500 + 8192)));

  const rtdose = await d.read(fixture("rtdose.dcm"), { untilPixelData: true });
  assert.strictEqual(rtdose.numberOfFrames, 15);
  assert.strictEqual(rtdose.getString(Tag.FrameIncrementPointer), "3004000C");
  assert.strictEqual(rtdose.getNumbers(Tag.GridFrameOffsetVector).length, 15);

  // sequences, PN
  const sr = await d.read(fixture("sr.dcm"));
  const content = sr.getSequence(Tag.ContentSequence);
  assert.ok(content.length > 0 && content[0] instanceof d.DicomDataset);
  assert.ok(content[0].has(Tag.RelationshipType));
  const pn = new d.PersonName("Yamada^Tarou=山田^太郎=やまだ^たろう");
  assert.strictEqual(pn.familyName, "Yamada");
  assert.strictEqual(pn.ideographic.givenName, "太郎");
  const sc = await d.read(fixture("sc_jpeg_baseline.dcm"), { untilPixelData: true });
  assert.strictEqual(sc.getPersonName(Tag.PatientName).givenName, "G");

  // bytes and streams give the same data set
  const bytes = fs.readFileSync(fixture("rtdose.dcm"));
  const fromBytes = await d.read(bytes, { untilPixelData: true });
  const fromStream = await d.read(Readable.from([bytes.subarray(0, 100), bytes.subarray(100)]), { untilPixelData: true });
  assert.deepStrictEqual(fromBytes.getStrings(Tag.GridFrameOffsetVector), rtdose.getStrings(Tag.GridFrameOffsetVector));
  assert.deepStrictEqual(fromStream.tags(), rtdose.tags());

  // encapsulated pixel data
  const rle = await d.read(fixture("mr_rle.dcm"), { untilPixelData: true });
  assert.strictEqual(rle.getBulkDataRef(Tag.PixelData).length, -1);
  const rleLoaded = await d.read(fixture("mr_rle.dcm"), { loadBulkData: true });
  assert.strictEqual(rleLoaded.get(Tag.PixelData).type, "fragments");

  // DICOM JSON
  const json = JSON.parse(await d.toDicomJson(fixture("mr_small.dcm")));
  assert.strictEqual(json["7FE00010"].BulkDataURI, "?offset=1500&length=8192");
  assert.strictEqual(json["00100010"].Value[0].Alphabetic, "CompressedSamples^MR1");
  assert.ok(Object.keys(json).every((k) => !k.startsWith("0002")));
  const filtered = JSON.parse(
    await d.toDicomJson(fixture("mr_small.dcm"), {
      includeTags: [Tag.PatientName, Tag.Rows, Tag.PixelData],
      excludeTags: [Tag.Rows],
      includeFileMeta: true,
      bulkDataUriBuilder: (ref) => `https://pacs.example/bulk/${ref.tag.toString(16)}/${ref.offset}/${ref.length}/${ref.vr}`,
    }),
  );
  assert.deepStrictEqual(Object.keys(filtered).filter((k) => !k.startsWith("0002")), ["00100010", "7FE00010"]);
  assert.ok(filtered["00020010"]);
  assert.strictEqual(filtered["7FE00010"].BulkDataURI, "https://pacs.example/bulk/7fe00010/1500/8192/OW");
  const edited = ds.toBuilder().set(Tag.PatientName, VR.PN, "Doe^Jane").build();
  const fromDataset = JSON.parse(await d.toDicomJson(edited));
  assert.strictEqual(fromDataset["00100010"].Value[0].Alphabetic, "Doe^Jane");
  assert.strictEqual(fromDataset["7FE00010"].BulkDataURI, "?offset=1500&length=8192");
  const inline = JSON.parse(await d.toDicomJson(fs.readFileSync(fixture("mr_small.dcm")), { bulkData: "inline" }));
  assert.ok(inline["7FE00010"].InlineBinary);

  // mutation and write
  const full = await d.read(fixture("mr_small.dcm"));
  const changed = full
    .toBuilder()
    .set(Tag.PatientName, VR.PN, "Doe^Jane")
    .set(Tag.PatientID, ["ID-42"])
    .setInt(Tag.SeriesNumber, VR.IS, 7)
    .setDouble(Tag.SliceThickness, VR.DS, 2.5)
    .set(Tag.ImageType, VR.CS, "ORIGINAL", "PRIMARY")
    .remove(Tag.StudyDescription)
    .setSequence(Tag.ReferencedStudySequence, [
      d.DicomDataset.empty().toBuilder().set(Tag.ReferencedSOPInstanceUID, VR.UI, "1.2.3.4").build(),
    ])
    .editSequenceItem(Tag.ReferencedStudySequence, 0, (item) => item.set(Tag.ReferencedSOPInstanceUID, VR.UI, "1.2.3.5"))
    .build();
  assert.strictEqual(full.getString(Tag.PatientName), "CompressedSamples^MR1", "the source is unchanged");
  const out = path.join(os.tmpdir(), `dcmnorm-node-dataset-${process.pid}.dcm`);
  await d.write(changed, out);
  const back = await d.read(out, { loadBulkData: true });
  assert.strictEqual(back.getString(Tag.PatientName), "Doe^Jane");
  assert.strictEqual(back.vr(Tag.PatientID), "LO");
  assert.strictEqual(back.getInt(Tag.SeriesNumber), 7);
  assert.strictEqual(back.getFloat(Tag.SliceThickness), 2.5);
  assert.deepStrictEqual(back.getStrings(Tag.ImageType), ["ORIGINAL", "PRIMARY"]);
  assert.ok(!back.has(Tag.StudyDescription));
  assert.strictEqual(back.getNestedDataset(Tag.ReferencedStudySequence).getString(Tag.ReferencedSOPInstanceUID), "1.2.3.5");
  assert.ok(back.getBytes(Tag.PixelData).equals(raw), "pixel data passed through from the source");
  fs.unlinkSync(out);
  const implicit = await d.read(await d.toBuffer(changed, { transferSyntax: TransferSyntax.ImplicitVRLittleEndian }), { loadBulkData: true });
  assert.strictEqual(implicit.transferSyntaxUid, "1.2.840.10008.1.2");
  assert.ok(implicit.getBytes(Tag.PixelData).equals(raw));
  const streamed = await d.read(Readable.from([fs.readFileSync(fixture("mr_small.dcm"))]), { untilPixelData: true });
  // a stream source is collected into a Buffer, so its references still resolve
  assert.ok((await d.read(await d.toBuffer(streamed), { loadBulkData: true })).getBytes(Tag.PixelData).equals(raw));
  await assert.rejects(d.toBuffer(full.toBuilder().setInt(Tag.Rows, VR.US, 70000).build()), (e) => e.kind === "InvalidArgument");
  assert.throws(() => full.toBuilder().set(Tag.TransferSyntaxUID, VR.UI, "1.2"));

  // rendering
  const gray = await d.renderPixels(fixture("mr_small.dcm"), 0);
  assert.deepStrictEqual([gray.width, gray.height, gray.channels, gray.pixels.length], [64, 64, 1, 64 * 64]);
  const rgb = await d.renderPixels(fixture("sc_jpeg_baseline.dcm"), 0, { outputWidth: 50 });
  assert.deepStrictEqual([rgb.width, rgb.height, rgb.channels, rgb.pixels.length], [50, 50, 3, 50 * 50 * 3]);
  const windowed = await d.renderPixels(fixture("mr_small.dcm"), 0, { windowCenter: 100, windowWidth: 50 });
  assert.ok(!windowed.pixels.equals(gray.pixels));
  const overlay = await d.renderPixels(fixture("overlay.dcm"), 0);
  assert.ok(overlay.overlays.length > 0 && overlay.selectedOverlayIndex === 0);

  // transfer syntaxes and transcoding
  const support = d.transferSyntaxSupport();
  assert.ok(support.find((ts) => ts.uid === TransferSyntax.ExplicitVRLittleEndian).canTranscodeTo);
  const transcoded = await d.transcodeTo(fixture("mr_rle.dcm"), TransferSyntax.ExplicitVRLittleEndian);
  assert.strictEqual((await d.read(transcoded)).transferSyntaxUid, TransferSyntax.ExplicitVRLittleEndian);

  // typed errors
  await assert.rejects(d.read(fixture("notdicom.txt")), (e) => e instanceof d.InvalidDicomError && e.code === "ERR_DCMNORM_INVALID_DICOM");
  await assert.rejects(d.read(fixture("does-not-exist.dcm")), (e) => e instanceof d.DcmnormError && e.kind === "Io");
  await assert.rejects(
    d.transcodeTo(fixture("mr_small.dcm"), "1.2.3.4.5"),
    (e) => e instanceof d.UnsupportedTransferSyntaxError && e.targetTransferSyntaxUid === "1.2.3.4.5" && e.sourceTransferSyntaxUid === "1.2.840.10008.1.2.1",
  );
  await assert.rejects(d.renderPixels(fixture("mr_small.dcm"), 3), (e) => e instanceof d.RenderError && e.frameIndex === 3);

  // the root README's Node.js sample, as written, on a copy of a fixture named scan.dcm
  const cwd = process.cwd();
  const tmp = fs.mkdtempSync(path.join(os.tmpdir(), "dcmnorm-readme-"));
  fs.copyFileSync(fixture("mr_small.dcm"), path.join(tmp, "scan.dcm"));
  process.chdir(tmp);
  try {
    const { read, renderPixels, transcodeTo, Tag, TransferSyntax } = require("../dcmnorm.js");
    const ds = await read("scan.dcm", { untilPixelData: true });
    console.log(ds.getString(Tag.StudyInstanceUID), ds.numberOfFrames);
    const { width, height, channels, pixels } = await renderPixels("scan.dcm", 0);
    await transcodeTo("scan.dcm", TransferSyntax.ExplicitVRLittleEndian, "out.dcm");
    assert.deepStrictEqual([width, height, channels, pixels.length], [64, 64, 1, 4096]);
    assert.ok(fs.statSync("out.dcm").size > 8192);
  } finally {
    process.chdir(cwd);
    fs.rmSync(tmp, { recursive: true, force: true });
  }

  console.log("dataset tests passed");
}

main().catch((error) => {
  console.error(error);
  process.exit(1);
});
