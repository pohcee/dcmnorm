package com.pohcee.dcmnorm;

/**
 * What this build of the native library can do with one transfer syntax, from {@link
 * Dcmnorm#transferSyntaxSupport()}. Which codecs are compiled in differs by build, so check this
 * rather than assume (e.g. before choosing a transcoding target).
 */
public final class TransferSyntaxSupport {
    private final String uid;
    private final String name;
    private final boolean encapsulated;
    private final boolean canReadDataset;
    private final boolean canWriteDataset;
    private final boolean canDecode;
    private final boolean canEncode;
    private final boolean canTranscodeTo;

    TransferSyntaxSupport(String uid, String name, boolean encapsulated, boolean canReadDataset,
        boolean canWriteDataset, boolean canDecode, boolean canEncode, boolean canTranscodeTo) {
        this.uid = uid;
        this.name = name;
        this.encapsulated = encapsulated;
        this.canReadDataset = canReadDataset;
        this.canWriteDataset = canWriteDataset;
        this.canDecode = canDecode;
        this.canEncode = canEncode;
        this.canTranscodeTo = canTranscodeTo;
    }

    public String getUid() {
        return uid;
    }

    public String getName() {
        return name;
    }

    /** The generated {@link TransferSyntax} constant, or {@code null} for one not in the dictionary. */
    public TransferSyntax getTransferSyntax() {
        return TransferSyntax.fromUid(uid);
    }

    /** Pixel data is encapsulated (compressed) in this transfer syntax. */
    public boolean isEncapsulated() {
        return encapsulated;
    }

    /** Data sets in this transfer syntax can be read. */
    public boolean canReadDataset() {
        return canReadDataset;
    }

    /** Data sets can be written in this transfer syntax. */
    public boolean canWriteDataset() {
        return canWriteDataset;
    }

    /** Pixel data can be decoded from this transfer syntax (for rendering or transcoding from it). */
    public boolean canDecode() {
        return canDecode;
    }

    /** Pixel data can be encoded into this transfer syntax. */
    public boolean canEncode() {
        return canEncode;
    }

    /** {@link Dcmnorm#transcode} can produce this transfer syntax (a native one, or an encodable one). */
    public boolean canTranscodeTo() {
        return canTranscodeTo;
    }

    @Override
    public String toString() {
        return uid + " (" + name + ")";
    }
}
