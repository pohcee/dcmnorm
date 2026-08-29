//! The in-memory DICOM object tree ([`InMemDicomObject`]) and its element type.
//!
//! Elements are stored as a `Vec<InMemElement>` kept sorted by tag, not a `BTreeMap`/`HashMap` -
//! see the crate README for why. `.get`/`.element` use binary search; `.put` uses a binary
//! search to find the insertion point, so lookups stay O(log n) even though insertion is O(n)
//! (objects are built once via a single ordered parse pass, then queried many times - the right
//! tradeoff for this access pattern).

use std::io::{Read, Write};

use dcmnorm_core::header::{DataElement, HasLength, Header, Tag};
use dcmnorm_core::value::{C, DataSetSequence, PixelFragmentSequence, Value};
use dcmnorm_core::{ops::AttributeOp, PrimitiveValue, VR};
use dcmnorm_encoding::transfer_syntax::TransferSyntax;
use dcmnorm_parser::dataset::{DataSetReader, DataSetWriter, DataToken};

use crate::error::{ReadError, WriteError};

/// The pixel data fragment type used by [`InMemDicomObject`] - a plain owned byte buffer.
pub type InMemFragment = Vec<u8>;

/// A data element within an [`InMemDicomObject`] tree (a primitive value, a nested sequence of
/// further `InMemDicomObject`s, or an encapsulated pixel data fragment sequence).
pub type InMemElement = DataElement<InMemDicomObject, InMemFragment>;

/// An in-memory DICOM data set: a flat, tag-ordered list of elements, each of which may itself
/// contain a nested list (for `SQ` values).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct InMemDicomObject {
    elements: Vec<InMemElement>,
}

impl HasLength for InMemDicomObject {
    fn length(&self) -> dcmnorm_core::header::Length {
        dcmnorm_core::header::Length::UNDEFINED
    }
}

impl InMemDicomObject {
    /// An empty object with no elements.
    pub fn new_empty() -> Self {
        InMemDicomObject { elements: Vec::new() }
    }

    /// Build an object directly from a list of elements, sorting them into tag order.
    /// Matches `dicom_object::InMemDicomObject::from_element_iter`.
    pub fn from_element_iter<T>(iter: T) -> Self
    where
        T: IntoIterator<Item = InMemElement>,
    {
        let mut elements: Vec<InMemElement> = iter.into_iter().collect();
        elements.sort_by_key(|e| e.tag());
        InMemDicomObject { elements }
    }

    /// Build a DIMSE command data set from a list of elements. DIMSE command sets have the
    /// same shape as a regular data set (a flat list of primitive elements, always Implicit VR
    /// Little Endian on the wire per PS3.7) - this is an alias for [`Self::from_element_iter`]
    /// kept for call-site clarity in `dimse.rs`/`scp.rs`.
    pub fn command_from_element_iter<T>(iter: T) -> Self
    where
        T: IntoIterator<Item = InMemElement>,
    {
        Self::from_element_iter(iter)
    }

    fn binary_search(&self, tag: Tag) -> Result<usize, usize> {
        self.elements.binary_search_by_key(&tag, |e| e.tag())
    }

    /// Look up an element by tag. Returns `None` if absent.
    pub fn get(&self, tag: Tag) -> Option<&InMemElement> {
        self.binary_search(tag).ok().map(|i| &self.elements[i])
    }

    /// Look up an element by tag, returning a `Result` (an `Err` with a descriptive message on
    /// absence) rather than `Option` - for call sites that want to `?`-propagate a hard failure
    /// on a required element.
    pub fn element(&self, tag: Tag) -> Result<&InMemElement, MissingElementError> {
        self.get(tag).ok_or(MissingElementError { tag })
    }

    /// Insert or replace a text element by tag and VR, creating it if absent. A thin
    /// convenience wrapper around [`Self::put`] for CLI-style attribute edits
    /// (`--set KEY=VALUE`) and DIMSE command/identifier construction that only ever deal in
    /// string values.
    pub fn put_str(&mut self, tag: Tag, vr: VR, value: impl Into<String>) {
        self.put(DataElement::new(tag, vr, PrimitiveValue::from(value.into())));
    }

