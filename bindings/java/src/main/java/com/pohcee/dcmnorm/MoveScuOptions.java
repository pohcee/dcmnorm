package com.pohcee.dcmnorm;

import com.pohcee.dcmnorm.internal.Json;

/** Optional arguments for {@link Dcmnorm#moveScu}. */
public final class MoveScuOptions {
    private String callingAeTitle;
    private String calledAeTitle;
    private Integer maxPduLength;
    private Integer timeoutMs;
    private String watchPath;
    private Integer staleDataTimeoutMs;
    private DimseLogger onLog;

    /** Defaults to {@code "DCMNORM"}. */
    public MoveScuOptions callingAeTitle(String callingAeTitle) {
        this.callingAeTitle = callingAeTitle;
        return this;
    }

    public MoveScuOptions calledAeTitle(String calledAeTitle) {
        this.calledAeTitle = calledAeTitle;
        return this;
    }

    /** Defaults to 16384. */
    public MoveScuOptions maxPduLength(int maxPduLength) {
        this.maxPduLength = maxPduLength;
        return this;
    }

    /** Absolute ceiling for the whole call - connect through release. */
    public MoveScuOptions timeoutMs(int timeoutMs) {
        this.timeoutMs = timeoutMs;
        return this;
    }

    /** Directory to watch for local write progress - typically this retrieve's own cache
     * destination. Both this and {@link #staleDataTimeoutMs} must be set for the watch to run. */
    public MoveScuOptions watchPath(String watchPath) {
        this.watchPath = watchPath;
        return this;
    }

    /** How long {@link #watchPath} may go without a new/modified file before the connection is
     * considered stale and aborted - independent of, and typically much shorter than, {@link
     * #timeoutMs}. */
    public MoveScuOptions staleDataTimeoutMs(int staleDataTimeoutMs) {
        this.staleDataTimeoutMs = staleDataTimeoutMs;
        return this;
    }

    /** Called synchronously, on the calling thread, with a debug line for each notable DIMSE
     * event - association open/close, and each C-MOVE-RQ/RSP (including every pending response,
     * so a slow multi-instance move is visible sub-operation by sub-operation). */
    public MoveScuOptions onLog(DimseLogger onLog) {
        this.onLog = onLog;
        return this;
    }

    DimseLogger getOnLog() {
        return onLog;
    }

    String toJson() {
        return Json.object().put("callingAeTitle", callingAeTitle).put("calledAeTitle", calledAeTitle)
            .put("maxPduLength", maxPduLength).put("timeoutMs", timeoutMs).put("watchPath", watchPath)
            .put("staleDataTimeoutMs", staleDataTimeoutMs).build();
    }
}
