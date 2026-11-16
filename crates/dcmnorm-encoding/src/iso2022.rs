//! ISO 2022 code extensions for a multi-valued Specific Character Set (0008,0005), as in
//! `\ISO 2022 IR 87` or `ISO 2022 IR 6\ISO 2022 IR 149` (PS3.5 6.1.2.5, PS3.3 C.12.1.1.2).
//!
//! Text in such a data set switches character sets with escape sequences: `ESC $ B` designates
//! JIS X 0208 to G0 (bytes 0x21-0x7E taken in pairs), `ESC $ ) C` KS X 1001 to G1 (bytes
//! 0xA1-0xFE in pairs), `ESC - A` ISO 8859-1 to G1, `ESC ( B` ASCII back to G0, and so on. At
//! each delimiter (value `\`, Person Name component `^` and group `=`, and CR, LF, TAB, FF) the
//! designations revert to those of the first value of Specific Character Set
//! (PS3.5 6.1.2.5.3). Decoding follows those rules to Unicode; encoding produces the escape
//! sequences for the declared character sets, so values read this way are written back in the
//! same repertoires.

use encoding::all::{
    EUC_JP, GBK, ISO_8859_1, ISO_8859_2, ISO_8859_3, ISO_8859_4, ISO_8859_5, ISO_8859_6,
    ISO_8859_7, ISO_8859_8, WINDOWS_1254, WINDOWS_31J, WINDOWS_874, WINDOWS_949,
};
use encoding::{DecoderTrap, EncoderTrap, Encoding};

/// A Defined Term of Specific Character Set taking part in ISO 2022 code extension.
#[derive(Debug, Copy, Clone, Eq, PartialEq, PartialOrd, Ord, Hash)]
#[repr(u8)]
pub(crate) enum Term {
    Ir6,
    Ir13,
    Ir87,
    Ir159,
    Ir100,
    Ir101,
    Ir109,
    Ir110,
    Ir126,
    Ir127,
    Ir138,
    Ir144,
    Ir148,
    Ir149,
    Ir58,
    Ir166,
}

const ALL_TERMS: [Term; 16] = [
    Term::Ir6,
    Term::Ir13,
    Term::Ir87,
    Term::Ir159,
    Term::Ir100,
    Term::Ir101,
    Term::Ir109,
    Term::Ir110,
    Term::Ir126,
    Term::Ir127,
    Term::Ir138,
    Term::Ir144,
    Term::Ir148,
    Term::Ir149,
    Term::Ir58,
    Term::Ir166,
];

impl Term {
    fn from_code(code: &str) -> Option<Term> {
        let code = code.trim();
        // the ISO 2022 terms, plus the single-byte spellings some writers use in value 1
        let number = code
            .strip_prefix("ISO 2022 IR ")
            .or_else(|| code.strip_prefix("ISO_IR "))
            .or_else(|| code.strip_prefix("ISO_IR_"));
        Some(match number {
            None if code.is_empty() => Term::Ir6,
            None => return None,
            Some("6") => Term::Ir6,
            Some("13") => Term::Ir13,
            Some("87") => Term::Ir87,
            Some("159") => Term::Ir159,
            Some("100") => Term::Ir100,
            Some("101") => Term::Ir101,
            Some("109") => Term::Ir109,
            Some("110") => Term::Ir110,
            Some("126") => Term::Ir126,
            Some("127") => Term::Ir127,
            Some("138") => Term::Ir138,
            Some("144") => Term::Ir144,
            Some("148") => Term::Ir148,
            Some("149") => Term::Ir149,
            Some("58") => Term::Ir58,
            Some("166") => Term::Ir166,
            Some(_) => return None,
        })
    }

