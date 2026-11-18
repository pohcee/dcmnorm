//! A plain, binding-neutral tree of a DICOM data set (#6): what the Java, Node and Python
//! bindings turn into their own typed, read-only `DicomDataset` objects, and what they hand back
//! for writing. Values are already typed (strings, integers, floats, tags, bytes, sequences) and
//! large bulk values can stay in the source as [`BulkDataRef`]s (offset/length), so a binding
//! needs no DICOM knowledge to build its objects and nothing native is held on to.
//!
//! [`encode_document`] / [`decode_document`] are a compact binary form of the same tree, for a
//! binding (JNI) where building every object through the foreign interface would cost one call
//! per value: one byte array crosses instead and the binding decodes it in its own language.

use std::io::{Read, Seek, SeekFrom};

use dcmnorm_core::value::{PixelFragmentSequence, Value as DicomValue};
use dcmnorm_core::{DataElement, Length, PrimitiveValue, Tag, VR};
use dcmnorm_object::{DeferredValueLocation, DeferredValues, InMemDicomObject};

use super::api_error::ApiError;

/// A data set as plain values: elements in ascending tag order.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct DatasetTree {
    pub elements: Vec<TreeElement>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct TreeElement {
    /// `(group << 16) | element`.
    pub tag: u32,
    pub vr: VR,
    pub value: TreeValue,
}

/// An element's value, by what it holds rather than by VR: text VRs (including DS, IS, PN, the
/// date/time VRs and UI) are [`TreeValue::Strings`], one per value and with the padding removed;
/// binary integer VRs (SS, US, SL, UL, SV, UV) are [`TreeValue::Ints`]; FL/FD are
/// [`TreeValue::Doubles`]; AT is [`TreeValue::Tags`]; the other binary VRs (OB, OW, OF, OD, OL,
/// OV, UN) are [`TreeValue::Bytes`] (little-endian, as in a little-endian file) unless the value
/// was left in the source ([`TreeValue::BulkRef`]).
#[derive(Clone, Debug, PartialEq)]
pub enum TreeValue {
    Empty,
    Strings(Vec<String>),
    /// UV values above `i64::MAX` wrap (they are 64-bit unsigned in the file).
    Ints(Vec<i64>),
    Doubles(Vec<f64>),
    Tags(Vec<u32>),
    Bytes(Vec<u8>),
    BulkRef(BulkDataRef),
    /// Encapsulated pixel data that was read: the Basic Offset Table and each fragment.
    Fragments {
        offset_table: Vec<u32>,
        fragments: Vec<Vec<u8>>,
    },
    Sequence(Vec<DatasetTree>),
}

/// A value left in the source instead of being read: its absolute byte offset there and its
/// length. `length` is `None` for encapsulated pixel data a read stopped at (until-pixel-data),
/// whose end is only found by walking its fragments. For encapsulated pixel data the range starts
/// at the first item header (the Basic Offset Table item), as a DICOM JSON BulkDataURI does.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BulkDataRef {
    pub offset: u64,
    pub length: Option<u64>,
}

impl From<DeferredValueLocation> for BulkDataRef {
    fn from(location: DeferredValueLocation) -> Self {
        Self {
            offset: location.offset,
            length: location.defined_length(),
        }
    }
}

impl From<BulkDataRef> for DeferredValueLocation {
    fn from(reference: BulkDataRef) -> Self {
        Self {
            offset: reference.offset,
            length: reference
                .length
                .unwrap_or(DeferredValueLocation::UNDEFINED_LENGTH),
        }
    }
}

/// A whole DICOM object: the transfer syntax it was read in, its File Meta Information (group
/// 0002, empty for a data set without one) and the data set.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct DatasetDocument {
    pub transfer_syntax_uid: String,
    pub file_meta: DatasetTree,
    pub dataset: DatasetTree,
}

impl DatasetTree {
    pub fn get(&self, tag: u32) -> Option<&TreeElement> {
        self.elements
            .binary_search_by_key(&tag, |element| element.tag)
            .ok()
            .map(|index| &self.elements[index])
    }

    fn sort(&mut self) {
        self.elements.sort_by_key(|element| element.tag);
    }
}

pub(crate) fn tag_u32(tag: Tag) -> u32 {
    (u32::from(tag.group()) << 16) | u32::from(tag.element())
}

pub(crate) fn tag_from_u32(tag: u32) -> Tag {
    Tag((tag >> 16) as u16, (tag & 0xFFFF) as u16)
}

