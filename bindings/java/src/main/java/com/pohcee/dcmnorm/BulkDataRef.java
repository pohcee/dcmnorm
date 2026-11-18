package com.pohcee.dcmnorm;

/**
 * A value left in its source instead of being read into the {@link DicomDataset}: where it is
 * (absolute byte offset into the file, {@code byte[]} or stream it was read from) and how long it
 * is. Large bulk values (binary VRs longer than 32 bytes) and PixelData are references unless
 * {@link ReadOptions#loadBulkData(boolean)} is set.
 *
 * <p>For encapsulated (compressed) pixel data the range starts at the first item header (the
 * Basic Offset Table item) and runs to the sequence delimiter, as a DICOM JSON {@code
 * BulkDataURI} does. Its length is {@code -1} ({@link #isLengthDefined()} false) when the read
 * stopped at the PixelData header ({@link ReadOptions#untilPixelData()}) without walking the
 * fragments.
 *
 * <p>This is also what a {@link JsonOptions#bulkDataUriBuilder} function receives for each value
 * it names.
 */
public final class BulkDataRef {
    private final int tag;
    private final VR vr;
    private final long offset;
    private final long length;

    public BulkDataRef(int tag, VR vr, long offset, long length) {
        this.tag = tag;
        this.vr = vr;
        this.offset = offset;
        this.length = length;
    }

    /** Constructed by native code (src/dataset.rs) for a {@code bulkDataUriBuilder}. */
    BulkDataRef(int tag, String vr, long offset, long length) {
        this(tag, VR.of(vr), offset, length);
    }

    public int getTag() {
        return tag;
    }

    public VR getVR() {
        return vr;
    }

    /** Absolute byte offset of the value in its source. */
    public long getOffset() {
        return offset;
    }

    /** The value's length in bytes, or {@code -1} when undefined (see the class doc). */
    public long getLength() {
        return length;
    }

    public boolean isLengthDefined() {
        return length >= 0;
    }

    @Override
    public boolean equals(Object other) {
        if (!(other instanceof BulkDataRef)) {
            return false;
        }
        BulkDataRef that = (BulkDataRef) other;
        return tag == that.tag && vr == that.vr && offset == that.offset && length == that.length;
    }

    @Override
    public int hashCode() {
        return Long.hashCode(offset) * 31 + tag;
    }

    @Override
    public String toString() {
        return Tag.toString(tag) + " " + vr + " @" + offset + (length >= 0 ? "+" + length : " (undefined length)");
    }
}