    /// Insert or replace an element, keeping the element list in tag order.
    pub fn put(&mut self, element: InMemElement) {
        match self.binary_search(element.tag()) {
            Ok(i) => self.elements[i] = element,
            Err(i) => self.elements.insert(i, element),
        }
    }

    /// Remove an element by tag. Returns whether an element was actually present.
    pub fn remove_element(&mut self, tag: Tag) -> bool {
        match self.binary_search(tag) {
            Ok(i) => {
                self.elements.remove(i);
                true
            }
            Err(_) => false,
        }
    }

    /// Iterate over all elements, in tag order.
    pub fn iter(&self) -> impl Iterator<Item = &InMemElement> {
        self.elements.iter()
    }

    /// The number of top-level elements in this object.
    pub fn len(&self) -> usize {
        self.elements.len()
    }

    pub fn is_empty(&self) -> bool {
        self.elements.is_empty()
    }

    /// Apply an attribute mutation, as returned by a pixel data encoder after transcoding (e.g.
    /// a Photometric Interpretation or Planar Configuration adjustment required by the new
    /// transfer syntax).
    pub fn apply(&mut self, op: AttributeOp) {
        use dcmnorm_core::ops::{AttributeAction, AttributeSelectorStep};

        // dcmnorm only ever applies single-step, top-level selectors (confirmed by its own
        // usage in io.rs - pixel data codec post-transcode attribute patches never target
        // nested sequences), so a full attribute-selector-path walk isn't needed here.
        let AttributeSelectorStep::Tag(tag) = *op.selector.first_step() else {
            return;
        };
        let current_vr = || self.get(tag).map(|e| e.vr()).unwrap_or(VR::UN);

        match op.action {
            AttributeAction::Remove => {
                self.remove_element(tag);
            }
            AttributeAction::Empty => {
                self.put(DataElement::new(tag, current_vr(), PrimitiveValue::Empty));
            }
            AttributeAction::SetVr(vr) => {
                if let Some(element) = self.get(tag) {
                    let value = element.value().clone();
                    self.put(DataElement::new(tag, vr, value));
                }
            }
            AttributeAction::Set(value) => {
                self.put(DataElement::new(tag, vr_for_action_value(&value), value));
            }
            AttributeAction::SetIfMissing(value)
                if self.get(tag).is_none() => {
                    self.put(DataElement::new(tag, vr_for_action_value(&value), value));
                }
            AttributeAction::SetStr(s) => {
                self.put(DataElement::new(tag, current_vr(), PrimitiveValue::from(s.into_owned())));
            }
            AttributeAction::SetStrIfMissing(s)
                if self.get(tag).is_none() => {
                    self.put(DataElement::new(tag, VR::CS, PrimitiveValue::from(s.into_owned())));
                }
            AttributeAction::Replace(value)
                if self.get(tag).is_some() => {
                    self.put(DataElement::new(tag, vr_for_action_value(&value), value));
                }
            AttributeAction::ReplaceStr(s)
                if self.get(tag).is_some() => {
                    self.put(DataElement::new(tag, current_vr(), PrimitiveValue::from(s.into_owned())));
                }
            _ => {}
        }
    }

    /// Attach a [`FileMetaTable`] built from a [`crate::FileMetaTableBuilder`], producing a
    /// [`crate::FileDicomObject`].
    pub fn with_meta(
        self,
        builder: crate::FileMetaTableBuilder,
    ) -> Result<crate::FileDicomObject<Self>, crate::error::WithMetaError> {
        let meta = builder.build()?;
        Ok(crate::FileDicomObject { meta, object: self })
    }

    /// Read a bare data set (no Part 10 preamble/meta group) from `source`, using the given
    /// transfer syntax.
    pub fn read_dataset_with_ts<R: Read>(source: R, ts: &TransferSyntax) -> Result<Self, ReadError> {
        let mut reader = DataSetReader::new_with_ts(source, ts)
            .map_err(|source| ReadError::Dataset { source })?;
        build_dataset(&mut reader, false)
    }

