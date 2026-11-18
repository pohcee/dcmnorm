package com.pohcee.dcmnorm;

/**
 * Raised by every {@link Dcmnorm} entry point (and {@link DicomVolumeHandle} method) that can
 * fail - a bad/malformed DICOM file, an invalid argument, a DIMSE association that could not be
 * established, and so on. Mirrors {@code DcmnormError} in the Python bindings and a rejected
 * {@code Error} in the Node bindings: this is an expected, catchable failure mode, not a
 * programming error, so it is a checked exception rather than a {@link RuntimeException}.
 *
 * <p>The native library classifies each failure ({@link #getKind()}) and raises the matching
 * subclass, so callers can tell them apart without parsing messages:
 *
 * <ul>
 *   <li>{@link InvalidDicomException}: not DICOM, or malformed DICOM;
 *   <li>{@link UnsupportedTransferSyntaxException}: a transfer syntax this build can't read,
 *       decode or encode (or an unknown UID);
 *   <li>{@link DecodeException} / {@link EncodeException}: pixel data could not be decoded from
 *       / encoded into a transfer syntax (or the output not written);
 *   <li>{@link RenderException}: a frame could not be rendered;
 *   <li>{@link DimseException}: a DIMSE association or operation failed;
 *   <li>this class itself for everything else ({@link Kind#IO}, {@link Kind#INVALID_ARGUMENT},
 *       {@link Kind#INTERNAL}, and the calls that predate the classification).
 * </ul>
 *
 * <p>{@link #getSourceTransferSyntaxUid()}, {@link #getTargetTransferSyntaxUid()} and {@link
 * #getFrameIndex()} carry the context where the failure has one (decode: source; encode:
 * target; an unsupported conversion: both; rendering or decoding one frame: its index).
 *
 * <p>A panic inside the native library (e.g. from a severely malformed DICOM file hitting an
 * edge case the parser does not expect) is also surfaced as this exception ({@link
 * Kind#INTERNAL}), with a message prefixed {@code "dcmnorm internal error: "}, rather than
 * crashing the JVM - every native entry point runs the actual dcmnorm call through Rust's {@code
 * catch_unwind} first.
 */
public class DcmnormException extends Exception {
    private static final long serialVersionUID = 2L;

    /** What kind of failure this is (the native library's classification). */
    public enum Kind {
        INVALID_DICOM,
        UNSUPPORTED_TRANSFER_SYNTAX,
        DECODE,
        ENCODE,
        RENDER,
        DIMSE,
        IO,
        INVALID_ARGUMENT,
        INTERNAL,
        /** Raised by a call that doesn't classify its failures. */
        OTHER;

        static Kind fromNative(String name) {
            if (name == null) {
                return OTHER;
            }
            switch (name) {
                case "InvalidDicom":
                    return INVALID_DICOM;
                case "UnsupportedTransferSyntax":
                    return UNSUPPORTED_TRANSFER_SYNTAX;
                case "Decode":
                    return DECODE;
                case "Encode":
                    return ENCODE;
                case "Render":
                    return RENDER;
                case "Dimse":
                    return DIMSE;
                case "Io":
                    return IO;
                case "InvalidArgument":
                    return INVALID_ARGUMENT;
                case "Internal":
                    return INTERNAL;
                default:
                    return OTHER;
            }
        }
    }

    private final Kind kind;
    private final String sourceTransferSyntaxUid;
    private final String targetTransferSyntaxUid;
    private final int frameIndex;

    public DcmnormException(String message) {
        this(message, Kind.OTHER, null, null, -1);
    }

    protected DcmnormException(String message, Kind kind, String sourceTransferSyntaxUid,
        String targetTransferSyntaxUid, int frameIndex) {
        super(message);
        this.kind = kind;
        this.sourceTransferSyntaxUid = sourceTransferSyntaxUid;
        this.targetTransferSyntaxUid = targetTransferSyntaxUid;
        this.frameIndex = frameIndex;
    }

    /** Constructed by native code (src/lib.rs, {@code throw_api_error}). */
    DcmnormException(String message, String kind, String sourceTransferSyntaxUid, String targetTransferSyntaxUid,
        int frameIndex) {
        this(message, Kind.fromNative(kind), sourceTransferSyntaxUid, targetTransferSyntaxUid, frameIndex);
    }

    public Kind getKind() {
        return kind;
    }

    /** The transfer syntax read from or decoded, where the failure involves one; else {@code null}. */
    public String getSourceTransferSyntaxUid() {
        return sourceTransferSyntaxUid;
    }

    /** The transfer syntax written or encoded to, where the failure involves one; else {@code null}. */
    public String getTargetTransferSyntaxUid() {
        return targetTransferSyntaxUid;
    }

    /** The 0-based frame being rendered or decoded, or {@code -1}. */
    public int getFrameIndex() {
        return frameIndex;
    }
}
