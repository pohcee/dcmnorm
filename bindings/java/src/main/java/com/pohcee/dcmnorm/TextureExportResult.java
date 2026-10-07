package com.pohcee.dcmnorm;

import com.pohcee.dcmnorm.internal.Json;
import java.util.List;
import java.util.Map;

/**
 * The result of {@link DicomVolumeHandle#exportTexture}, {@link Dcmnorm#exportFrameTexture}, or
 * {@link Dcmnorm#exportFrameStackTexture}: a lossless (or, when downsampled, bounded-error)
 * GPU-upload-ready texture payload - see {@code dcmnorm::dicom_io::texture_export}'s own module
 * doc (in the main Rust crate) for the full format contract, and the main README's "Export a GPU
 * texture (.gputex)" section.
 *
 * <p>Constructed directly from native code from {@code
 * dcmnorm::dicom_io::texture_export::TextureMeta::to_json()} - the single source of truth for
 * this payload's field names, already shared with the CLI sidecar and the render-server - rather
 * than a hand-rolled equivalent, so this constructor is not part of the public API.
 */
public final class TextureExportResult {
    private final String contentKind;
    private final String sampleFormat;
    private final String compression;
    private final boolean lossless;
    private final int width;
    private final int height;
    private final int depth;
    private final double rescaleSlope;
    private final double rescaleIntercept;
    private final double rowSpacingMm;
    private final double colSpacingMm;
    private final double sliceSpacingMm;
    private final double[] origin;
    private final double[] rowDir;
    private final double[] colDir;
    private final double[] normalDir;
    private final Double defaultWindowCenter;
    private final Double defaultWindowWidth;
    private final boolean invert;
    private final double[] layerWindowCenters;
    private final double[] layerWindowWidths;
    private final boolean[] layerInvert;
    private final int nativeWidth;
    private final int nativeHeight;
    private final int nativeDepth;
    private final boolean downsampled;
    private final long payloadBytesRaw;
    private final long payloadBytesStored;
    private final byte[] data;

    TextureExportResult(String metaJson, byte[] data) {
        Map<String, Object> meta = Json.asMap(Json.parse(metaJson));
        this.contentKind = Json.asString(meta.get("contentKind"));
        this.sampleFormat = Json.asString(meta.get("sampleFormat"));
        this.compression = Json.asString(meta.get("compression"));
        this.lossless = Json.asBoolean(meta.get("lossless"));
        this.width = Json.asInt(meta.get("width"));
        this.height = Json.asInt(meta.get("height"));
        this.depth = Json.asInt(meta.get("depth"));
        this.rescaleSlope = Json.asDouble(meta.get("rescaleSlope"));
        this.rescaleIntercept = Json.asDouble(meta.get("rescaleIntercept"));
        this.rowSpacingMm = Json.asDouble(meta.get("rowSpacingMm"));
        this.colSpacingMm = Json.asDouble(meta.get("colSpacingMm"));
        this.sliceSpacingMm = Json.asDouble(meta.get("sliceSpacingMm"));
        this.origin = Json.asDoubleArray(meta.get("origin"));
        this.rowDir = Json.asDoubleArray(meta.get("rowDir"));
        this.colDir = Json.asDoubleArray(meta.get("colDir"));
        this.normalDir = Json.asDoubleArray(meta.get("normalDir"));
        this.defaultWindowCenter = Json.asNullableDouble(meta.get("defaultWindowCenter"));
        this.defaultWindowWidth = Json.asNullableDouble(meta.get("defaultWindowWidth"));
        this.invert = Json.asBoolean(meta.get("invert"));
        this.layerWindowCenters = Json.asDoubleArray(meta.get("layerWindowCenters"));
        this.layerWindowWidths = Json.asDoubleArray(meta.get("layerWindowWidths"));
        this.layerInvert = Json.asBooleanArray(meta.get("layerInvert"));

        List<Object> nativeDims = Json.asList(meta.get("nativeDims"));
        this.nativeWidth = Json.asInt(nativeDims.get(0));
        this.nativeHeight = Json.asInt(nativeDims.get(1));
        this.nativeDepth = Json.asInt(nativeDims.get(2));

        this.downsampled = Json.asBoolean(meta.get("downsampled"));
        this.payloadBytesRaw = Json.asLong(meta.get("payloadBytesRaw"));
        this.payloadBytesStored = Json.asLong(meta.get("payloadBytesStored"));
        this.data = data;
    }