    #[doc(hidden)]
    pub fn into_elements(self) -> Vec<InMemElement> {
        self.elements
    }

    /// Write this object as a bare data set (no Part 10 preamble/meta group) using the given
    /// transfer syntax.
    pub fn write_dataset_with_ts<W: Write>(&self, to: W, ts: &TransferSyntax) -> Result<(), WriteError> {
        let mut writer =
            DataSetWriter::with_ts(to, ts).map_err(|source| WriteError::Dataset { source })?;
        for element in &self.elements {
            write_element_tokens(&mut writer, element)?;
        }
        Ok(())
    }
}

fn vr_for_action_value(value: &PrimitiveValue) -> VR {
    // Best-effort: attribute-op Set values built by dcmnorm's own pixel data codecs are always
    // numeric (Photometric Interpretation aside, which goes through SetStr) - default to the
    // VR that matches the value's own shape.
    use dcmnorm_core::value::PrimitiveValue as PV;
    match value {
        PV::U16(_) => VR::US,
        PV::I16(_) => VR::SS,
        PV::U32(_) => VR::UL,
        PV::I32(_) => VR::SL,
        PV::Strs(_) | PV::Str(_) => VR::CS,
        _ => VR::UN,
    }
}

/// A required element was not present in the object.
#[derive(Debug, Clone, Copy)]
pub struct MissingElementError {
    pub tag: Tag,
}

impl std::fmt::Display for MissingElementError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "element {:?} not found", self.tag)
    }
}

impl std::error::Error for MissingElementError {}

/// Fold a `DataToken` stream into a tree of elements. `in_item` controls the stop condition:
/// `false` runs until the token stream is exhausted (top-level data set); `true` runs until a
/// matching `ItemEnd` token (a sequence item's nested data set), consuming that `ItemEnd`.
fn build_dataset(
    tokens: &mut impl Iterator<Item = Result<DataToken, dcmnorm_parser::dataset::read::Error>>,
    in_item: bool,
) -> Result<InMemDicomObject, ReadError> {
    let mut elements = Vec::new();

    loop {
        let Some(token) = tokens.next() else {
            if in_item {
                return Err(ReadError::UnexpectedToken { context: "expected ItemEnd, got end of stream" });
            }
            break;
        };
        let token = token?;

        match token {
            DataToken::ItemEnd if in_item => break,

            DataToken::ElementHeader(header) => {
                let value = expect_primitive_value(tokens)?;
                elements.push(DataElement::new(header.tag, header.vr, value));
            }

            DataToken::SequenceStart { tag, len } => {
                let mut items: C<InMemDicomObject> = C::new();
                loop {
                    match next_required(tokens)? {
                        DataToken::ItemStart { .. } => {
                            items.push(build_dataset(tokens, true)?);
                        }
                        DataToken::SequenceEnd => break,
                        _ => {
                            return Err(ReadError::UnexpectedToken {
                                context: "expected ItemStart or SequenceEnd in sequence",
                            })
                        }
                    }
                }
                let value = Value::Sequence(DataSetSequence::new(items, len));
                elements.push(DataElement::new(tag, VR::SQ, value));
            }

            DataToken::PixelSequenceStart => {
                let value = read_pixel_sequence(tokens)?;
                elements.push(DataElement::new(Tag(0x7FE0, 0x0010), VR::OB, value));
            }

            _ => {
                return Err(ReadError::UnexpectedToken {
                    context: "unexpected token at data set top level",
                })
            }
        }
    }

    elements.sort_by_key(|e| e.tag());
    Ok(InMemDicomObject { elements })
}