const PIXEL_DATA: u32 = 0x7FE0_0010;

// -------------------------------------------------------------------------------------------
// object -> tree
// -------------------------------------------------------------------------------------------

/// The tree of `object`, taking each value `deferred` lists (a deferring read's skipped values)
/// as a [`BulkDataRef`] instead of from the object, where it is only a placeholder.
pub fn object_to_tree(object: &InMemDicomObject, deferred: Option<&DeferredValues>) -> DatasetTree {
    let elements = object
        .iter()
        .map(|element| {
            let tag = element.header().tag;
            let vr = element.vr();
            let value = match deferred.and_then(|scope| scope.value(tag)) {
                Some(location) => TreeValue::BulkRef(location.into()),
                None => value_to_tree(tag, vr, element.value(), deferred),
            };
            TreeElement {
                tag: tag_u32(tag),
                vr,
                value,
            }
        })
        .collect();
    let mut tree = DatasetTree { elements };
    tree.sort();
    tree
}

/// The File Meta Information of `object` as a tree (without the group length).
pub fn meta_to_tree(meta: &dcmnorm_object::FileMetaTable) -> DatasetTree {
    let elements = meta
        .to_element_iter()
        .filter(|element| element.header().tag != Tag(0x0002, 0x0000))
        .map(|element| TreeElement {
            tag: tag_u32(element.header().tag),
            vr: element.vr(),
            value: value_to_tree(element.header().tag, element.vr(), element.value(), None),
        })
        .collect();
    let mut tree = DatasetTree { elements };
    tree.sort();
    tree
}

/// The document of a file object (`deferred` as for [`object_to_tree`]).
pub fn file_object_to_document(
    object: &dcmnorm_object::DefaultDicomObject,
    deferred: Option<&DeferredValues>,
) -> DatasetDocument {
    DatasetDocument {
        transfer_syntax_uid: trim_uid(object.meta().transfer_syntax()).to_owned(),
        file_meta: meta_to_tree(object.meta()),
        dataset: object_to_tree(object, deferred),
    }
}

fn trim_uid(uid: &str) -> &str {
    uid.trim_end_matches(|c: char| c == '\0' || c.is_whitespace())
}

fn value_to_tree(
    tag: Tag,
    vr: VR,
    value: &DicomValue<InMemDicomObject, Vec<u8>>,
    deferred: Option<&DeferredValues>,
) -> TreeValue {
    match value {
        DicomValue::Sequence(sequence) => TreeValue::Sequence(
            sequence
                .items()
                .iter()
                .enumerate()
                .map(|(index, item)| {
                    object_to_tree(item, deferred.and_then(|scope| scope.item(tag, index)))
                })
                .collect(),
        ),
        DicomValue::PixelSequence(sequence) => TreeValue::Fragments {
            offset_table: sequence.offset_table().to_vec(),
            fragments: sequence.fragments().to_vec(),
        },
        DicomValue::Primitive(primitive) => primitive_to_tree(vr, primitive),
    }
}

fn is_binary_vr(vr: VR) -> bool {
    matches!(
        vr,
        VR::OB | VR::OW | VR::OF | VR::OD | VR::OL | VR::OV | VR::UN
    )
}

/// Text VRs whose leading spaces are insignificant too (PS3.5 6.2), besides the trailing padding
/// every text value has.
fn trims_leading_space(vr: VR) -> bool {
    matches!(
        vr,
        VR::AE
            | VR::AS
            | VR::CS
            | VR::DA
            | VR::DS
            | VR::DT
            | VR::IS
            | VR::TM
            | VR::UI
            | VR::LO
            | VR::SH
            | VR::PN
    )
}

fn clean_text(vr: VR, text: &str) -> String {
    let trimmed = text.trim_end_matches([' ', '\0']);
    if trims_leading_space(vr) {
        trimmed.trim_start_matches(' ').to_owned()
    } else {
        trimmed.to_owned()
    }
}

