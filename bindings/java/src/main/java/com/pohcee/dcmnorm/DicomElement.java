package com.pohcee.dcmnorm;

import java.util.ArrayList;
import java.util.Arrays;
import java.util.Collections;
import java.util.List;
import java.util.Objects;

/**
 * One attribute of a {@link DicomDataset}: its tag, VR and value, immutable. Most code reads
 * values through the dataset's typed accessors ({@link DicomDataset#getString(int)}, ...); this
 * class is for walking a dataset generically ({@code for (DicomElement e : dataset)}).
 *
 * <p>How the value is held ({@link #getValueType()}) follows the VR ({@link VR#kind()}): text
 * VRs - DS, IS, PN, UI and the date/time VRs included - hold one {@code String} per value with
 * the padding removed; binary integer VRs hold {@code long}s; FL/FD {@code double}s; AT tags;
 * the other binary VRs bytes (little-endian) unless the value was left in the source as a {@link
 * BulkDataRef}; SQ a list of item datasets.
 */
public final class DicomElement {
    /** How an element's value is held. */
    public enum ValueType {
        /** No value (zero length). */
        EMPTY,
        STRINGS,
        INTS,
        DOUBLES,
        TAGS,
        BYTES,
        /** Left in the source: {@link #getBulkDataRef()}. */
        BULK_DATA,
        /** Encapsulated pixel data that was read: {@link #getFragments()}. */
        FRAGMENTS,
        SEQUENCE
    }

    /** Encapsulated pixel data as read: the Basic Offset Table and the fragments. */
    public static final class Fragments {
        private final int[] offsetTable;
        private final List<byte[]> fragments;

        Fragments(int[] offsetTable, List<byte[]> fragments) {
            this.offsetTable = offsetTable;
            this.fragments = Collections.unmodifiableList(fragments);
        }

        /** The Basic Offset Table (empty when the file has none). A copy. */
        public int[] getOffsetTable() {
            return offsetTable.clone();
        }

        public int getFragmentCount() {
            return fragments.size();
        }

        /** Fragment {@code index}'s bytes. A copy. */
        public byte[] getFragment(int index) {
            return fragments.get(index).clone();
        }

        List<byte[]> fragmentsUnsafe() {
            return fragments;
        }

        int[] offsetTableUnsafe() {
            return offsetTable;
        }

        @Override
        public boolean equals(Object other) {
            if (!(other instanceof Fragments)) {
                return false;
            }
            Fragments that = (Fragments) other;
            if (!Arrays.equals(offsetTable, that.offsetTable) || fragments.size() != that.fragments.size()) {
                return false;
            }
            for (int i = 0; i < fragments.size(); i++) {
                if (!Arrays.equals(fragments.get(i), that.fragments.get(i))) {
                    return false;
                }
            }
            return true;
        }

        @Override
        public int hashCode() {
            return Arrays.hashCode(offsetTable) * 31 + fragments.size();
        }
    }

    private final int tag;
    private final VR vr;
    private final ValueType valueType;
    // null | String[] | long[] | double[] | int[] (tags) | byte[] | BulkDataRef | Fragments | List<DicomDataset>
    private final Object value;

    private DicomElement(int tag, VR vr, ValueType valueType, Object value) {
        this.tag = tag;
        this.vr = Objects.requireNonNull(vr, "vr");
        this.valueType = valueType;
        this.value = value;
    }

    static DicomElement empty(int tag, VR vr) {
        return new DicomElement(tag, vr, ValueType.EMPTY, null);
    }

    static DicomElement strings(int tag, VR vr, String[] values) {
        return new DicomElement(tag, vr, ValueType.STRINGS, values);
    }

    static DicomElement ints(int tag, VR vr, long[] values) {
        return new DicomElement(tag, vr, ValueType.INTS, values);
    }

    static DicomElement doubles(int tag, VR vr, double[] values) {
        return new DicomElement(tag, vr, ValueType.DOUBLES, values);
    }

    static DicomElement tags(int tag, VR vr, int[] values) {
        return new DicomElement(tag, vr, ValueType.TAGS, values);
    }

    static DicomElement bytes(int tag, VR vr, byte[] value) {
        return new DicomElement(tag, vr, ValueType.BYTES, value);
    }

