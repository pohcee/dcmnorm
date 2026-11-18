package com.pohcee.dcmnorm;

import java.nio.file.Path;

/**
 * How {@link Dcmnorm#write} writes a data set. Immutable; each method returns a new instance.
 *
 * <p>File Meta Information is always created for the output, as dcm4che's {@code
 * createFileMetaInformation(tsuid)} does: Media Storage SOP Class/Instance UID from the data set's
 * SOP Class/Instance UID, the transfer syntax written, and dcmnorm as the implementation (the
 * source's Source Application Entity Title is kept).
 */
public final class WriteOptions {
    private final String transferSyntaxUid;
    private final Path bulkDataPath;
    private final byte[] bulkDataBytes;

    private WriteOptions(String transferSyntaxUid, Path bulkDataPath, byte[] bulkDataBytes) {
        this.transferSyntaxUid = transferSyntaxUid;
        this.bulkDataPath = bulkDataPath;
        this.bulkDataBytes = bulkDataBytes;
    }

    /** Write in the data set's own transfer syntax, bulk data from the source it was read from. */
    public static WriteOptions defaults() {
        return new WriteOptions(null, null, null);
    }

    /** Write in (transcode to) this transfer syntax. */
    public static WriteOptions transferSyntax(String uid) {
        return new WriteOptions(uid, null, null);
    }

    public static WriteOptions transferSyntax(TransferSyntax transferSyntax) {
        return transferSyntax(transferSyntax.uid());
    }

    /** Read the data set's {@link BulkDataRef}s from this file instead of the source it was read
     * from - needed for a data set read from an {@code InputStream} (whose bytes are gone), or one
     * moved to another location since. */
    public WriteOptions bulkDataSource(Path file) {
        return new WriteOptions(transferSyntaxUid, file, null);
    }

    /** As {@link #bulkDataSource(Path)}, from the complete object's bytes. */
    public WriteOptions bulkDataSource(byte[] bytes) {
        return new WriteOptions(transferSyntaxUid, null, bytes);
    }

    String getTransferSyntaxUid() {
        return transferSyntaxUid;
    }

    Path getBulkDataPath() {
        return bulkDataPath;
    }

    byte[] getBulkDataBytes() {
        return bulkDataBytes;
    }
}
