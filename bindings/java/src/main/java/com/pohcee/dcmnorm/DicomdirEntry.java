package com.pohcee.dcmnorm;

import com.pohcee.dcmnorm.internal.Json;
import java.util.ArrayList;
import java.util.Collections;
import java.util.List;
import java.util.Objects;

/**
 * One file of the File-set for {@link Dcmnorm#writeDicomdir}: its File ID plus its header,
 * either read from a DICOM file ({@link #fromFile}; pixel data isn't loaded) or given as DICOM
 * JSON ({@link #fromJson}).
 *
 * <p>The File ID is 1-8 components of 1-8 characters from {@code A-Z}, {@code 0-9} and {@code _}
 * (PS3.10 8.2), e.g. {@code ["DICOM", "ST000001", "SE000001", "IM000001"]}; it is checked by
 * {@link Dcmnorm#writeDicomdir}, not here.
 */
public final class DicomdirEntry {
    private final List<String> fileId;
    private final String path;
    private final String json;
    private String format;
    private String transferSyntaxUid;

    private DicomdirEntry(List<String> fileId, String path, String json) {
        this.fileId = Collections.unmodifiableList(new ArrayList<>(Objects.requireNonNull(fileId, "fileId")));
        this.path = path;
        this.json = json;
    }

    /** An entry whose header is read from the DICOM file at {@code path}. */
    public static DicomdirEntry fromFile(List<String> fileId, String path) {
        return new DicomdirEntry(fileId, Objects.requireNonNull(path, "path"), null);
    }

    /** An entry whose header is the referenced file's DICOM JSON (what {@link Dcmnorm#readJson}
     * returns; {@code BulkDataURI} elements are ignored). */
    public static DicomdirEntry fromJson(List<String> fileId, String json) {
        return new DicomdirEntry(fileId, null, Objects.requireNonNull(json, "json"));
    }

    /** {@code "flat"} (default, {@link Dcmnorm#readJson}'s default) or {@code "standard"}, for a
     * {@link #fromJson} entry. */
    public DicomdirEntry format(String format) {
        this.format = format;
        return this;
    }

    /** The referenced file's transfer syntax. For a {@link #fromJson} entry it defaults to the
     * JSON's TransferSyntaxUID (0002,0010), else Explicit VR Little Endian; for a {@link
     * #fromFile} entry, to the file's. */
    public DicomdirEntry transferSyntaxUid(String transferSyntaxUid) {
        this.transferSyntaxUid = transferSyntaxUid;
        return this;
    }

    public List<String> getFileId() {
        return fileId;
    }

    /** The file path of a {@link #fromFile} entry, else {@code null}. */
    public String getPath() {
        return path;
    }

    /** The DICOM JSON of a {@link #fromJson} entry, else {@code null}. */
    public String getJson() {
        return json;
    }

    String toJson() {
        return Json.object()
            .putStrings("fileId", fileId)
            .put("path", path)
            .put("json", json)
            .put("format", format)
            .put("transferSyntaxUid", transferSyntaxUid)
            .build();
    }

    @Override
    public String toString() {
        return "DicomdirEntry{fileId=" + String.join("\\", fileId) + (path != null ? ", path=" + path : ", json") + "}";
    }
}
