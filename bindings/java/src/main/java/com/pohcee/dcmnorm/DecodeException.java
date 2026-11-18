package com.pohcee.dcmnorm;

/** Pixel data could not be decoded from its transfer syntax ({@link #getSourceTransferSyntaxUid()}); {@link #getFrameIndex()} is the frame, when decoding one. See {@link DcmnormException}. */
public class DecodeException extends DcmnormException {
    private static final long serialVersionUID = 1L;

    public DecodeException(String message) {
        super(message, Kind.DECODE, null, null, -1);
    }

    /** Constructed by native code (src/lib.rs, {@code throw_api_error}). */
    DecodeException(String message, String kind, String sourceTransferSyntaxUid, String targetTransferSyntaxUid,
        int frameIndex) {
        super(message, Kind.DECODE, sourceTransferSyntaxUid, targetTransferSyntaxUid, frameIndex);
    }
}
