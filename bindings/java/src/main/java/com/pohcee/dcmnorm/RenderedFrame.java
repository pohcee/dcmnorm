package com.pohcee.dcmnorm;

import com.pohcee.dcmnorm.internal.Json;
import java.util.ArrayList;
import java.util.List;
import java.util.Map;

/**
 * The result of {@link Dcmnorm#renderFrame} / {@link DicomVolumeHandle#reformat}: one encoded
 * image frame plus overlay metadata.
 *
 * <p>Constructed directly from native code (one {@code NewObject} call with this exact {@code
 * (String, byte[])} constructor signature - see {@code new_rendered_frame} in {@code src/lib.rs})
 * rather than field-by-field, so this constructor is not part of the public API; it parses the
 * metadata JSON itself.
 */
public final class RenderedFrame {
    private final String mimeType;
    private final int width;
    private final int height;
    private final byte[] data;
    private final List<OverlaySummary> overlays;
    private final Integer selectedOverlayIndex;

    RenderedFrame(String metadataJson, byte[] data) {
        Map<String, Object> metadata = Json.asMap(Json.parse(metadataJson));
        this.mimeType = Json.asString(metadata.get("mimeType"));
        this.width = Json.asInt(metadata.get("width"));
        this.height = Json.asInt(metadata.get("height"));
        this.data = data;

        List<Object> rawOverlays = Json.asList(metadata.get("overlays"));
        List<OverlaySummary> parsedOverlays = new ArrayList<>(rawOverlays.size());
        for (Object rawOverlay : rawOverlays) {
            Map<String, Object> overlay = Json.asMap(rawOverlay);
            parsedOverlays.add(new OverlaySummary(
                Json.asInt(overlay.get("index")),
                Json.asInt(overlay.get("group")),
                Json.asInt(overlay.get("rows")),
                Json.asInt(overlay.get("columns")),
                (String) overlay.get("overlayType"),
                (String) overlay.get("label")));
        }
        this.overlays = parsedOverlays;

        Object selected = metadata.get("selectedOverlayIndex");
        this.selectedOverlayIndex = selected == null ? null : Json.asInt(selected);
    }

    /** {@code "image/jpeg"} or {@code "image/png"}. */
    public String getMimeType() {
        return mimeType;
    }

    public int getWidth() {
        return width;
    }

    public int getHeight() {
        return height;
    }

    /** The encoded image bytes. */
    public byte[] getData() {
        return data;
    }

    /** Every overlay plane present on the source instance, regardless of whether one was
     * rendered into {@link #getData()}. Always empty for a {@link DicomVolumeHandle#reformat}
     * result (MPR reformats do not currently support overlay rendering). */
    public List<OverlaySummary> getOverlays() {
        return overlays;
    }

    /** Which overlay (by {@link OverlaySummary#getIndex()}) was actually composited into {@link
     * #getData()}, or {@code null} if none was. */
    public Integer getSelectedOverlayIndex() {
        return selectedOverlayIndex;
    }
}
