package com.pohcee.dcmnorm;

import com.pohcee.dcmnorm.internal.Json;

/**
 * Optional arguments shared by {@link DicomVolumeHandle#exportTexture}, {@link
 * Dcmnorm#exportFrameTexture}, and {@link Dcmnorm#exportFrameStackTexture}. Not every field
 * applies to every call site: {@link #frameIndex} only applies to {@code exportFrameTexture};
 * {@link #targetMaxDim} only applies to {@code exportTexture}/{@code exportFrameTexture} (a
 * frame stack is never resampled - see {@code dcmnorm::dicom_io::pack_frame_stack_texture}'s own
 * doc). An unused field is simply ignored by the call site it doesn't apply to.
 */
public final class TextureExportOptions {
    private Integer frameIndex;
    private Integer targetMaxDim;
    private String compression;
    private Double windowCenter;
    private Double windowWidth;

    /** {@code exportFrameTexture} only. Defaults to 0. */
    public TextureExportOptions frameIndex(int frameIndex) {
        this.frameIndex = frameIndex;
        return this;
    }

    /** {@code exportTexture}/{@code exportFrameTexture} only. Caps the longest of
     * width/height/depth, proportionally downsampling if the native data exceeds it; omitted
     * means full native resolution. */
    public TextureExportOptions targetMaxDim(int targetMaxDim) {
        this.targetMaxDim = targetMaxDim;
        return this;
    }

    /** {@code "gzip"} (default) or {@code "none"}. */
    public TextureExportOptions compression(String compression) {
        this.compression = compression;
        return this;
    }

    /** Purely informational, carried through to the result ({@link
     * TextureExportResult#getDefaultWindowCenter()}) for the client's initial render - the
     * exported samples are never windowed. Must be set together with {@link #windowWidth}. */
    public TextureExportOptions windowCenter(double windowCenter) {
        this.windowCenter = windowCenter;
        return this;
    }

    public TextureExportOptions windowWidth(double windowWidth) {
        this.windowWidth = windowWidth;
        return this;
    }

    String toJson() {
        return Json.object().put("frameIndex", frameIndex).put("targetMaxDim", targetMaxDim)
            .put("compression", compression).put("windowCenter", windowCenter).put("windowWidth", windowWidth)
            .build();
    }
}
