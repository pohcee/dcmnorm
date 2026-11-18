package com.pohcee.dcmnorm;

import java.util.Arrays;
import java.util.Collections;
import java.util.Iterator;
import java.util.List;
import java.util.Map;
import java.util.OptionalDouble;
import java.util.TreeMap;

/**
 * A read-only DICOM data set as plain Java objects: what {@link Dcmnorm#read} returns, built in
 * one native call. It holds no native resources (nothing to close) and is safe to share between
 * threads; edits go through {@link #toBuilder()}.
 *
 * <p>The accessors follow dcm4che's {@code Attributes}: {@code getString(Tag.StudyInstanceUID)},
 * {@code getInt(Tag.NumberOfFrames, 1)}, {@code getSequence(Tag.ReferencedSeriesSequence)}.
 * Single-value accessors take the first value (or value {@code index}) and return the default
 * (or {@code null}) when the attribute is absent, empty, or that value is empty; array accessors
 * return {@code null} when the attribute is absent and an empty array when it has no value.
 * Numbers convert as you'd expect: {@code getInt} reads an IS/DS string or a US/SL/... binary
 * value, {@code getString} formats a binary number. A value that can't be parsed gives the
 * default ({@code getInts}/{@code getDoubles} throw {@link NumberFormatException} instead, as
 * dcm4che does).
 *
 * <p>Large bulk values - PixelData, and binary VRs longer than 32 bytes - are usually not in the
 * data set but {@link BulkDataRef}s into the source ({@link #getBulkDataRef}); {@link
 * Dcmnorm#write} reads them from there. Arrays returned by accessors are copies.
 */
public final class DicomDataset implements Iterable<DicomElement> {
    static final DicomDataset EMPTY = new DicomDataset(new TreeMap<>(Integer::compareUnsigned), null, null, null);

    private final TreeMap<Integer, DicomElement> elements;
    private final DicomDataset fileMeta;
    private final String transferSyntaxUid;
    // what the dataset was read from (String path or byte[]), for Dcmnorm.write's bulk data
    private final Object source;

    DicomDataset(TreeMap<Integer, DicomElement> elements, DicomDataset fileMeta, String transferSyntaxUid, Object source) {
        this.elements = elements;
        this.fileMeta = fileMeta;
        this.transferSyntaxUid = transferSyntaxUid;
        this.source = source;
    }

    /** An empty data set, to build a new one from: {@code DicomDataset.empty().toBuilder()}. */
    public static DicomDataset empty() {
        return EMPTY;
    }

    // ---------------------------------------------------------------------------------------
    // Elements
    // ---------------------------------------------------------------------------------------

    public int size() {
        return elements.size();
    }

    public boolean isEmpty() {
        return elements.isEmpty();
    }

    public boolean contains(int tag) {
        return elements.containsKey(tag);
    }

    /** Present and not empty. */
    public boolean containsValue(int tag) {
        DicomElement element = elements.get(tag);
        return element != null && !element.isEmpty();
    }

    /** The element, or {@code null}. */
    public DicomElement get(int tag) {
        return elements.get(tag);
    }

    /** The element's VR, or {@code null} if absent. */
    public VR vr(int tag) {
        DicomElement element = elements.get(tag);
        return element == null ? null : element.getVR();
    }

    /** Every tag, ascending (unsigned). */
    public int[] tags() {
        int[] tags = new int[elements.size()];
        int i = 0;
        for (Integer tag : elements.keySet()) {
            tags[i++] = tag;
        }
        return tags;
    }

    /** The elements in ascending tag order. */
    @Override
    public Iterator<DicomElement> iterator() {
        return Collections.unmodifiableCollection(elements.values()).iterator();
    }

    // ---------------------------------------------------------------------------------------
    // Strings
    // ---------------------------------------------------------------------------------------

    public String getString(int tag) {
        return getString(tag, 0, null);
    }

    public String getString(int tag, String defaultValue) {
        return getString(tag, 0, defaultValue);
    }

    /** Value {@code index} as text; {@code defaultValue} if absent or empty. */
    public String getString(int tag, int index, String defaultValue) {
        DicomElement element = elements.get(tag);
        if (element == null) {
            return defaultValue;
        }
        Object value = element.rawValue();
        switch (element.getValueType()) {
            case STRINGS: {
                String[] strings = (String[]) value;
                return index < strings.length && !strings[index].isEmpty() ? strings[index] : defaultValue;
            }
            case INTS: {
                long[] ints = (long[]) value;
                return index < ints.length ? Long.toString(ints[index]) : defaultValue;
            }
            case DOUBLES: {
                double[] doubles = (double[]) value;
                return index < doubles.length ? formatDouble(doubles[index]) : defaultValue;
            }
            case TAGS: {
                int[] tags = (int[]) value;
                return index < tags.length ? String.format("%08X", tags[index]) : defaultValue;
            }
            default:
                return defaultValue;
        }
    }

