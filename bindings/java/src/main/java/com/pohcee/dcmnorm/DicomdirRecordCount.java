package com.pohcee.dcmnorm;

/** How many Directory Records of one type {@link Dcmnorm#writeDicomdir} wrote. */
public final class DicomdirRecordCount {
    private final String recordType;
    private final int count;

    DicomdirRecordCount(String recordType, int count) {
        this.recordType = recordType;
        this.count = count;
    }

    /** Directory Record Type, e.g. {@code "PATIENT"} or {@code "IMAGE"}. */
    public String getRecordType() {
        return recordType;
    }

    public int getCount() {
        return count;
    }

    @Override
    public String toString() {
        return "DicomdirRecordCount{recordType=" + recordType + ", count=" + count + "}";
    }
}