fn primitive_to_tree(vr: VR, primitive: &PrimitiveValue) -> TreeValue {
    if matches!(primitive, PrimitiveValue::Empty) {
        return TreeValue::Empty;
    }
    if is_binary_vr(vr) {
        return TreeValue::Bytes(primitive.to_bytes().into_owned());
    }
    match (vr, primitive) {
        (VR::AT, PrimitiveValue::Tags(tags)) => {
            TreeValue::Tags(tags.iter().map(|tag| tag_u32(*tag)).collect())
        }
        (VR::AT, PrimitiveValue::U16(words)) => TreeValue::Tags(
            words
                .chunks_exact(2)
                .map(|pair| (u32::from(pair[0]) << 16) | u32::from(pair[1]))
                .collect(),
        ),
        (VR::AT, PrimitiveValue::U32(values)) => TreeValue::Tags(values.to_vec()),
        (_, PrimitiveValue::I16(values)) => {
            TreeValue::Ints(values.iter().map(|v| i64::from(*v)).collect())
        }
        (_, PrimitiveValue::U16(values)) => {
            TreeValue::Ints(values.iter().map(|v| i64::from(*v)).collect())
        }
        (_, PrimitiveValue::I32(values)) => {
            TreeValue::Ints(values.iter().map(|v| i64::from(*v)).collect())
        }
        (_, PrimitiveValue::U32(values)) => {
            TreeValue::Ints(values.iter().map(|v| i64::from(*v)).collect())
        }
        (_, PrimitiveValue::I64(values)) => TreeValue::Ints(values.to_vec()),
        (_, PrimitiveValue::U64(values)) => {
            TreeValue::Ints(values.iter().map(|v| *v as i64).collect())
        }
        (_, PrimitiveValue::U8(values)) if !matches!(vr, VR::FL | VR::FD) => {
            TreeValue::Ints(values.iter().map(|v| i64::from(*v)).collect())
        }
        (_, PrimitiveValue::F32(values)) => {
            TreeValue::Doubles(values.iter().map(|v| f64::from(*v)).collect())
        }
        (_, PrimitiveValue::F64(values)) => TreeValue::Doubles(values.to_vec()),
        (VR::SS | VR::US | VR::SL | VR::UL | VR::SV | VR::UV, other) => {
            let strings = other.to_multi_str();
            match strings
                .iter()
                .map(|s| s.trim().parse::<i64>())
                .collect::<Result<Vec<_>, _>>()
            {
                Ok(ints) => TreeValue::Ints(ints),
                Err(_) => TreeValue::Bytes(other.to_bytes().into_owned()),
            }
        }
        (VR::FL | VR::FD, other) => {
            let strings = other.to_multi_str();
            match strings
                .iter()
                .map(|s| s.trim().parse::<f64>())
                .collect::<Result<Vec<_>, _>>()
            {
                Ok(doubles) => TreeValue::Doubles(doubles),
                Err(_) => TreeValue::Bytes(other.to_bytes().into_owned()),
            }
        }
        (_, other) => TreeValue::Strings(
            other
                .to_multi_str()
                .iter()
                .map(|text| clean_text(vr, text))
                .collect(),
        ),
    }
}

// -------------------------------------------------------------------------------------------
// tree -> object
// -------------------------------------------------------------------------------------------

/// Where a [`TreeValue::BulkRef`] is read from when a tree is turned back into an object: the
/// source the tree was read from (its file or bytes), or nothing (then a reference is an error).
pub enum BulkSource<'a> {
    None,
    Path(&'a std::path::Path),
    Bytes(&'a [u8]),
}

/// How [`tree_to_object`] treats a [`TreeValue::BulkRef`].
pub(crate) enum BulkRefHandling<'a, 's> {
    /// Read each from the source, decoded with the source transfer syntax's byte order.
    Resolve {
        source: &'a mut BulkReader<'s>,
        big_endian: bool,
        encapsulated: bool,
    },
    /// Keep each as an empty placeholder, recording it in the returned [`DeferredValues`] (for
    /// the deferred-aware JSON writer).
    Defer,
}

/// Reads byte ranges out of a [`BulkSource`] (a file is opened once, on first use).
pub(crate) struct BulkReader<'a> {
    source: &'a BulkSource<'a>,
    file: Option<std::io::BufReader<std::fs::File>>,
}

impl<'a> BulkReader<'a> {
    pub(crate) fn new(source: &'a BulkSource<'a>) -> Self {
        Self { source, file: None }
    }