    /** Every value as text (multi-valued attributes split on {@code \}); {@code null} if absent. */
    public String[] getStrings(int tag) {
        DicomElement element = elements.get(tag);
        if (element == null) {
            return null;
        }
        Object value = element.rawValue();
        switch (element.getValueType()) {
            case STRINGS:
                return ((String[]) value).clone();
            case INTS:
                return Arrays.stream((long[]) value).mapToObj(Long::toString).toArray(String[]::new);
            case DOUBLES:
                return Arrays.stream((double[]) value).mapToObj(DicomDataset::formatDouble).toArray(String[]::new);
            case TAGS:
                return Arrays.stream((int[]) value).mapToObj(t -> String.format("%08X", t)).toArray(String[]::new);
            default:
                return new String[0];
        }
    }

    static String formatDouble(double value) {
        if (value == Math.rint(value) && !Double.isInfinite(value) && Math.abs(value) < 1e15) {
            return Long.toString((long) value);
        }
        return Double.toString(value);
    }

    // ---------------------------------------------------------------------------------------
    // Numbers
    // ---------------------------------------------------------------------------------------

    public int getInt(int tag, int defaultValue) {
        return getInt(tag, 0, defaultValue);
    }

    public int getInt(int tag, int index, int defaultValue) {
        long value = getLong(tag, index, Long.MIN_VALUE);
        return value == Long.MIN_VALUE ? defaultValue : (int) value;
    }

    public long getLong(int tag, long defaultValue) {
        return getLong(tag, 0, defaultValue);
    }

    public long getLong(int tag, int index, long defaultValue) {
        DicomElement element = elements.get(tag);
        if (element == null) {
            return defaultValue;
        }
        Object value = element.rawValue();
        switch (element.getValueType()) {
            case INTS: {
                long[] ints = (long[]) value;
                return index < ints.length ? ints[index] : defaultValue;
            }
            case DOUBLES: {
                double[] doubles = (double[]) value;
                return index < doubles.length ? (long) doubles[index] : defaultValue;
            }
            case TAGS: {
                int[] tags = (int[]) value;
                return index < tags.length ? Integer.toUnsignedLong(tags[index]) : defaultValue;
            }
            case STRINGS: {
                String[] strings = (String[]) value;
                if (index >= strings.length) {
                    return defaultValue;
                }
                try {
                    return parseLong(strings[index]);
                } catch (NumberFormatException invalid) {
                    return defaultValue;
                }
            }
            default:
                return defaultValue;
        }
    }

    private static long parseLong(String text) {
        String trimmed = text.trim();
        try {
            return Long.parseLong(trimmed.startsWith("+") ? trimmed.substring(1) : trimmed);
        } catch (NumberFormatException notAnInteger) {
            return (long) Double.parseDouble(trimmed);
        }
    }

    /** Every value as an {@code int}; {@code null} if absent. */
    public int[] getInts(int tag) {
        long[] longs = getLongs(tag);
        if (longs == null) {
            return null;
        }
        int[] ints = new int[longs.length];
        for (int i = 0; i < longs.length; i++) {
            ints[i] = (int) longs[i];
        }
        return ints;
    }

    /** Every value as a {@code long}; {@code null} if absent. */
    public long[] getLongs(int tag) {
        DicomElement element = elements.get(tag);
        if (element == null) {
            return null;
        }
        Object value = element.rawValue();
        switch (element.getValueType()) {
            case INTS:
                return ((long[]) value).clone();
            case DOUBLES:
                return Arrays.stream((double[]) value).mapToLong(d -> (long) d).toArray();
            case TAGS:
                return Arrays.stream((int[]) value).mapToLong(Integer::toUnsignedLong).toArray();
            case STRINGS:
                return Arrays.stream((String[]) value).mapToLong(DicomDataset::parseLong).toArray();
            default:
                return new long[0];
        }
    }

    public double getDouble(int tag, double defaultValue) {
        return getDouble(tag, 0, defaultValue);
    }

