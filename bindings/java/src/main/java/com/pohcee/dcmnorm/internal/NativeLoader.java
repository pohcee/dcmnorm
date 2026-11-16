package com.pohcee.dcmnorm.internal;

import java.io.BufferedReader;
import java.io.IOException;
import java.io.InputStream;
import java.io.InputStreamReader;
import java.io.OutputStream;
import java.nio.charset.StandardCharsets;
import java.nio.file.AtomicMoveNotSupportedException;
import java.nio.file.Files;
import java.nio.file.Path;
import java.nio.file.Paths;
import java.nio.file.StandardCopyOption;
import java.security.DigestInputStream;
import java.security.MessageDigest;
import java.security.NoSuchAlgorithmException;
import java.util.Locale;
import java.util.regex.Pattern;

/**
 * Loads the native {@code dcmnorm_java} library exactly once. Tries, in order:
 *
 * <ol>
 *   <li>The file named by the system property {@code dcmnorm.library.path}, when set - for
 *       containers that ship the {@code .so} on disk and want no extraction at all.
 *   <li>A resource bundled on the classpath at {@code /native/<os>-<arch>/libdcmnorm_java.so}
 *       (the shape a consumer gets from a jar built by {@code build-in-docker.sh} - mirrors
 *       {@code bindings/node} committing a prebuilt {@code .node} file and {@code
 *       bindings/python} committing a prebuilt wheel, so a consumer with no Rust toolchain can
 *       still use this binding). It is extracted once per library build to a per-user cache,
 *       {@code <cache>/dcmnorm/<sha256>/libdcmnorm_java.so}, and loaded from there by every
 *       later JVM - copying the ~28 MB library on every start was nearly all of the binding's
 *       first-use cost (pohcee/dcmnorm#5). The SHA-256 comes from the sidecar resource
 *       {@code libdcmnorm_java.so.sha256} written at build time, so checking the cache reads a
 *       few bytes, not the library. The file is written to a temporary name in the same
 *       directory and atomically renamed, so concurrent JVMs never load a half-written copy.
 *       When the cache is not writable, the library is extracted to a temporary file instead
 *       (the previous behaviour).
 *   <li>A plain {@code System.loadLibrary("dcmnorm_java")} lookup on {@code java.library.path} -
 *       the fast path for local development, where the {@code .so} was just built straight into
 *       a directory already on that path.
 * </ol>
 *
 * <p>The cache directory is {@code dcmnorm.cache.dir} when set, else {@code
 * $XDG_CACHE_HOME/dcmnorm} or {@code ~/.cache/dcmnorm} (Linux), {@code ~/Library/Caches/dcmnorm}
 * (macOS), {@code %LOCALAPPDATA%\dcmnorm} (Windows).
 *
 * <p><strong>Only linux-x64 is currently packaged.</strong> A consumer on another OS/architecture
 * needs a native library built for their platform on {@code java.library.path} (see this
 * module's own README).
 */
public final class NativeLoader {
    private static final String LIBRARY_NAME = "dcmnorm_java";
    private static final String LIBRARY_FILE = "lib" + LIBRARY_NAME + ".so";
    /** System property: an absolute path to the native library, loaded as is. */
    public static final String LIBRARY_PATH_PROPERTY = "dcmnorm.library.path";
    /** System property: the directory to cache the extracted native library in. */
    public static final String CACHE_DIR_PROPERTY = "dcmnorm.cache.dir";
    private static final Pattern SHA256 = Pattern.compile("[0-9a-f]{64}");
    private static volatile boolean loaded = false;

    private NativeLoader() {
    }

