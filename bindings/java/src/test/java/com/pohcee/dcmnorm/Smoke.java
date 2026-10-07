package com.pohcee.dcmnorm;

import java.io.BufferedReader;
import java.io.IOException;
import java.io.InputStream;
import java.io.InputStreamReader;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.nio.file.Path;
import java.util.ArrayList;
import java.util.List;
import java.util.Map;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.atomic.AtomicBoolean;
import java.util.regex.Matcher;
import java.util.regex.Pattern;
import java.util.zip.GZIPInputStream;

import com.pohcee.dcmnorm.internal.Json;

/**
 * Core / render / MPR / texture-export / DIMSE smoke test - the Java counterpart of
 * bindings/node/test/smoke.js and bindings/python/test/smoke.py (plus a slice of
 * bindings/python/test/smoke_dimse.py, run against {@code dcmtalk storescp} as the SCP side
 * since this binding does not implement its own {@code start_dicom_server} equivalent - see the
 * README's scope note). Run via {@code ./build.sh test} (or by hand - see that script for the
 * exact classpath/java.library.path/arguments it uses).
 *
 * <p>Takes the repository's {@code test/files} directory as {@code args[0]} and the {@code
 * dcmtalk} binary's path as {@code args[1]}, rather than deriving either from {@code
 * System.getProperty("user.dir")} - Java has no {@code __file__}-relative path convenience, so
 * resolving them unambiguously is build.sh's job, not this class's.
 */
public final class Smoke {
    private static Path fixtures;
    private static Path dcmtalkBinary;

    private static void check(boolean condition, String message) {
        if (!condition) {
            throw new AssertionError(message);
        }
    }

    public static void main(String[] args) throws Exception {
        if (args.length < 2) {
            System.err.println("usage: Smoke <fixtures-dir> <dcmtalk-binary>");
            System.exit(2);
        }
        fixtures = Path.of(args[0]);
        dcmtalkBinary = Path.of(args[1]);

        testCore();
        testRendering();
        Path tempDir = Files.createTempDirectory("dcmnorm-java-smoke-");
        testMpr(tempDir);
        testDimse(tempDir);

        System.out.println("smoke test passed");
    }

    // -------------------------------------------------------------------------------------------

    private static void testCore() throws Exception {
        String fixture = fixtures.resolve("us.dcm").toString();

        check(Dcmnorm.checkDicom(fixture), "checkDicom should be true for a real DICOM file");
        check(!Dcmnorm.checkDicom(fixtures.resolve("notdicom.txt").toString()), "checkDicom should be false for a non-DICOM file");

        Map<String, Object> tags = Json.asMap(Json.parse(Dcmnorm.readTags(fixture, List.of("StudyInstanceUID", "SOPInstanceUID"))));
        check(tags.containsKey("0020000D"), "expected StudyInstanceUID (0020000D) in filtered readTags output");
        check(tags.containsKey("00080018"), "expected SOPInstanceUID (00080018) in filtered readTags output");
        check(!tags.containsKey("00080060"), "Modality (00080060) was not requested and should be filtered out");

        String fullJson = Dcmnorm.readJson(fixture);
        Map<String, Object> full = Json.asMap(Json.parse(fullJson));
        check(full.size() > tags.size(), "full readJson should have more keys than the filtered readTags");
        Map<String, Object> pixelData = Json.asMap(full.get("PixelData"));
        check(pixelData.get("BulkDataURI") instanceof String && !pixelData.containsKey("InlineBinary"),
            "readJson's default bulkData mode should be 'uri' (matching the CLI), not inline-embed PixelData");
        check(fullJson.length() < 5000, "readJson output for a filtered non-bulk fixture should stay small (was " + fullJson.length() + " bytes)");

        Map<String, Object> inline = Json.asMap(Json.parse(Dcmnorm.readJson(fixture, new ReadJsonOptions().bulkData("inline"))));
        Map<String, Object> inlinePixelData = Json.asMap(inline.get("PixelData"));
        check(inlinePixelData.get("InlineBinary") instanceof String && ((String) inlinePixelData.get("InlineBinary")).length() > 1000,
            "readJson with bulkData=inline should base64-embed PixelData");

        Path tempDir = Files.createTempDirectory("dcmnorm-java-core-smoke-");
        Path edited = tempDir.resolve("edited.dcm");
        Dcmnorm.editTags(fixture, new EditTagsOptions().outputPath(edited.toString())
            .set(Map.of("PatientName", "SMOKE^TEST")).removePrivateTags(true));
        Map<String, Object> editedTags = Json.asMap(Json.parse(Dcmnorm.readTags(edited.toString(), List.of("PatientName"))));
        check("SMOKE^TEST".equals(editedTags.get("00100010")), "editTags should have set PatientName");

        Path transcoded = tempDir.resolve("transcoded.dcm");
        Dcmnorm.transcode(fixture, transcoded.toString(), "1.2.840.10008.1.2.1");
        check(Dcmnorm.checkDicom(transcoded.toString()), "transcoded output should still be valid DICOM");

        try {
            Dcmnorm.readTags(fixture, List.of("NotARealKeyword"));
            throw new AssertionError("readTags with an invalid tag keyword should raise");
        } catch (DcmnormException error) {
            check(error.getMessage().contains("invalid --filter key"), "unexpected error message: " + error.getMessage());
        }

        System.out.println("core: OK");
    }