    public double getDouble(int tag, int index, double defaultValue) {
        DicomElement element = elements.get(tag);
        if (element == null) {
            return defaultValue;
        }
        Object value = element.rawValue();
        switch (element.getValueType()) {
            case DOUBLES: {
                double[] doubles = (double[]) value;
                return index < doubles.length ? doubles[index] : defaultValue;
            }
            case INTS: {
                long[] ints = (long[]) value;
                return index < ints.length ? ints[index] : defaultValue;
            }
            case STRINGS: {
                String[] strings = (String[]) value;
                if (index >= strings.length) {
                    return defaultValue;
                }
                try {
                    return Double.parseDouble(strings[index].trim());
                } catch (NumberFormatException invalid) {
                    return defaultValue;
                }
            }
            default:
                return defaultValue;
        }
    }

    /** Every value as a {@code double}; {@code null} if absent. */
    public double[] getDoubles(int tag) {
        DicomElement element = elements.get(tag);
        if (element == null) {
            return null;
        }
        Object value = element.rawValue();
        switch (element.getValueType()) {
            case DOUBLES:
                return ((double[]) value).clone();
            case INTS:
                return Arrays.stream((long[]) value).asDoubleStream().toArray();
            case STRINGS:
                return Arrays.stream((String[]) value).mapToDouble(s -> Double.parseDouble(s.trim())).toArray();
            default:
                return new double[0];
        }
    }

    public float getFloat(int tag, float defaultValue) {
        return (float) getDouble(tag, 0, defaultValue);
    }

    /** Every value as a {@code float}; {@code null} if absent. */
    public float[] getFloats(int tag) {
        double[] doubles = getDoubles(tag);
        if (doubles == null) {
            return null;
        }
        float[] floats = new float[doubles.length];
        for (int i = 0; i < doubles.length; i++) {
            floats[i] = (float) doubles[i];
        }
        return floats;
    }

    /** An AT attribute's tags; {@code null} if absent. */
    public int[] getTags(int tag) {
        DicomElement element = elements.get(tag);
        if (element == null) {
            return null;
        }
        if (element.getValueType() == DicomElement.ValueType.TAGS) {
            return ((int[]) element.rawValue()).clone();
        }
        int[] ints = getInts(tag);
        return ints == null ? new int[0] : ints;
    }

    // ---------------------------------------------------------------------------------------
    // Bytes, bulk data, sequences, names
    // ---------------------------------------------------------------------------------------

    /** A binary value that was read (OB, OW, UN, ...; little-endian), or text as UTF-8; {@code
     * null} if absent or left in the source ({@link #getBulkDataRef}). A copy. */
    public byte[] getBytes(int tag) {
        DicomElement element = elements.get(tag);
        if (element == null) {
            return null;
        }
        switch (element.getValueType()) {
            case BYTES:
                return ((byte[]) element.rawValue()).clone();
            case EMPTY:
                return new byte[0];
            case STRINGS:
                return String.join("\\", (String[]) element.rawValue()).getBytes(java.nio.charset.StandardCharsets.UTF_8);
            default:
                return null;
        }
    }

    /** Where the value is in the source, if it was left there; else {@code null}. */
    public BulkDataRef getBulkDataRef(int tag) {
        DicomElement element = elements.get(tag);
        return element == null ? null : element.getBulkDataRef();
    }

    /** A sequence's items (unmodifiable; empty for an empty sequence), or {@code null} if absent
     * or not a sequence. */
    public List<DicomDataset> getSequence(int tag) {
        DicomElement element = elements.get(tag);
        return element == null ? null : element.getItems();
    }

    /** Item 0 of a sequence, or {@code null}. */
    public DicomDataset getNestedDataset(int tag) {
        return getNestedDataset(tag, 0);
    }

    public DicomDataset getNestedDataset(int tag, int index) {
        List<DicomDataset> items = getSequence(tag);
        return items != null && index < items.size() ? items.get(index) : null;
    }

    /** A PN value split into its groups and components, or {@code null}. */
    public PersonName getPersonName(int tag) {
        return getPersonName(tag, 0);
    }

    public PersonName getPersonName(int tag, int index) {
        String value = getString(tag, index, null);
        return value == null ? null : new PersonName(value);
    }

    // ---------------------------------------------------------------------------------------
    // File Meta Information and transfer syntax
    // ---------------------------------------------------------------------------------------

    /** The transfer syntax the data set was read in ({@code null} for one built from scratch or
     * a sequence item). */
    public String transferSyntaxUid() {
        return transferSyntaxUid;
    }

    /** {@link #transferSyntaxUid()} as the generated enum, or {@code null}. */
    public TransferSyntax transferSyntax() {
        return TransferSyntax.fromUid(transferSyntaxUid);
    }

    /** The File Meta Information (group 0002) read with the data set; empty if it had none. Not
     * written back: {@link Dcmnorm#write} creates it afresh. */
    public DicomDataset fileMeta() {
        return fileMeta == null ? EMPTY : fileMeta;
    }

    Object source() {
        return source;
    }

