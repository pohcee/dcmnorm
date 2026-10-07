package com.pohcee.dcmnorm.internal;

import java.util.ArrayList;
import java.util.LinkedHashMap;
import java.util.List;
import java.util.Map;

/**
 * A tiny, dependency-free JSON encoder/decoder used only to cross the JNI boundary.
 *
 * <p>This binding talks JSON across every native call - not because dcmnorm itself needs it
 * (the Node and Python bindings pass structured options as native JS objects/Python keyword
 * arguments instead), but because hand-writing JNI field-by-field marshaling for a dozen
 * "options" shapes, each with several optional fields, is both a lot of repetitive unsafe-ish
 * code AND a common source of a particular failure mode: {@code GetFieldID}/{@code
 * GetMethodID} return {@code null} on a signature-string typo, which the compiler cannot catch,
 * and calling through a null id crashes the JVM rather than raising a catchable exception. A
 * single, small, well-tested JSON codec bounds that risk to one place - and this project already
 * "talks JSON everywhere" (see the main README and both other bindings' own docs), so it is not
 * a foreign idiom here. Structured values with a binary payload ({@link
 * com.pohcee.dcmnorm.RenderedFrame}, {@link com.pohcee.dcmnorm.RenderedMovie}, {@link
 * com.pohcee.dcmnorm.TextureExportResult}) still cross as one real JNI object construction call
 * each - this codec only covers their metadata half, plus every options/result value that has no
 * binary payload at all.
 */
public final class Json {
    private Json() {
    }

    // ---------------------------------------------------------------------------------------
    // Encoding
    // ---------------------------------------------------------------------------------------

    /** Builds a JSON object text incrementally. A {@code null} value omits the key entirely -
     * this is what lets the Rust side's {@code Option<T>} fields default correctly. */
    public static final class ObjectBuilder {
        private final StringBuilder sb = new StringBuilder("{");
        private boolean first = true;

        private ObjectBuilder putRaw(String key, String encodedValueOrNull) {
            if (encodedValueOrNull == null) {
                return this;
            }
            if (!first) {
                sb.append(',');
            }
            first = false;
            sb.append(encodeString(key)).append(':').append(encodedValueOrNull);
            return this;
        }

        public ObjectBuilder put(String key, String value) {
            return putRaw(key, value == null ? null : encodeString(value));
        }

        public ObjectBuilder put(String key, Boolean value) {
            return putRaw(key, value == null ? null : value.toString());
        }

        public ObjectBuilder put(String key, Integer value) {
            return putRaw(key, value == null ? null : value.toString());
        }

        public ObjectBuilder put(String key, Long value) {
            return putRaw(key, value == null ? null : value.toString());
        }

        public ObjectBuilder put(String key, Double value) {
            return putRaw(key, value == null ? null : encodeNumber(value));
        }

        public ObjectBuilder putStrings(String key, List<String> values) {
            return putRaw(key, values == null ? null : encodeStringArray(values));
        }

        public ObjectBuilder putInts(String key, List<Integer> values) {
            if (values == null) {
                return this;
            }
            StringBuilder out = new StringBuilder("[");
            for (int i = 0; i < values.size(); i++) {
                if (i > 0) {
                    out.append(',');
                }
                out.append(values.get(i));
            }
            return putRaw(key, out.append(']').toString());
        }

        public ObjectBuilder putDoubles(String key, double[] values) {
            if (values == null) {
                return this;
            }
            StringBuilder out = new StringBuilder("[");
            for (int i = 0; i < values.length; i++) {
                if (i > 0) {
                    out.append(',');
                }
                out.append(encodeNumber(values[i]));
            }
            return putRaw(key, out.append(']').toString());
        }

        public ObjectBuilder putStringMap(String key, Map<String, String> values) {
            if (values == null) {
                return this;
            }
            StringBuilder out = new StringBuilder("{");
            boolean innerFirst = true;
            for (Map.Entry<String, String> entry : values.entrySet()) {
                if (!innerFirst) {
                    out.append(',');
                }
                innerFirst = false;
                out.append(encodeString(entry.getKey())).append(':').append(encodeString(entry.getValue()));
            }
            return putRaw(key, out.append('}').toString());
        }

