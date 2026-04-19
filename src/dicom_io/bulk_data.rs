use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use base64::Engine as _;
use dicom_core::value::{PixelFragmentSequence, Value as DicomValue};
use dicom_core::{PrimitiveValue, Tag, VR};
use dicom_dictionary_std::{tags, uids};
use serde_json::{Map as JsonMap, Value as JsonValue};

use super::common::invalid_json_value;
use super::types::{
    BulkRepresentation, DicomJsonBulkDataMode, DicomJsonError, DicomJsonWriteOptions,
    ElementLocation, ParsedHeader, TransferSyntaxInfo, ITEM_DELIMITATION_TAG, ITEM_TAG,
    SEQUENCE_DELIMITATION_TAG,
};

const INLINE_BINARY_URI_THRESHOLD: usize = 32;

pub(super) fn bulk_json_value<I, P>(
    tag: Tag,
    vr: VR,
    value: &DicomValue<I, P>,
    options: DicomJsonWriteOptions<'_>,
) -> Result<JsonValue, DicomJsonError>
where
    P: AsRef<[u8]>,
{
    let mut object = JsonMap::new();
    match bulk_representation(tag, vr, value, options)? {
        BulkRepresentation::Uri(uri) => {
            object.insert("BulkDataURI".to_owned(), JsonValue::String(uri));
        }
        BulkRepresentation::InlineBinary(data) => {
            object.insert("InlineBinary".to_owned(), JsonValue::String(data));
        }
    }
    Ok(JsonValue::Object(object))
}

pub(super) fn bulk_representation<I, P>(
    tag: Tag,
    vr: VR,
    value: &DicomValue<I, P>,
    options: DicomJsonWriteOptions<'_>,
) -> Result<BulkRepresentation, DicomJsonError>
where
    P: AsRef<[u8]>,
{
    if options.bulk_data_mode == DicomJsonBulkDataMode::Uri {
        if let Some(source) = options.bulk_data_source {
            let location = locate_root_element_value(source, tag)?
                .ok_or(DicomJsonError::BulkDataNotFound(tag))?;
            if location.length <= INLINE_BINARY_URI_THRESHOLD {
                let raw_bytes = &source[location.offset..location.offset + location.length];
                return Ok(BulkRepresentation::InlineBinary(
                    BASE64_STANDARD.encode(raw_bytes),
                ));
            }
            let uri = match options.bulk_data_uri_base {
                Some(base) => format!(
                    "{}?offset={}&length={}",
                    base, location.offset, location.length
                ),
                None => format!("?offset={}&length={}", location.offset, location.length),
            };
            return Ok(BulkRepresentation::Uri(uri));
        }
    }

    let raw_bytes = raw_value_bytes(tag, vr, value, options.bulk_data_source)?;
    Ok(BulkRepresentation::InlineBinary(
        BASE64_STANDARD.encode(raw_bytes),
    ))
}

pub(super) fn raw_value_bytes<I, P>(
    tag: Tag,
    vr: VR,
    value: &DicomValue<I, P>,
    bulk_data_source: Option<&[u8]>,
) -> Result<Vec<u8>, DicomJsonError>
where
    P: AsRef<[u8]>,
{
    if let Some(source) = bulk_data_source {
        if let Some(location) = locate_root_element_value(source, tag)? {
            return Ok(source[location.offset..location.offset + location.length].to_vec());
        }
    }

    match value {
        DicomValue::Primitive(primitive) => Ok(primitive.to_bytes().into_owned()),
        DicomValue::PixelSequence(pixel_sequence) => Ok(pixel_sequence_to_bytes(pixel_sequence)),
        DicomValue::Sequence(_) => Err(DicomJsonError::UnsupportedBulkDataVr { tag, vr }),
    }
}

