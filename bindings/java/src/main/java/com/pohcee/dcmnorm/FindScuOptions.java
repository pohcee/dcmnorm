package com.pohcee.dcmnorm;

import com.pohcee.dcmnorm.internal.Json;

/** Optional arguments for {@link Dcmnorm#findScu}. */
public final class FindScuOptions {
    private String callingAeTitle;
    private String calledAeTitle;
    private Integer maxPduLength;
    private Integer timeoutMs;
    private DimseLogger onLog;

    /** Defaults to {@code "DCMNORM"}. */
    public FindScuOptions callingAeTitle(String callingAeTitle) {
        this.callingAeTitle = callingAeTitle;
        return this;
    }

    public FindScuOptions calledAeTitle(String calledAeTitle) {
        this.calledAeTitle = calledAeTitle;
        return this;
    }

    /** Defaults to 16384. */
    public FindScuOptions maxPduLength(int maxPduLength) {
        this.maxPduLength = maxPduLength;
        return this;
    }

    public FindScuOptions timeoutMs(int timeoutMs) {
        this.timeoutMs = timeoutMs;
        return this;
    }

    /** Called synchronously, on the calling thread, with a debug line for each notable DIMSE
     * event - association open/close, and each C-FIND-RQ/RSP (query values are not logged, only
     * the tag keys queried, since the Identifier commonly carries PHI). */
    public FindScuOptions onLog(DimseLogger onLog) {
        this.onLog = onLog;
        return this;
    }

    DimseLogger getOnLog() {
        return onLog;
    }

    String toJson() {
        return Json.object().put("callingAeTitle", callingAeTitle).put("calledAeTitle", calledAeTitle)
            .put("maxPduLength", maxPduLength).put("timeoutMs", timeoutMs).build();
    }
}
