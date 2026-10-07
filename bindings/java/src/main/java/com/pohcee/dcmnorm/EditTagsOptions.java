package com.pohcee.dcmnorm;

import com.pohcee.dcmnorm.internal.Json;
import java.util.List;
import java.util.Map;

/** Optional arguments for {@link Dcmnorm#editTags}. */
public final class EditTagsOptions {
    private String outputPath;
    private Map<String, String> set;
    private List<String> remove;
    private Boolean removePrivateTags;

    /** Writes back to the input file in place if not given. */
    public EditTagsOptions outputPath(String outputPath) {
        this.outputPath = outputPath;
        return this;
    }

    /** {@code {tagKeyOrKeyword: value}} attributes to set. */
    public EditTagsOptions set(Map<String, String> set) {
        this.set = set;
        return this;
    }

    /** Tag keys/keywords to remove. */
    public EditTagsOptions remove(List<String> remove) {
        this.remove = remove;
        return this;
    }

    public EditTagsOptions removePrivateTags(boolean removePrivateTags) {
        this.removePrivateTags = removePrivateTags;
        return this;
    }

    String toJson() {
        return Json.object()
            .put("outputPath", outputPath)
            .putStringMap("set", set)
            .putStrings("remove", remove)
            .put("removePrivateTags", removePrivateTags)
            .build();
    }
}
