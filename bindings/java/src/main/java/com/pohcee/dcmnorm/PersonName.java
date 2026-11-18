package com.pohcee.dcmnorm;

/**
 * A Person Name (PN) value split into its component groups (alphabetic, ideographic, phonetic;
 * separated by {@code =}) and components (family, given, middle, prefix, suffix; separated by
 * {@code ^}), as in PS3.5 6.2.1. Missing parts are empty strings, never {@code null}.
 */
public final class PersonName {
    /** One component group: the five name components. */
    public static final class Group {
        private final String[] components;

        Group(String text) {
            String[] parts = text.split("\\^", -1);
            components = new String[5];
            for (int i = 0; i < 5; i++) {
                components[i] = i < parts.length ? parts[i] : "";
            }
            if (parts.length > 5) {
                // more than five components: keep the rest with the suffix, as written
                StringBuilder suffix = new StringBuilder(components[4]);
                for (int i = 5; i < parts.length; i++) {
                    suffix.append('^').append(parts[i]);
                }
                components[4] = suffix.toString();
            }
        }

        public String getFamilyName() {
            return components[0];
        }

        public String getGivenName() {
            return components[1];
        }

        public String getMiddleName() {
            return components[2];
        }

        public String getNamePrefix() {
            return components[3];
        }

        public String getNameSuffix() {
            return components[4];
        }

        public boolean isEmpty() {
            for (String component : components) {
                if (!component.isEmpty()) {
                    return false;
                }
            }
            return true;
        }

        /** The group as DICOM writes it, trailing empty components dropped. */
        @Override
        public String toString() {
            int last = 4;
            while (last > 0 && components[last].isEmpty()) {
                last--;
            }
            StringBuilder text = new StringBuilder(components[0]);
            for (int i = 1; i <= last; i++) {
                text.append('^').append(components[i]);
            }
            return text.toString();
        }
    }

    private final String value;
    private final Group alphabetic;
    private final Group ideographic;
    private final Group phonetic;

    public PersonName(String value) {
        this.value = value == null ? "" : value;
        String[] groups = this.value.split("=", -1);
        this.alphabetic = new Group(groups.length > 0 ? groups[0] : "");
        this.ideographic = new Group(groups.length > 1 ? groups[1] : "");
        this.phonetic = new Group(groups.length > 2 ? groups[2] : "");
    }

    public Group getAlphabetic() {
        return alphabetic;
    }

    public Group getIdeographic() {
        return ideographic;
    }

    public Group getPhonetic() {
        return phonetic;
    }

    /** Shorthand for {@code getAlphabetic().getFamilyName()}. */
    public String getFamilyName() {
        return alphabetic.getFamilyName();
    }

    /** Shorthand for {@code getAlphabetic().getGivenName()}. */
    public String getGivenName() {
        return alphabetic.getGivenName();
    }

    /** The value as stored, e.g. {@code "Yamada^Tarou=山田^太郎=やまだ^たろう"}. */
    @Override
    public String toString() {
        return value;
    }
}
