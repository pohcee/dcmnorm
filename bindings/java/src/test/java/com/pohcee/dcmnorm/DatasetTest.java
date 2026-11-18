package com.pohcee.dcmnorm;

import java.awt.image.BufferedImage;
import java.awt.image.DataBufferByte;
import java.io.ByteArrayInputStream;
import java.io.ByteArrayOutputStream;
import java.io.FilterInputStream;
import java.io.IOException;
import java.io.InputStream;
import java.nio.file.Files;
import java.nio.file.Path;
import java.util.Arrays;
import java.util.List;
import java.util.Map;
import java.util.concurrent.atomic.AtomicInteger;

import javax.imageio.ImageIO;

import com.pohcee.dcmnorm.internal.Json;

/**
 * Tests of the typed dataset API (#6): read / accessors / JSON / mutation and write / rendering
 * / typed errors / generated constants. Plain {@code main()} like the other test classes here;
 * {@link Smoke} runs it, and it runs alone with the same classpath and {@code
 * -Djava.library.path} as Smoke: {@code java ... com.pohcee.dcmnorm.DatasetTest <test/files>}.
 */
public final class DatasetTest {
    private static Path fixtures;

    private static void check(boolean condition, String message) {
        if (!condition) {
            throw new AssertionError(message);
        }
    }

    private static void checkEquals(Object expected, Object actual, String message) {
        if (expected == null ? actual != null : !expected.equals(actual)) {
            throw new AssertionError(message + ": expected <" + expected + "> but was <" + actual + ">");
        }
    }

    public static void main(String[] args) throws Exception {
        if (args.length < 1) {
            System.err.println("usage: DatasetTest <fixtures-dir>");
            System.exit(2);
        }
        run(Path.of(args[0]));
        System.out.println("dataset tests passed");
    }

    static void run(Path fixturesDir) throws Exception {
        fixtures = fixturesDir;
        testConstants();
        testAccessors();
        testSequencesAndPersonNames();
        testStreamsAndBytes();
        testEncapsulated();
        testJson();
        testMutationRoundTrip();
        testRendering();
        testTransferSyntaxesAndTranscode();
        testErrors();
        testReadmeSample();
    }

    private static Path fixture(String name) {
        return fixtures.resolve(name);
    }

    // -------------------------------------------------------------------------------------------

    private static void testConstants() {
        checkEquals(0x0020000D, Tag.StudyInstanceUID, "Tag.StudyInstanceUID");
        checkEquals(0x7FE00010, Tag.PixelData, "Tag.PixelData");
        checkEquals("(0020,000D)", Tag.toString(Tag.StudyInstanceUID), "Tag.toString");
        checkEquals("StudyInstanceUID", ElementDictionary.keywordOf(Tag.StudyInstanceUID), "keywordOf");
        checkEquals(VR.PN, ElementDictionary.vrOf(Tag.PatientName), "vrOf");
        checkEquals(Tag.PatientName, ElementDictionary.tagForKeyword("PatientName"), "tagForKeyword");
        checkEquals(-1, ElementDictionary.tagForKeyword("NoSuchKeyword"), "unknown keyword");
        checkEquals(VR.Kind.INT, VR.US.kind(), "VR kind");
        TransferSyntax jpeg = TransferSyntax.fromUid("1.2.840.10008.1.2.4.50\0");
        checkEquals(TransferSyntax.JPEGBaseline8Bit, jpeg, "TransferSyntax.fromUid");
        check(jpeg.isLossy() && jpeg.isEncapsulated() && jpeg.isCompressed(), "JPEG baseline properties");
        check(!TransferSyntax.ImplicitVRLittleEndian.isExplicitVR(), "implicit VR");
        check(TransferSyntax.ExplicitVRBigEndian.isBigEndian() && TransferSyntax.ExplicitVRBigEndian.isRetired(), "BE");
        checkEquals("1.2.840.10008.5.1.4.1.1.2", SOPClassUID.CTImageStorage, "SOPClassUID");
        checkEquals("CT Image Storage", SOPClassUID.nameOf(SOPClassUID.CTImageStorage), "SOPClassUID.nameOf");
    }