        /** Embeds an already-encoded JSON value (object/array/etc.) verbatim under {@code key}. */
        public ObjectBuilder putRawJson(String key, String json) {
            return putRaw(key, json);
        }

        public String build() {
            return sb.append('}').toString();
        }
    }

    public static ObjectBuilder object() {
        return new ObjectBuilder();
    }

    public static String encodeStringArray(List<String> values) {
        StringBuilder out = new StringBuilder("[");
        for (int i = 0; i < values.size(); i++) {
            if (i > 0) {
                out.append(',');
            }
            out.append(encodeString(values.get(i)));
        }
        return out.append(']').toString();
    }

    public static String encodeNumber(double value) {
        if (Double.isNaN(value) || Double.isInfinite(value)) {
            throw new IllegalArgumentException("cannot encode non-finite number " + value + " as JSON");
        }
        // Java never omits the fractional digit (e.g. Double.toString(40.0) == "40.0"), so the
        // result is always a syntactically valid JSON number as-is.
        return Double.toString(value);
    }

    public static String encodeString(String value) {
        StringBuilder out = new StringBuilder(value.length() + 2);
        out.append('"');
        for (int i = 0; i < value.length(); i++) {
            char c = value.charAt(i);
            switch (c) {
                case '"':
                    out.append("\\\"");
                    break;
                case '\\':
                    out.append("\\\\");
                    break;
                case '\n':
                    out.append("\\n");
                    break;
                case '\r':
                    out.append("\\r");
                    break;
                case '\t':
                    out.append("\\t");
                    break;
                default:
                    if (c < 0x20) {
                        out.append(String.format("\\u%04x", (int) c));
                    } else {
                        out.append(c);
                    }
            }
        }
        return out.append('"').toString();
    }

    // ---------------------------------------------------------------------------------------
    // Decoding - a minimal recursive-descent parser producing a plain Object graph (Map<String,
    // Object>, List<Object>, String, Double, Boolean, or null), plus small typed accessors.
    // ---------------------------------------------------------------------------------------

    public static Object parse(String json) {
        Parser parser = new Parser(json);
        parser.skipWhitespace();
        Object value = parser.parseValue();
        parser.skipWhitespace();
        if (parser.pos != json.length()) {
            throw new IllegalArgumentException("unexpected trailing content in JSON at offset " + parser.pos);
        }
        return value;
    }

    @SuppressWarnings("unchecked")
    public static Map<String, Object> asMap(Object value) {
        return (Map<String, Object>) value;
    }

    @SuppressWarnings("unchecked")
    public static List<Object> asList(Object value) {
        return (List<Object>) value;
    }

    public static String asString(Object value) {
        return (String) value;
    }

    public static boolean asBoolean(Object value) {
        return (Boolean) value;
    }

    public static int asInt(Object value) {
        return ((Number) value).intValue();
    }

    public static long asLong(Object value) {
        return ((Number) value).longValue();
    }

    public static double asDouble(Object value) {
        return ((Number) value).doubleValue();
    }

    public static Double asNullableDouble(Object value) {
        return value == null ? null : ((Number) value).doubleValue();
    }

    public static double[] asDoubleArray(Object value) {
        List<Object> list = asList(value);
        double[] result = new double[list.size()];
        for (int i = 0; i < list.size(); i++) {
            result[i] = asDouble(list.get(i));
        }
        return result;
    }

    public static boolean[] asBooleanArray(Object value) {
        List<Object> list = asList(value);
        boolean[] result = new boolean[list.size()];
        for (int i = 0; i < list.size(); i++) {
            result[i] = asBoolean(list.get(i));
        }
        return result;
    }

    public static List<String> asStringList(Object value) {
        List<Object> list = asList(value);
        List<String> result = new ArrayList<>(list.size());
        for (Object item : list) {
            result.add(asString(item));
        }
        return result;
    }

    private static final class Parser {
        private final String s;
        private int pos;

        Parser(String s) {
            this.s = s;
            this.pos = 0;
        }