    // -------------------------------------------------------------------------------------------

    private static void testRendering() throws Exception {
        String overlayFixture = fixtures.resolve("overlay.dcm").toString();
        String overlayMultiFixture = fixtures.resolve("overlay_multi.dcm").toString();
        String overlayEmbeddedFixture = fixtures.resolve("overlay_embedded.dcm").toString();
        String wsiFixture = fixtures.resolve("wsi.dcm").toString();

        RenderedFrame withOverlay = Dcmnorm.renderFrame(overlayFixture, new RenderFrameOptions().format("png"));
        check(withOverlay.getOverlays().size() == 1, "overlay.dcm should report exactly one overlay plane");
        check(withOverlay.getSelectedOverlayIndex() != null && withOverlay.getSelectedOverlayIndex() == 0,
            "the first overlay should render by default");

        RenderedFrame withoutOverlay = Dcmnorm.renderFrame(overlayFixture, new RenderFrameOptions().format("png").showOverlays(false));
        check(withoutOverlay.getSelectedOverlayIndex() == null, "showOverlays=false should render no overlay");
        check(!java.util.Arrays.equals(withOverlay.getData(), withoutOverlay.getData()),
            "rendering with and without the overlay should differ");

        RenderedFrame redOverlay = Dcmnorm.renderFrame(overlayFixture, new RenderFrameOptions().format("png").overlayColor("255,0,0"));
        check(!java.util.Arrays.equals(withOverlay.getData(), redOverlay.getData()), "a different overlayColor should change the rendered bytes");

        try {
            Dcmnorm.renderFrame(overlayFixture, new RenderFrameOptions().format("png").overlayIndex(5));
            throw new AssertionError("an out-of-range overlayIndex should raise");
        } catch (DcmnormException error) {
            check(error.getMessage().contains("overlay index"), "unexpected error message: " + error.getMessage());
        }

        RenderedFrame multiFirst = Dcmnorm.renderFrame(overlayMultiFixture, new RenderFrameOptions().format("png"));
        check(multiFirst.getOverlays().size() == 2, "overlay_multi.dcm should report two overlay planes");
        check(multiFirst.getSelectedOverlayIndex() == 0, "first overlay should be selected");
        RenderedFrame multiSecond = Dcmnorm.renderFrame(overlayMultiFixture, new RenderFrameOptions().format("png").overlayIndex(1));
        check(multiSecond.getSelectedOverlayIndex() == 1, "second overlay should be selected");
        check(!java.util.Arrays.equals(multiFirst.getData(), multiSecond.getData()), "selecting a different overlay index should change the bytes");

        RenderedFrame embedded = Dcmnorm.renderFrame(overlayEmbeddedFixture, new RenderFrameOptions().format("png"));
        check(embedded.getOverlays().size() == 1, "overlay_embedded.dcm should report one overlay plane");
        check(embedded.getSelectedOverlayIndex() == 0, "embedded overlay should be selected");

        // wsi.dcm is JPEG Baseline - exercises the in-house dcmnorm-jpeg decoder rather than an
        // uncompressed or JPEG2000/openjpeg-sys path.
        RenderedFrame wsiFrame = Dcmnorm.renderFrame(wsiFixture, new RenderFrameOptions().format("png"));
        check(wsiFrame.getWidth() == 240, "wsi.dcm (JPEG Baseline) should decode to 240x240");
        check(wsiFrame.getHeight() == 240, "wsi.dcm (JPEG Baseline) should decode to 240x240");
        check(wsiFrame.getData().length > 0, "JPEG-decoded frame should produce non-empty PNG bytes");

        RenderedMovie movie = Dcmnorm.renderMovie(fixtures.resolve("us.dcm").toString(), new RenderMovieOptions().outputWidth(32));
        check("video/mp4".equals(movie.getMimeType()), "renderMovie should report video/mp4");
        check(movie.getData().length > 0, "renderMovie should produce non-empty bytes");

        System.out.println("rendering: OK");
    }