/// Like [`InMemDicomObject::read_dataset_with_ts`], but stops consuming the token stream as
/// soon as a top-level element's tag exceeds `stop_tag` - a fast path for partial/filtered
/// reads (`dcmnorm`'s `--filter` CLI flag) that don't need the rest of the data set. Doesn't
/// attempt early-stop *inside* nested sequences (real `--filter` usage targets top-level
/// tags), so a stop tag inside a sequence just falls back to reading that whole sequence.
pub(crate) fn read_dataset_until(
    source: impl Read,
    ts: &TransferSyntax,
    stop_tag: Tag,
) -> Result<InMemDicomObject, ReadError> {
    let mut reader =
        DataSetReader::new_with_ts(source, ts).map_err(|source| ReadError::Dataset { source })?;
    let mut elements = Vec::new();

    loop {
        let Some(token) = reader.next() else { break };
        let token = token?;
        match token {
            DataToken::ElementHeader(header) => {
                let value = expect_primitive_value(&mut reader)?;
                let tag = header.tag;
                elements.push(DataElement::new(tag, header.vr, value));
                if tag >= stop_tag {
                    break;
                }
            }
            DataToken::SequenceStart { tag, len } => {
                let mut items: C<InMemDicomObject> = C::new();
                loop {
                    match next_required(&mut reader)? {
                        DataToken::ItemStart { .. } => items.push(build_dataset(&mut reader, true)?),
                        DataToken::SequenceEnd => break,
                        _ => {
                            return Err(ReadError::UnexpectedToken {
                                context: "expected ItemStart or SequenceEnd in sequence",
                            })
                        }
                    }
                }
                elements.push(DataElement::new(tag, VR::SQ, Value::Sequence(DataSetSequence::new(items, len))));
                if tag >= stop_tag {
                    break;
                }
            }
            DataToken::PixelSequenceStart => {
                // Pixel data is always the last element in a data set (highest possible tag,
                // 7FE0,0010) - reaching it means we're already past any plausible stop tag, so
                // there's no early-stop benefit in trying to partially consume it. Read it in
                // full via the shared sequence-building logic and then stop.
                let value = read_pixel_sequence(&mut reader)?;
                elements.push(DataElement::new(Tag(0x7FE0, 0x0010), VR::OB, value));
                break;
            }
            _ => {
                return Err(ReadError::UnexpectedToken {
                    context: "unexpected token at data set top level",
                })
            }
        }
    }

    elements.sort_by_key(|e| e.tag());
    Ok(InMemDicomObject { elements })
}

/// Read the token sequence following a `PixelSequenceStart` token (already consumed by the
/// caller). Per `dcmnorm_parser::dataset::read::DataSetReader`'s actual protocol (confirmed
/// against its own test fixtures): the offset table is itself framed as the *first item* -
/// `ItemStart { len }`, then `OffsetTable(vec)` only if `len > 0` (an empty offset table item
/// produces no `OffsetTable` token at all), then `ItemEnd` - followed by one
/// `ItemStart`/`ItemValue`/`ItemEnd` triple per fragment, terminated by `SequenceEnd`.
fn read_pixel_sequence(
    tokens: &mut impl Iterator<Item = Result<DataToken, dcmnorm_parser::dataset::read::Error>>,
) -> Result<Value<InMemDicomObject, InMemFragment>, ReadError> {
    let offset_table = match next_required(tokens)? {
        DataToken::ItemStart { len } if len == dcmnorm_core::header::Length(0) => {
            expect_item_end(tokens)?;
            Vec::new()
        }
        DataToken::ItemStart { .. } => match next_required(tokens)? {
            DataToken::OffsetTable(t) => {
                expect_item_end(tokens)?;
                t
            }
            _ => {
                return Err(ReadError::UnexpectedToken {
                    context: "expected OffsetTable in the first item of encapsulated pixel data",
                })
            }
        },
        _ => {
            return Err(ReadError::UnexpectedToken {
                context: "expected ItemStart for the offset table item",
            })
        }
    };

    let mut fragments: C<InMemFragment> = C::new();
    loop {
        match next_required(tokens)? {
            DataToken::ItemStart { .. } => {
                let bytes = match next_required(tokens)? {
                    DataToken::ItemValue(bytes) => bytes,
                    _ => {
                        return Err(ReadError::UnexpectedToken {
                            context: "expected ItemValue for pixel data fragment",
                        })
                    }
                };
                expect_item_end(tokens)?;
                fragments.push(bytes);
            }
            DataToken::SequenceEnd => break,
            _ => {
                return Err(ReadError::UnexpectedToken {
                    context: "expected ItemStart or SequenceEnd in pixel data sequence",
                })
            }
        }
    }

    Ok(Value::PixelSequence(PixelFragmentSequence::new(offset_table, fragments)))
}

