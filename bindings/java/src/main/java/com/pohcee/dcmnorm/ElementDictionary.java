package com.pohcee.dcmnorm;

import com.pohcee.dcmnorm.internal.NativeLoader;

/**
 * Run-time lookups in dcmnorm's standard data dictionary - the same dictionary {@link Tag} is
 * generated from (dcm4che has a class of the same name for the same purpose).
 */
public final class ElementDictionary {
    static {
        NativeLoader.ensureLoaded();
    }

    private ElementDictionary() {
    }

    /** The standard keyword of {@code tag} ({@code "StudyInstanceUID"}), repeating groups such as
     * {@code 60xx} included, or {@code null} for a private or unknown tag. */
    public static String keywordOf(int tag) {
        return nativeKeywordOf(tag);
    }

    /** The dictionary VR of {@code tag}, or {@code null} for a private or unknown tag. For an
     * attribute with alternative VRs this is the one dcmnorm uses (US for US/SS, OW for OB/OW). */
    public static VR vrOf(int tag) {
        return VR.of(nativeVrOf(tag));
    }

    /** The tag of a standard keyword ({@code "StudyInstanceUID"}), or of a tag expression
     * ({@code "(0020,000D)"}, {@code "0020000D"}); {@code -1} if it is neither. */
    public static int tagForKeyword(String keyword) {
        long tag = keyword == null ? -1 : nativeTagForKeyword(keyword);
        return tag < 0 ? -1 : (int) tag;
    }

    private static native String nativeKeywordOf(int tag);

    private static native String nativeVrOf(int tag);

    private static native long nativeTagForKeyword(String keyword);
}