    // -------------------------------------------------------------------------------------------

    private static void testMpr(Path tempDir) throws Exception {
        String ctFixture = fixtures.resolve("ct.dcm").toString();

        int sliceCount = 4;
        List<String> slicePaths = new ArrayList<>();
        for (int i = 0; i < sliceCount; i++) {
            Path slicePath = tempDir.resolve("ct-slice-" + i + ".dcm");
            Dcmnorm.editTags(ctFixture, new EditTagsOptions().outputPath(slicePath.toString())
                .set(Map.of("ImagePositionPatient", "-151.493508\\-36.6564417\\" + (1115.0 + i))));
            slicePaths.add(slicePath.toString());
        }

        try (DicomVolumeHandle volume = Dcmnorm.buildVolume(slicePaths)) {
            check(volume.getRows() == 512, "ct.dcm fixture should be 512 rows");
            check(volume.getCols() == 512, "ct.dcm fixture should be 512 cols");
            check(volume.getNumSlices() == sliceCount, "volume should have " + sliceCount + " slices");

            TextureExportResult volumeTexture = volume.exportTexture(new TextureExportOptions().compression("gzip").windowCenter(40).windowWidth(400));
            check("volume".equals(volumeTexture.getContentKind()), "contentKind should be volume");
            check("int16".equals(volumeTexture.getSampleFormat()), "sampleFormat should be int16");
            check("gzip".equals(volumeTexture.getCompression()), "compression should be gzip");
            check(volumeTexture.isLossless(), "volume texture should be lossless");
            check(volumeTexture.getWidth() == 512 && volumeTexture.getHeight() == 512, "volume texture should be 512x512");
            check(volumeTexture.getDepth() == sliceCount, "volume texture depth should match slice count");
            check(volumeTexture.getNativeWidth() == 512 && volumeTexture.getNativeHeight() == 512 && volumeTexture.getNativeDepth() == sliceCount,
                "native dims should match");
            check(!volumeTexture.isDownsampled(), "volume texture should not be downsampled");
            check(volumeTexture.getDefaultWindowCenter() == 40.0, "defaultWindowCenter should round-trip");
            check(volumeTexture.getDefaultWindowWidth() == 400.0, "defaultWindowWidth should round-trip");
            check(volumeTexture.getPayloadBytesRaw() == 512L * 512L * sliceCount * 2L, "payloadBytesRaw should match raw int16 volume size");
            check(volumeTexture.getPayloadBytesStored() == volumeTexture.getData().length, "payloadBytesStored should match data.length");

            byte[] decompressed = gunzip(volumeTexture.getData());
            check(decompressed.length == volumeTexture.getPayloadBytesRaw(), "decompressed length should match payloadBytesRaw");

            int bytesPerSlice = 512 * 512 * 2;
            byte[] firstSlice = java.util.Arrays.copyOfRange(decompressed, 0, bytesPerSlice);
            for (int i = 1; i < sliceCount; i++) {
                byte[] slice = java.util.Arrays.copyOfRange(decompressed, i * bytesPerSlice, (i + 1) * bytesPerSlice);
                check(java.util.Arrays.equals(slice, firstSlice), "slice " + i + " should be byte-identical to slice 0");
            }

            TextureExportResult frameTexture = Dcmnorm.exportFrameTexture(ctFixture, new TextureExportOptions().compression("none"));
            check("image2d".equals(frameTexture.getContentKind()), "contentKind should be image2d");
            check(frameTexture.getDepth() == 1, "image2d depth should be 1");
            check(frameTexture.getWidth() == 512 && frameTexture.getHeight() == 512, "frame texture should be 512x512");
            check(java.util.Arrays.equals(frameTexture.getData(), firstSlice),
                "exportFrameTexture on the unedited base file should byte-match the volume texture's first slice");

            TextureExportResult downsampled = volume.exportTexture(new TextureExportOptions().targetMaxDim(2).compression("none"));
            check(downsampled.isDownsampled(), "downsampled texture should report downsampled=true");
            check(downsampled.getWidth() <= 2 && downsampled.getHeight() <= 2 && downsampled.getDepth() <= 2, "downsampled dims should be capped");
            check(downsampled.getNativeWidth() == 512 && downsampled.getNativeHeight() == 512 && downsampled.getNativeDepth() == sliceCount,
                "native dims should still report the pre-downsample size");

            try {
                volume.exportTexture(new TextureExportOptions().compression("bogus"));
                throw new AssertionError("an invalid compression value should raise");
            } catch (DcmnormException error) {
                check(error.getMessage().contains("compression"), "unexpected error message: " + error.getMessage());
            }

            double[] nativeBasis = volume.getNativeBasis();
            double[] rowDir = java.util.Arrays.copyOfRange(nativeBasis, 0, 3);
            double[] colDir = java.util.Arrays.copyOfRange(nativeBasis, 3, 6);
            RenderedFrame reformatted = volume.reformat(new ReformatParams(volume.getCenter(), rowDir, colDir, 64, 64, volume.getMinSpacingMm())
                .format("png"));
            check("image/png".equals(reformatted.getMimeType()), "reformat should report image/png");
            check(reformatted.getWidth() == 64 && reformatted.getHeight() == 64, "reformat output should be 64x64");
            check(reformatted.getData().length > 0, "reformat should produce non-empty bytes");

            TextureExportResult stack = Dcmnorm.exportFrameStackTexture(List.of(new FrameStackSource(ctFixture, List.of(0))),
                new TextureExportOptions().compression("none"));
            check("framestack".equals(stack.getContentKind()), "contentKind should be framestack");
            check(stack.getDepth() == 1, "frame stack depth should be 1");
        }

        System.out.println("mpr/texture-export: OK");
    }

