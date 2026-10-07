package com.pohcee.dcmnorm;

/**
 * One DICOM overlay plane (group {@code 60xx}) present on an instance, as reported by {@link
 * RenderedFrame#getOverlays()}.
 */
public final class OverlaySummary {
    private final int index;
    private final int group;
    private final int rows;
    private final int columns;
    private final String overlayType;
    private final String label;

    OverlaySummary(int index, int group, int rows, int columns, String overlayType, String label) {
        this.index = index;
        this.group = group;
        this.rows = rows;
        this.columns = columns;
        this.overlayType = overlayType;
        this.label = label;
    }

    /** 0-based ordinal among the overlay groups present on this instance, ascending by group -
     * the value {@code overlayIndex} (on render/reformat options) selects by. */
    public int getIndex() {
        return index;
    }

    /** The raw DICOM overlay group, e.g. {@code 0x6000}, {@code 0x6002}, ... {@code 0x601E}. */
    public int getGroup() {
        return group;
    }

    public int getRows() {
        return rows;
    }

    public int getColumns() {
        return columns;
    }

    /** {@code OverlayType} (60xx,0040): {@code "G"} (graphics) or {@code "R"} (ROI), or {@code
     * null} if absent. */
    public String getOverlayType() {
        return overlayType;
    }

    /** {@code OverlayLabel} (60xx,1500), falling back to {@code OverlayDescription}
     * (60xx,0022), or {@code null} if neither is present. */
    public String getLabel() {
        return label;
    }

    @Override
    public String toString() {
        return "OverlaySummary{index=" + index + ", group=" + group + ", rows=" + rows + ", columns=" + columns
            + ", overlayType=" + overlayType + ", label=" + label + "}";
    }
}
