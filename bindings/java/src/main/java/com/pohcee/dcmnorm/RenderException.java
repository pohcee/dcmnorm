package com.pohcee.dcmnorm;

/** A frame could not be rendered (frame index out of range, unsupported photometric interpretation or bit depth, invalid window, ...). {@link #getFrameIndex()} is the frame. See {@link DcmnormException}. */
public class RenderException extends DcmnormException {
    private static final long serialVersionUID = 1L;

    public RenderException(String message) {
        super(message, Kind.RENDER, null, null, -1);
    }

    /** Constructed by native code (src/lib.rs, {@code throw_api_error}). */
    RenderException(String message, String kind, String sourceTransferSyntaxUid, String targetTransferSyntaxUid,
        int frameIndex) {
        super(message, Kind.RENDER, sourceTransferSyntaxUid, targetTransferSyntaxUid, frameIndex);
    }
}
