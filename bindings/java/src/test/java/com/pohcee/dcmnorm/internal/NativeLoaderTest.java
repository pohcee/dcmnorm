package com.pohcee.dcmnorm.internal;

import java.io.ByteArrayInputStream;
import java.io.IOException;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.nio.file.Path;
import java.security.MessageDigest;
import java.util.ArrayList;
import java.util.List;
import java.util.concurrent.Callable;
import java.util.concurrent.ExecutorService;
import java.util.concurrent.Executors;
import java.util.concurrent.Future;
import java.util.concurrent.atomic.AtomicInteger;

/**
 * Tests NativeLoader's extraction cache (pohcee/dcmnorm#5) with a stand-in library resource - no
 * native code is loaded. A standalone main() like Smoke.java (this module has no test framework):
 * run by {@code build.sh test}, or directly:
 *
 * <pre>
 * java -cp target/classes:target/test-classes com.pohcee.dcmnorm.internal.NativeLoaderTest
 * </pre>
 */
public final class NativeLoaderTest {
    private static int checks = 0;

    public static void main(String[] args) throws Exception {
        Path root = Files.createTempDirectory("dcmnorm-loader-test");
        try {
            extractsOnceAndReusesTheCachedCopy(root.resolve("reuse"));
            withoutSidecarHashesTheResource(root.resolve("nosidecar"));
            newBuildGetsItsOwnCacheEntry(root.resolve("rebuild"));
            unwritableCacheFallsBackToTempFile(root.resolve("readonly"));
            absentResourceIsNull(root.resolve("absent"));
            concurrentExtractionsAgree(root.resolve("concurrent"));
        } finally {
            deleteRecursively(root);
        }
        System.out.println("NativeLoaderTest: " + checks + " checks passed");
    }

    /** The platform's library file name, as NativeLoader caches it. */
    private static final String LIBRARY_FILE = System.mapLibraryName("dcmnorm_java");

    private static final byte[] LIBRARY = "not really a shared library, only bytes to cache".getBytes(StandardCharsets.US_ASCII);

    private static NativeLoader.ResourceOpener bytes(byte[] content, AtomicInteger opens) {
        return () -> {
            opens.incrementAndGet();
            return new ByteArrayInputStream(content);
        };
    }

    private static NativeLoader.ResourceOpener sidecarFor(byte[] content) throws Exception {
        String line = sha256(content) + "  libdcmnorm_java.so\n";
        return () -> new ByteArrayInputStream(line.getBytes(StandardCharsets.US_ASCII));
    }

    private static final NativeLoader.ResourceOpener NO_SIDECAR = () -> null;

    private static void extractsOnceAndReusesTheCachedCopy(Path cache) throws Exception {
        AtomicInteger opens = new AtomicInteger();
        Path first = NativeLoader.extractLibrary(bytes(LIBRARY, opens), sidecarFor(LIBRARY), cache);
        check(first.equals(cache.resolve(sha256(LIBRARY)).resolve(LIBRARY_FILE)), "cache path keyed by hash: " + first);
        check(java.util.Arrays.equals(Files.readAllBytes(first), LIBRARY), "cached copy has the library's bytes");
        check(opens.get() == 1, "first start copies the library once");

        Path second = NativeLoader.extractLibrary(bytes(LIBRARY, opens), sidecarFor(LIBRARY), cache);
        check(second.equals(first), "second start reuses the cached copy");
        check(opens.get() == 1, "second start does not read the library resource at all");
        try (var files = Files.list(first.getParent())) {
            check(files.count() == 1, "no partial files left behind");
        }
    }

    private static void withoutSidecarHashesTheResource(Path cache) throws Exception {
        AtomicInteger opens = new AtomicInteger();
        Path path = NativeLoader.extractLibrary(bytes(LIBRARY, opens), NO_SIDECAR, cache);
        check(path.equals(cache.resolve(sha256(LIBRARY)).resolve(LIBRARY_FILE)), "hash computed when no sidecar");
        NativeLoader.extractLibrary(bytes(LIBRARY, opens), NO_SIDECAR, cache);
        check(opens.get() == 3, "hash (1) + copy (1), then hash only (1)");
    }