    public static synchronized void ensureLoaded() {
        if (loaded) {
            return;
        }
        String explicitPath = System.getProperty(LIBRARY_PATH_PROPERTY);
        if (explicitPath != null && !explicitPath.isEmpty()) {
            System.load(Paths.get(explicitPath).toAbsolutePath().toString());
            loaded = true;
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
                    + "classpath at /native/<os>-<arch>/libdcmnorm_java.so, or set -D"
                    + LIBRARY_PATH_PROPERTY + "=/path/to/libdcmnorm_java.so.");
        }
    }

    /** A source of the bundled library's bytes, opened once per extraction. */
    interface ResourceOpener {
        /** The resource stream, or {@code null} when it is absent. */
        InputStream open() throws IOException;
    }

    private static void loadFromBundledResource() {
        String resourcePath = "/native/" + platformDirectoryName() + "/" + LIBRARY_FILE;
        ResourceOpener library = () -> NativeLoader.class.getResourceAsStream(resourcePath);
        ResourceOpener sidecar = () -> NativeLoader.class.getResourceAsStream(resourcePath + ".sha256");
        Path file;
        try {
            file = extractLibrary(library, sidecar, cacheDirectory());
        } catch (IOException error) {
            throw new RuntimeException("failed extracting bundled native library: " + error.getMessage(), error);
        }
        if (file == null) {
            throw new RuntimeException("no bundled resource at " + resourcePath);
        }
        System.load(file.toAbsolutePath().toString());
    }

    /**
     * The file to load the bundled library from: its cached copy under {@code cacheRoot}
     * (extracted now if missing), or a fresh temporary file when the cache cannot be used.
     * {@code null} when the library resource is absent. Package-private for tests.
     */
    static Path extractLibrary(ResourceOpener library, ResourceOpener sidecar, Path cacheRoot) throws IOException {
        String hash = readSidecarHash(sidecar);
        if (hash == null) {
            hash = hashResource(library);
            if (hash == null) {
                return null;
            }
        }
        if (cacheRoot != null) {
            try {
                Path cached = cacheRoot.resolve(hash).resolve(LIBRARY_FILE);
                if (Files.isRegularFile(cached)) {
                    return cached;
                }
                Files.createDirectories(cached.getParent());
                Path partial = Files.createTempFile(cached.getParent(), LIBRARY_NAME, ".partial");
                try {
                    if (!copyResource(library, partial)) {
                        return null;
                    }
                    moveIntoPlace(partial, cached);
                } finally {
                    Files.deleteIfExists(partial);
                }
                return cached;
            } catch (IOException | SecurityException | UnsupportedOperationException cacheUnusable) {
                // not writable (read-only home, sandbox): fall through to a temporary file
            }
        }
        Path tempFile = Files.createTempFile(LIBRARY_NAME, ".so");
        tempFile.toFile().deleteOnExit();
        if (!copyResource(library, tempFile)) {
            Files.deleteIfExists(tempFile);
            return null;
        }
        return tempFile;
    }

    /** Atomically renames {@code partial} to {@code target}; another JVM may have won the race. */
    private static void moveIntoPlace(Path partial, Path target) throws IOException {
        try {
            Files.move(partial, target, StandardCopyOption.ATOMIC_MOVE);
        } catch (AtomicMoveNotSupportedException notAtomic) {
            if (!Files.isRegularFile(target)) {
                Files.move(partial, target, StandardCopyOption.REPLACE_EXISTING);
            }
        } catch (IOException raced) {
            // a concurrent JVM renamed its identical copy into place first
            if (!Files.isRegularFile(target)) {
                throw raced;
            }
        }
    }

    private static boolean copyResource(ResourceOpener library, Path target) throws IOException {
        try (InputStream in = library.open()) {
            if (in == null) {
                return false;
            }
            try (OutputStream out = Files.newOutputStream(target)) {
                in.transferTo(out);
            }
            return true;
        }
    }

    /** The SHA-256 in the sidecar resource ({@code sha256sum} output), or {@code null}. */
    private static String readSidecarHash(ResourceOpener sidecar) throws IOException {
        try (InputStream in = sidecar.open()) {
            if (in == null) {
                return null;
            }
            BufferedReader reader = new BufferedReader(new InputStreamReader(in, StandardCharsets.US_ASCII));
            String line = reader.readLine();
            if (line == null) {
                return null;
            }
            String hash = line.trim().split("\\s+")[0].toLowerCase(Locale.ROOT);
            return SHA256.matcher(hash).matches() ? hash : null;
        }
    }

    /** Hashes the library itself when no sidecar was packaged (a jar from an older build). */
    private static String hashResource(ResourceOpener library) throws IOException {
        MessageDigest digest;
        try {
            digest = MessageDigest.getInstance("SHA-256");
        } catch (NoSuchAlgorithmException impossible) {
            throw new IOException(impossible);
        }
        try (InputStream in = library.open()) {
            if (in == null) {
                return null;
            }
            try (DigestInputStream digesting = new DigestInputStream(in, digest)) {
                digesting.transferTo(OutputStream.nullOutputStream());
            }
        }
        StringBuilder hex = new StringBuilder(64);
        for (byte b : digest.digest()) {
            hex.append(String.format("%02x", b));
        }
        return hex.toString();
    }

    /** The per-user cache directory for extracted libraries, or {@code null} when there is none. */
    static Path cacheDirectory() {
        String explicit = System.getProperty(CACHE_DIR_PROPERTY);
        if (explicit != null && !explicit.isEmpty()) {
            return Paths.get(explicit);
        }
        String os = System.getProperty("os.name", "").toLowerCase(Locale.ROOT);
        String home = System.getProperty("user.home");
        if (os.contains("win")) {
            String localAppData = System.getenv("LOCALAPPDATA");
            return localAppData == null || localAppData.isEmpty() ? null : Paths.get(localAppData, "dcmnorm");
        }
        if (os.contains("mac")) {
            return home == null || home.isEmpty() ? null : Paths.get(home, "Library", "Caches", "dcmnorm");
        }
        String xdg = System.getenv("XDG_CACHE_HOME");
        if (xdg != null && !xdg.isEmpty()) {
            return Paths.get(xdg, "dcmnorm");
        }
        return home == null || home.isEmpty() ? null : Paths.get(home, ".cache", "dcmnorm");
    }

    private static String platformDirectoryName() {
        String os = System.getProperty("os.name", "").toLowerCase(Locale.ROOT);
        String arch = System.getProperty("os.arch", "").toLowerCase(Locale.ROOT);
        String osName = os.contains("linux") ? "linux" : os.contains("mac") ? "darwin" : os.contains("win") ? "win32" : os;
        String archName = (arch.equals("amd64") || arch.equals("x86_64")) ? "x64"
            : (arch.equals("aarch64") || arch.equals("arm64")) ? "arm64" : arch;
        return osName + "-" + archName;
    }
}
