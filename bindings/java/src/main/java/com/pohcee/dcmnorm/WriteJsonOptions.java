package com.pohcee.dcmnorm;

import com.pohcee.dcmnorm.internal.Json;

/** Optional arguments for {@link Dcmnorm#writeJson}. */
public final class WriteJsonOptions {
    private String format;
    private String bulkDataSourcePath;

    /** {@code "flat"} (default) or {@code "standard"} - must match the format the JSON was read
     * with ({@link ReadJsonOptions#format}); never auto-detected. */
    public WriteJsonOptions format(String format) {
        this.format = format;
        return this;
    }

    /** Resolves {@code "?offset=..&length=.."} {@code BulkDataURI} references (as produced by
     * {@link Dcmnorm#readJson}'s default {@code bulkData: "uri"} mode) against that file's
     * bytes - mirrors the CLI's {@code --bulk-data-source}. */
    public WriteJsonOptions bulkDataSourcePath(String bulkDataSourcePath) {
        this.bulkDataSourcePath = bulkDataSourcePath;
        return this;
    }

    String toJson() {
        return Json.object().put("format", format).put("bulkDataSourcePath", bulkDataSourcePath).build();
    }
}