    private static byte[] gunzip(byte[] data) throws IOException {
        try (GZIPInputStream in = new GZIPInputStream(new java.io.ByteArrayInputStream(data));
             java.io.ByteArrayOutputStream out = new java.io.ByteArrayOutputStream()) {
            in.transferTo(out);
            return out.toByteArray();
        }
    }

    // -------------------------------------------------------------------------------------------

    private static void testDimse(Path tempDir) throws Exception {
        Path cachePath = tempDir.resolve("scp-cache");
        Files.createDirectories(cachePath);

        ProcessBuilder processBuilder = new ProcessBuilder(
            dcmtalkBinary.toString(), "storescp", "0", "--ae-title", "DCMNORM-JAVA-SMOKE", "--cache-path", cachePath.toString());
        processBuilder.redirectErrorStream(true);
        Process scp = processBuilder.start();
        try {
            int port = readStorescpPort(scp);
            String destination = "127.0.0.1:" + port;

            List<String> logLines = new ArrayList<>();
            DimseLogger logger = message -> {
                synchronized (logLines) {
                    logLines.add(message);
                }
            };

            int echoStatus = Dcmnorm.echoScu(destination, new EchoScuOptions().onLog(logger));
            check(echoStatus == 0, "echoScu should succeed against a live storescp, got status " + echoStatus);
            check(!logLines.isEmpty(), "onLog should have been called for echoScu");

            String fixture = fixtures.resolve("us.dcm").toString();
            List<StoreScuResult> storeResults = Dcmnorm.storeScu(destination, List.of(fixture), new StoreScuOptions().onLog(logger));
            check(storeResults.size() == 1, "storeScu should report one result");
            check(storeResults.get(0).getStatus() == 0, "storeScu should succeed, got status " + storeResults.get(0).getStatus());
            check(!storeResults.get(0).getSopInstanceUid().isEmpty(), "storeScu result should carry a SOP Instance UID");

            // dcmtalk storescp only implements C-ECHO/C-STORE natively (see StandaloneScpHandlers
            // in exec/dcmtalk/src/main.rs) - findScu/moveScu against it exercise the "association
            // succeeds, the request itself is rejected" path, still useful as a marshaling check
            // even without a peer that actually answers C-FIND/C-MOVE.
            try {
                Dcmnorm.findScu(destination, Map.of("PatientName", ""));
            } catch (DcmnormException expected) {
                // Either an explicit rejection or a non-zero status surfaced as an error is fine -
                // the point of this call is exercising the request/response marshaling, not
                // asserting a specific peer behavior dcmtalk storescp was never meant to provide.
            }

            // A connection a peer actively refuses (nothing listening) should raise, not hang or
            // silently report success - exercises the association-failure error path without
            // needing a second server.
            try {
                int unusedPort = findUnusedPort();
                Dcmnorm.echoScu("127.0.0.1:" + unusedPort, new EchoScuOptions().timeoutMs(2000));
                throw new AssertionError("echoScu against a closed port should raise");
            } catch (DcmnormException expected) {
                // expected
            }
        } finally {
            scp.destroy();
            scp.waitFor(5, TimeUnit.SECONDS);
        }

        System.out.println("dimse: OK");
    }