    private static void testAccessors() throws Exception {
        DicomDataset ds = Dcmnorm.read(fixture("mr_small.dcm"), ReadOptions.untilPixelData());
        checkEquals("1.2.840.10008.1.2.1", ds.transferSyntaxUid(), "transferSyntaxUid");
        checkEquals(TransferSyntax.ExplicitVRLittleEndian, ds.transferSyntax(), "transferSyntax");
        checkEquals(64, ds.getInt(Tag.Rows, 0), "Rows");
        checkEquals("64", ds.getString(Tag.Columns), "Columns as string");
        checkEquals(1, ds.numberOfFrames(), "numberOfFrames default");
        checkEquals(VR.PN, ds.vr(Tag.PatientName), "vr(PatientName)");
        checkEquals("CompressedSamples^MR1", ds.getString(Tag.PatientName), "PatientName");
        checkEquals("default", ds.getString(Tag.StudyDescription + 0x7FFF, "default"), "default for absent");
        check(ds.getStrings(0x00091001) == null, "absent getStrings is null");
        checkEquals("MR", ds.getString(Tag.Modality), "Modality");
        String[] imageType = ds.getStrings(Tag.ImageType);
        check(Arrays.equals(new String[] {"DERIVED", "SECONDARY", "OTHER"}, imageType), "ImageType multi-value: "
            + Arrays.toString(imageType));
        checkEquals("SECONDARY", ds.getString(Tag.ImageType, 1, null), "ImageType[1]");
        double[] spacing = ds.getDoubles(Tag.PixelSpacing);
        check(spacing != null && spacing.length == 2 && spacing[0] == 0.3125, "PixelSpacing DS: " + Arrays.toString(spacing));
        checkEquals(1.0, ds.pixelAspectRatio(), "pixel aspect ratio from PixelSpacing");

        BulkDataRef pixels = ds.getBulkDataRef(Tag.PixelData);
        check(pixels != null, "PixelData is a bulk data reference");
        checkEquals(1500L, pixels.getOffset(), "PixelData offset");
        checkEquals(8192L, pixels.getLength(), "PixelData length");
        checkEquals(Tag.PixelData, ds.tags()[ds.size() - 1], "untilPixelData stops at PixelData");
        check(ds.getBytes(Tag.PixelData) == null, "pixel data not loaded");

        DicomDataset meta = ds.fileMeta();
        check(meta.contains(Tag.TransferSyntaxUID), "file meta has the transfer syntax");
        checkEquals(ds.transferSyntaxUid(), meta.getString(Tag.TransferSyntaxUID), "file meta transfer syntax");

        DicomDataset loaded = Dcmnorm.read(fixture("mr_small.dcm"), ReadOptions.full().loadBulkData(true));
        byte[] raw = loaded.getBytes(Tag.PixelData);
        check(raw != null && raw.length == 8192, "loadBulkData reads PixelData");
        byte[] file = Files.readAllBytes(fixture("mr_small.dcm"));
        check(Arrays.equals(Arrays.copyOfRange(file, 1500, 1500 + 8192), raw), "the reference points at the value");

        DicomDataset rtdose = Dcmnorm.read(fixture("rtdose.dcm"), ReadOptions.untilPixelData());
        checkEquals(15, rtdose.numberOfFrames(), "NumberOfFrames");
        check(Arrays.equals(new int[] {0x3004000C}, rtdose.getTags(Tag.FrameIncrementPointer)), "AT value");
        checkEquals("3004000C", rtdose.getString(Tag.FrameIncrementPointer), "AT as string");
        double[] offsets = rtdose.getDoubles(Tag.GridFrameOffsetVector);
        check(offsets != null && offsets.length == 15, "GridFrameOffsetVector multi-value");

        DicomDataset us2 = Dcmnorm.read(fixture("us2.dcm"), ReadOptions.untilPixelData());
        checkEquals(227, us2.numberOfFrames(), "us2 frames");
        checkEquals(1.0, us2.pixelAspectRatio(), "us2 PixelAspectRatio 356\\356");
        check(us2.frameTimeMillis().isPresent() || us2.frameTimeVector() != null || us2.framesPerSecond().isPresent()
            || !us2.contains(Tag.FrameTime), "frame timing accessors");
    }