pub(super) fn resolve_flat_bulk_bytes(
    keyword: &str,
    json: &JsonValue,
    bulk_data_source: Option<&[u8]>,
) -> Result<Option<Vec<u8>>, DicomJsonError> {
    let JsonValue::Object(object) = json else {
        return Ok(None);
    };

    if let Some(JsonValue::String(encoded)) = object.get("InlineBinary") {
        let bytes = BASE64_STANDARD
            .decode(encoded)
            .map_err(|_| invalid_json_value(keyword, "InlineBinary is not valid base64"))?;
        return Ok(Some(bytes));
    }

    if let Some(JsonValue::String(uri)) = object.get("BulkDataURI") {
        let source = bulk_data_source
            .ok_or_else(|| DicomJsonError::MissingBulkDataSource(uri.clone()))?;
        return Ok(Some(resolve_bulk_data_uri(uri, source)?));
    }

    Ok(None)
}

pub(super) fn resolve_standard_bulk_bytes(
    tag: Tag,
    vr: VR,
    object: &JsonMap<String, JsonValue>,
    bulk_data_source: Option<&[u8]>,
) -> Result<Option<Vec<u8>>, DicomJsonError> {
    if let Some(JsonValue::String(encoded)) = object.get("InlineBinary") {
        let bytes = BASE64_STANDARD.decode(encoded).map_err(|_| {
            DicomJsonError::InvalidStandardElement {
                tag: super::common::tag_key(tag),
                message: "InlineBinary is not valid base64".to_owned(),
            }
        })?;
        return Ok(Some(bytes));
    }

    if let Some(JsonValue::String(uri)) = object.get("BulkDataURI") {
        let source = bulk_data_source
            .ok_or_else(|| DicomJsonError::MissingBulkDataSource(uri.clone()))?;
        return Ok(Some(resolve_bulk_data_uri(uri, source)?));
    }

    if is_bulk_vr(vr) && tag == tags::PIXEL_DATA {
        return Ok(None);
    }

    Ok(None)
}

pub(super) fn raw_bytes_to_dicom_value(
    tag: Tag,
    vr: VR,
    bytes: &[u8],
    transfer_syntax_uid: &str,
) -> Result<DicomValue<dicom_object::InMemDicomObject>, DicomJsonError> {
    if tag == tags::PIXEL_DATA && is_encapsulated_transfer_syntax(transfer_syntax_uid) {
        return pixel_sequence_from_bytes(bytes);
    }

    let little_endian = is_little_endian_transfer_syntax(transfer_syntax_uid)?;

    let primitive = match vr {
        VR::OB | VR::UN => PrimitiveValue::U8(bytes.to_vec().into()),
        VR::OW => PrimitiveValue::U16(decode_u16_values(tag, vr, bytes, little_endian)?.into()),
        VR::OF => PrimitiveValue::F32(decode_f32_values(tag, vr, bytes, little_endian)?.into()),
        VR::OD => PrimitiveValue::F64(decode_f64_values(tag, vr, bytes, little_endian)?.into()),
        VR::OL => PrimitiveValue::U32(decode_u32_values(tag, vr, bytes, little_endian)?.into()),
        VR::OV => PrimitiveValue::U64(decode_u64_values(tag, vr, bytes, little_endian)?.into()),
        _ => return Err(DicomJsonError::UnsupportedBulkDataVr { tag, vr }),
    };

    Ok(primitive.into())
}

pub(super) fn is_bulk_value<I, P>(tag: Tag, vr: VR, value: &DicomValue<I, P>) -> bool {
    matches!(value, DicomValue::PixelSequence(_))
        || (primitive_is_bulk(vr) && tag != tags::WAVEFORM_DATA)
}

pub(super) fn needs_custom_standard_bulk<I, P>(
    tag: Tag,
    vr: VR,
    value: &DicomValue<I, P>,
) -> bool {
    matches!(value, DicomValue::PixelSequence(_)) || (primitive_is_bulk(vr) && tag == tags::PIXEL_DATA)
}

pub(super) fn primitive_is_bulk(vr: VR) -> bool {
    matches!(vr, VR::OB | VR::OD | VR::OF | VR::OL | VR::OV | VR::OW | VR::UN)
}

pub(super) fn is_bulk_vr(vr: VR) -> bool {
    primitive_is_bulk(vr)
}

