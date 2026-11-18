package com.pohcee.dcmnorm;

import com.pohcee.dcmnorm.internal.Json;

/**
 * Display parameters for {@link Dcmnorm#renderImage} / {@link Dcmnorm#renderPixels}. A fluent,
 * mutable builder; the defaults render a frame as a viewer would by default.
 *
 * <p>The pipeline, for grayscale: stored values, then the Modality LUT (Rescale Slope/Intercept
 * or a Modality LUT Sequence; {@link #modalityLut}), then the VOI transform ({@link #voiLut}):
 * the {@link #window} given here, else the data set's VOI LUT Sequence, else its first Window
 * Center/Width, else the frame's own min-max range; then the presentation LUT shape - INVERSE
 * when Presentation LUT Shape says so or, without one, for MONOCHROME1 (taken from the data set;
 * there is no override). Color: palette color, YBR and RGB are converted to RGB, and an embedded
 * ICC profile is applied ({@link #iccProfile}). Then the overlay, if any ({@link #overlays}),
 * and the output size ({@link #outputSize}). The result is always 8 bits per sample: 1 sample
 * (gray) or 3 (RGB). Pixel aspect ratio is not corrected for - see {@link
 * DicomDataset#pixelAspectRatio()}.
 */
public final class RenderParams {
    private Double windowCenter;
    private Double windowWidth;
    private Boolean voiLut;
    private Boolean modalityLut;
    private Boolean iccProfile;
    private Integer outputWidth;
    private Integer outputHeight;
    private Boolean overlays;
    private Integer overlayIndex;
    private String overlayColor;

    public static RenderParams defaults() {
        return new RenderParams();
    }

    /** An explicit VOI window, overriding the data set's windows and VOI LUT. */
    public RenderParams window(double center, double width) {
        this.windowCenter = center;
        this.windowWidth = width;
        return this;
    }

    /** {@code false}: no VOI transform at all - values are scaled from the frame's min-max range
     * to 0-255 (the window, if given, is ignored). Default {@code true}. */
    public RenderParams voiLut(boolean apply) {
        this.voiLut = apply;
        return this;
    }

    /** {@code false}: skip the Modality LUT / rescale. Default {@code true}. */
    public RenderParams modalityLut(boolean apply) {
        this.modalityLut = apply;
        return this;
    }

    /** {@code false}: don't apply an embedded ICC profile to color images. Default {@code true}. */
    public RenderParams iccProfile(boolean apply) {
        this.iccProfile = apply;
        return this;
    }

    /** Scale to exactly this size. */
    public RenderParams outputSize(int width, int height) {
        this.outputWidth = width;
        this.outputHeight = height;
        return this;
    }

    /** Scale to this width, height from the aspect ratio of the stored image. */
    public RenderParams outputWidth(int width) {
        this.outputWidth = width;
        return this;
    }

    /** Scale to this height, width from the aspect ratio of the stored image. */
    public RenderParams outputHeight(int height) {
        this.outputHeight = height;
        return this;
    }

    /** Composite the first overlay plane (group 60xx), when there is one. Default {@code true}. */
    public RenderParams overlays(boolean show) {
        this.overlays = show;
        return this;
    }

    /** Which overlay to composite (0-based, by {@link OverlaySummary#getIndex()}). */
    public RenderParams overlayIndex(int index) {
        this.overlayIndex = index;
        return this;
    }

    /** Overlay color, {@code "R,G,B"} or {@code "#RRGGBB"} (default green). */
    public RenderParams overlayColor(String color) {
        this.overlayColor = color;
        return this;
    }

    String toJson() {
        return Json.object().put("windowCenter", windowCenter).put("windowWidth", windowWidth).put("voiLut", voiLut)
            .put("modalityLut", modalityLut).put("iccProfile", iccProfile).put("outputWidth", outputWidth)
            .put("outputHeight", outputHeight).put("showOverlays", overlays).put("overlayIndex", overlayIndex)
            .put("overlayColor", overlayColor).build();
    }
}