    private static void testSequencesAndPersonNames() throws Exception {
        DicomDataset sr = Dcmnorm.read(fixture("sr.dcm"), ReadOptions.full());
        List<DicomDataset> content = sr.getSequence(Tag.ContentSequence);
        check(content != null && !content.isEmpty(), "ContentSequence items");
        check(sr.getNestedDataset(Tag.ContentSequence) == content.get(0), "getNestedDataset");
        check(content.get(0).contains(Tag.RelationshipType), "items are datasets");
        check(sr.getSequence(Tag.PatientName) == null, "getSequence on a non-sequence");
        try {
            content.add(DicomDataset.empty());
            throw new AssertionError("sequence lists must be unmodifiable");
        } catch (UnsupportedOperationException expected) {
            // ok
        }

        PersonName name = new PersonName("Yamada^Tarou=山田^太郎=やまだ^たろう");
        checkEquals("Yamada", name.getFamilyName(), "PN family");
        checkEquals("太郎", name.getIdeographic().getGivenName(), "PN ideographic given");
        checkEquals("やまだ", name.getPhonetic().getFamilyName(), "PN phonetic family");
        DicomDataset sc = Dcmnorm.read(fixture("sc_jpeg_baseline.dcm"), ReadOptions.untilPixelData());
        PersonName patient = sc.getPersonName(Tag.PatientName);
        checkEquals("Lestrade", patient.getFamilyName(), "PatientName family");
        checkEquals("G", patient.getGivenName(), "PatientName given");
        check(patient.getIdeographic().isEmpty(), "no ideographic group");
    }

    /** Counts how much of a stream was read. */
    private static final class CountingStream extends FilterInputStream {
        final AtomicInteger count = new AtomicInteger();

        CountingStream(InputStream in) {
            super(in);
        }

        @Override
        public int read() throws IOException {
            int b = super.read();
            if (b >= 0) {
                count.incrementAndGet();
            }
            return b;
        }

        @Override
        public int read(byte[] buffer, int offset, int length) throws IOException {
            int n = super.read(buffer, offset, Math.min(length, 700));
            if (n > 0) {
                count.addAndGet(n);
            }
            return n;
        }
    }

    private static void testStreamsAndBytes() throws Exception {
        byte[] bytes = Files.readAllBytes(fixture("us2.dcm"));
        DicomDataset fromFile = Dcmnorm.read(fixture("us2.dcm"), ReadOptions.untilPixelData());
        DicomDataset fromBytes = Dcmnorm.read(bytes, ReadOptions.untilPixelData());
        CountingStream stream = new CountingStream(new ByteArrayInputStream(bytes));
        DicomDataset fromStream = Dcmnorm.read(stream, ReadOptions.untilPixelData());
        checkEquals(fromFile, fromBytes, "file and byte[] reads");
        checkEquals(fromFile, fromStream, "file and stream reads");
        long pixelOffset = fromStream.getBulkDataRef(Tag.PixelData).getOffset();
        check(stream.count.get() <= pixelOffset + 64 * 1024,
            "untilPixelData reads no further than the PixelData header (read " + stream.count.get() + " of "
                + bytes.length + ")");
        // the full read of a stream skips the 29 MB of pixel data instead of keeping it
        DicomDataset full = Dcmnorm.read(new ByteArrayInputStream(bytes), ReadOptions.full());
        checkEquals(fromFile.getBulkDataRef(Tag.PixelData), full.getBulkDataRef(Tag.PixelData), "full stream read");
    }

    private static void testEncapsulated() throws Exception {
        DicomDataset until = Dcmnorm.read(fixture("mr_rle.dcm"), ReadOptions.untilPixelData());
        BulkDataRef ref = until.getBulkDataRef(Tag.PixelData);
        check(!ref.isLengthDefined(), "encapsulated PixelData has no length when the read stops at it");
        DicomDataset full = Dcmnorm.read(fixture("mr_rle.dcm"));
        checkEquals(6128L, full.getBulkDataRef(Tag.PixelData).getLength(), "encapsulated PixelData length");
        DicomDataset loaded = Dcmnorm.read(fixture("mr_rle.dcm"), ReadOptions.full().loadBulkData(true));
        DicomElement.Fragments fragments = loaded.get(Tag.PixelData).getFragments();
        check(fragments != null && fragments.getFragmentCount() == 1, "fragments read");
        // writing either form gives the same pixel data
        DicomDataset a = Dcmnorm.read(Dcmnorm.toBytes(until, WriteOptions.defaults()), ReadOptions.full().loadBulkData(true));
        checkEquals(fragments, a.get(Tag.PixelData).getFragments(), "fragments written from an undefined-length ref");
    }

