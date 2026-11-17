package com.pohcee.dcmnorm;

import java.util.Collections;
import java.util.LinkedHashMap;
import java.util.List;
import java.util.Map;

/** What {@link Dcmnorm#writeDicomdir} wrote. */
public final class WriteDicomdirResult {
    private final int records;
    private final List<DicomdirRecordCount> recordTypes;
    private final List<DicomdirSynthesized> synthesized;
    private final List<String> warnings;

    WriteDicomdirResult(int records, List<DicomdirRecordCount> recordTypes, List<DicomdirSynthesized> synthesized,
        List<String> warnings) {
        this.records = records;
        this.recordTypes = Collections.unmodifiableList(recordTypes);
        this.synthesized = Collections.unmodifiableList(synthesized);
        this.warnings = Collections.unmodifiableList(warnings);
    }

    /** Total Directory Records written. */
    public int getRecords() {
        return records;
    }

    /** Records per Directory Record Type, in first-seen order. */
    public List<DicomdirRecordCount> getRecordTypes() {
        return recordTypes;
    }

    /** {@link #getRecordTypes()} as a {@code recordType -> count} map, in first-seen order. */
    public Map<String, Integer> getRecordCounts() {
        Map<String, Integer> counts = new LinkedHashMap<>();
        for (DicomdirRecordCount count : recordTypes) {
            counts.put(count.getRecordType(), count.getCount());
        }
        return counts;
    }

    /** Type 1 key values that had to be made up or copied from another attribute. */
    public List<DicomdirSynthesized> getSynthesized() {
        return synthesized;
    }

    public List<String> getWarnings() {
        return warnings;
    }

    @Override
    public String toString() {
        return "WriteDicomdirResult{records=" + records + ", recordTypes=" + getRecordCounts() + ", synthesized="
            + synthesized.size() + ", warnings=" + warnings.size() + "}";
    }
}