    fn read_at(&mut self, offset: u64, buffer: &mut [u8]) -> Result<(), ApiError> {
        match self.source {
            BulkSource::None => Err(ApiError::invalid_argument(
                "the data set references bulk data in its source (read with bulk data left in the source), \
                 but no source is available to read it from: read it from a file or bytes, or pass the source",
            )),
            BulkSource::Bytes(bytes) => {
                let start = usize::try_from(offset).unwrap_or(usize::MAX);
                let end = start.checked_add(buffer.len()).filter(|end| *end <= bytes.len()).ok_or_else(|| {
                    ApiError::invalid_argument(format!(
                        "bulk data reference {offset}+{} is outside the source ({} bytes)",
                        buffer.len(),
                        bytes.len()
                    ))
                })?;
                buffer.copy_from_slice(&bytes[start..end]);
                Ok(())
            }
            BulkSource::Path(path) => {
                if self.file.is_none() {
                    let file = std::fs::File::open(path)
                        .map_err(|error| ApiError::io(format!("opening bulk data source {}: {error}", path.display())))?;
                    self.file = Some(std::io::BufReader::new(file));
                }
                let file = self.file.as_mut().expect("opened above");
                file.seek(SeekFrom::Start(offset))
                    .and_then(|_| file.read_exact(buffer))
                    .map_err(|error| ApiError::io(format!("reading bulk data at offset {offset}: {error}")))
            }
        }
    }

    fn read_range(&mut self, offset: u64, length: u64) -> Result<Vec<u8>, ApiError> {
        let length = usize::try_from(length)
            .map_err(|_| ApiError::invalid_argument("bulk data too large"))?;
        let mut buffer = vec![0u8; length];
        self.read_at(offset, &mut buffer)?;
        Ok(buffer)
    }

    /// Encapsulated pixel data starting at `offset` (the first item header): the Basic Offset
    /// Table and the fragments, up to the sequence delimiter - or, with `length`, to the end of
    /// that range.
    fn read_fragments(
        &mut self,
        offset: u64,
        length: Option<u64>,
    ) -> Result<(Vec<u32>, Vec<Vec<u8>>), ApiError> {
        let mut position = offset;
        let end = length.map(|length| offset + length);
        let mut items = Vec::new();
        loop {
            if end.is_some_and(|end| position >= end) {
                break;
            }
            let mut header = [0u8; 8];
            self.read_at(position, &mut header)?;
            position += 8;
            let group = u16::from_le_bytes([header[0], header[1]]);
            let element = u16::from_le_bytes([header[2], header[3]]);
            let item_length = u32::from_le_bytes([header[4], header[5], header[6], header[7]]);
            match (group, element) {
                (0xFFFE, 0xE0DD) => break,
                (0xFFFE, 0xE000) if item_length != u32::MAX => {
                    items.push(self.read_range(position, u64::from(item_length))?);
                    position += u64::from(item_length);
                }
                _ => {
                    return Err(ApiError::new(
                        super::api_error::ApiErrorKind::InvalidDicom,
                        format!("unexpected item ({group:04X},{element:04X}) in encapsulated pixel data at {position}"),
                    ))
                }
            }
        }
        let mut items = items.into_iter();
        let offset_table = items
            .next()
            .map(|table| {
                table
                    .chunks_exact(4)
                    .map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]]))
                    .collect()
            })
            .unwrap_or_default();
        Ok((offset_table, items.collect()))
    }
}

/// Turn `tree` back into an object (and, with [`BulkRefHandling::Defer`], the deferred
/// locations of its references). `top_level` is whether this is the outermost data set, where
/// only PixelData can be encapsulated.
pub(crate) fn tree_to_object(
    tree: &DatasetTree,
    handling: &mut BulkRefHandling<'_, '_>,
    top_level: bool,
) -> Result<(InMemDicomObject, DeferredValues), ApiError> {
    let mut elements = Vec::with_capacity(tree.elements.len());
    let mut deferred = DeferredValues::default();
    for element in &tree.elements {
        let tag = tag_from_u32(element.tag);
        let vr = element.vr;
        let value: DicomValue<InMemDicomObject, Vec<u8>> = match &element.value {
            TreeValue::Sequence(items) => {
                let mut objects = Vec::with_capacity(items.len());
                let mut scopes = Vec::with_capacity(items.len());
                for item in items {
                    let (object, scope) = tree_to_object(item, handling, false)?;
                    objects.push(object);
                    scopes.push(scope);
                }
                deferred.insert_items(tag, scopes);
                DicomValue::new_sequence(objects, Length::UNDEFINED)
            }
            TreeValue::Fragments {
                offset_table,
                fragments,
            } => DicomValue::PixelSequence(PixelFragmentSequence::new(
                offset_table.clone(),
                fragments.clone(),
            )),
            TreeValue::BulkRef(reference) => match handling {
                BulkRefHandling::Defer => {
                    deferred.insert_value(tag, (*reference).into());
                    if top_level && element.tag == PIXEL_DATA && reference.length.is_none() {
                        DicomValue::PixelSequence(PixelFragmentSequence::new(
                            Vec::<u32>::new(),
                            Vec::<Vec<u8>>::new(),
                        ))
                    } else {
                        DicomValue::Primitive(PrimitiveValue::U8(Vec::new().into()))
                    }
                }
                BulkRefHandling::Resolve {
                    source,
                    big_endian,
                    encapsulated,
                } => {
                    if top_level
                        && element.tag == PIXEL_DATA
                        && (*encapsulated || reference.length.is_none())
                    {
                        let (offset_table, fragments) =
                            source.read_fragments(reference.offset, reference.length)?;
                        DicomValue::PixelSequence(PixelFragmentSequence::new(
                            offset_table,
                            fragments,
                        ))
                    } else {
                        let length = reference.length.ok_or_else(|| {
                            ApiError::invalid_argument(format!(
                                "bulk data reference for {tag} has no length"
                            ))
                        })?;
                        let bytes = source.read_range(reference.offset, length)?;
                        DicomValue::Primitive(binary_to_primitive(vr, bytes, *big_endian))
                    }
                }
            },
            TreeValue::Bytes(bytes) => {
                DicomValue::Primitive(binary_to_primitive(vr, bytes.clone(), false))
            }
            other => DicomValue::Primitive(tree_value_to_primitive(tag, vr, other)?),
        };
        elements.push(DataElement::new(tag, vr, value));
    }
    Ok((InMemDicomObject::from_element_iter(elements), deferred))
}

