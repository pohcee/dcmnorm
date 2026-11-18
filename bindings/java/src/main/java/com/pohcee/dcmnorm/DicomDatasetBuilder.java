package com.pohcee.dcmnorm;

import java.util.ArrayList;
import java.util.Arrays;
import java.util.List;
import java.util.TreeMap;
import java.util.function.Consumer;

/**
 * Builds a modified copy of a {@link DicomDataset} (from {@link DicomDataset#toBuilder()}), for
 * metadata correction, de-identification, study split/merge and the like; write the result with
 * {@link Dcmnorm#write}. The source dataset is unchanged, and values left in the source ({@link
 * BulkDataRef}s, PixelData) stay references: {@code write} reads them from the source, so pixel
 * data never passes through the Java heap.
 *
 * <pre>{@code
 * DicomDataset edited = ds.toBuilder()
 *     .set(Tag.PatientName, VR.PN, "Doe^Jane")
 *     .remove(Tag.OtherPatientIDsSequence)
 *     .editSequenceItem(Tag.ReferencedStudySequence, 0, item -> item.set(Tag.ReferencedSOPInstanceUID, VR.UI, uid))
 *     .build();
 * }</pre>
 *
 * <p>Values are given as text for any VR - {@code set(Tag.Rows, VR.US, "512")} - or typed with
 * {@code setInt}/{@code setDouble}/...; the native writer converts them to the VR's encoding and
 * rejects one that doesn't fit (e.g. a US value out of range). File Meta Information (group 0002)
 * can't be set here: {@code write} creates it.
 */
public final class DicomDatasetBuilder {
    private final TreeMap<Integer, DicomElement> elements;
    private final DicomDataset fileMeta;
    private final String transferSyntaxUid;
    private final Object source;

    DicomDatasetBuilder(DicomDataset from) {
        this.elements = new TreeMap<>(from.elementsUnsafe());
        this.fileMeta = from.fileMeta();
        this.transferSyntaxUid = from.transferSyntaxUid();
        this.source = from.source();
    }

    private DicomDatasetBuilder put(DicomElement element) {
        int group = element.getTag() >>> 16;
        if (group == 0x0002) {
            throw new IllegalArgumentException(Tag.toString(element.getTag())
                + ": File Meta Information is created by Dcmnorm.write, not set in the data set");
        }
        elements.put(element.getTag(), element);
        return this;
    }

    private static VR dictionaryVr(int tag) {
        VR vr = ElementDictionary.vrOf(tag);
        if (vr == null) {
            throw new IllegalArgumentException(Tag.toString(tag) + " is not in the dictionary: give its VR");
        }
        return vr;
    }

    /** Set text values ({@code values} empty: an empty attribute). Any VR: a binary one parses
     * the text when written (US {@code "512"}, AT {@code "00100010"}, FD {@code "1.5"}). */
    public DicomDatasetBuilder set(int tag, VR vr, String... values) {
        if (vr == VR.SQ) {
            throw new IllegalArgumentException("use setSequence for SQ");
        }
        if (values == null || values.length == 0) {
            return put(DicomElement.empty(tag, vr));
        }
        String[] copy = values.clone();
        for (int i = 0; i < copy.length; i++) {
            if (copy[i] == null) {
                copy[i] = "";
            }
        }
        return put(DicomElement.strings(tag, vr, copy));
    }

    /** {@link #set(int, VR, String...)} with the dictionary VR of {@code tag}. */
    public DicomDatasetBuilder set(int tag, String... values) {
        return set(tag, dictionaryVr(tag), values);
    }

    /** dcm4che's name for {@link #set(int, VR, String...)}. */
    public DicomDatasetBuilder setString(int tag, VR vr, String... values) {
        return set(tag, vr, values);
    }

    public DicomDatasetBuilder setInt(int tag, VR vr, int... values) {
        return setLong(tag, vr, Arrays.stream(values).asLongStream().toArray());
    }

    public DicomDatasetBuilder setLong(int tag, VR vr, long... values) {
        return put(values.length == 0 ? DicomElement.empty(tag, vr) : DicomElement.ints(tag, vr, values.clone()));
    }

    public DicomDatasetBuilder setDouble(int tag, VR vr, double... values) {
        return put(values.length == 0 ? DicomElement.empty(tag, vr) : DicomElement.doubles(tag, vr, values.clone()));
    }

    /** An AT attribute. */
    public DicomDatasetBuilder setTags(int tag, int... tags) {
        return put(tags.length == 0 ? DicomElement.empty(tag, VR.AT) : DicomElement.tags(tag, VR.AT, tags.clone()));
    }

    /** A binary value (OB, OW, UN, ...), little-endian. Copied. */
    public DicomDatasetBuilder setBytes(int tag, VR vr, byte[] value) {
        return put(value.length == 0 ? DicomElement.empty(tag, vr) : DicomElement.bytes(tag, vr, value.clone()));
    }

    /** An attribute with no value (zero length) - e.g. to blank a type 2 attribute. */
    public DicomDatasetBuilder setNull(int tag, VR vr) {
        return put(DicomElement.empty(tag, vr));
    }

    public DicomDatasetBuilder setSequence(int tag, List<DicomDataset> items) {
        return put(DicomElement.sequence(tag, items));
    }

    /** Append an item to a sequence (creating it if absent). */
    public DicomDatasetBuilder addSequenceItem(int tag, DicomDataset item) {
        List<DicomDataset> items = new ArrayList<>();
        DicomElement existing = elements.get(tag);
        if (existing != null && existing.getItems() != null) {
            items.addAll(existing.getItems());
        }
        items.add(item);
        return put(DicomElement.sequence(tag, items));
    }

    /** Edit item {@code index} of a sequence in place (a nested edit, at any depth by nesting). */
    public DicomDatasetBuilder editSequenceItem(int tag, int index, Consumer<DicomDatasetBuilder> edit) {
        DicomElement existing = elements.get(tag);
        List<DicomDataset> items = existing == null ? null : existing.getItems();
        if (items == null || index < 0 || index >= items.size()) {
            throw new IndexOutOfBoundsException(Tag.toString(tag) + " has no item " + index);
        }
        DicomDatasetBuilder item = items.get(index).toBuilder();
        edit.accept(item);
        List<DicomDataset> edited = new ArrayList<>(items);
        edited.set(index, item.build());
        return put(DicomElement.sequence(tag, edited));
    }

    public DicomDatasetBuilder removeSequenceItem(int tag, int index) {
        DicomElement existing = elements.get(tag);
        List<DicomDataset> items = existing == null ? null : existing.getItems();
        if (items == null || index < 0 || index >= items.size()) {
            throw new IndexOutOfBoundsException(Tag.toString(tag) + " has no item " + index);
        }
        List<DicomDataset> edited = new ArrayList<>(items);
        edited.remove(index);
        return put(DicomElement.sequence(tag, edited));
    }

    /** Copy an element (value included, references and all) from another dataset; nothing if
     * it doesn't have it. */
    public DicomDatasetBuilder copyFrom(DicomDataset other, int tag) {
        DicomElement element = other.get(tag);
        return element == null ? this : put(element);
    }

    public DicomDatasetBuilder remove(int tag) {
        elements.remove(tag);
        return this;
    }

    /** Remove every private attribute (odd group) at the top level. */
    public DicomDatasetBuilder removePrivateTags() {
        elements.keySet().removeIf(tag -> ((tag >>> 16) & 1) == 1);
        return this;
    }

    public DicomDataset build() {
        return new DicomDataset(new TreeMap<>(elements), fileMeta, transferSyntaxUid, source);
    }
}