pub(super) fn resolve_bulk_data_uri(
    uri: &str,
    source: &[u8],
) -> Result<Vec<u8>, DicomJsonError> {
    let (offset, length) = parse_bulk_data_uri(uri)?;
    let end = offset.saturating_add(length);
    if end > source.len() {
        return Err(DicomJsonError::BulkDataOutOfRange {
            uri: uri.to_owned(),
            length: source.len(),
        });
    }

    Ok(source[offset..end].to_vec())
}

fn parse_bulk_data_uri(uri: &str) -> Result<(usize, usize), DicomJsonError> {
    let query = uri
        .split_once('?')
        .map(|(_, query)| query)
        .unwrap_or_else(|| uri.trim_start_matches('?'));

    let mut offset = None;
    let mut length = None;

    for part in query.split('&') {
        let Some((key, value)) = part.split_once('=') else {
            continue;
        };

        match key {
            "offset" => offset = value.parse::<usize>().ok(),
            "length" => length = value.parse::<usize>().ok(),
            _ => {}
        }
    }

    match (offset, length) {
        (Some(offset), Some(length)) => Ok((offset, length)),
        _ => Err(DicomJsonError::InvalidBulkDataUri(uri.to_owned())),
    }
}

fn locate_root_element_value(
    source: &[u8],
    target: Tag,
) -> Result<Option<ElementLocation>, DicomJsonError> {
    let file_start = if source.len() >= 132 && &source[128..132] == b"DICM" {
        132
    } else {
        0
    };

    let mut position = file_start;
    let mut transfer_syntax_uid = uids::EXPLICIT_VR_LITTLE_ENDIAN.to_owned();

    while position + 8 <= source.len() {
        let header = parse_element_header(source, position, true, true)?;
        if header.tag.group() != 0x0002 {
            break;
        }

        let value_offset = position + header.header_length;
        let Some(value_length) = header.length else {
            return Err(DicomJsonError::InvalidBulkDataUri(
                "file meta group contains undefined-length element".to_owned(),
            ));
        };

        if header.tag == target {
            return Ok(Some(ElementLocation {
                offset: value_offset,
                length: value_length,
            }));
        }

        if header.tag == tags::TRANSFER_SYNTAX_UID {
            transfer_syntax_uid = decode_dicom_text(&source[value_offset..value_offset + value_length]);
        }

        position = value_offset + value_length;
    }

    let syntax = transfer_syntax_from_uid(transfer_syntax_uid.as_str())?;
    locate_tag_in_dataset(source, position, target, syntax.explicit_vr, syntax.little_endian)
}

fn locate_tag_in_dataset(
    source: &[u8],
    mut position: usize,
    target: Tag,
    explicit_vr: bool,
    little_endian: bool,
) -> Result<Option<ElementLocation>, DicomJsonError> {
    while position + 8 <= source.len() {
        let header = parse_element_header(source, position, explicit_vr, little_endian)?;
        let value_offset = position + header.header_length;

        if header.tag == target {
            let length = if let Some(length) = header.length {
                length
            } else {
                skip_undefined_length_value(source, value_offset, explicit_vr, little_endian)?
                    .saturating_sub(value_offset)
            };

            return Ok(Some(ElementLocation {
                offset: value_offset,
                length,
            }));
        }

        position = if let Some(length) = header.length {
            value_offset + length
        } else {
            skip_undefined_length_value(source, value_offset, explicit_vr, little_endian)? + 8
        };
    }

    Ok(None)
}

fn skip_undefined_length_value(
    source: &[u8],
    mut position: usize,
    explicit_vr: bool,
    little_endian: bool,
) -> Result<usize, DicomJsonError> {
    while position + 8 <= source.len() {
        let tag = read_tag(source, position, little_endian)?;
        if tag == SEQUENCE_DELIMITATION_TAG || tag == ITEM_DELIMITATION_TAG {
            return Ok(position);
        }

        if tag == ITEM_TAG {
            let item_length = read_u32(source, position + 4, little_endian)? as usize;
            position += 8;
            position = if item_length == u32::MAX as usize {
                skip_undefined_length_value(source, position, explicit_vr, little_endian)? + 8
            } else {
                position + item_length
            };
            continue;
        }

        let header = parse_element_header(source, position, explicit_vr, little_endian)?;
        let value_offset = position + header.header_length;
        position = if let Some(length) = header.length {
            value_offset + length
        } else {
            skip_undefined_length_value(source, value_offset, explicit_vr, little_endian)? + 8
        };
    }

    Err(DicomJsonError::InvalidBulkDataUri(
        "unterminated undefined-length value".to_owned(),
    ))
}