    private static int findUnusedPort() throws IOException {
        try (java.net.ServerSocket socket = new java.net.ServerSocket(0)) {
            return socket.getLocalPort();
        }
    }

    private static final Pattern STORESCP_PORT_PATTERN = Pattern.compile("listening on port (\\d+)");

    private static int readStorescpPort(Process process) throws IOException {
        InputStream stdout = process.getInputStream();
        BufferedReader reader = new BufferedReader(new InputStreamReader(stdout, StandardCharsets.UTF_8));
        long deadline = System.nanoTime() + TimeUnit.SECONDS.toNanos(10);
        while (System.nanoTime() < deadline) {
            String line = reader.readLine();
            if (line == null) {
                break;
            }
            Matcher matcher = STORESCP_PORT_PATTERN.matcher(line);
            if (matcher.find()) {
                // Drain the rest of the process's stdout on a daemon thread so it never blocks
                // on a full pipe buffer for the rest of the test.
                AtomicBoolean stop = new AtomicBoolean(false);
                Thread drain = new Thread(() -> {
                    try {
                        while (!stop.get() && reader.readLine() != null) {
                            // discard
                        }
                    } catch (IOException ignored) {
                        // process exited
                    }
                });
                drain.setDaemon(true);
                drain.start();
                return Integer.parseInt(matcher.group(1));
            }
        }
        throw new IOException("dcmtalk storescp did not report a listening port within 10s");
    }
}