fn expect_item_end(
    tokens: &mut impl Iterator<Item = Result<DataToken, dcmnorm_parser::dataset::read::Error>>,
) -> Result<(), ReadError> {
    match next_required(tokens)? {
        DataToken::ItemEnd => Ok(()),
        _ => Err(ReadError::UnexpectedToken { context: "expected ItemEnd" }),
    }
}

fn next_required(
    tokens: &mut impl Iterator<Item = Result<DataToken, dcmnorm_parser::dataset::read::Error>>,
) -> Result<DataToken, ReadError> {
    tokens
        .next()
        .ok_or(ReadError::UnexpectedToken { context: "unexpected end of token stream" })?
        .map_err(ReadError::from)
}

fn expect_primitive_value(
    tokens: &mut impl Iterator<Item = Result<DataToken, dcmnorm_parser::dataset::read::Error>>,
) -> Result<PrimitiveValue, ReadError> {
    match next_required(tokens)? {
        DataToken::PrimitiveValue(v) => Ok(v),
        _ => Err(ReadError::UnexpectedToken { context: "expected PrimitiveValue after ElementHeader" }),
    }
}

/// Write one element (recursively, for sequences/pixel data) as its token sequence.
pub(crate) fn write_element_tokens<W: Write>(
    writer: &mut DataSetWriter<W, dcmnorm_encoding::transfer_syntax::DynEncoder<'_, W>>,
    element: &InMemElement,
) -> Result<(), WriteError> {
    match element.value() {
        Value::Primitive(pv) => {
            writer
                .write_sequence([
                    DataToken::ElementHeader(dcmnorm_core::header::DataElementHeader::new(
                        element.tag(),
                        element.vr(),
                        dcmnorm_core::header::Length::defined({
                            let len = pv.calculate_byte_len() as u32;
                            len + (len % 2)
                        }),
                    )),
                    DataToken::PrimitiveValue(pv.clone()),
                ])
                .map_err(|source| WriteError::Dataset { source })?;
        }
        Value::Sequence(seq) => {
            writer
                .write(DataToken::SequenceStart { tag: element.tag(), len: seq.length() })
                .map_err(|source| WriteError::Dataset { source })?;
            for item in seq.items() {
                writer
                    .write(DataToken::ItemStart { len: dcmnorm_core::header::Length::UNDEFINED })
                    .map_err(|source| WriteError::Dataset { source })?;
                for item_element in item.iter() {
                    write_element_tokens(writer, item_element)?;
                }
                writer.write(DataToken::ItemEnd).map_err(|source| WriteError::Dataset { source })?;
            }
            writer.write(DataToken::SequenceEnd).map_err(|source| WriteError::Dataset { source })?;
        }
        Value::PixelSequence(pix) => {
            writer
                .write(DataToken::PixelSequenceStart)
                .map_err(|source| WriteError::Dataset { source })?;
            // The offset table is itself framed as the first item, matching the protocol
            // DataSetReader expects on read-back (see read_pixel_sequence's doc comment) - the
            // writer does not wrap DataToken::OffsetTable in an item on its own.
            let offset_table = pix.offset_table();
            writer
                .write(DataToken::ItemStart {
                    len: dcmnorm_core::header::Length::defined((offset_table.len() * 4) as u32),
                })
                .map_err(|source| WriteError::Dataset { source })?;
            if !offset_table.is_empty() {
                writer
                    .write(DataToken::OffsetTable(offset_table.to_vec()))
                    .map_err(|source| WriteError::Dataset { source })?;
            }
            writer.write(DataToken::ItemEnd).map_err(|source| WriteError::Dataset { source })?;
            for fragment in pix.fragments() {
                writer
                    .write(DataToken::ItemStart { len: dcmnorm_core::header::Length::defined(fragment.len() as u32) })
                    .map_err(|source| WriteError::Dataset { source })?;
                writer
                    .write(DataToken::ItemValue(fragment.clone()))
                    .map_err(|source| WriteError::Dataset { source })?;
                writer.write(DataToken::ItemEnd).map_err(|source| WriteError::Dataset { source })?;
            }
            writer.write(DataToken::SequenceEnd).map_err(|source| WriteError::Dataset { source })?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use dcmnorm_encoding::transfer_syntax::TransferSyntaxIndex;
    use dcmnorm_transcode::TransferSyntaxRegistry;

    /// Documents the actual, precise boundary of a real, currently-live constraint (not a bug
    /// to fix here - see below): the default character set codec used when no
    /// SpecificCharacterSet (0008,0005) is declared is `DefaultCharacterSetCodec`, which despite
    /// its "ISO_IR 6" name actually encodes as ISO-8859-1/Latin-1 (a strict superset of 7-bit
    /// ASCII), not strict 7-bit ASCII - inherited unchanged from the mechanically-ported
    /// `dcmnorm-encoding` (see its README; this crate's write orchestration was deliberately
    /// left behavior-preserving in Phase 4 of the dicom-rs removal plan). So Western-European
    /// accented text (French/German/Spanish names, etc.) already writes successfully without
    /// any declared charset - only text with characters *outside* Latin-1 (CJK, Cyrillic, Greek,
    /// emoji, ...) still fails. `dcmnorm`'s `scp.rs` unconditionally injects
    /// `SpecificCharacterSet=ISO_IR 192` (UTF-8) before writing synthesized datasets precisely
    /// to cover that remaining gap, since C-FIND results can carry PHI in any script. Making the
    /// writer natively permissive for the full Unicode range (removing the need for that
    /// workaround entirely) is a deliberate, not-yet-done follow-up: doing it safely for every
    /// caller - including the DIMSE streaming paths, which write directly into a
    /// caller-supplied sink rather than a buffer this crate could retry into - needs a
    /// buffering strategy that doesn't regress memory usage for large pixel-data payloads, not
    /// a change to make incidentally.
    #[test]
    fn default_charset_covers_latin1_but_not_wider_unicode() {
        let ts = TransferSyntaxRegistry
            .get(dcmnorm_dictionary::uids::EXPLICIT_VR_LITTLE_ENDIAN)
            .unwrap();

        // Latin-1-representable accented text: succeeds even with no declared charset.
        let latin1_object = InMemDicomObject::from_element_iter([DataElement::new(
            Tag(0x0010, 0x0010),
            VR::PN,
            PrimitiveValue::from("M\u{fc}ller^J\u{f6}rg".to_owned()), // "Müller^Jörg"
        )]);
        let mut buf = Vec::new();
        latin1_object
            .write_dataset_with_ts(&mut buf, ts)
            .expect("Latin-1-representable text should write even without a declared charset");

        // Text outside Latin-1 (CJK): fails with no declared charset...
        let cjk_object = InMemDicomObject::from_element_iter([DataElement::new(
            Tag(0x0010, 0x0010),
            VR::PN,
            PrimitiveValue::from("\u{5f20}^\u{4f1f}".to_owned()), // "张^伟"
        )]);
        let mut buf2 = Vec::new();
        assert!(
            cjk_object.write_dataset_with_ts(&mut buf2, ts).is_err(),
            "expected non-Latin-1 text to fail to encode without a declared charset - if this \
             now succeeds, the default codec's behavior changed and scp.rs's \
             SpecificCharacterSet injection workaround may no longer be necessary"
        );

        // ...but succeeds once a UTF-8 charset is declared, matching scp.rs's workaround.
        let cjk_with_charset = InMemDicomObject::from_element_iter([
            DataElement::new(
                Tag(0x0008, 0x0005),
                VR::CS,
                PrimitiveValue::from("ISO_IR 192".to_owned()),
            ),
            DataElement::new(
                Tag(0x0010, 0x0010),
                VR::PN,
                PrimitiveValue::from("\u{5f20}^\u{4f1f}".to_owned()),
            ),
        ]);
        let mut buf3 = Vec::new();
        cjk_with_charset
            .write_dataset_with_ts(&mut buf3, ts)
            .expect("declared SpecificCharacterSet=ISO_IR 192 should make CJK text writable");
    }
}
