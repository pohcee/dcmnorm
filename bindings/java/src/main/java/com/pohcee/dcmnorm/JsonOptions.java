package com.pohcee.dcmnorm;

import com.pohcee.dcmnorm.internal.Json;
import java.util.function.Function;

/**
 * Options for {@link Dcmnorm#toDicomJson}: DICOM JSON as PS3.18 Annex F defines it (the DICOMweb
 * metadata form - tag keys, {@code vr}, {@code Value}, PN as {@code Alphabetic}/{@code
 * Ideographic}/{@code Phonetic} objects, {@code InlineBinary}/{@code BulkDataURI}). A fluent,
 * mutable builder.
 *
 * <pre>{@code
 * String json = Dcmnorm.toDicomJson(in, new JsonOptions()
 *     .excludeTags(Tag.PixelData)
 *     .bulkDataUriBuilder(ref -> baseUrl + "/bulk/" + Tag.toString(ref.getTag()) + "?offset=" + ref.getOffset()));
 * }</pre>
 */
public final class JsonOptions {
    /** How bulk values are written. */
    public enum BulkData {
        /** Values longer than 32 bytes (and PixelData) as {@code BulkDataURI}, never read from the
         * source (the default); shorter ones inline. */
        URI,
        /** Every value inline, base64 {@code InlineBinary} (reads PixelData). */
        INLINE
    }

    private int[] includeTags = new int[0];
    private int[] excludeTags = new int[0];
    private BulkData bulkData = BulkData.URI;
    private String bulkDataUriBase;
    private Function<BulkDataRef, String> bulkDataUriBuilder;
    private boolean includeFileMeta;
    private boolean keywordAndVM;

    public static JsonOptions defaults() {
        return new JsonOptions();
    }

    /** Only these top-level attributes (default: all). */
    public JsonOptions includeTags(int... tags) {
        this.includeTags = tags.clone();
        return this;
    }

    /** Leave these top-level attributes out. */
    public JsonOptions excludeTags(int... tags) {
        this.excludeTags = tags.clone();
        return this;
    }

    public JsonOptions bulkData(BulkData mode) {
        this.bulkData = mode;
        return this;
    }

    /** Prefix for the default {@code BulkDataURI} form {@code <base>?offset=N&length=M} (e.g. a
     * {@code file://} URI or a URL serving the source). */
    public JsonOptions bulkDataUriBase(String base) {
        this.bulkDataUriBase = base;
        return this;
    }

    /** Build each {@code BulkDataURI} yourself from where the value is in the source (called on
     * the calling thread, once per value written as a URI; an exception or a {@code null} fails
     * the call). Takes precedence over {@link #bulkDataUriBase}. */
    public JsonOptions bulkDataUriBuilder(Function<BulkDataRef, String> builder) {
        this.bulkDataUriBuilder = builder;
        return this;
    }

    /** Include the File Meta Information (group 0002). Off by default: it isn't part of the data
     * set a DICOMweb metadata response describes. */
    public JsonOptions includeFileMeta(boolean include) {
        this.includeFileMeta = include;
        return this;
    }

    /** Add dcmnorm's {@code Keyword} and {@code VM} members to each top-level attribute (not part
     * of PS3.18; off by default). */
    public JsonOptions keywordAndVM(boolean add) {
        this.keywordAndVM = add;
        return this;
    }

    int[] getIncludeTags() {
        return includeTags;
    }

    int[] getExcludeTags() {
        return excludeTags;
    }

    Function<BulkDataRef, String> getBulkDataUriBuilder() {
        return bulkDataUriBuilder;
    }

    String toJson() {
        return Json.object().put("bulkData", bulkData == BulkData.INLINE ? "inline" : "uri")
            .put("bulkDataUriBase", bulkDataUriBase).put("includeFileMeta", includeFileMeta)
            .put("keywordAndVm", keywordAndVM).build();
    }
}