    TreeMap<Integer, DicomElement> elementsUnsafe() {
        return elements;
    }

    // ---------------------------------------------------------------------------------------
    // Multi-frame and pixel geometry
    // ---------------------------------------------------------------------------------------

    /** NumberOfFrames, or 1 when absent. */
    public int numberOfFrames() {
        return Math.max(1, getInt(Tag.NumberOfFrames, 1));
    }

    /** FrameTime (0018,1063): milliseconds per frame, if given. */
    public OptionalDouble frameTimeMillis() {
        double frameTime = getDouble(Tag.FrameTime, Double.NaN);
        return Double.isNaN(frameTime) ? OptionalDouble.empty() : OptionalDouble.of(frameTime);
    }

    /** FrameTimeVector (0018,1065): milliseconds from the previous frame, per frame; {@code null}
     * if absent. */
    public double[] frameTimeVector() {
        return contains(Tag.FrameTimeVector) ? getDoubles(Tag.FrameTimeVector) : null;
    }

    /** The frame rate in frames per second: CineRate, else RecommendedDisplayFrameRate, else
     * 1000 / FrameTime; empty if none is given. */
    public OptionalDouble framesPerSecond() {
        double cineRate = getDouble(Tag.CineRate, Double.NaN);
        if (!Double.isNaN(cineRate) && cineRate > 0) {
            return OptionalDouble.of(cineRate);
        }
        double recommended = getDouble(Tag.RecommendedDisplayFrameRate, Double.NaN);
        if (!Double.isNaN(recommended) && recommended > 0) {
            return OptionalDouble.of(recommended);
        }
        OptionalDouble frameTime = frameTimeMillis();
        return frameTime.isPresent() && frameTime.getAsDouble() > 0
            ? OptionalDouble.of(1000.0 / frameTime.getAsDouble())
            : OptionalDouble.empty();
    }

    /**
     * The pixel aspect ratio as vertical / horizontal pixel size: from PixelAspectRatio
     * (0028,0034), else from PixelSpacing (row spacing / column spacing), else 1. Rendering does
     * not correct for it ({@link Dcmnorm#renderImage}): scale the image's height by {@code 1 /
     * pixelAspectRatio()} (or its width by the ratio) to display non-square pixels.
     */
    public double pixelAspectRatio() {
        double[] ratio = contains(Tag.PixelAspectRatio) ? getDoubles(Tag.PixelAspectRatio) : null;
        if (ratio != null && ratio.length == 2 && ratio[0] > 0 && ratio[1] > 0) {
            return ratio[0] / ratio[1];
        }
        double[] spacing = contains(Tag.PixelSpacing) ? getDoubles(Tag.PixelSpacing) : null;
        if (spacing != null && spacing.length == 2 && spacing[0] > 0 && spacing[1] > 0) {
            return spacing[0] / spacing[1];
        }
        return 1.0;
    }

    // ---------------------------------------------------------------------------------------

    /** A builder starting from this data set's elements (and keeping its source, so references
     * still resolve on write). */
    public DicomDatasetBuilder toBuilder() {
        return new DicomDatasetBuilder(this);
    }

    @Override
    public boolean equals(Object other) {
        if (!(other instanceof DicomDataset)) {
            return false;
        }
        DicomDataset that = (DicomDataset) other;
        return elements.equals(that.elements) && java.util.Objects.equals(transferSyntaxUid, that.transferSyntaxUid)
            && fileMeta().elements.equals(that.fileMeta().elements);
    }

    @Override
    public int hashCode() {
        return elements.keySet().hashCode();
    }

    @Override
    public String toString() {
        StringBuilder text = new StringBuilder("DicomDataset[").append(elements.size()).append(" elements");
        if (transferSyntaxUid != null) {
            text.append(", ").append(transferSyntaxUid);
        }
        return text.append(']').toString();
    }

    /** A multi-line listing of every element, nested items indented (for debugging). */
    public String dump() {
        StringBuilder text = new StringBuilder();
        dump(text, "");
        return text.toString();
    }

    private void dump(StringBuilder text, String indent) {
        for (Map.Entry<Integer, DicomElement> entry : elements.entrySet()) {
            DicomElement element = entry.getValue();
            String keyword = ElementDictionary.keywordOf(entry.getKey());
            text.append(indent).append(element).append(keyword == null ? "" : "  " + keyword).append('\n');
            List<DicomDataset> items = element.getItems();
            if (items != null) {
                for (int i = 0; i < items.size(); i++) {
                    text.append(indent).append("  > item ").append(i).append('\n');
                    items.get(i).dump(text, indent + "    ");
                }
            }
        }
    }
}