    fn code(self) -> &'static str {
        match self {
            Term::Ir6 => "ISO 2022 IR 6",
            Term::Ir13 => "ISO 2022 IR 13",
            Term::Ir87 => "ISO 2022 IR 87",
            Term::Ir159 => "ISO 2022 IR 159",
            Term::Ir100 => "ISO 2022 IR 100",
            Term::Ir101 => "ISO 2022 IR 101",
            Term::Ir109 => "ISO 2022 IR 109",
            Term::Ir110 => "ISO 2022 IR 110",
            Term::Ir126 => "ISO 2022 IR 126",
            Term::Ir127 => "ISO 2022 IR 127",
            Term::Ir138 => "ISO 2022 IR 138",
            Term::Ir144 => "ISO 2022 IR 144",
            Term::Ir148 => "ISO 2022 IR 148",
            Term::Ir149 => "ISO 2022 IR 149",
            Term::Ir58 => "ISO 2022 IR 58",
            Term::Ir166 => "ISO 2022 IR 166",
        }
    }

    fn bit(self) -> u32 {
        1 << (self as u32)
    }

    /// The single-byte G1 encoding and the final byte of its `ESC - F` designation.
    fn single_byte_g1(self) -> Option<(&'static dyn Encoding, u8)> {
        Some(match self {
            Term::Ir100 => (ISO_8859_1, b'A'),
            Term::Ir101 => (ISO_8859_2, b'B'),
            Term::Ir109 => (ISO_8859_3, b'C'),
            Term::Ir110 => (ISO_8859_4, b'D'),
            Term::Ir126 => (ISO_8859_7, b'F'),
            Term::Ir127 => (ISO_8859_6, b'G'),
            Term::Ir138 => (ISO_8859_8, b'H'),
            Term::Ir144 => (ISO_8859_5, b'L'),
            // ISO 8859-9 and TIS 620 agree with these over 0xA0-0xFF, the G1 range
            Term::Ir148 => (WINDOWS_1254, b'M'),
            Term::Ir166 => (WINDOWS_874, b'T'),
            _ => return None,
        })
    }

    fn from_single_byte_final(final_byte: u8) -> Option<Term> {
        ALL_TERMS
            .iter()
            .copied()
            .find(|term| term.single_byte_g1().is_some_and(|(_, f)| f == final_byte))
    }
}

/// The character set designated to G0 (bytes 0x21-0x7E).
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
enum G0 {
    /// ASCII (ISO-IR 6), or JIS X 0201 Romaji (ISO-IR 14), which only differ at 0x5C/0x7E
    Ascii,
    /// JIS X 0208 (ISO-IR 87), two bytes per character
    Jis0208,
    /// JIS X 0212 (ISO-IR 159), two bytes per character
    Jis0212,
}

/// The character set designated to G1 (bytes 0xA1-0xFE).
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
enum G1 {
    None,
    /// JIS X 0201 Katakana (ISO-IR 13)
    Katakana,
    /// KS X 1001 (ISO-IR 149), two bytes per character
    Ksc,
    /// GB 2312 (ISO-IR 58), two bytes per character
    Gb2312,
    /// an ISO 8859 right-hand part
    Single(Term),
}

/// The character sets declared by a multi-valued Specific Character Set.
#[derive(Debug, Copy, Clone, Eq, PartialEq, PartialOrd, Ord, Hash)]
pub(crate) struct Iso2022 {
    /// value 1: the designations in effect at the start of every value and after delimiters
    initial: Term,
    /// all the declared terms, as `Term::bit`s
    declared: u32,
}

impl Iso2022 {
    /// The character sets of the values of a Specific Character Set. Values that are not
    /// supported are left out; `None` when none is supported.
    pub(crate) fn from_codes<S: AsRef<str>>(codes: &[S]) -> Option<Self> {
        let (first, rest) = codes.split_first()?;
        let initial = Term::from_code(first.as_ref());
        let mut declared = initial.map_or(0, Term::bit);
        for code in rest {
            if let Some(term) = Term::from_code(code.as_ref()) {
                declared |= term.bit();
            }
        }
        if declared == 0 {
            return None;
        }
        Some(Iso2022 {
            initial: initial.unwrap_or(Term::Ir6),
            declared: declared | Term::Ir6.bit(),
        })
    }

    /// The Specific Character Set value for these character sets.
    pub(crate) fn name(&self) -> String {
        let mut values = vec![self.initial.code()];
        values.extend(
            ALL_TERMS
                .iter()
                .copied()
                .filter(|term| *term != self.initial && *term != Term::Ir6 && self.declares(*term))
                .map(Term::code),
        );
        values.join("\\")
    }

    fn declares(&self, term: Term) -> bool {
        self.declared & term.bit() != 0
    }

    fn initial_state(&self) -> (G0, G1) {
        let g1 = match self.initial {
            Term::Ir13 => G1::Katakana,
            Term::Ir149 => G1::Ksc,
            Term::Ir58 => G1::Gb2312,
            term if term.single_byte_g1().is_some() => G1::Single(term),
            _ => G1::None,
        };
        (G0::Ascii, g1)
    }

