package com.pohcee.dcmnorm;

/**
 * A value {@link Dcmnorm#writeDicomdir} made up (or copied from another attribute) for a Type 1
 * key the instances lacked (IHE RAD TF-2 Table 4.47.4-1).
 */
public final class DicomdirSynthesized {
    private final String recordType;
    private final String entity;
    private final String attribute;
    private final String value;
    private final String source;

    DicomdirSynthesized(String recordType, String entity, String attribute, String value, String source) {
        this.recordType = recordType;
        this.entity = entity;
        this.attribute = attribute;
        this.value = value;
        this.source = source;
    }

    /** e.g. {@code "STUDY"}. */
    public String getRecordType() {
        return recordType;
    }

    /** The Study/Series/SOP Instance UID; for PATIENT the patient's name, or {@code "patient of
     * study <UID>"} when the patient has no name either. */
    public String getEntity() {
        return entity;
    }

    /** Attribute keyword, e.g. {@code "StudyID"}. */
    public String getAttribute() {
        return attribute;
    }

    public String getValue() {
        return value;
    }

    /** Keyword of the instance attribute it was copied from (e.g. {@code "SeriesDate"}), or
     * {@code null}. */
    public String getSource() {
        return source;
    }

    @Override
    public String toString() {
        return "DicomdirSynthesized{recordType=" + recordType + ", entity=" + entity + ", attribute=" + attribute
            + ", value=" + value + ", source=" + source + "}";
    }
}
