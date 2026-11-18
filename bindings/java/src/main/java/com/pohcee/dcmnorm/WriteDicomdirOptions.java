package com.pohcee.dcmnorm;

import com.pohcee.dcmnorm.internal.Json;

/** Optional arguments for {@link Dcmnorm#writeDicomdir}. */
public final class WriteDicomdirOptions {
    private String fileSetId;
    private String sopInstanceUid;

    /** File-set ID (0004,1130): up to 16 characters from {@code A-Z}, {@code 0-9} and {@code _}.
     * Default empty. */
    public WriteDicomdirOptions fileSetId(String fileSetId) {
        this.fileSetId = fileSetId;
        return this;
    }

    /** SOP Instance UID of the DICOMDIR itself. Generated when not given; set it for
     * reproducible output. */
    public WriteDicomdirOptions sopInstanceUid(String sopInstanceUid) {
        this.sopInstanceUid = sopInstanceUid;
        return this;
    }

    String toJson() {
        return Json.object().put("fileSetId", fileSetId).put("sopInstanceUid", sopInstanceUid).build();
    }
}
