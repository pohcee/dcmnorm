package com.pohcee.dcmnorm;

/**
 * The result of {@link Dcmnorm#renderMovie}: an encoded video.
 *
 * <p>Constructed directly from native code (one {@code NewObject} call with this exact {@code
 * (String, byte[])} constructor signature) - see {@link RenderedFrame}'s own doc for why.
 */
public final class RenderedMovie {
    private final String mimeType;
    private final byte[] data;

    RenderedMovie(String mimeType, byte[] data) {
        this.mimeType = mimeType;
        this.data = data;
    }

    /** Always {@code "video/mp4"} today. */
    public String getMimeType() {
        return mimeType;
    }

    /** The encoded video bytes. */
    public byte[] getData() {
        return data;
    }
}
