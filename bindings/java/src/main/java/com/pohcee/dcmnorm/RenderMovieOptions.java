package com.pohcee.dcmnorm;

import com.pohcee.dcmnorm.internal.Json;

/** Optional arguments for {@link Dcmnorm#renderMovie}. */
public final class RenderMovieOptions {
    private Integer outputWidth;
    private Integer outputHeight;
    private Double windowCenter;
    private Double windowWidth;
    private Double fps;

    public RenderMovieOptions outputWidth(int outputWidth) {
        this.outputWidth = outputWidth;
        return this;
    }

    public RenderMovieOptions outputHeight(int outputHeight) {
        this.outputHeight = outputHeight;
        return this;
    }

    public RenderMovieOptions windowCenter(double windowCenter) {
        this.windowCenter = windowCenter;
        return this;
    }

    public RenderMovieOptions windowWidth(double windowWidth) {
        this.windowWidth = windowWidth;
        return this;
    }

    /** Defaults to 24. */
    public RenderMovieOptions fps(double fps) {
        this.fps = fps;
        return this;
    }

    String toJson() {
        return Json.object().put("outputWidth", outputWidth).put("outputHeight", outputHeight)
            .put("windowCenter", windowCenter).put("windowWidth", windowWidth).put("fps", fps).build();
    }
}