/// Raw value bytes as the parser would have produced them: OW/OF/OD/OL/OV as typed values in the
/// byte order they were stored in, OB/UN (and any odd remainder) as bytes.
fn binary_to_primitive(vr: VR, bytes: Vec<u8>, big_endian: bool) -> PrimitiveValue {
    macro_rules! typed {
        ($variant:ident, $ty:ty, $width:expr) => {{
            if bytes.len() % $width != 0 {
                return PrimitiveValue::U8(bytes.into());
            }
            let values: Vec<$ty> = bytes
                .chunks_exact($width)
                .map(|chunk| {
                    let array: [u8; $width] = chunk.try_into().expect("exact chunk");
                    if big_endian {
                        <$ty>::from_be_bytes(array)
                    } else {
                        <$ty>::from_le_bytes(array)
                    }
                })
                .collect();
            PrimitiveValue::$variant(values.into())
        }};
    }
    match vr {
        VR::OW => typed!(U16, u16, 2),
        VR::OF => typed!(F32, f32, 4),
        VR::OD => typed!(F64, f64, 8),
        VR::OL => typed!(U32, u32, 4),
        VR::OV => typed!(U64, u64, 8),
        _ => PrimitiveValue::U8(bytes.into()),
    }
}

fn format_double(value: f64) -> String {
    if value.fract() == 0.0 && value.abs() < 1e15 {
        format!("{}", value as i64)
    } else {
        format!("{value}")
    }
}

