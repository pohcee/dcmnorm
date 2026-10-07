package com.pohcee.dcmnorm;

import com.pohcee.dcmnorm.internal.Json;

/** Optional arguments for {@link Dcmnorm#storeScu}. */
public final class StoreScuOptions {
    private String callingAeTitle;
    private String calledAeTitle;
    private Integer maxPduLength;
    private Boolean neverTranscode;
    private Integer timeoutMs;
    private DimseLogger onLog;

    /** Defaults to {@code "DCMNORM"}. */
    public StoreScuOptions callingAeTitle(String callingAeTitle) {
        this.callingAeTitle = callingAeTitle;
        return this;
    }

    public StoreScuOptions calledAeTitle(String calledAeTitle) {
        this.calledAeTitle = calledAeTitle;
        return this;
    }

    /** Defaults to 16384. */
    public StoreScuOptions maxPduLength(int maxPduLength) {
        this.maxPduLength = maxPduLength;
        return this;
    }

    /** When true, only each file's own transfer syntax is proposed - a peer that doesn't
     * support it fails that file rather than receiving a transcoded copy. */
    public StoreScuOptions neverTranscode(boolean neverTranscode) {
        this.neverTranscode = neverTranscode;
        return this;
    }

    public StoreScuOptions timeoutMs(int timeoutMs) {
        this.timeoutMs = timeoutMs;
        return this;
    }

    /** Called synchronously, on the calling thread, with a debug line for each notable DIMSE
     * event. */
    public StoreScuOptions onLog(DimseLogger onLog) {
        this.onLog = onLog;
        return this;
    }

    DimseLogger getOnLog() {
        return onLog;
    }

    String toJson() {
        return Json.object().put("callingAeTitle", callingAeTitle).put("calledAeTitle", calledAeTitle)
            .put("maxPduLength", maxPduLength).put("neverTranscode", neverTranscode).put("timeoutMs", timeoutMs)
            .build();
    }
}
