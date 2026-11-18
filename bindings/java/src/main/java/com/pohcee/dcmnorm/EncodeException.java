package com.pohcee.dcmnorm;

/** Pixel data could not be encoded into the target transfer syntax ({@link #getTargetTransferSyntaxUid()}), or the output could not be written. See {@link DcmnormException}. */
public class EncodeException extends DcmnormException {
    private static final long serialVersionUID = 1L;

    public EncodeException(String message) {
        super(message, Kind.ENCODE, null, null, -1);
    }

    /** Constructed by native code (src/lib.rs, {@code throw_api_error}). */
    EncodeException(String message, String kind, String sourceTransferSyntaxUid, String targetTransferSyntaxUid,
        int frameIndex) {
        super(message, Kind.ENCODE, sourceTransferSyntaxUid, targetTransferSyntaxUid, frameIndex);
    }
}