    pub(crate) fn decode(&self, text: &[u8]) -> String {
        let (initial_g0, initial_g1) = self.initial_state();
        let (mut g0, mut g1) = (initial_g0, initial_g1);
        let mut out = String::with_capacity(text.len());
        let mut i = 0;
        while i < text.len() {
            let byte = text[i];
            if byte == 0x1B {
                if let Some((consumed, designation)) = parse_escape(&text[i + 1..]) {
                    match designation {
                        Designation::G0(set) => g0 = set,
                        Designation::G1(set) => g1 = set,
                    }
                    i += 1 + consumed;
                } else {
                    out.push('\u{1B}');
                    i += 1;
                }
                continue;
            }
            if byte < 0x80 {
                if g0 != G0::Ascii && (0x21..0x7F).contains(&byte) {
                    match text.get(i + 1) {
                        Some(&second) if (0x21..0x7F).contains(&second) => {
                            let bytes: &[u8] = if g0 == G0::Jis0208 {
                                &[byte | 0x80, second | 0x80]
                            } else {
                                &[0x8F, byte | 0x80, second | 0x80]
                            };
                            out.push_str(&decode_lossy(EUC_JP, bytes));
                            i += 2;
                        }
                        _ => {
                            out.push('\u{FFFD}');
                            i += 1;
                        }
                    }
                    continue;
                }
                out.push(byte as char);
                if is_delimiter(byte) {
                    g0 = initial_g0;
                    g1 = initial_g1;
                }
                i += 1;
                continue;
            }
            match g1 {
                G1::Ksc | G1::Gb2312 if text.get(i + 1).is_some_and(|b| *b >= 0xA1) => {
                    let encoding: &dyn Encoding = if g1 == G1::Ksc { WINDOWS_949 } else { GBK };
                    out.push_str(&decode_lossy(encoding, &text[i..i + 2]));
                    i += 2;
                }
                G1::Katakana => {
                    out.push_str(&decode_lossy(WINDOWS_31J, &text[i..i + 1]));
                    i += 1;
                }
                G1::Single(term) => {
                    let (encoding, _) = term.single_byte_g1().expect("single-byte G1 set");
                    out.push_str(&decode_lossy(encoding, &text[i..i + 1]));
                    i += 1;
                }
                // no G1 designation: as the default repertoire decoder does, read Latin-1
                _ => {
                    out.push(byte as char);
                    i += 1;
                }
            }
        }
        out
    }

    pub(crate) fn encode(&self, text: &str) -> Result<Vec<u8>, String> {
        let (initial_g0, initial_g1) = self.initial_state();
        let (mut g0, mut g1) = (initial_g0, initial_g1);
        let mut out = Vec::with_capacity(text.len());
        for ch in text.chars() {
            if ch.is_ascii() {
                if g0 != G0::Ascii {
                    out.extend_from_slice(b"\x1b(B");
                    g0 = G0::Ascii;
                }
                out.push(ch as u8);
                if is_delimiter(ch as u8) {
                    g0 = initial_g0;
                    g1 = initial_g1;
                }
                continue;
            }
            let Some((bytes, designation)) = self.encode_char(ch, g0, g1) else {
                return Err(format!(
                    "character {ch:?} cannot be encoded in Specific Character Set {}",
                    self.name()
                ));
            };
            match designation {
                Designation::G0(set) if set != g0 => {
                    out.extend_from_slice(escape_for_g0(set));
                    g0 = set;
                }
                Designation::G1(set) if set != g1 => {
                    out.extend_from_slice(&escape_for_g1(set));
                    g1 = set;
                }
                _ => {}
            }
            out.extend_from_slice(&bytes);
        }
        if g0 != G0::Ascii {
            out.extend_from_slice(b"\x1b(B");
        }
        Ok(out)
    }

    /// The bytes of one non-ASCII character and the set it needs, preferring the sets
    /// currently designated (no escape sequence) over the others.
    fn encode_char(&self, ch: char, g0: G0, g1: G1) -> Option<(Vec<u8>, Designation)> {
        let current = [designation_term(Designation::G0(g0)), designation_term(Designation::G1(g1))];
        let mut order: Vec<Term> = current.iter().copied().flatten().collect();
        order.push(self.initial);
        order.extend(ALL_TERMS.iter().copied());
        order
            .into_iter()
            .filter(|term| self.declares(*term))
            .find_map(|term| encode_in(term, ch))
    }
}

#[derive(Debug, Copy, Clone, Eq, PartialEq)]
enum Designation {
    G0(G0),
    G1(G1),
}