fn parse_element_header(
    source: &[u8],
    position: usize,
    explicit_vr: bool,
    little_endian: bool,
) -> Result<ParsedHeader, DicomJsonError> {
    let tag = read_tag(source, position, little_endian)?;

    if explicit_vr {
        if position + 8 > source.len() {
            return Err(DicomJsonError::InvalidBulkDataUri(
                "truncated explicit-VR element header".to_owned(),
            ));
        }

        let vr_bytes = [source[position + 4], source[position + 5]];
        let vr = VR::from_binary(vr_bytes).unwrap_or(VR::UN);
        if matches!(
            vr,
            VR::OB
                | VR::OD
                | VR::OF
                | VR::OL
                | VR::OV
                | VR::OW
                | VR::SQ
                | VR::UC
                | VR::UR
                | VR::UT
                | VR::UN
        ) {
            if position + 12 > source.len() {
                return Err(DicomJsonError::InvalidBulkDataUri(
                    "truncated extended explicit-VR element header".to_owned(),
                ));
            }

            let length = read_u32(source, position + 8, little_endian)?;
            Ok(ParsedHeader {
                tag,
                header_length: 12,
                length: if length == u32::MAX {
                    None
                } else {
                    Some(length as usize)
                },
            })
        } else {
            let length = read_u16(source, position + 6, little_endian)? as usize;
            Ok(ParsedHeader {
                tag,
                header_length: 8,
                length: Some(length),
            })
        }
    } else {
        if position + 8 > source.len() {
            return Err(DicomJsonError::InvalidBulkDataUri(
                "truncated implicit-VR element header".to_owned(),
            ));
        }

        let length = read_u32(source, position + 4, little_endian)?;
        Ok(ParsedHeader {
            tag,
            header_length: 8,
            length: if length == u32::MAX {
                None
            } else {
                Some(length as usize)
            },
        })
    }
}

fn read_tag(source: &[u8], position: usize, little_endian: bool) -> Result<Tag, DicomJsonError> {
    Ok(Tag(
        read_u16(source, position, little_endian)?,
        read_u16(source, position + 2, little_endian)?,
    ))
}

fn read_u16(source: &[u8], position: usize, little_endian: bool) -> Result<u16, DicomJsonError> {
    let bytes = source
        .get(position..position + 2)
        .ok_or_else(|| DicomJsonError::InvalidBulkDataUri("truncated 16-bit value".to_owned()))?;

    Ok(if little_endian {
        u16::from_le_bytes([bytes[0], bytes[1]])
    } else {
        u16::from_be_bytes([bytes[0], bytes[1]])
    })
}

fn read_u32(source: &[u8], position: usize, little_endian: bool) -> Result<u32, DicomJsonError> {
    let bytes = source
        .get(position..position + 4)
        .ok_or_else(|| DicomJsonError::InvalidBulkDataUri("truncated 32-bit value".to_owned()))?;

    Ok(if little_endian {
        u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
    } else {
        u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
    })
}

fn read_u16_le(source: &[u8], position: usize) -> Result<u16, DicomJsonError> {
    let bytes = source
        .get(position..position + 2)
        .ok_or_else(|| DicomJsonError::InvalidBulkDataUri("truncated 16-bit value".to_owned()))?;
    Ok(u16::from_le_bytes([bytes[0], bytes[1]]))
}

fn read_u32_le(source: &[u8], position: usize) -> Result<u32, DicomJsonError> {
    let bytes = source
        .get(position..position + 4)
        .ok_or_else(|| DicomJsonError::InvalidBulkDataUri("truncated 32-bit value".to_owned()))?;
    Ok(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
}

fn decode_dicom_text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes)
        .trim_matches(char::from(0))
        .trim_end()
        .to_owned()
}

