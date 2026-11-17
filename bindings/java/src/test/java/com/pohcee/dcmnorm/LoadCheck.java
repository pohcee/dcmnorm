package com.pohcee.dcmnorm;

import java.nio.file.Files;
import java.nio.file.Path;

/**
 * The release build's load check for a natives jar, on every platform (the Java counterpart of
 * bindings/node/load-check.js). Run with the API classes and exactly one natives jar on the
 * classpath and nothing on {@code java.library.path}, so the library must come out of the jar:
 *
 * <pre>
 * java -cp target/classes:target/test-classes:natives/target/dcmnorm-java-natives-*-linux-x86_64.jar \
 *   com.pohcee.dcmnorm.LoadCheck ../../test/files
 * </pre>
 *
 * Parses a DICOM fixture, then round-trips it through MPEG-2 (FFmpeg's native encoder and
 * decoder) and back to Explicit VR Little Endian, which fails unless FFmpeg is linked in.
 */
public final class LoadCheck {
    private static final String MPEG2_MP_ML = "1.2.840.10008.1.2.4.100";
    private static final String EXPLICIT_VR_LE = "1.2.840.10008.1.2.1";

    public static void main(String[] args) throws Exception {
        if (args.length != 1) {
            System.err.println("usage: LoadCheck <fixtures-dir>");
            System.exit(2);
        }
        String fixture = Path.of(args[0]).resolve("us.dcm").toString();
        if (Dcmnorm.readJson(fixture).isEmpty()) {
            throw new AssertionError("readJson returned nothing");
        }
        Path dir = Files.createTempDirectory("dcmnorm-load-check");
        String mpeg = dir.resolve("mpeg2.dcm").toString();
        String back = dir.resolve("back.dcm").toString();
        Dcmnorm.transcode(fixture, mpeg, MPEG2_MP_ML);
        Dcmnorm.transcode(mpeg, back, EXPLICIT_VR_LE);
        if (!Dcmnorm.readJson(back).contains("\"" + EXPLICIT_VR_LE + "\"")) {
            throw new AssertionError("MPEG-2 round trip did not come back as Explicit VR Little Endian");
        }
        System.out.println("ok: parse + MPEG-2 encode/decode round trip");
    }
}