    private static void testJson() throws Exception {
        Path file = fixture("mr_small.dcm");
        String json = Dcmnorm.toDicomJson(file, new JsonOptions());
        Map<String, Object> map = Json.asMap(Json.parse(json));
        Map<String, Object> pixels = Json.asMap(map.get("7FE00010"));
        checkEquals("OW", pixels.get("vr"), "PixelData vr");
        checkEquals("?offset=1500&length=8192", pixels.get("BulkDataURI"), "BulkDataURI");
        Map<String, Object> name = Json.asMap(Json.asList(Json.asMap(map.get("00100010")).get("Value")).get(0));
        checkEquals("CompressedSamples^MR1", name.get("Alphabetic"), "PN as Annex F object");
        check(map.keySet().stream().noneMatch(key -> key.startsWith("0002")), "no file meta by default");
        check(!Json.asMap(map.get("00100010")).containsKey("Keyword"), "no Keyword/VM by default");

        String filtered = Dcmnorm.toDicomJson(file, new JsonOptions()
            .includeTags(Tag.PatientName, Tag.Rows, Tag.PixelData)
            .excludeTags(Tag.Rows)
            .includeFileMeta(true)
            .bulkDataUriBuilder(ref -> "https://pacs.example/bulk/" + Integer.toHexString(ref.getTag()) + "/"
                + ref.getOffset() + "/" + ref.getLength() + "/" + ref.getVR()));
        Map<String, Object> f = Json.asMap(Json.parse(filtered));
        check(f.containsKey("00100010") && f.containsKey("7FE00010") && !f.containsKey("00280010"), "include/exclude");
        check(f.containsKey("00020010"), "includeFileMeta");
        checkEquals("https://pacs.example/bulk/7fe00010/1500/8192/OW", Json.asMap(f.get("7FE00010")).get("BulkDataURI"),
            "bulkDataUriBuilder");

        byte[] bytes = Files.readAllBytes(file);
        checkEquals(json, Dcmnorm.toDicomJson(bytes, new JsonOptions()), "JSON from byte[]");
        checkEquals(json, Dcmnorm.toDicomJson(new ByteArrayInputStream(bytes), new JsonOptions()), "JSON from a stream");

        String inline = Dcmnorm.toDicomJson(file, new JsonOptions().bulkData(JsonOptions.BulkData.INLINE));
        check(Json.asMap(Json.asMap(Json.parse(inline)).get("7FE00010")).containsKey("InlineBinary"), "inline mode");

        DicomDataset ds = Dcmnorm.read(file, ReadOptions.untilPixelData());
        String fromDataset = Dcmnorm.toDicomJson(ds.toBuilder().set(Tag.PatientName, VR.PN, "Doe^Jane").build(),
            new JsonOptions());
        Map<String, Object> d = Json.asMap(Json.parse(fromDataset));
        checkEquals("Doe^Jane",
            Json.asMap(Json.asList(Json.asMap(d.get("00100010")).get("Value")).get(0)).get("Alphabetic"),
            "JSON of an edited dataset");
        checkEquals("?offset=1500&length=8192", Json.asMap(d.get("7FE00010")).get("BulkDataURI"), "dataset BulkDataURI");

        try {
            Dcmnorm.toDicomJson(file, new JsonOptions().bulkDataUriBuilder(ref -> {
                throw new IllegalStateException("boom");
            }));
            throw new AssertionError("a throwing URI builder must fail the call");
        } catch (DcmnormException expected) {
            check(expected.getMessage().contains("boom"), "builder exception message: " + expected.getMessage());
        }
    }

