package com.pohcee.dcmnorm;

/** One file's outcome from {@link Dcmnorm#storeScu}. */
public final class StoreScuResult {
    private final String sopInstanceUid;
    private final int status;

    StoreScuResult(String sopInstanceUid, int status) {
        this.sopInstanceUid = sopInstanceUid;
        this.status = status;
    }

    public String getSopInstanceUid() {
        return sopInstanceUid;
    }

    /** The C-STORE response Status code - 0 means success. A non-zero status here is just data
     * (the peer rejected this one instance); {@link Dcmnorm#storeScu} only raises {@link
     * DcmnormException} if the association itself could not be established, or none of the
     * requested files could be read as DICOM at all. */
    public int getStatus() {
        return status;
    }

    @Override
    public String toString() {
        return "StoreScuResult{sopInstanceUid=" + sopInstanceUid + ", status=" + status + "}";
    }
}
