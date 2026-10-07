package com.pohcee.dcmnorm;

/** The terminal outcome of {@link Dcmnorm#moveScu}, regardless of success/warning/failure. */
public final class MoveScuResult {
    private final int status;
    private final int completed;
    private final int failed;
    private final int warning;
    private final int remaining;
    private final boolean cancelled;
    private final String cancelledVia;

    MoveScuResult(int status, int completed, int failed, int warning, int remaining, boolean cancelled, String cancelledVia) {
        this.status = status;
        this.completed = completed;
        this.failed = failed;
        this.warning = warning;
        this.remaining = remaining;
        this.cancelled = cancelled;
        this.cancelledVia = cancelledVia;
    }

    /** The terminal C-MOVE-RSP Status code - 0 means every sub-operation succeeded. */
    public int getStatus() {
        return status;
    }

    public int getCompleted() {
        return completed;
    }

    public int getFailed() {
        return failed;
    }

    public int getWarning() {
        return warning;
    }

    public int getRemaining() {
        return remaining;
    }

    /** Always {@code false} today - {@link Dcmnorm#moveScu} has no early abort/release, unlike
     * the Python bindings' handle-based {@code move_scu} (see this module's README). Reserved so
     * a future handle-based variant can reuse this class unchanged. */
    public boolean isCancelled() {
        return cancelled;
    }

    /** {@code "release"}/{@code "abort"} when {@link #isCancelled()} is true, or {@code null}. */
    public String getCancelledVia() {
        return cancelledVia;
    }

    @Override
    public String toString() {
        return "MoveScuResult{status=" + status + ", completed=" + completed + ", failed=" + failed + ", warning="
            + warning + ", remaining=" + remaining + ", cancelled=" + cancelled + ", cancelledVia=" + cancelledVia + "}";
    }
}