fn tree_value_to_primitive(
    tag: Tag,
    vr: VR,
    value: &TreeValue,
) -> Result<PrimitiveValue, ApiError> {
    let out_of_range = |value: &dyn std::fmt::Display| {
        ApiError::invalid_argument(format!("value {value} is out of range for {tag} ({vr})"))
    };
    Ok(match value {
        TreeValue::Empty => PrimitiveValue::Empty,
        TreeValue::Strings(strings) => match vr {
            VR::SS | VR::US | VR::SL | VR::UL | VR::SV | VR::UV | VR::FL | VR::FD | VR::AT => {
                let ints = || -> Result<Vec<i64>, ApiError> {
                    strings
                        .iter()
                        .map(|s| {
                            s.trim().parse::<i64>().map_err(|_| {
                                ApiError::invalid_argument(format!(
                                    "'{s}' is not a number for {tag} ({vr})"
                                ))
                            })
                        })
                        .collect()
                };
                match vr {
                    VR::FL | VR::FD => {
                        let doubles = strings
                            .iter()
                            .map(|s| {
                                s.trim().parse::<f64>().map_err(|_| {
                                    ApiError::invalid_argument(format!(
                                        "'{s}' is not a number for {tag} ({vr})"
                                    ))
                                })
                            })
                            .collect::<Result<Vec<_>, _>>()?;
                        return tree_value_to_primitive(tag, vr, &TreeValue::Doubles(doubles));
                    }
                    VR::AT => {
                        let tags = strings
                            .iter()
                            .map(|s| {
                                let hex: String =
                                    s.chars().filter(|c| c.is_ascii_hexdigit()).collect();
                                u32::from_str_radix(&hex, 16).map_err(|_| {
                                    ApiError::invalid_argument(format!(
                                        "'{s}' is not a tag for {tag} (AT)"
                                    ))
                                })
                            })
                            .collect::<Result<Vec<_>, _>>()?;
                        return tree_value_to_primitive(tag, vr, &TreeValue::Tags(tags));
                    }
                    _ => return tree_value_to_primitive(tag, vr, &TreeValue::Ints(ints()?)),
                }
            }
            _ if strings.len() == 1 => PrimitiveValue::Str(strings[0].clone()),
            _ => PrimitiveValue::Strs(strings.iter().cloned().collect()),
        },
        TreeValue::Ints(ints) => match vr {
            VR::SS => PrimitiveValue::I16(
                ints.iter()
                    .map(|v| i16::try_from(*v).map_err(|_| out_of_range(v)))
                    .collect::<Result<_, _>>()?,
            ),
            VR::US => PrimitiveValue::U16(
                ints.iter()
                    .map(|v| u16::try_from(*v).map_err(|_| out_of_range(v)))
                    .collect::<Result<_, _>>()?,
            ),
            VR::SL => PrimitiveValue::I32(
                ints.iter()
                    .map(|v| i32::try_from(*v).map_err(|_| out_of_range(v)))
                    .collect::<Result<_, _>>()?,
            ),
            VR::UL => PrimitiveValue::U32(
                ints.iter()
                    .map(|v| u32::try_from(*v).map_err(|_| out_of_range(v)))
                    .collect::<Result<_, _>>()?,
            ),
            VR::SV => PrimitiveValue::I64(ints.iter().copied().collect()),
            VR::UV => PrimitiveValue::U64(ints.iter().map(|v| *v as u64).collect()),
            VR::FL | VR::FD => {
                return tree_value_to_primitive(
                    tag,
                    vr,
                    &TreeValue::Doubles(ints.iter().map(|v| *v as f64).collect()),
                )
            }
            VR::AT => {
                return tree_value_to_primitive(
                    tag,
                    vr,
                    &TreeValue::Tags(
                        ints.iter()
                            .map(|v| u32::try_from(*v).map_err(|_| out_of_range(v)))
                            .collect::<Result<_, _>>()?,
                    ),
                )
            }
            _ if is_binary_vr(vr) => PrimitiveValue::U8(
                ints.iter()
                    .map(|v| u8::try_from(*v).map_err(|_| out_of_range(v)))
                    .collect::<Result<_, _>>()?,
            ),
            _ => strings_primitive(ints.iter().map(|v| v.to_string()).collect()),
        },
        TreeValue::Doubles(doubles) => match vr {
            VR::FL => PrimitiveValue::F32(doubles.iter().map(|v| *v as f32).collect()),
            VR::FD => PrimitiveValue::F64(doubles.iter().copied().collect()),
            VR::SS | VR::US | VR::SL | VR::UL | VR::SV | VR::UV => {
                return tree_value_to_primitive(
                    tag,
                    vr,
                    &TreeValue::Ints(doubles.iter().map(|v| *v as i64).collect()),
                )
            }
            _ => strings_primitive(doubles.iter().map(|v| format_double(*v)).collect()),
        },
        TreeValue::Tags(tags) => match vr {
            VR::AT => PrimitiveValue::Tags(tags.iter().map(|tag| tag_from_u32(*tag)).collect()),
            _ => strings_primitive(tags.iter().map(|tag| format!("{tag:08X}")).collect()),
        },
        TreeValue::Bytes(bytes) => binary_to_primitive(vr, bytes.clone(), false),
        TreeValue::BulkRef(_) | TreeValue::Fragments { .. } | TreeValue::Sequence(_) => {
            return Err(ApiError::invalid_argument(format!(
                "{tag}: unexpected value kind for VR {vr}"
            )))
        }
    })
}

fn strings_primitive(strings: Vec<String>) -> PrimitiveValue {
    if strings.len() == 1 {
        PrimitiveValue::Str(strings.into_iter().next().expect("one"))
    } else {
        PrimitiveValue::Strs(strings.into())
    }
}

