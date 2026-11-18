package com.pohcee.dcmnorm;

/** A transfer syntax this build can't read, decode or encode, or an unknown transfer syntax UID. The UID concerned is {@link #getSourceTransferSyntaxUid()} and/or {@link #getTargetTransferSyntaxUid()}. See {@link DcmnormException}. */
public class UnsupportedTransferSyntaxException extends DcmnormException {
    private static final long serialVersionUID = 1L;

    public UnsupportedTransferSyntaxException(String message) {
        super(message, Kind.UNSUPPORTED_TRANSFER_SYNTAX, null, null, -1);
    }

    /** Constructed by native code (src/lib.rs, {@code throw_api_error}). */
    UnsupportedTransferSyntaxException(String message, String kind, String sourceTransferSyntaxUid, String targetTransferSyntaxUid,
        int frameIndex) {
        super(message, Kind.UNSUPPORTED_TRANSFER_SYNTAX, sourceTransferSyntaxUid, targetTransferSyntaxUid, frameIndex);
    }
}
