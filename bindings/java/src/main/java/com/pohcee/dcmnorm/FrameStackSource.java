package com.pohcee.dcmnorm;

import com.pohcee.dcmnorm.internal.Json;
import java.util.List;

/**
 * One source file (and, for a cine/multiframe instance, the specific frames within it) to pack
 * into a frame-stack texture - see {@link Dcmnorm#exportFrameStackTexture}.
 */
public final class FrameStackSource {
    private final String filePath;
    private final List<Integer> frameIndices;

    public FrameStackSource(String filePath) {
        this(filePath, null);
    }

    /** @param frameIndices defaults to {@code [0]} if {@code null}. */
    public FrameStackSource(String filePath, List<Integer> frameIndices) {
        this.filePath = filePath;
        this.frameIndices = frameIndices;
    }

    String toJson() {
        return Json.object().put("filePath", filePath).putInts("frameIndices", frameIndices).build();
    }
}