    private static void testMutationRoundTrip() throws Exception {
        Path file = fixture("mr_small.dcm");
        DicomDataset ds = Dcmnorm.read(file, ReadOptions.full());
        DicomDataset edited = ds.toBuilder()
            .set(Tag.PatientName, VR.PN, "Doe^Jane")
            .set(Tag.PatientID, "ID-42")
            .setInt(Tag.SeriesNumber, VR.IS, 7)
            .set(Tag.ImageType, VR.CS, "ORIGINAL", "PRIMARY")
            .setDouble(Tag.SliceThickness, VR.DS, 2.5)
            .remove(Tag.StudyDescription)
            .setSequence(Tag.ReferencedStudySequence, List.of(DicomDataset.empty().toBuilder()
                .set(Tag.ReferencedSOPClassUID, VR.UI, "1.2.840.10008.3.1.2.3.1")
                .set(Tag.ReferencedSOPInstanceUID, VR.UI, "1.2.3.4")
                .build()))
            .build();
        check(ds.getString(Tag.PatientName).equals("CompressedSamples^MR1"), "the source dataset is unchanged");
        DicomDataset nested = edited.toBuilder()
            .editSequenceItem(Tag.ReferencedStudySequence, 0, item -> item.set(Tag.ReferencedSOPInstanceUID, VR.UI, "1.2.3.5"))
            .build();

        Path out = Files.createTempFile("dcmnorm-dataset-test", ".dcm");
        Dcmnorm.write(nested, out, WriteOptions.defaults());
        DicomDataset back = Dcmnorm.read(out, ReadOptions.full().loadBulkData(true));
        checkEquals("Doe^Jane", back.getString(Tag.PatientName), "PatientName round trip");
        checkEquals("ID-42", back.getString(Tag.PatientID), "PatientID with dictionary VR");
        checkEquals(VR.LO, back.vr(Tag.PatientID), "dictionary VR used");
        checkEquals(7, back.getInt(Tag.SeriesNumber, 0), "IS from an int");
        checkEquals(2.5, back.getDouble(Tag.SliceThickness, 0), "DS from a double");
        check(Arrays.equals(new String[] {"ORIGINAL", "PRIMARY"}, back.getStrings(Tag.ImageType)), "multi-value set");
        check(!back.contains(Tag.StudyDescription), "removed");
        checkEquals("1.2.3.5", back.getNestedDataset(Tag.ReferencedStudySequence).getString(Tag.ReferencedSOPInstanceUID),
            "nested sequence edit");
        byte[] original = Dcmnorm.read(file, ReadOptions.full().loadBulkData(true)).getBytes(Tag.PixelData);
        check(Arrays.equals(original, back.getBytes(Tag.PixelData)), "pixel data passed through from the source");
        checkEquals(ds.getString(Tag.SOPInstanceUID), back.fileMeta().getString(Tag.MediaStorageSOPInstanceUID),
            "file meta created from the data set");

        // to an OutputStream and to bytes, transcoded
        ByteArrayOutputStream stream = new ByteArrayOutputStream();
        Dcmnorm.write(edited, stream, WriteOptions.transferSyntax(TransferSyntax.ImplicitVRLittleEndian));
        DicomDataset implicit = Dcmnorm.read(stream.toByteArray(), ReadOptions.full().loadBulkData(true));
        checkEquals("1.2.840.10008.1.2", implicit.transferSyntaxUid(), "transcoded on write");
        check(Arrays.equals(original, implicit.getBytes(Tag.PixelData)), "pixel data through a transcoding write");
        check(Arrays.equals(stream.toByteArray(),
            Dcmnorm.toBytes(edited, WriteOptions.transferSyntax(TransferSyntax.ImplicitVRLittleEndian))), "toBytes");

        // a stream-read dataset needs its bulk data source named
        DicomDataset fromStream = Dcmnorm.read(Files.newInputStream(file), ReadOptions.untilPixelData());
        try {
            Dcmnorm.toBytes(fromStream, WriteOptions.defaults());
            throw new AssertionError("writing references without a source must fail");
        } catch (DcmnormException expected) {
            checkEquals(DcmnormException.Kind.INVALID_ARGUMENT, expected.getKind(), "no bulk source kind");
        }
        byte[] written = Dcmnorm.toBytes(fromStream, WriteOptions.defaults().bulkDataSource(file));
        check(Arrays.equals(original, Dcmnorm.read(written, ReadOptions.full().loadBulkData(true)).getBytes(Tag.PixelData)),
            "bulkDataSource");

        try {
            ds.toBuilder().set(Tag.TransferSyntaxUID, VR.UI, "1.2.3");
            throw new AssertionError("group 0002 can't be set");
        } catch (IllegalArgumentException expected) {
            // ok
        }
        try {
            Dcmnorm.toBytes(ds.toBuilder().setInt(Tag.Rows, VR.US, 70000).build(), WriteOptions.defaults());
            throw new AssertionError("an out-of-range US must be rejected");
        } catch (DcmnormException expected) {
            checkEquals(DcmnormException.Kind.INVALID_ARGUMENT, expected.getKind(), "out of range kind");
        }
        Files.deleteIfExists(out);
    }

