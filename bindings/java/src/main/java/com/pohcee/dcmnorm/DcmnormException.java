package com.pohcee.dcmnorm;

/**
 * Raised by every {@link Dcmnorm} entry point (and {@link DicomVolumeHandle} method) that can
 * fail - a bad/malformed DICOM file, an invalid argument, a DIMSE association that could not be
 * established, and so on. Mirrors {@code DcmnormError} in the Python bindings and a rejected
 * {@code Error} in the Node bindings: this is an expected, catchable failure mode, not a
 * programming error, so it is a checked exception rather than a {@link RuntimeException}.
 *
 * <p>A panic inside the native library (e.g. from a severely malformed DICOM file hitting an
 * edge case the parser does not expect) is also surfaced as this exception, with a message
 * prefixed {@code "dcmnorm internal error: "}, rather than crashing the JVM - every native entry
 * point runs the actual dcmnorm call through Rust's {@code catch_unwind} first.
 */
public final class DcmnormException extends Exception {
    private static final long serialVersionUID = 1L;

    public DcmnormException(String message) {
        super(message);
    }
}
