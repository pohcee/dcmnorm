package com.pohcee.dcmnorm;

import com.pohcee.dcmnorm.internal.Json;

/** Optional arguments for {@link Dcmnorm#readJson}. */
public final class ReadJsonOptions {
    private String format;
    private String keyStyle;
    private String bulkData;

    /** {@code "flat"} (default) or {@code "standard"}. */
    public ReadJsonOptions format(String format) {
        this.format = format;
        return this;
    }

    /** {@code "name"} (default) or {@code "hex"}. */
    public ReadJsonOptions keyStyle(String keyStyle) {
        this.keyStyle = keyStyle;
        return this;
    }

    /** {@code "uri"} (default, matching the CLI) or {@code "inline"}. */
    public ReadJsonOptions bulkData(String bulkData) {
        this.bulkData = bulkData;
        return this;
    }

    String toJson() {
        return Json.object().put("format", format).put("keyStyle", keyStyle).put("bulkData", bulkData).build();
    }
}