fn designation_term(designation: Designation) -> Option<Term> {
    match designation {
        Designation::G0(G0::Jis0208) => Some(Term::Ir87),
        Designation::G0(G0::Jis0212) => Some(Term::Ir159),
        Designation::G0(G0::Ascii) => None,
        Designation::G1(G1::Katakana) => Some(Term::Ir13),
        Designation::G1(G1::Ksc) => Some(Term::Ir149),
        Designation::G1(G1::Gb2312) => Some(Term::Ir58),
        Designation::G1(G1::Single(term)) => Some(term),
        Designation::G1(G1::None) => None,
    }
}

/// One character in one character set, if that set has it.
fn encode_in(term: Term, ch: char) -> Option<(Vec<u8>, Designation)> {
    let mut buffer = [0u8; 4];
    let text: &str = ch.encode_utf8(&mut buffer);
    let in_g1_range = |bytes: &[u8]| bytes.iter().all(|b| (0xA1..=0xFE).contains(b));
    match term {
        Term::Ir87 => {
            let bytes = EUC_JP.encode(text, EncoderTrap::Strict).ok()?;
            (bytes.len() == 2 && in_g1_range(&bytes))
                .then(|| (bytes.iter().map(|b| b & 0x7F).collect(), Designation::G0(G0::Jis0208)))
        }
        Term::Ir13 => {
            let code = ch as u32;
            (0xFF61..=0xFF9F).contains(&code)
                .then(|| (vec![(code - 0xFF61 + 0xA1) as u8], Designation::G1(G1::Katakana)))
        }
        Term::Ir149 | Term::Ir58 => {
            let (encoding, set): (&dyn Encoding, G1) = if term == Term::Ir149 {
                (WINDOWS_949, G1::Ksc)
            } else {
                (GBK, G1::Gb2312)
            };
            let bytes = encoding.encode(text, EncoderTrap::Strict).ok()?;
            (bytes.len() == 2 && in_g1_range(&bytes)).then_some((bytes, Designation::G1(set)))
        }
        term => {
            let (encoding, _) = term.single_byte_g1()?;
            let bytes = encoding.encode(text, EncoderTrap::Strict).ok()?;
            (bytes.len() == 1 && bytes[0] >= 0xA0)
                .then_some((bytes, Designation::G1(G1::Single(term))))
        }
    }
}

fn escape_for_g0(set: G0) -> &'static [u8] {
    match set {
        G0::Ascii => b"\x1b(B",
        G0::Jis0208 => b"\x1b$B",
        G0::Jis0212 => b"\x1b$(D",
    }
}

fn escape_for_g1(set: G1) -> Vec<u8> {
    match set {
        G1::Katakana => b"\x1b)I".to_vec(),
        G1::Ksc => b"\x1b$)C".to_vec(),
        G1::Gb2312 => b"\x1b$)A".to_vec(),
        G1::Single(term) => {
            let (_, final_byte) = term.single_byte_g1().expect("single-byte G1 set");
            vec![0x1B, b'-', final_byte]
        }
        G1::None => Vec::new(),
    }
}

/// The designation of the escape sequence whose bytes after ESC start `rest`, and how many of
/// those bytes it has.
fn parse_escape(rest: &[u8]) -> Option<(usize, Designation)> {
    Some(match rest {
        [b'(', b'B' | b'J', ..] => (2, Designation::G0(G0::Ascii)),
        [b')', b'I', ..] => (2, Designation::G1(G1::Katakana)),
        [b'$', b'B' | b'@', ..] => (2, Designation::G0(G0::Jis0208)),
        [b'$', b'(', b'D', ..] => (3, Designation::G0(G0::Jis0212)),
        [b'$', b')', b'C', ..] => (3, Designation::G1(G1::Ksc)),
        [b'$', b')', b'A', ..] => (3, Designation::G1(G1::Gb2312)),
        [b'-', final_byte, ..] => (2, Designation::G1(G1::Single(Term::from_single_byte_final(*final_byte)?))),
        _ => return None,
    })
}

/// The delimiters after which designations revert to value 1's (PS3.5 6.1.2.5.3).
fn is_delimiter(byte: u8) -> bool {
    matches!(byte, b'\\' | b'^' | b'=' | b'\r' | b'\n' | b'\t' | 0x0C)
}

fn decode_lossy(encoding: &dyn Encoding, bytes: &[u8]) -> String {
    encoding
        .decode(bytes, DecoderTrap::Replace)
        .unwrap_or_else(|_| "\u{FFFD}".to_owned())
}

#[cfg(test)]
mod tests {
    use super::Iso2022;

