package com.pohcee.dcmnorm;

/**
 * How {@link Dcmnorm#read} reads a data set. Immutable; each method returns a new instance.
 *
 * <ul>
 *   <li>{@link #untilPixelData()}: stop at the PixelData header, as dcm4che's {@code
 *       readDatasetUntilPixelData()} does - nothing past it is read (for an {@code InputStream},
 *       nothing past it is consumed), and PixelData is a {@link BulkDataRef} to where its value
 *       starts. The usual choice for ingest metadata.
 *   <li>{@link #full()} (the default): the whole data set, elements after PixelData included;
 *       large bulk values are still {@link BulkDataRef}s, skipped (by seeking in a file or
 *       {@code byte[]}, by reading and discarding from a stream), not read.
 *   <li>{@link #loadBulkData(boolean) loadBulkData(true)}: read bulk values into the data set as
 *       bytes instead (PixelData as raw bytes, or as fragments when encapsulated).
 * </ul>
 *
 * <p>A source without File Meta Information, or with a deflated data set, can't be read with
 * references: it is read in full and its bulk values are in memory (with {@code untilPixelData},
 * PixelData and what follows it are left out).
 */
public final class ReadOptions {
    private final boolean untilPixelData;
    private final boolean loadBulkData;

    private ReadOptions(boolean untilPixelData, boolean loadBulkData) {
        this.untilPixelData = untilPixelData;
        this.loadBulkData = loadBulkData;
    }

    /** Read up to the PixelData header and stop. */
    public static ReadOptions untilPixelData() {
        return new ReadOptions(true, false);
    }

    /** Read the whole data set (the default). */
    public static ReadOptions full() {
        return new ReadOptions(false, false);
    }

    /** Read bulk values into memory ({@code true}) or leave them in the source as {@link
     * BulkDataRef}s ({@code false}, the default). */
    public ReadOptions loadBulkData(boolean load) {
        return new ReadOptions(untilPixelData, load);
    }

    public boolean isUntilPixelData() {
        return untilPixelData;
    }

    public boolean isLoadBulkData() {
        return loadBulkData;
    }
}