// -------------------------------------------------------------------------------------------
// Binary form
// -------------------------------------------------------------------------------------------
//
// Big-endian throughout (Java's ByteBuffer/DataOutputStream default):
//
//   document := "DCMT" u8:version(=1) string:transfer_syntax_uid tree:file_meta tree:dataset
//   tree     := u32:count element*
//   element  := u32:tag u8:vr[0] u8:vr[1] u8:kind payload
//   string   := u32:byte_length utf8
//   kind 0 Empty
//        1 Strings    u32:n string*
//        2 Ints       u32:n i64*
//        3 Doubles    u32:n f64*
//        4 Tags       u32:n u32*
//        5 Bytes      u32:length bytes
//        6 BulkRef    u64:offset i64:length (-1 = undefined)
//        7 Fragments  u32:n u32* (offset table) u32:m (u32:length bytes)*
//        8 Sequence   u32:n tree*

pub const DOCUMENT_MAGIC: &[u8; 4] = b"DCMT";
pub const DOCUMENT_VERSION: u8 = 1;

pub fn encode_document(document: &DatasetDocument) -> Vec<u8> {
    let mut out = Vec::with_capacity(4096);
    out.extend_from_slice(DOCUMENT_MAGIC);
    out.push(DOCUMENT_VERSION);
    put_string(&mut out, &document.transfer_syntax_uid);
    encode_tree(&mut out, &document.file_meta);
    encode_tree(&mut out, &document.dataset);
    out
}

fn put_u32(out: &mut Vec<u8>, value: u32) {
    out.extend_from_slice(&value.to_be_bytes());
}

fn put_len(out: &mut Vec<u8>, length: usize) {
    put_u32(
        out,
        u32::try_from(length).expect("value too large for the binary dataset form"),
    );
}

fn put_string(out: &mut Vec<u8>, text: &str) {
    put_len(out, text.len());
    out.extend_from_slice(text.as_bytes());
}

fn encode_tree(out: &mut Vec<u8>, tree: &DatasetTree) {
    put_len(out, tree.elements.len());
    for element in &tree.elements {
        put_u32(out, element.tag);
        let vr = element.vr.to_string().as_bytes().to_owned();
        out.push(vr[0]);
        out.push(vr[1]);
        match &element.value {
            TreeValue::Empty => out.push(0),
            TreeValue::Strings(strings) => {
                out.push(1);
                put_len(out, strings.len());
                for text in strings {
                    put_string(out, text);
                }
            }
            TreeValue::Ints(ints) => {
                out.push(2);
                put_len(out, ints.len());
                for value in ints {
                    out.extend_from_slice(&value.to_be_bytes());
                }
            }
            TreeValue::Doubles(doubles) => {
                out.push(3);
                put_len(out, doubles.len());
                for value in doubles {
                    out.extend_from_slice(&value.to_be_bytes());
                }
            }
            TreeValue::Tags(tags) => {
                out.push(4);
                put_len(out, tags.len());
                for tag in tags {
                    put_u32(out, *tag);
                }
            }
            TreeValue::Bytes(bytes) => {
                out.push(5);
                put_len(out, bytes.len());
                out.extend_from_slice(bytes);
            }
            TreeValue::BulkRef(reference) => {
                out.push(6);
                out.extend_from_slice(&reference.offset.to_be_bytes());
                let length = reference.length.map(|length| length as i64).unwrap_or(-1);
                out.extend_from_slice(&length.to_be_bytes());
            }
            TreeValue::Fragments {
                offset_table,
                fragments,
            } => {
                out.push(7);
                put_len(out, offset_table.len());
                for entry in offset_table {
                    put_u32(out, *entry);
                }
                put_len(out, fragments.len());
                for fragment in fragments {
                    put_len(out, fragment.len());
                    out.extend_from_slice(fragment);
                }
            }
            TreeValue::Sequence(items) => {
                out.push(8);
                put_len(out, items.len());
                for item in items {
                    encode_tree(out, item);
                }
            }
        }
    }
}

struct Decoder<'a> {
    bytes: &'a [u8],
    position: usize,
}