    fn charset(codes: &[&str]) -> Iso2022 {
        Iso2022::from_codes(codes).expect("supported character sets")
    }

    /// pohcee/dcmnorm#24: the bytes pydicom writes for these names.
    #[test]
    fn decodes_and_encodes_japanese_jis_x_0208() {
        let codec = charset(&["", "ISO 2022 IR 87"]);
        let bytes = b"\x1b$B;n83\x1b(B^\x1b$BJ8;z\x1b(B";
        assert_eq!(codec.decode(bytes), "試験^文字");
        assert_eq!(codec.encode("試験^文字").unwrap(), bytes);
    }

    #[test]
    fn decodes_and_encodes_korean_ks_x_1001() {
        let codec = charset(&["", "ISO 2022 IR 149"]);
        let bytes = b"\x1b$)C\xc5\xd7\xbd\xba\xc6\xae^\x1b$)C\xb9\xae\xc0\xda";
        assert_eq!(codec.decode(bytes), "테스트^문자");
        assert_eq!(codec.encode("테스트^문자").unwrap(), bytes);
    }

    /// Clunie's charset test SCSH31 (PS3.5 H.3.1): Yamada^Tarou=山田^太郎=やまだ^たろう
    #[test]
    fn decodes_ps3_5_h_3_1_example() {
        let codec = charset(&["", "ISO 2022 IR 87"]);
        let bytes = b"Yamada^Tarou=\x1b$B;3ED\x1b(B^\x1b$BB@O:\x1b(B=\x1b$B$d$^$@\x1b(B^\x1b$B$?$m$&\x1b(B";
        let text = "Yamada^Tarou=山田^太郎=やまだ^たろう";
        assert_eq!(codec.decode(bytes), text);
        assert_eq!(codec.encode(text).unwrap(), bytes);
    }

    /// PS3.5 H.3.2 (SCSH32): value 1 ISO 2022 IR 13 puts half-width katakana in G1.
    #[test]
    fn decodes_ps3_5_h_3_2_example() {
        let codec = charset(&["ISO 2022 IR 13", "ISO 2022 IR 87"]);
        let bytes = b"\xd4\xcf\xc0\xde^\xc0\xdb\xb3=\x1b$B;3ED\x1b(B^\x1b$BB@O:\x1b(B=\x1b$B$d$^$@\x1b(B^\x1b$B$?$m$&\x1b(B";
        let text = "ﾔﾏﾀﾞ^ﾀﾛｳ=山田^太郎=やまだ^たろう";
        assert_eq!(codec.decode(bytes), text);
        assert_eq!(codec.encode(text).unwrap(), bytes);
    }

    /// PS3.5 I.2 (SCSI2): Hong^Gildong=洪^吉洞=홍^길동 in KS X 1001 with re-designation after
    /// every delimiter.
    #[test]
    fn decodes_ps3_5_i_2_example() {
        let codec = charset(&["", "ISO 2022 IR 149"]);
        let bytes = b"Hong^Gildong=\x1b$)C\xfb\xf3^\x1b$)C\xd1\xce\xd4\xd7=\x1b$)C\xc8\xab^\x1b$)C\xb1\xe6\xb5\xbf";
        let text = "Hong^Gildong=洪^吉洞=홍^길동";
        assert_eq!(codec.decode(bytes), text);
        assert_eq!(codec.encode(text).unwrap(), bytes);
    }

    #[test]
    fn latin1_g1_designation_and_value_reset() {
        let codec = charset(&["ISO 2022 IR 6", "ISO 2022 IR 100", "ISO 2022 IR 144"]);
        let bytes = b"\x1b-A\xfc\\\x1b-L\xb8";
        assert_eq!(codec.decode(bytes), "ü\\И");
        assert_eq!(codec.encode("ü\\И").unwrap(), bytes);
        assert!(codec.encode("試").is_err(), "undeclared repertoire");
    }

    #[test]
    fn names_and_unknown_values() {
        assert_eq!(charset(&["", "ISO 2022 IR 87"]).name(), "ISO 2022 IR 6\\ISO 2022 IR 87");
        assert_eq!(charset(&["", "BOGUS", "ISO 2022 IR 149"]).name(), "ISO 2022 IR 6\\ISO 2022 IR 149");
        assert!(Iso2022::from_codes(&["BOGUS", "ALSO BOGUS"]).is_none());
        // a stray escape sequence that is not a designation is kept
        assert_eq!(charset(&["", "ISO 2022 IR 87"]).decode(b"a\x1b%Gb"), "a\u{1b}%Gb");
    }
}