    /** {@code "volume"}, {@code "image2d"}, or {@code "framestack"}. */
    public String getContentKind() {
        return contentKind;
    }

    /** {@code "int16"} or {@code "uint16"}. */
    public String getSampleFormat() {
        return sampleFormat;
    }

    /** {@code "none"} or {@code "gzip"} - matches how {@link #getData()} is actually encoded. */
    public String getCompression() {
        return compression;
    }

    /** {@code false} means {@link #getData()} is a bounded-error quantization (see {@link
     * #getRescaleSlope()}/{@link #getRescaleIntercept()}), not an exact round-trip of the source
     * samples. */
    public boolean isLossless() {
        return lossless;
    }

    public int getWidth() {
        return width;
    }

    public int getHeight() {
        return height;
    }

    /** Always 1 for {@code contentKind: "image2d"}. */
    public int getDepth() {
        return depth;
    }

    /** {@code texel * rescaleSlope + rescaleIntercept} recovers the physical value (e.g. HU). */
    public double getRescaleSlope() {
        return rescaleSlope;
    }

    public double getRescaleIntercept() {
        return rescaleIntercept;
    }

    public double getRowSpacingMm() {
        return rowSpacingMm;
    }

    public double getColSpacingMm() {
        return colSpacingMm;
    }

    /** {@code 0} for {@code contentKind: "image2d"} or {@code "framestack"}. */
    public double getSliceSpacingMm() {
        return sliceSpacingMm;
    }

    /** {@code [x, y, z]}, LPS mm, center of voxel (0,0,0). Carries no meaning for {@code
     * contentKind: "framestack"}. */
    public double[] getOrigin() {
        return origin;
    }

    public double[] getRowDir() {
        return rowDir;
    }

    public double[] getColDir() {
        return colDir;
    }

    public double[] getNormalDir() {
        return normalDir;
    }

    public Double getDefaultWindowCenter() {
        return defaultWindowCenter;
    }

    public Double getDefaultWindowWidth() {
        return defaultWindowWidth;
    }

    /** Whether the client shader should display {@code 1.0 - windowedIntensity} rather than
     * {@code windowedIntensity}. Always {@code false} for {@code contentKind: "volume"}. */
    public boolean isInvert() {
        return invert;
    }

    /** {@code contentKind: "framestack"} only (empty otherwise): each layer's own default
     * window, index-aligned with the layers. Prefer these over {@link #getDefaultWindowCenter()}/
     * {@link #getDefaultWindowWidth()} whenever non-empty. */
    public double[] getLayerWindowCenters() {
        return layerWindowCenters;
    }

    public double[] getLayerWindowWidths() {
        return layerWindowWidths;
    }

    public boolean[] getLayerInvert() {
        return layerInvert;
    }

    /** {@code (width, height, depth)} before any capability/progressive-driven downsampling. */
    public int getNativeWidth() {
        return nativeWidth;
    }

    public int getNativeHeight() {
        return nativeHeight;
    }

    public int getNativeDepth() {
        return nativeDepth;
    }

    public boolean isDownsampled() {
        return downsampled;
    }

    /** Uncompressed byte length of {@link #getData()}'s content. */
    public long getPayloadBytesRaw() {
        return payloadBytesRaw;
    }

    /** {@code getData().length} - included on the result too (not just derivable from {@link
     * #getData()}) so a caller can log/cap transfer size without touching the buffer itself. */
    public long getPayloadBytesStored() {
        return payloadBytesStored;
    }

    public byte[] getData() {
        return data;
    }
}