    private static void testRendering() throws Exception {
        BufferedImage gray = Dcmnorm.renderImage(fixture("mr_small.dcm"), 0, new RenderParams());
        checkEquals(64, gray.getWidth(), "gray width");
        checkEquals(64, gray.getHeight(), "gray height");
        checkEquals(BufferedImage.TYPE_BYTE_GRAY, gray.getType(), "gray image type");
        RenderedPixels grayPixels = Dcmnorm.renderPixels(fixture("mr_small.dcm"), 0, new RenderParams());
        checkEquals(64 * 64, grayPixels.getPixels().length, "gray pixel bytes");
        byte[] imageBytes = ((DataBufferByte) gray.getRaster().getDataBuffer()).getData();
        check(Arrays.equals(grayPixels.getPixels(), imageBytes), "renderImage wraps the rendered pixels");

        BufferedImage color = Dcmnorm.renderImage(fixture("sc_jpeg_baseline.dcm"), 0, new RenderParams());
        checkEquals(BufferedImage.TYPE_3BYTE_BGR, color.getType(), "color image type");
        RenderedPixels rgb = Dcmnorm.renderPixels(fixture("sc_jpeg_baseline.dcm"), 0, new RenderParams());
        checkEquals(3, rgb.getChannels(), "rgb channels");
        checkEquals(100 * 100 * 3, rgb.getPixels().length, "rgb pixel bytes");
        int argb = color.getRGB(10, 20);
        int offset = (20 * 100 + 10) * 3;
        checkEquals(((rgb.getPixels()[offset] & 0xFF) << 16) | ((rgb.getPixels()[offset + 1] & 0xFF) << 8)
            | (rgb.getPixels()[offset + 2] & 0xFF), argb & 0xFFFFFF, "BGR image shows the RGB pixels");
        BufferedImage rgbImage = rgb.toBufferedImage();
        checkEquals(argb, rgbImage.getRGB(10, 20), "an RGB-ordered image is the same picture");
        check(ImageIO.write(color, "png", new ByteArrayOutputStream()), "ImageIO can write the image");

        BufferedImage scaled = Dcmnorm.renderImage(Files.readAllBytes(fixture("us2.dcm")), 226,
            new RenderParams().outputWidth(90));
        checkEquals(90, scaled.getWidth(), "scaled width");
        BufferedImage windowed = Dcmnorm.renderImage(fixture("mr_small.dcm"), 0, new RenderParams().window(100, 50));
        check(!Arrays.equals(imageBytes, ((DataBufferByte) windowed.getRaster().getDataBuffer()).getData()),
            "an explicit window changes the output");
        RenderedPixels overlay = Dcmnorm.renderPixels(fixture("overlay.dcm"), 0, new RenderParams());
        check(!overlay.getOverlays().isEmpty() && overlay.getSelectedOverlayIndex() != null, "overlay composited");
        RenderedPixels noOverlay = Dcmnorm.renderPixels(fixture("overlay.dcm"), 0, new RenderParams().overlays(false));
        check(noOverlay.getSelectedOverlayIndex() == null, "overlays off");
    }

