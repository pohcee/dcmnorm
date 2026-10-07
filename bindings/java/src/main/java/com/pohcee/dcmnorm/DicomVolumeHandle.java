package com.pohcee.dcmnorm;

/**
 * An opaque, read-only handle around a volume built by {@link Dcmnorm#buildVolume} - see that
 * method's own doc. Building a volume is the expensive step (reads + decodes every slice in a
 * series); keep this handle resident (e.g. in your own volume cache) so every subsequent {@link
 * #reformat} call for a rotate/scroll/window-level change is cheap.
 *
 * <p><strong>Must be closed exactly once</strong> ({@link #close()}, ideally via
 * try-with-resources) to free the native memory it holds. Unlike the Node bindings (where a
 * {@code DicomVolumeHandle} is freed automatically once V8 garbage-collects it) or the Python
 * bindings (reference-counted via PyO3), the JVM has no destructor hook a native library can
 * rely on running promptly - a plain finalizer would run at an unpredictable time (if ever), so
 * this class deliberately does not use one. Every method after {@link #close()} throws {@link
 * IllegalStateException}. {@link #close()} itself is idempotent.
 *
 * <p>Thread-safe for concurrent reads ({@link #reformat}/{@link #exportTexture} calls) once
 * built - the underlying volume is immutable - but {@link #close()} must not race a concurrent
 * call to another method on the same handle.
 */
public final class DicomVolumeHandle implements AutoCloseable {
    private long nativeHandle;
    private volatile boolean closed;

    DicomVolumeHandle(long nativeHandle) {
        this.nativeHandle = nativeHandle;
    }

    private long checkOpen() {
        if (closed) {
            throw new IllegalStateException("DicomVolumeHandle is already closed");
        }
        return nativeHandle;
    }

    /** Rows in the source slices (image height). */
    public int getRows() {
        return Dcmnorm.nativeVolumeRows(checkOpen());
    }

    /** Columns in the source slices (image width). */
    public int getCols() {
        return Dcmnorm.nativeVolumeCols(checkOpen());
    }

    public int getNumSlices() {
        return Dcmnorm.nativeVolumeNumSlices(checkOpen());
    }

    /** The volume's own acquisition-native orientation, for seeding an "axial" reformat -
     * {@code [rowDir(3), colDir(3)]}, 6 elements. */
    public double[] getNativeBasis() {
        return Dcmnorm.nativeVolumeNativeBasis(checkOpen());
    }

    /** The volume's own physical center, in patient/LPS mm - a reasonable default reformat
     * origin, {@code [x, y, z]}. */
    public double[] getCenter() {
        return Dcmnorm.nativeVolumeCenter(checkOpen());
    }

    /** The volume's own smallest voxel dimension, in mm - a reasonable default output spacing. */
    public double getMinSpacingMm() {
        return Dcmnorm.nativeVolumeMinSpacingMm(checkOpen());
    }

    /** Resamples one plane through this volume - see {@link ReformatParams}'s own doc. */
    public RenderedFrame reformat(ReformatParams params) throws DcmnormException {
        return Dcmnorm.nativeVolumeReformat(checkOpen(), params.toJson());
    }

    public TextureExportResult exportTexture() throws DcmnormException {
        return exportTexture(new TextureExportOptions());
    }

    /** Packs this volume's own native voxel lattice - see {@link TextureExportOptions}'s own
     * doc. */
    public TextureExportResult exportTexture(TextureExportOptions options) throws DcmnormException {
        return Dcmnorm.nativeVolumeExportTexture(checkOpen(), options.toJson());
    }

    @Override
    public synchronized void close() {
        if (closed) {
            return;
        }
        closed = true;
        Dcmnorm.nativeFreeVolume(nativeHandle);
        nativeHandle = 0;
    }
}
