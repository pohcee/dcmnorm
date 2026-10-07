package com.pohcee.dcmnorm;

import com.pohcee.dcmnorm.internal.Json;

/** Optional arguments for {@link Dcmnorm#renderFrame}. */
public final class RenderFrameOptions {
    private String format;
    private Integer outputWidth;
    private Integer outputHeight;
    private Double windowCenter;
    private Double windowWidth;
    private Integer frameIndex;
    private Integer jpegQuality;
    private Boolean showOverlays;
    private Integer overlayIndex;
    private String overlayColor;

    /** {@code "jpeg"} (default) or {@code "png"}. */
    public RenderFrameOptions format(String format) {
        this.format = format;
        return this;
    }

    /** Explicit output width in pixels. Combined with {@link #outputHeight}, the image is
     * scaled to the exact dimensions; alone, the height is computed from the aspect ratio. */
    public RenderFrameOptions outputWidth(int outputWidth) {
        this.outputWidth = outputWidth;
        return this;
    }

    public RenderFrameOptions outputHeight(int outputHeight) {
        this.outputHeight = outputHeight;
        return this;
    }

    public RenderFrameOptions windowCenter(double windowCenter) {
        this.windowCenter = windowCenter;
        return this;
    }

    public RenderFrameOptions windowWidth(double windowWidth) {
        this.windowWidth = windowWidth;
        return this;
    }

    /** 0-based. Defaults to 0. */
    public RenderFrameOptions frameIndex(int frameIndex) {
        this.frameIndex = frameIndex;
        return this;
    }

    /** 1-100. Defaults to 90. Only applies when {@link #format} is {@code "jpeg"}. */
    public RenderFrameOptions jpegQuality(int jpegQuality) {
        this.jpegQuality = jpegQuality;
        return this;
    }

    /** Defaults to {@code true}: if the instance has one or more DICOM overlay planes (group
     * {@code 60xx}), the first available overlay composites onto the image. */
    public RenderFrameOptions showOverlays(boolean showOverlays) {
        this.showOverlays = showOverlays;
        return this;
    }

    /** Selects a different overlay (0-based, by {@link OverlaySummary#getIndex()}) than the
     * default (the first available one) - matches the CLI's {@code --overlay-index}. */
    public RenderFrameOptions overlayIndex(int overlayIndex) {
        this.overlayIndex = overlayIndex;
        return this;
    }

    /** {@code "R,G,B"} (0-255 each) or {@code "#RRGGBB"}. Defaults to green. */
    public RenderFrameOptions overlayColor(String overlayColor) {
        this.overlayColor = overlayColor;
        return this;
    }

    String toJson() {
        return Json.object().put("format", format).put("outputWidth", outputWidth).put("outputHeight", outputHeight)
            .put("windowCenter", windowCenter).put("windowWidth", windowWidth).put("frameIndex", frameIndex)
            .put("jpegQuality", jpegQuality).put("showOverlays", showOverlays).put("overlayIndex", overlayIndex)
            .put("overlayColor", overlayColor).build();
    }
}
