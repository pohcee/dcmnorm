package com.pohcee.dcmnorm;

/** Not DICOM, or malformed DICOM (bad preamble, truncated or unparseable data set). See {@link DcmnormException}. */
public class InvalidDicomException extends DcmnormException {
    private static final long serialVersionUID = 1L;

    public InvalidDicomException(String message) {
        super(message, Kind.INVALID_DICOM, null, null, -1);
    }

    /** Constructed by native code (src/lib.rs, {@code throw_api_error}). */
    InvalidDicomException(String message, String kind, String sourceTransferSyntaxUid, String targetTransferSyntaxUid,
        int frameIndex) {
        super(message, Kind.INVALID_DICOM, sourceTransferSyntaxUid, targetTransferSyntaxUid, frameIndex);
    }
}
