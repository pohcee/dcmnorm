package com.pohcee.dcmnorm;

import java.io.ByteArrayOutputStream;
import java.io.DataOutputStream;
import java.io.IOException;
import java.io.UncheckedIOException;
import java.nio.ByteBuffer;
import java.nio.charset.StandardCharsets;
import java.util.ArrayList;
import java.util.List;
import java.util.TreeMap;

/**
 * The binary data set form native code produces and consumes ({@code dataset_tree.rs},
 * {@code encode_document} / {@code decode_document}): one {@code byte[]} per data set across JNI
 * instead of one JNI call per value. Big-endian:
 *
 * <pre>
 * document := "DCMT" u8:1 string:transferSyntaxUid tree:fileMeta tree:dataset
 * tree     := u32:count element*
 * element  := u32:tag u8 u8 (VR) u8:kind payload
 * kind 0 empty | 1 strings | 2 ints (i64) | 3 doubles | 4 tags (u32) | 5 bytes
 *      6 bulk ref (u64 offset, i64 length) | 7 fragments | 8 sequence (u32 n, tree*)
 * </pre>
 */
final class DatasetCodec {
    private static final byte[] MAGIC = {'D', 'C', 'M', 'T'};
    private static final int VERSION = 1;

    private DatasetCodec() {
    }

    static DicomDataset decode(byte[] bytes, Object source) {
        ByteBuffer buffer = ByteBuffer.wrap(bytes);
        for (byte b : MAGIC) {
            if (buffer.get() != b) {
                throw new IllegalStateException("not a binary data set");
            }
        }
        int version = buffer.get();
        if (version != VERSION) {
            throw new IllegalStateException("unsupported binary data set version " + version);
        }
        String transferSyntaxUid = string(buffer);
        TreeMap<Integer, DicomElement> meta = tree(buffer);
        TreeMap<Integer, DicomElement> dataset = tree(buffer);
        DicomDataset fileMeta = new DicomDataset(meta, null, null, null);
        return new DicomDataset(dataset, fileMeta, transferSyntaxUid.isEmpty() ? null : transferSyntaxUid, source);
    }

    private static String string(ByteBuffer buffer) {
        int length = buffer.getInt();
        String text = new String(buffer.array(), buffer.position(), length, StandardCharsets.UTF_8);
        buffer.position(buffer.position() + length);
        return text;
    }

    private static TreeMap<Integer, DicomElement> tree(ByteBuffer buffer) {
        int count = buffer.getInt();
        TreeMap<Integer, DicomElement> elements = new TreeMap<>(Integer::compareUnsigned);
        for (int i = 0; i < count; i++) {
            int tag = buffer.getInt();
            String code = new String(new byte[] {buffer.get(), buffer.get()}, StandardCharsets.US_ASCII);
            VR vr = VR.of(code);
            if (vr == null) {
                vr = VR.UN;
            }
            int kind = buffer.get();
            DicomElement element;
            switch (kind) {
                case 0:
                    element = DicomElement.empty(tag, vr);
                    break;
                case 1: {
                    String[] strings = new String[buffer.getInt()];
                    for (int j = 0; j < strings.length; j++) {
                        strings[j] = string(buffer);
                    }
                    element = DicomElement.strings(tag, vr, strings);
                    break;
                }
                case 2: {
                    long[] ints = new long[buffer.getInt()];
                    for (int j = 0; j < ints.length; j++) {
                        ints[j] = buffer.getLong();
                    }
                    element = DicomElement.ints(tag, vr, ints);
                    break;
                }
                case 3: {
                    double[] doubles = new double[buffer.getInt()];
                    for (int j = 0; j < doubles.length; j++) {
                        doubles[j] = buffer.getDouble();
                    }
                    element = DicomElement.doubles(tag, vr, doubles);
                    break;
                }
                case 4: {
                    int[] tags = new int[buffer.getInt()];
                    for (int j = 0; j < tags.length; j++) {
                        tags[j] = buffer.getInt();
                    }
                    element = DicomElement.tags(tag, vr, tags);
                    break;
                }
                case 5: {
                    byte[] value = new byte[buffer.getInt()];
                    buffer.get(value);
                    element = DicomElement.bytes(tag, vr, value);
                    break;
                }
                case 6: {
                    long offset = buffer.getLong();
                    long length = buffer.getLong();
                    element = DicomElement.bulkData(tag, vr, new BulkDataRef(tag, vr, offset, length));
                    break;
                }
                case 7: {
                    int[] offsetTable = new int[buffer.getInt()];
                    for (int j = 0; j < offsetTable.length; j++) {
                        offsetTable[j] = buffer.getInt();
                    }
                    int fragmentCount = buffer.getInt();
                    List<byte[]> fragments = new ArrayList<>(fragmentCount);
                    for (int j = 0; j < fragmentCount; j++) {
                        byte[] fragment = new byte[buffer.getInt()];
                        buffer.get(fragment);
                        fragments.add(fragment);
                    }
                    element = DicomElement.fragments(tag, vr, new DicomElement.Fragments(offsetTable, fragments));
                    break;
                }
                case 8: {
                    int itemCount = buffer.getInt();
                    List<DicomDataset> items = new ArrayList<>(itemCount);
                    for (int j = 0; j < itemCount; j++) {
                        items.add(new DicomDataset(tree(buffer), null, null, null));
                    }
                    element = DicomElement.sequence(tag, items);
                    break;
                }
                default:
                    throw new IllegalStateException("unknown value kind " + kind);
            }
            elements.put(tag, element);
        }
        return elements;
    }

