package com.pohcee.dcmnorm.internal;

import java.io.IOException;
import java.io.InputStream;
import java.io.OutputStream;
import java.nio.file.Files;
import java.nio.file.Path;

/**
 * Loads the native {@code dcmnorm_java} library exactly once. Tries, in order:
 *
 * <ol>
 *   <li>A resource bundled on the classpath at {@code /native/<os>-<arch>/libdcmnorm_java.so}
 *       (the shape a consumer gets from a jar built by {@code build-in-docker.sh} - mirrors
 *       {@code bindings/node} committing a prebuilt {@code .node} file and {@code
 *       bindings/python} committing a prebuilt wheel, so a consumer with no Rust toolchain can
 *       still use this binding).
 *   <li>A plain {@code System.loadLibrary("dcmnorm_java")} lookup on {@code java.library.path} -
 *       the fast path for local development, where the {@code .so} was just built straight into
 *       a directory already on that path.
 * </ol>
 *
 * <p><strong>Only linux-x64 is currently packaged.</strong> A consumer on another OS/architecture
 * needs a native library built for their platform on {@code java.library.path} (see this
 * module's own README).
 */
public final class NativeLoader {
    private static final String LIBRARY_NAME = "dcmnorm_java";
    private static volatile boolean loaded = false;

    private NativeLoader() {
    }

    public static synchronized void ensureLoaded() {
        if (loaded) {
            return;
        }
        RuntimeException resourceFailure = null;
        try {
            loadFromBundledResource();
            loaded = true;
            return;
        } catch (RuntimeException error) {
            resourceFailure = error;
        }
        try {
            System.loadLibrary(LIBRARY_NAME);
            loaded = true;
        } catch (UnsatisfiedLinkError linkError) {
            throw new UnsatisfiedLinkError(
                "Could not load the native dcmnorm_java library: no bundled resource for this platform ("
                    + resourceFailure.getMessage() + "), and System.loadLibrary(\"" + LIBRARY_NAME
                    + "\") failed (" + linkError.getMessage() + "). Build it with bindings/java/build.sh "
                    + "(or build-in-docker.sh) and put the resulting .so on java.library.path, or on the "
                    + "classpath at /native/<os>-<arch>/libdcmnorm_java.so.");
        }
    }

    private static void loadFromBundledResource() {
        String resourcePath = "/native/" + platformDirectoryName() + "/lib" + LIBRARY_NAME + ".so";
        try (InputStream in = NativeLoader.class.getResourceAsStream(resourcePath)) {
            if (in == null) {
                throw new RuntimeException("no bundled resource at " + resourcePath);
            }
            Path tempFile = Files.createTempFile(LIBRARY_NAME, ".so");
            tempFile.toFile().deleteOnExit();
            try (OutputStream out = Files.newOutputStream(tempFile)) {
                in.transferTo(out);
            }
            System.load(tempFile.toAbsolutePath().toString());
        } catch (IOException error) {
            throw new RuntimeException("failed extracting bundled native library: " + error.getMessage(), error);
        }
    }

    private static String platformDirectoryName() {
        String os = System.getProperty("os.name", "").toLowerCase(java.util.Locale.ROOT);
        String arch = System.getProperty("os.arch", "").toLowerCase(java.util.Locale.ROOT);
        String osName = os.contains("linux") ? "linux" : os.contains("mac") ? "darwin" : os.contains("win") ? "win32" : os;
        String archName = (arch.equals("amd64") || arch.equals("x86_64")) ? "x64"
            : (arch.equals("aarch64") || arch.equals("arm64")) ? "arm64" : arch;
        return osName + "-" + archName;
    }
}
