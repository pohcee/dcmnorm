package com.pohcee.dcmnorm;

import com.pohcee.dcmnorm.internal.Json;
import java.awt.Transparency;
import java.awt.color.ColorSpace;
import java.awt.image.BufferedImage;
import java.awt.image.ColorModel;
import java.awt.image.ComponentColorModel;
import java.awt.image.DataBuffer;
import java.awt.image.DataBufferByte;
import java.awt.image.Raster;
import java.awt.image.WritableRaster;
import java.util.ArrayList;
import java.util.List;
import java.util.Map;

/**
 * One rendered frame as raw 8-bit pixels, from {@link Dcmnorm#renderPixels}: {@code width *
 * height * channels} bytes, rows top to bottom, no row padding; 1 channel (gray) or 3
 * (interleaved R,G,B - or B,G,R when it came from {@link Dcmnorm#renderImage}, see {@link
 * #isBgr()}).
 *
 * <p><b>Buffer ownership.</b> The native renderer's output is copied once, into the {@code
 * byte[]} this object holds; no PNG/JPEG encoding happens anywhere. {@link #getPixels()} returns
 * that array itself, not a copy, and {@link #toBufferedImage()} wraps the same array as the
 * image's {@link DataBufferByte}: the image and this object share the pixels, so writing to
 * one shows in the other. Copy first ({@code getPixels().clone()}) if you need them apart.
 */
public final class RenderedPixels {
    private final int width;
    private final int height;
    private final int channels;
    private final boolean bgr;
    private final int frameIndex;
    private final byte[] pixels;
    private final List<OverlaySummary> overlays;
    private final Integer selectedOverlayIndex;

    /** Constructed by native code (src/dataset.rs, {@code nativeRenderPixels}). */
    RenderedPixels(String metadataJson, byte[] pixels) {
        Map<String, Object> metadata = Json.asMap(Json.parse(metadataJson));
        this.width = Json.asInt(metadata.get("width"));
        this.height = Json.asInt(metadata.get("height"));
        this.channels = Json.asInt(metadata.get("channels"));
        this.bgr = Json.asBoolean(metadata.get("bgr"));
        this.frameIndex = Json.asInt(metadata.get("frameIndex"));
        this.pixels = pixels;
        List<OverlaySummary> parsed = new ArrayList<>();
        for (Object raw : Json.asList(metadata.get("overlays"))) {
            Map<String, Object> overlay = Json.asMap(raw);
            parsed.add(new OverlaySummary(Json.asInt(overlay.get("index")), Json.asInt(overlay.get("group")),
                Json.asInt(overlay.get("rows")), Json.asInt(overlay.get("columns")), (String) overlay.get("overlayType"),
                (String) overlay.get("label")));
        }
        this.overlays = parsed;
        Object selected = metadata.get("selectedOverlayIndex");
        this.selectedOverlayIndex = selected == null ? null : Json.asInt(selected);
    }

    public int getWidth() {
        return width;
    }

    public int getHeight() {
        return height;
    }

    /** 1 (gray) or 3 (color). */
    public int getChannels() {
        return channels;
    }

    /** Whether 3-channel pixels are in B,G,R order (as {@code renderImage} asks for, to back a
     * {@code TYPE_3BYTE_BGR} image as they are) rather than R,G,B. */
    public boolean isBgr() {
        return bgr;
    }

    public int getFrameIndex() {
        return frameIndex;
    }

    /** The pixels themselves - shared, not copied (see the class doc). */
    public byte[] getPixels() {
        return pixels;
    }

    /** Every overlay plane on the instance, whether or not one was composited. */
    public List<OverlaySummary> getOverlays() {
        return overlays;
    }

    /** Which overlay was composited into the pixels, or {@code null}. */
    public Integer getSelectedOverlayIndex() {
        return selectedOverlayIndex;
    }

    /**
     * A {@link BufferedImage} over {@link #getPixels()} - no copy: {@code TYPE_BYTE_GRAY} for one
     * channel, {@code TYPE_3BYTE_BGR} for B,G,R pixels, and an equivalent sRGB {@code TYPE_CUSTOM}
     * image for R,G,B ones. Works with {@code ImageIO.write}, {@code Graphics2D} and so on.
     */
    public BufferedImage toBufferedImage() {
        DataBufferByte buffer = new DataBufferByte(pixels, pixels.length);
        int[] bandOffsets;
        ColorSpace space;
        if (channels == 1) {
            bandOffsets = new int[] {0};
            space = ColorSpace.getInstance(ColorSpace.CS_GRAY);
        } else {
            bandOffsets = bgr ? new int[] {2, 1, 0} : new int[] {0, 1, 2};
            space = ColorSpace.getInstance(ColorSpace.CS_sRGB);
        }
        WritableRaster raster =
            Raster.createInterleavedRaster(buffer, width, height, width * channels, channels, bandOffsets, null);
        ColorModel model = new ComponentColorModel(space, false, false, Transparency.OPAQUE, DataBuffer.TYPE_BYTE);
        return new BufferedImage(model, raster, false, null);
    }
}