fn transfer_syntax_from_uid(uid: &str) -> Result<TransferSyntaxInfo, DicomJsonError> {
    match uid {
        uids::IMPLICIT_VR_LITTLE_ENDIAN => Ok(TransferSyntaxInfo {
            explicit_vr: false,
            little_endian: true,
        }),
        uids::EXPLICIT_VR_LITTLE_ENDIAN
        | "1.2.840.10008.1.2.1.99"
        | "1.2.840.10008.1.2.4.90"
        | "1.2.840.10008.1.2.4.91"
        | "1.2.840.10008.1.2.5" => Ok(TransferSyntaxInfo {
            explicit_vr: true,
            little_endian: true,
        }),
        "1.2.840.10008.1.2.2" => Ok(TransferSyntaxInfo {
            explicit_vr: true,
            little_endian: false,
        }),
        other if other.starts_with("1.2.840.10008.1.2.4.") => Ok(TransferSyntaxInfo {
            explicit_vr: true,
            little_endian: true,
        }),
        other => Err(DicomJsonError::UnsupportedTransferSyntax(other.to_owned())),
    }
}

fn is_little_endian_transfer_syntax(uid: &str) -> Result<bool, DicomJsonError> {
    Ok(transfer_syntax_from_uid(uid)?.little_endian)
}

fn is_encapsulated_transfer_syntax(uid: &str) -> bool {
    !matches!(
        uid,
        uids::IMPLICIT_VR_LITTLE_ENDIAN
            | uids::EXPLICIT_VR_LITTLE_ENDIAN
            | "1.2.840.10008.1.2.2"
            | "1.2.840.10008.1.2.1.99"
    )
}

fn pixel_sequence_from_bytes(
    bytes: &[u8],
) -> Result<DicomValue<dicom_object::InMemDicomObject>, DicomJsonError> {
    let mut cursor = 0usize;
    let mut offset_table = Vec::new();
    let mut fragments = Vec::new();
    let mut first_item = true;

    while cursor + 8 <= bytes.len() {
        let tag = Tag(read_u16_le(bytes, cursor)?, read_u16_le(bytes, cursor + 2)?);
        let length = read_u32_le(bytes, cursor + 4)? as usize;
        cursor += 8;

        if tag == SEQUENCE_DELIMITATION_TAG {
            break;
        }

        if tag != ITEM_TAG {
            return Err(DicomJsonError::InvalidBulkDataUri(
                "encapsulated pixel data does not start with an item tag".to_owned(),
            ));
        }

        if cursor + length > bytes.len() {
            return Err(DicomJsonError::InvalidBulkDataUri(
                "encapsulated pixel data item exceeds available bytes".to_owned(),
            ));
        }

        let item_bytes = &bytes[cursor..cursor + length];
        if first_item {
            if item_bytes.len() % 4 != 0 {
                return Err(DicomJsonError::InvalidBulkDataUri(
                    "basic offset table length is not divisible by 4".to_owned(),
                ));
            }

            for chunk in item_bytes.chunks_exact(4) {
                offset_table.push(u32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]));
            }
            first_item = false;
        } else {
            fragments.push(item_bytes.to_vec());
        }

        cursor += length;
    }

    Ok(DicomValue::<dicom_object::InMemDicomObject>::from(
        PixelFragmentSequence::new(offset_table, fragments),
    ))
}

fn pixel_sequence_to_bytes<P>(pixel_sequence: &PixelFragmentSequence<P>) -> Vec<u8>
where
    P: AsRef<[u8]>,
{
    let mut bytes = Vec::new();
    let offset_table = pixel_sequence.offset_table();

    bytes.extend_from_slice(&ITEM_TAG.group().to_le_bytes());
    bytes.extend_from_slice(&ITEM_TAG.element().to_le_bytes());
    bytes.extend_from_slice(&((offset_table.len() * 4) as u32).to_le_bytes());
    for offset in offset_table {
        bytes.extend_from_slice(&offset.to_le_bytes());
    }

    for fragment in pixel_sequence.fragments() {
        bytes.extend_from_slice(&ITEM_TAG.group().to_le_bytes());
        bytes.extend_from_slice(&ITEM_TAG.element().to_le_bytes());
        bytes.extend_from_slice(&(fragment.as_ref().len() as u32).to_le_bytes());
        bytes.extend_from_slice(fragment.as_ref());
    }

    bytes.extend_from_slice(&SEQUENCE_DELIMITATION_TAG.group().to_le_bytes());
    bytes.extend_from_slice(&SEQUENCE_DELIMITATION_TAG.element().to_le_bytes());
    bytes.extend_from_slice(&0_u32.to_le_bytes());
    bytes
}