    static DicomElement bulkData(int tag, VR vr, BulkDataRef reference) {
        return new DicomElement(tag, vr, ValueType.BULK_DATA, reference);
    }

    static DicomElement fragments(int tag, VR vr, Fragments fragments) {
        return new DicomElement(tag, vr, ValueType.FRAGMENTS, fragments);
    }

    static DicomElement sequence(int tag, List<DicomDataset> items) {
        return new DicomElement(tag, VR.SQ, ValueType.SEQUENCE, Collections.unmodifiableList(new ArrayList<>(items)));
    }

    public int getTag() {
        return tag;
    }

    public VR getVR() {
        return vr;
    }

    public ValueType getValueType() {
        return valueType;
    }

    /** The standard keyword, or {@code null} for a private/unknown tag (a dictionary lookup). */
    public String getKeyword() {
        return ElementDictionary.keywordOf(tag);
    }

    /** The value multiplicity: number of values, items or fragments (1 for bytes/bulk data). */
    public int getVM() {
        switch (valueType) {
            case EMPTY:
                return 0;
            case STRINGS:
                return ((String[]) value).length;
            case INTS:
                return ((long[]) value).length;
            case DOUBLES:
                return ((double[]) value).length;
            case TAGS:
                return ((int[]) value).length;
            case SEQUENCE:
                return ((List<?>) value).size();
            case FRAGMENTS:
                return ((Fragments) value).getFragmentCount();
            default:
                return 1;
        }
    }

    public boolean isEmpty() {
        return valueType == ValueType.EMPTY;
    }

    /** The bulk data reference, or {@code null} if the value was read. */
    public BulkDataRef getBulkDataRef() {
        return valueType == ValueType.BULK_DATA ? (BulkDataRef) value : null;
    }

    /** Encapsulated pixel data that was read, or {@code null}. */
    public Fragments getFragments() {
        return valueType == ValueType.FRAGMENTS ? (Fragments) value : null;
    }

    /** The items of a sequence (unmodifiable), or {@code null} if this isn't one. */
    @SuppressWarnings("unchecked")
    public List<DicomDataset> getItems() {
        return valueType == ValueType.SEQUENCE ? (List<DicomDataset>) value : null;
    }

    /** The raw value object, shared, never handed out: callers get copies. */
    Object rawValue() {
        return value;
    }

    @Override
    public boolean equals(Object other) {
        if (!(other instanceof DicomElement)) {
            return false;
        }
        DicomElement that = (DicomElement) other;
        if (tag != that.tag || vr != that.vr || valueType != that.valueType) {
            return false;
        }
        switch (valueType) {
            case EMPTY:
                return true;
            case STRINGS:
                return Arrays.equals((String[]) value, (String[]) that.value);
            case INTS:
                return Arrays.equals((long[]) value, (long[]) that.value);
            case DOUBLES:
                return Arrays.equals((double[]) value, (double[]) that.value);
            case TAGS:
                return Arrays.equals((int[]) value, (int[]) that.value);
            case BYTES:
                return Arrays.equals((byte[]) value, (byte[]) that.value);
            default:
                return value.equals(that.value);
        }
    }

    @Override
    public int hashCode() {
        return tag * 31 + valueType.hashCode();
    }

    @Override
    public String toString() {
        StringBuilder text = new StringBuilder(Tag.toString(tag)).append(' ').append(vr).append(' ');
        switch (valueType) {
            case EMPTY:
                return text.append("(empty)").toString();
            case STRINGS:
                return text.append(String.join("\\", (String[]) value)).toString();
            case INTS:
                return text.append(Arrays.toString((long[]) value)).toString();
            case DOUBLES:
                return text.append(Arrays.toString((double[]) value)).toString();
            case TAGS: {
                int[] tags = (int[]) value;
                for (int i = 0; i < tags.length; i++) {
                    text.append(i == 0 ? "" : "\\").append(Tag.toString(tags[i]));
                }
                return text.toString();
            }
            case BYTES:
                return text.append(((byte[]) value).length).append(" bytes").toString();
            case BULK_DATA:
                return text.append(value).toString();
            case FRAGMENTS:
                return text.append(getVM()).append(" fragments").toString();
            default:
                return text.append(getVM()).append(" items").toString();
        }
    }
}