        void skipWhitespace() {
            while (pos < s.length() && Character.isWhitespace(s.charAt(pos))) {
                pos++;
            }
        }

        private char peek() {
            if (pos >= s.length()) {
                throw new IllegalArgumentException("unexpected end of JSON input");
            }
            return s.charAt(pos);
        }

        private void expect(char c) {
            if (peek() != c) {
                throw new IllegalArgumentException("expected '" + c + "' at offset " + pos + " in JSON input");
            }
            pos++;
        }

        Object parseValue() {
            skipWhitespace();
            char c = peek();
            switch (c) {
                case '{':
                    return parseObject();
                case '[':
                    return parseArray();
                case '"':
                    return parseString();
                case 't':
                    expectLiteral("true");
                    return Boolean.TRUE;
                case 'f':
                    expectLiteral("false");
                    return Boolean.FALSE;
                case 'n':
                    expectLiteral("null");
                    return null;
                default:
                    return parseNumber();
            }
        }

        private void expectLiteral(String literal) {
            if (!s.regionMatches(pos, literal, 0, literal.length())) {
                throw new IllegalArgumentException("invalid JSON literal at offset " + pos);
            }
            pos += literal.length();
        }

        private Map<String, Object> parseObject() {
            Map<String, Object> result = new LinkedHashMap<>();
            expect('{');
            skipWhitespace();
            if (peek() == '}') {
                pos++;
                return result;
            }
            while (true) {
                skipWhitespace();
                String key = parseString();
                skipWhitespace();
                expect(':');
                Object value = parseValue();
                result.put(key, value);
                skipWhitespace();
                char next = peek();
                if (next == ',') {
                    pos++;
                    continue;
                }
                expect('}');
                break;
            }
            return result;
        }

        private List<Object> parseArray() {
            List<Object> result = new ArrayList<>();
            expect('[');
            skipWhitespace();
            if (peek() == ']') {
                pos++;
                return result;
            }
            while (true) {
                result.add(parseValue());
                skipWhitespace();
                char next = peek();
                if (next == ',') {
                    pos++;
                    continue;
                }
                expect(']');
                break;
            }
            return result;
        }

        private String parseString() {
            expect('"');
            StringBuilder out = new StringBuilder();
            while (true) {
                char c = peek();
                pos++;
                if (c == '"') {
                    break;
                }
                if (c == '\\') {
                    char escape = peek();
                    pos++;
                    switch (escape) {
                        case '"':
                            out.append('"');
                            break;
                        case '\\':
                            out.append('\\');
                            break;
                        case '/':
                            out.append('/');
                            break;
                        case 'n':
                            out.append('\n');
                            break;
                        case 'r':
                            out.append('\r');
                            break;
                        case 't':
                            out.append('\t');
                            break;
                        case 'b':
                            out.append('\b');
                            break;
                        case 'f':
                            out.append('\f');
                            break;
                        case 'u':
                            String hex = s.substring(pos, pos + 4);
                            out.append((char) Integer.parseInt(hex, 16));
                            pos += 4;
                            break;
                        default:
                            throw new IllegalArgumentException("invalid escape '\\" + escape + "' in JSON string");
                    }
                } else {
                    out.append(c);
                }
            }
            return out.toString();
        }

        private Double parseNumber() {
            int start = pos;
            if (peek() == '-') {
                pos++;
            }
            while (pos < s.length() && Character.isDigit(s.charAt(pos))) {
                pos++;
            }
            if (pos < s.length() && s.charAt(pos) == '.') {
                pos++;
                while (pos < s.length() && Character.isDigit(s.charAt(pos))) {
                    pos++;
                }
            }
            if (pos < s.length() && (s.charAt(pos) == 'e' || s.charAt(pos) == 'E')) {
                pos++;
                if (pos < s.length() && (s.charAt(pos) == '+' || s.charAt(pos) == '-')) {
                    pos++;
                }
                while (pos < s.length() && Character.isDigit(s.charAt(pos))) {
                    pos++;
                }
            }
            if (pos == start) {
                throw new IllegalArgumentException("invalid JSON number at offset " + pos);
            }
            return Double.parseDouble(s.substring(start, pos));
        }
    }
}