fn decode_u16_values(
    tag: Tag,
    vr: VR,
    bytes: &[u8],
    little_endian: bool,
) -> Result<Vec<u16>, DicomJsonError> {
    decode_fixed_width_values(bytes, 2, |chunk| {
        if little_endian {
            u16::from_le_bytes([chunk[0], chunk[1]])
        } else {
            u16::from_be_bytes([chunk[0], chunk[1]])
        }
    })
    .map_err(|_| DicomJsonError::InvalidBulkDataLength {
        tag,
        vr,
        length: bytes.len(),
    })
}

fn decode_u32_values(
    tag: Tag,
    vr: VR,
    bytes: &[u8],
    little_endian: bool,
) -> Result<Vec<u32>, DicomJsonError> {
    decode_fixed_width_values(bytes, 4, |chunk| {
        if little_endian {
            u32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]])
        } else {
            u32::from_be_bytes([chunk[0], chunk[1], chunk[2], chunk[3]])
        }
    })
    .map_err(|_| DicomJsonError::InvalidBulkDataLength {
        tag,
        vr,
        length: bytes.len(),
    })
}

fn decode_u64_values(
    tag: Tag,
    vr: VR,
    bytes: &[u8],
    little_endian: bool,
) -> Result<Vec<u64>, DicomJsonError> {
    decode_fixed_width_values(bytes, 8, |chunk| {
        if little_endian {
            u64::from_le_bytes([
                chunk[0], chunk[1], chunk[2], chunk[3], chunk[4], chunk[5], chunk[6], chunk[7],
            ])
        } else {
            u64::from_be_bytes([
                chunk[0], chunk[1], chunk[2], chunk[3], chunk[4], chunk[5], chunk[6], chunk[7],
            ])
        }
    })
    .map_err(|_| DicomJsonError::InvalidBulkDataLength {
        tag,
        vr,
        length: bytes.len(),
    })
}

fn decode_f32_values(
    tag: Tag,
    vr: VR,
    bytes: &[u8],
    little_endian: bool,
) -> Result<Vec<f32>, DicomJsonError> {
    decode_fixed_width_values(bytes, 4, |chunk| {
        let bits = if little_endian {
            u32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]])
        } else {
            u32::from_be_bytes([chunk[0], chunk[1], chunk[2], chunk[3]])
        };
        f32::from_bits(bits)
    })
    .map_err(|_| DicomJsonError::InvalidBulkDataLength {
        tag,
        vr,
        length: bytes.len(),
    })
}

fn decode_f64_values(
    tag: Tag,
    vr: VR,
    bytes: &[u8],
    little_endian: bool,
) -> Result<Vec<f64>, DicomJsonError> {
    decode_fixed_width_values(bytes, 8, |chunk| {
        let bits = if little_endian {
            u64::from_le_bytes([
                chunk[0], chunk[1], chunk[2], chunk[3], chunk[4], chunk[5], chunk[6], chunk[7],
            ])
        } else {
            u64::from_be_bytes([
                chunk[0], chunk[1], chunk[2], chunk[3], chunk[4], chunk[5], chunk[6], chunk[7],
            ])
        };
        f64::from_bits(bits)
    })
    .map_err(|_| DicomJsonError::InvalidBulkDataLength {
        tag,
        vr,
        length: bytes.len(),
    })
}

fn decode_fixed_width_values<T>(
    bytes: &[u8],
    width: usize,
    convert: impl Fn(&[u8]) -> T,
) -> Result<Vec<T>, ()> {
    if bytes.len() % width != 0 {
        return Err(());
    }

    Ok(bytes.chunks_exact(width).map(convert).collect())
}