    static byte[] encode(DicomDataset dataset) {
        try {
            ByteArrayOutputStream bytes = new ByteArrayOutputStream(4096);
            DataOutputStream out = new DataOutputStream(bytes);
            out.write(MAGIC);
            out.write(VERSION);
            string(out, dataset.transferSyntaxUid() == null ? "" : dataset.transferSyntaxUid());
            tree(out, dataset.fileMeta());
            tree(out, dataset);
            out.flush();
            return bytes.toByteArray();
        } catch (IOException impossible) {
            throw new UncheckedIOException(impossible);
        }
    }

    private static void string(DataOutputStream out, String text) throws IOException {
        byte[] utf8 = text.getBytes(StandardCharsets.UTF_8);
        out.writeInt(utf8.length);
        out.write(utf8);
    }

    private static void tree(DataOutputStream out, DicomDataset dataset) throws IOException {
        out.writeInt(dataset.size());
        for (DicomElement element : dataset) {
            out.writeInt(element.getTag());
            String code = element.getVR().name();
            out.write(code.charAt(0));
            out.write(code.charAt(1));
            Object value = element.rawValue();
            switch (element.getValueType()) {
                case EMPTY:
                    out.write(0);
                    break;
                case STRINGS: {
                    out.write(1);
                    String[] strings = (String[]) value;
                    out.writeInt(strings.length);
                    for (String text : strings) {
                        string(out, text);
                    }
                    break;
                }
                case INTS: {
                    out.write(2);
                    long[] ints = (long[]) value;
                    out.writeInt(ints.length);
                    for (long v : ints) {
                        out.writeLong(v);
                    }
                    break;
                }
                case DOUBLES: {
                    out.write(3);
                    double[] doubles = (double[]) value;
                    out.writeInt(doubles.length);
                    for (double v : doubles) {
                        out.writeDouble(v);
                    }
                    break;
                }
                case TAGS: {
                    out.write(4);
                    int[] tags = (int[]) value;
                    out.writeInt(tags.length);
                    for (int t : tags) {
                        out.writeInt(t);
                    }
                    break;
                }
                case BYTES: {
                    out.write(5);
                    byte[] data = (byte[]) value;
                    out.writeInt(data.length);
                    out.write(data);
                    break;
                }
                case BULK_DATA: {
                    out.write(6);
                    BulkDataRef reference = (BulkDataRef) value;
                    out.writeLong(reference.getOffset());
                    out.writeLong(reference.getLength());
                    break;
                }
                case FRAGMENTS: {
                    out.write(7);
                    DicomElement.Fragments fragments = (DicomElement.Fragments) value;
                    int[] offsetTable = fragments.offsetTableUnsafe();
                    out.writeInt(offsetTable.length);
                    for (int entry : offsetTable) {
                        out.writeInt(entry);
                    }
                    out.writeInt(fragments.fragmentsUnsafe().size());
                    for (byte[] fragment : fragments.fragmentsUnsafe()) {
                        out.writeInt(fragment.length);
                        out.write(fragment);
                    }
                    break;
                }
                case SEQUENCE: {
                    out.write(8);
                    List<DicomDataset> items = element.getItems();
                    out.writeInt(items.size());
                    for (DicomDataset item : items) {
                        tree(out, item);
                    }
                    break;
                }
                default:
                    throw new IllegalStateException("unknown value type " + element.getValueType());
            }
        }
    }
}