impl<'a> Decoder<'a> {
    fn take(&mut self, length: usize) -> Result<&'a [u8], ApiError> {
        let end = self
            .position
            .checked_add(length)
            .filter(|end| *end <= self.bytes.len())
            .ok_or_else(|| ApiError::invalid_argument("truncated binary data set"))?;
        let slice = &self.bytes[self.position..end];
        self.position = end;
        Ok(slice)
    }

    fn u8(&mut self) -> Result<u8, ApiError> {
        Ok(self.take(1)?[0])
    }

    fn u32(&mut self) -> Result<u32, ApiError> {
        Ok(u32::from_be_bytes(
            self.take(4)?.try_into().expect("4 bytes"),
        ))
    }

    fn u64(&mut self) -> Result<u64, ApiError> {
        Ok(u64::from_be_bytes(
            self.take(8)?.try_into().expect("8 bytes"),
        ))
    }

    fn count(&mut self) -> Result<usize, ApiError> {
        let count = self.u32()? as usize;
        // every counted entry takes at least one byte, so a count beyond what is left is corrupt
        if count > self.bytes.len() - self.position {
            return Err(ApiError::invalid_argument(
                "corrupt binary data set (count)",
            ));
        }
        Ok(count)
    }

    fn string(&mut self) -> Result<String, ApiError> {
        let length = self.u32()? as usize;
        let bytes = self.take(length)?;
        String::from_utf8(bytes.to_vec())
            .map_err(|_| ApiError::invalid_argument("binary data set: invalid UTF-8"))
    }

    fn tree(&mut self, depth: usize) -> Result<DatasetTree, ApiError> {
        if depth > 64 {
            return Err(ApiError::invalid_argument(
                "binary data set nests too deeply",
            ));
        }
        let count = self.count()?;
        let mut elements = Vec::with_capacity(count);
        for _ in 0..count {
            let tag = self.u32()?;
            let vr_bytes = self.take(2)?;
            let vr_text = std::str::from_utf8(vr_bytes)
                .map_err(|_| ApiError::invalid_argument("binary data set: bad VR"))?;
            let vr: VR = vr_text.parse().map_err(|_| {
                ApiError::invalid_argument(format!("binary data set: unknown VR '{vr_text}'"))
            })?;
            let value = match self.u8()? {
                0 => TreeValue::Empty,
                1 => {
                    let n = self.count()?;
                    TreeValue::Strings((0..n).map(|_| self.string()).collect::<Result<_, _>>()?)
                }
                2 => {
                    let n = self.count()?;
                    TreeValue::Ints(
                        (0..n)
                            .map(|_| self.u64().map(|v| v as i64))
                            .collect::<Result<_, _>>()?,
                    )
                }
                3 => {
                    let n = self.count()?;
                    TreeValue::Doubles(
                        (0..n)
                            .map(|_| self.u64().map(f64::from_bits))
                            .collect::<Result<_, _>>()?,
                    )
                }
                4 => {
                    let n = self.count()?;
                    TreeValue::Tags((0..n).map(|_| self.u32()).collect::<Result<_, _>>()?)
                }
                5 => {
                    let length = self.u32()? as usize;
                    TreeValue::Bytes(self.take(length)?.to_vec())
                }
                6 => {
                    let offset = self.u64()?;
                    let length = self.u64()? as i64;
                    TreeValue::BulkRef(BulkDataRef {
                        offset,
                        length: (length >= 0).then_some(length as u64),
                    })
                }
                7 => {
                    let n = self.count()?;
                    let offset_table = (0..n).map(|_| self.u32()).collect::<Result<_, _>>()?;
                    let m = self.count()?;
                    let mut fragments = Vec::with_capacity(m);
                    for _ in 0..m {
                        let length = self.u32()? as usize;
                        fragments.push(self.take(length)?.to_vec());
                    }
                    TreeValue::Fragments {
                        offset_table,
                        fragments,
                    }
                }
                8 => {
                    let n = self.count()?;
                    TreeValue::Sequence(
                        (0..n)
                            .map(|_| self.tree(depth + 1))
                            .collect::<Result<_, _>>()?,
                    )
                }
                other => {
                    return Err(ApiError::invalid_argument(format!(
                        "binary data set: unknown value kind {other}"
                    )))
                }
            };
            elements.push(TreeElement { tag, vr, value });
        }
        let mut tree = DatasetTree { elements };
        tree.sort();
        Ok(tree)
    }
}

pub fn decode_document(bytes: &[u8]) -> Result<DatasetDocument, ApiError> {
    let mut decoder = Decoder { bytes, position: 0 };
    if decoder.take(4)? != DOCUMENT_MAGIC {
        return Err(ApiError::invalid_argument(
            "not a binary data set (bad magic)",
        ));
    }
    let version = decoder.u8()?;
    if version != DOCUMENT_VERSION {
        return Err(ApiError::invalid_argument(format!(
            "unsupported binary data set version {version}"
        )));
    }
    let transfer_syntax_uid = decoder.string()?;
    let file_meta = decoder.tree(0)?;
    let dataset = decoder.tree(0)?;
    Ok(DatasetDocument {
        transfer_syntax_uid,
        file_meta,
        dataset,
    })
}
