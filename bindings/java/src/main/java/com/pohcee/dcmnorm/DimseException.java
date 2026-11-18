package com.pohcee.dcmnorm;

/** A DIMSE association or operation failed. See {@link DcmnormException}. */
public class DimseException extends DcmnormException {
    private static final long serialVersionUID = 1L;

    public DimseException(String message) {
        super(message, Kind.DIMSE, null, null, -1);
    }

    /** Constructed by native code (src/lib.rs, {@code throw_api_error}). */
    DimseException(String message, String kind, String sourceTransferSyntaxUid, String targetTransferSyntaxUid,
        int frameIndex) {
        super(message, Kind.DIMSE, sourceTransferSyntaxUid, targetTransferSyntaxUid, frameIndex);
    }
}
