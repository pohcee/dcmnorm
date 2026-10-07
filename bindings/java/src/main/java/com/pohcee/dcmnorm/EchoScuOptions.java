package com.pohcee.dcmnorm;

import com.pohcee.dcmnorm.internal.Json;

/** Optional arguments for {@link Dcmnorm#echoScu}. */
public final class EchoScuOptions {
    private String callingAeTitle;
    private String calledAeTitle;
    private Integer timeoutMs;
    private DimseLogger onLog;

    /** Defaults to {@code "DCMNORM"}. */
    public EchoScuOptions callingAeTitle(String callingAeTitle) {
        this.callingAeTitle = callingAeTitle;
        return this;
    }

    public EchoScuOptions calledAeTitle(String calledAeTitle) {
        this.calledAeTitle = calledAeTitle;
        return this;
    }

    public EchoScuOptions timeoutMs(int timeoutMs) {
        this.timeoutMs = timeoutMs;
        return this;
    }

    /** Called synchronously, on the calling thread, with a debug line for each notable DIMSE
     * event - association open/close, the request sent, the response received. */
    public EchoScuOptions onLog(DimseLogger onLog) {
        this.onLog = onLog;
        return this;
    }

    DimseLogger getOnLog() {
        return onLog;
    }

    String toJson() {
        return Json.object().put("callingAeTitle", callingAeTitle).put("calledAeTitle", calledAeTitle)
            .put("timeoutMs", timeoutMs).build();
    }
}