    private static void newBuildGetsItsOwnCacheEntry(Path cache) throws Exception {
        byte[] rebuilt = "a different build of the library".getBytes(StandardCharsets.US_ASCII);
        Path old = NativeLoader.extractLibrary(bytes(LIBRARY, new AtomicInteger()), sidecarFor(LIBRARY), cache);
        Path fresh = NativeLoader.extractLibrary(bytes(rebuilt, new AtomicInteger()), sidecarFor(rebuilt), cache);
        check(!old.equals(fresh), "a new library build is not served the old cached copy");
        check(java.util.Arrays.equals(Files.readAllBytes(fresh), rebuilt), "new build's bytes");
    }

    private static void unwritableCacheFallsBackToTempFile(Path cache) throws Exception {
        // a regular file where the cache directory should be: it cannot be created
        Files.createDirectories(cache.getParent());
        Files.write(cache, new byte[] {1});
        Path path = NativeLoader.extractLibrary(bytes(LIBRARY, new AtomicInteger()), sidecarFor(LIBRARY), cache);
        check(!path.startsWith(cache), "falls back outside the unusable cache: " + path);
        check(java.util.Arrays.equals(Files.readAllBytes(path), LIBRARY), "temp copy has the library's bytes");
        Files.delete(path);

        Path noCache = NativeLoader.extractLibrary(bytes(LIBRARY, new AtomicInteger()), sidecarFor(LIBRARY), null);
        check(noCache != null && java.util.Arrays.equals(Files.readAllBytes(noCache), LIBRARY), "no cache directory at all");
        Files.delete(noCache);
    }

    private static void absentResourceIsNull(Path cache) throws Exception {
        NativeLoader.ResourceOpener absent = () -> null;
        check(NativeLoader.extractLibrary(absent, NO_SIDECAR, cache) == null, "absent resource, no sidecar");
        check(NativeLoader.extractLibrary(absent, sidecarFor(LIBRARY), cache) == null, "absent resource, with sidecar");
        check(!Files.exists(cache.resolve(sha256(LIBRARY)).resolve(LIBRARY_FILE)), "nothing cached");
    }

    private static void concurrentExtractionsAgree(Path cache) throws Exception {
        ExecutorService pool = Executors.newFixedThreadPool(8);
        try {
            List<Callable<Path>> tasks = new ArrayList<>();
            for (int i = 0; i < 32; i++) {
                tasks.add(() -> NativeLoader.extractLibrary(bytes(LIBRARY, new AtomicInteger()), sidecarFor(LIBRARY), cache));
            }
            Path expected = cache.resolve(sha256(LIBRARY)).resolve(LIBRARY_FILE);
            for (Future<Path> result : pool.invokeAll(tasks)) {
                check(result.get().equals(expected), "every concurrent extraction returns the cached path");
            }
            check(java.util.Arrays.equals(Files.readAllBytes(expected), LIBRARY), "complete file after the race");
            try (var files = Files.list(expected.getParent())) {
                check(files.count() == 1, "no partial files left behind after the race");
            }
        } finally {
            pool.shutdownNow();
        }
    }

    private static String sha256(byte[] content) throws Exception {
        StringBuilder hex = new StringBuilder();
        for (byte b : MessageDigest.getInstance("SHA-256").digest(content)) {
            hex.append(String.format("%02x", b));
        }
        return hex.toString();
    }

    private static void check(boolean condition, String what) {
        if (!condition) {
            throw new AssertionError("FAILED: " + what);
        }
        checks++;
    }

    private static void deleteRecursively(Path root) throws IOException {
        if (!Files.exists(root)) {
            return;
        }
        try (var paths = Files.walk(root)) {
            paths.sorted(java.util.Comparator.reverseOrder()).forEach(path -> path.toFile().delete());
        }
    }
}
