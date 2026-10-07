package com.pohcee.dcmnorm;

import com.pohcee.dcmnorm.internal.Json;

/** Arguments for {@link DicomVolumeHandle#reformat}. */
public final class ReformatParams {
    private final double[] origin;
    private final double[] rowDir;
    private final double[] colDir;
    private final int outputWidth;
    private final int outputHeight;
    private final double spacingMm;
    private Double windowCenter;
    private Double windowWidth;
    private String format;
    private Integer jpegQuality;
    private String interpolation;
    private Double slabThicknessMm;
    private String slabProjection;

    /**
     * @param origin world-space (mm) point at the CENTER of the output image, {@code [x, y, z]}
     * @param rowDir unit vector: direction of travel across the output image as the column
     *     index increases, {@code [x, y, z]}
     * @param colDir unit vector: direction of travel down the output image as the row index
     *     increases, {@code [x, y, z]}
     * @param spacingMm physical size of one output pixel, in mm (the same value in both output
     *     axes)
     */
    public ReformatParams(double[] origin, double[] rowDir, double[] colDir, int outputWidth, int outputHeight,
        double spacingMm) {
        requireVec3(origin, "origin");
        requireVec3(rowDir, "rowDir");
        requireVec3(colDir, "colDir");
        this.origin = origin;
        this.rowDir = rowDir;
        this.colDir = colDir;
        this.outputWidth = outputWidth;
        this.outputHeight = outputHeight;
        this.spacingMm = spacingMm;
    }

    private static void requireVec3(double[] values, String name) {
        if (values == null || values.length != 3) {
            throw new IllegalArgumentException(name + " must have exactly 3 values (x, y, z)");
        }
    }

    public ReformatParams windowCenter(double windowCenter) {
        this.windowCenter = windowCenter;
        return this;
    }

    public ReformatParams windowWidth(double windowWidth) {
        this.windowWidth = windowWidth;
        return this;
    }

    /** {@code "jpeg"} (default) or {@code "png"}. */
    public ReformatParams format(String format) {
        this.format = format;
        return this;
    }

    public ReformatParams jpegQuality(int jpegQuality) {
        this.jpegQuality = jpegQuality;
        return this;
    }

    /** {@code "trilinear"} (default) or {@code "nearest"} (faster - a reasonable choice for a
     * live-drag preview frame). */
    public ReformatParams interpolation(String interpolation) {
        this.interpolation = interpolation;
        return this;
    }

    /** Default 0 (an infinitely-thin plane). A positive value turns on a thick-slab reformat
     * centered on {@code origin}, combined per {@link #slabProjection}. */
    public ReformatParams slabThicknessMm(double slabThicknessMm) {
        this.slabThicknessMm = slabThicknessMm;
        return this;
    }

    /** {@code "mip"} (maximum intensity, default), {@code "minip"}, or {@code "average"}. Only
     * meaningful when {@link #slabThicknessMm} is positive. */
    public ReformatParams slabProjection(String slabProjection) {
        this.slabProjection = slabProjection;
        return this;
    }

    String toJson() {
        return Json.object().putDoubles("origin", origin).putDoubles("rowDir", rowDir).putDoubles("colDir", colDir)
            .put("outputWidth", outputWidth).put("outputHeight", outputHeight).put("spacingMm", spacingMm)
            .put("windowCenter", windowCenter).put("windowWidth", windowWidth).put("format", format)
            .put("jpegQuality", jpegQuality).put("interpolation", interpolation)
            .put("slabThicknessMm", slabThicknessMm).put("slabProjection", slabProjection).build();
    }
}