    private static void testTransferSyntaxesAndTranscode() throws Exception {
        List<TransferSyntaxSupport> support = Dcmnorm.transferSyntaxSupport();
        check(support.size() > 20, "transfer syntax support listed");
        TransferSyntaxSupport explicit = Dcmnorm.transferSyntaxSupport(TransferSyntax.ExplicitVRLittleEndian.uid());
        check(explicit != null && explicit.canTranscodeTo() && !explicit.isEncapsulated(), "Explicit VR LE support");
        checkEquals(TransferSyntax.ExplicitVRLittleEndian, explicit.getTransferSyntax(), "support -> enum");
        TransferSyntaxSupport rle = Dcmnorm.transferSyntaxSupport(TransferSyntax.RLELossless.uid());
        check(rle != null && rle.canDecode(), "RLE decode");

        byte[] input = Files.readAllBytes(fixture("mr_rle.dcm"));
        byte[] native_ = Dcmnorm.transcode(input, TransferSyntax.ExplicitVRLittleEndian.uid());
        checkEquals("1.2.840.10008.1.2.1", Dcmnorm.read(native_).transferSyntaxUid(), "transcode(byte[])");
        ByteArrayOutputStream out = new ByteArrayOutputStream();
        Dcmnorm.transcode(new ByteArrayInputStream(input), out, TransferSyntax.ExplicitVRLittleEndian.uid());
        check(Arrays.equals(native_, out.toByteArray()), "transcode(InputStream, OutputStream)");
        checkEquals(native_.length, Dcmnorm.transcode(new ByteArrayInputStream(input), "1.2.840.10008.1.2.1").length,
            "transcode(InputStream)");
    }

    /** The root README's Java sample, as written (paths resolved in a temporary directory). */
    private static void testReadmeSample() throws Exception {
        Path dir = Files.createTempDirectory("dcmnorm-readme");
        Files.copy(fixture("mr_small.dcm"), dir.resolve("scan.dcm"));
        Path scan = dir.resolve("scan.dcm");
        java.io.File frame = dir.resolve("frame.png").toFile();
        DicomDataset ds = Dcmnorm.read(scan, ReadOptions.untilPixelData());
        System.out.println(ds.getString(Tag.StudyInstanceUID) + " " + ds.numberOfFrames());
        BufferedImage image = Dcmnorm.renderImage(scan, 0, new RenderParams());
        ImageIO.write(image, "png", frame);
        Dcmnorm.transcode(scan, dir.resolve("out.dcm"), TransferSyntax.ExplicitVRLittleEndian.uid());
        check(frame.length() > 0 && Files.size(dir.resolve("out.dcm")) > 8192, "README sample outputs");
    }

    private static void testErrors() throws Exception {
        try {
            Dcmnorm.read(fixture("notdicom.txt"));
            throw new AssertionError("not DICOM");
        } catch (InvalidDicomException expected) {
            checkEquals(DcmnormException.Kind.INVALID_DICOM, expected.getKind(), "kind");
        }
        try {
            Dcmnorm.read(fixture("does-not-exist.dcm"));
            throw new AssertionError("missing file");
        } catch (DcmnormException expected) {
            checkEquals(DcmnormException.Kind.IO, expected.getKind(), "missing file kind");
        }
        try {
            Dcmnorm.transcode(Files.readAllBytes(fixture("mr_small.dcm")), "1.2.3.4.5");
            throw new AssertionError("unknown transfer syntax");
        } catch (UnsupportedTransferSyntaxException expected) {
            checkEquals("1.2.3.4.5", expected.getTargetTransferSyntaxUid(), "target UID");
            checkEquals("1.2.840.10008.1.2.1", expected.getSourceTransferSyntaxUid(), "source UID");
        }
        try {
            Dcmnorm.renderImage(fixture("mr_small.dcm"), 3, new RenderParams());
            throw new AssertionError("frame out of range");
        } catch (RenderException expected) {
            checkEquals(3, expected.getFrameIndex(), "frame index");
        }
        try {
            Dcmnorm.renderImage(fixture("mr_small.dcm"), 0, new RenderParams().window(10, -1));
            throw new AssertionError("bad window");
        } catch (RenderException expected) {
            // ok
        }
        try {
            Dcmnorm.transcode(fixture("notdicom.txt").toString(), Files.createTempFile("x", ".dcm").toString(),
                "1.2.840.10008.1.2.1");
            throw new AssertionError("transcode not DICOM");
        } catch (InvalidDicomException expected) {
            // the existing path-based call is typed too
        }
    }
}
