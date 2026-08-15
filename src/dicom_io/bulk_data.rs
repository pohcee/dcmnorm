use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use base64::Engine as _;
use dicom_core::value::{PixelFragmentSequence, Value as DicomValue};
use dicom_core::{PrimitiveValue, Tag, VR};
use dicom_dictionary_std::{tags, uids};
use serde_json::{Map as JsonMap, Value as JsonValue};
use std::fs;

use super::common::invalid_json_value;
use super::types::{
    BulkRepresentation, DicomJsonBulkDataMode, DicomJsonError, DicomJsonWriteOptions,
    ElementLocation, ParsedHeader, TransferSyntaxInfo, ITEM_DELIMITATION_TAG, ITEM_TAG,
    SEQUENCE_DELIMITATION_TAG,
};

pub(crate) const INLINE_BINARY_URI_THRESHOLD: usize = 32;

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
    let raw_bytes = raw_value_bytes(tag, vr, value, None)?;
    let expected_bytes_for_lookup = match value {
        DicomValue::PixelSequence(_) => None,
        _ => Some(raw_bytes.as_slice()),
    };

    if options.bulk_data_mode == DicomJsonBulkDataMode::Uri {
        if let Some(source) = options.bulk_data_source {
            if raw_bytes.len() <= INLINE_BINARY_URI_THRESHOLD {
                return Ok(BulkRepresentation::InlineBinary(
                    BASE64_STANDARD.encode(&raw_bytes),
                ));
            }

            // locate_element_value re-scans the raw file by hand (tag by tag, from the start of
            // the dataset) to find this element's byte range - it's a best-effort accelerator
            // that lets the client fetch large values on demand instead of inlining them here,
            // not something JSON generation should ever hard-fail on. Real-world files can
            // contain constructs this hand-rolled scanner doesn't model correctly (e.g. a
            // Siemens CSA header stored as a private sequence of VR=UN elements with undefined
            // length, which per DICOM PS3.5 6.2.2 requires switching to Implicit VR parsing for
            // its nested content - a rule this scanner doesn't implement) and desync partway
            // through the dataset - if that happens, fall back to inlining THIS element (we
            // already have its correctly decoded bytes via the proper parser, in raw_bytes)
            // rather than failing the entire study's metadata - a working (if occasionally
            // larger) response beats a broken one.
            //
            // Once the scan has failed once for this source, bulk_scan_failed (set by the
            // caller, shared across every element of the same file/write pass) short-circuits
            // every later attempt: every bulk element restarts its scan from the same beginning
            // position, so if one already desynced, all the rest are equally doomed - retrying
            // each independently turned one failure into every bulk element paying its own full
            // failed scan (a multi-second stall for files with several such elements, since
            // there's no early delimiter to find and the scan runs to EOF each time).
            let already_broken = options.bulk_scan_failed.is_some_and(|cell| cell.get());
            if !already_broken {
                // bulk_scan_cursor: elements are visited by the JSON writer in ascending tag
                // order, which for a conformant DICOM dataset is also ascending file-offset
                // order (top-level elements and, within a sequence, its items are both required
                // to appear in that order). So each successful lookup can hand the NEXT one a
                // hint to resume scanning from, instead of restarting at the dataset's start
                // every time. This matters most for a tag that repeats across many sibling
                // sequence items (e.g. a private per-item block) - without the hint, finding the
                // Nth occurrence means walking past all N-1 earlier non-matching ones from
                // scratch, for every one of the N elements: O(N^2) total instead of O(N).
                // Purely a speed hint, not a correctness assumption: if scanning from the hint
                // doesn't find the tag, locate_element_value retries once from the true start
                // before giving up, so an out-of-order visit can never cause a false miss.
                let resume_hint = options.bulk_scan_cursor.map(|cell| cell.get()).unwrap_or(0);
                match locate_element_value(source, tag, expected_bytes_for_lookup, resume_hint) {
                    Ok(Some(location)) => {
                        if let Some(cell) = options.bulk_scan_cursor {
                            let end = location.offset + location.length;
                            if end > cell.get() {
                                cell.set(end);
                            }
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
                    Ok(None) => {}
                    Err(_) => {
                        if let Some(cell) = options.bulk_scan_failed {
                            cell.set(true);
                        }
                    }
                }
            }

            return Ok(BulkRepresentation::InlineBinary(
                BASE64_STANDARD.encode(&raw_bytes),
            ));
        }
    }

    Ok(BulkRepresentation::InlineBinary(
        BASE64_STANDARD.encode(&raw_bytes),
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
        if let Some(location) = locate_element_value(source, tag, None, 0)? {
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
        return Ok(Some(resolve_bulk_data_uri_with_optional_source(
            uri,
            bulk_data_source,
        )?));
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
        return Ok(Some(resolve_bulk_data_uri_with_optional_source(
            uri,
            bulk_data_source,
        )?));
    }

    if is_bulk_vr(vr) && tag == tags::PIXEL_DATA {
        return Ok(None);
    }

    // No InlineBinary/BulkDataURI given for a non-PixelData bulk element (VM 0):
    // this is how a legitimately empty OB/OW/UN/etc. element round-trips, since
    // the writer omits both keys rather than emit an empty one of them (see
    // write_standard_json_value). PixelData is excluded above since an empty
    // image is never legitimate and more likely signals a caller bug.
    Ok(Some(Vec::new()))
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

pub(super) fn primitive_is_bulk(vr: VR) -> bool {
    matches!(
        vr,
        VR::OB | VR::OD | VR::OF | VR::OL | VR::OV | VR::OW | VR::UN
    )
}

pub(super) fn is_bulk_vr(vr: VR) -> bool {
    primitive_is_bulk(vr)
}

pub(super) fn resolve_bulk_data_uri(uri: &str, source: &[u8]) -> Result<Vec<u8>, DicomJsonError> {
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

fn resolve_bulk_data_uri_with_optional_source(
    uri: &str,
    bulk_data_source: Option<&[u8]>,
) -> Result<Vec<u8>, DicomJsonError> {
    // "file://" is a self-contained reference to an arbitrary file on disk and
    // never depends on bulk_data_source - check it first, regardless of whether
    // a source was given, so a document can mix "?offset=..&length=.." elements
    // (resolved against bulk_data_source) with "file://" elements (resolved
    // independently) in the same write.
    if let Some(source) = try_read_bulk_data_uri_source(uri)? {
        return resolve_bulk_data_uri(uri, source.as_slice());
    }

    if let Some(source) = bulk_data_source {
        return resolve_bulk_data_uri(uri, source);
    }

    Err(DicomJsonError::MissingBulkDataSource(uri.to_owned()))
}

fn try_read_bulk_data_uri_source(uri: &str) -> Result<Option<Vec<u8>>, DicomJsonError> {
    let Some(path) = file_path_from_bulk_data_uri(uri)? else {
        return Ok(None);
    };

    fs::read(path)
        .map(Some)
        .map_err(|_| DicomJsonError::MissingBulkDataSource(uri.to_owned()))
}

fn file_path_from_bulk_data_uri(uri: &str) -> Result<Option<String>, DicomJsonError> {
    if !uri.starts_with("file://") {
        return Ok(None);
    }

    let after_scheme = &uri[7..];
    let path_part = after_scheme
        .split_once('?')
        .map(|(path, _)| path)
        .unwrap_or(after_scheme);

    if path_part.is_empty() {
        return Err(DicomJsonError::InvalidBulkDataUri(uri.to_owned()));
    }

    let local_path = if let Some(path) = path_part.strip_prefix("localhost/") {
        format!("/{path}")
    } else {
        path_part.to_owned()
    };

    percent_decode_uri_path(local_path.as_str())
        .map(Some)
        .ok_or_else(|| DicomJsonError::InvalidBulkDataUri(uri.to_owned()))
}

fn percent_decode_uri_path(path: &str) -> Option<String> {
    let bytes = path.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;

    while index < bytes.len() {
        if bytes[index] == b'%' {
            if index + 2 >= bytes.len() {
                return None;
            }

            let hi = decode_hex_nibble(bytes[index + 1])?;
            let lo = decode_hex_nibble(bytes[index + 2])?;
            decoded.push((hi << 4) | lo);
            index += 3;
            continue;
        }

        decoded.push(bytes[index]);
        index += 1;
    }

    String::from_utf8(decoded).ok()
}

fn decode_hex_nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
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

// Upper bound on how many elements/items a single scan attempt (locate_tag_in_dataset,
// skip_undefined_length_value, and locate_element_value_by_matching_bytes each get their own
// fresh budget) will walk before giving up. A well-formed DICOM dataset - even a large one with
// thousands of per-frame functional-group items - stays well under this; it exists purely to
// bound the worst case when the scan has desynced (e.g. into high-entropy compressed pixel data,
// where nearly any 2 bytes can look like a plausible tag) and would otherwise treat a multi-MB
// remaining byte range as an unbounded number of tiny fake elements before finally hitting EOF -
// observed taking upwards of 15-20 SECONDS for a single ~500KB file before this cap existed.
const MAX_SCAN_STEPS: usize = 20_000;

fn take_scan_step(steps: &mut usize) -> Result<(), DicomJsonError> {
    if *steps == 0 {
        return Err(DicomJsonError::InvalidBulkDataUri(
            "scan exceeded maximum step budget".to_owned(),
        ));
    }
    *steps -= 1;
    Ok(())
}

// The byte-content fallback search (locate_element_value_by_matching_bytes) has a fundamentally
// different cost shape than the tag-walking scan above: each outer-loop iteration calls
// find_subslice, whose own cost is proportional to how much of the remaining buffer it has to
// examine (up to the whole thing), not to a fixed per-iteration step. So it gets its own, much
// smaller attempt budget, plus a cap on the needle size: an element big enough to matter here
// should already have been found by the direct tag scan, and matching a large needle by content
// against a large haystack (e.g. long zero-padding runs, common in private/CSA blobs) is exactly
// the shape that made a single scan attempt take upwards of 10+ seconds before this cap existed.
const MAX_MATCH_NEEDLE_LEN: usize = 64 * 1024;
const MAX_MATCH_SCAN_ATTEMPTS: usize = 2_000;

fn locate_element_value(
    source: &[u8],
    target: Tag,
    expected_bytes: Option<&[u8]>,
    resume_hint: usize,
) -> Result<Option<ElementLocation>, DicomJsonError> {
    let has_part10_preamble = source.len() >= 132 && &source[128..132] == b"DICM";

    let mut position = if has_part10_preamble { 132 } else { 0 };
    let mut transfer_syntax_uid = if has_part10_preamble {
        uids::EXPLICIT_VR_LITTLE_ENDIAN.to_owned()
    } else {
        uids::IMPLICIT_VR_LITTLE_ENDIAN.to_owned()
    };

    if has_part10_preamble {
        let mut meta_steps = MAX_SCAN_STEPS;
        while position + 8 <= source.len() {
            take_scan_step(&mut meta_steps)?;
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

            if header.tag == target
                && value_matches(source, value_offset, value_length, expected_bytes)
            {
                return Ok(Some(ElementLocation {
                    offset: value_offset,
                    length: value_length,
                }));
            }

            if header.tag == tags::TRANSFER_SYNTAX_UID {
                transfer_syntax_uid =
                    decode_dicom_text(&source[value_offset..value_offset + value_length]);
            }

            position = value_offset + value_length;
        }
    }

    let syntax = transfer_syntax_from_uid(transfer_syntax_uid.as_str())?;
    let dataset_start = position;
    let scan_from = resume_hint.max(dataset_start);

    if scan_from > dataset_start {
        let mut steps = MAX_SCAN_STEPS;
        if let Some(location) = locate_tag_in_dataset(
            source,
            scan_from,
            target,
            syntax.explicit_vr,
            syntax.little_endian,
            expected_bytes,
            &mut steps,
        )? {
            return Ok(Some(location));
        }
        // Not found from the hint onward - retry once from the true dataset start, in case this
        // particular tag actually precedes the hint (an out-of-order visit relative to file
        // layout). This keeps the hint a pure speed optimization, never a correctness risk.
    }

    let mut steps = MAX_SCAN_STEPS;
    if let Some(location) = locate_tag_in_dataset(
        source,
        dataset_start,
        target,
        syntax.explicit_vr,
        syntax.little_endian,
        expected_bytes,
        &mut steps,
    )? {
        return Ok(Some(location));
    }

    if let Some(expected) = expected_bytes {
        return locate_element_value_by_matching_bytes(
            source,
            target,
            expected,
            syntax.explicit_vr,
            syntax.little_endian,
        );
    }

    Ok(None)
}

fn locate_tag_in_dataset(
    source: &[u8],
    mut position: usize,
    target: Tag,
    explicit_vr: bool,
    little_endian: bool,
    expected_bytes: Option<&[u8]>,
    steps: &mut usize,
) -> Result<Option<ElementLocation>, DicomJsonError> {
    while position + 8 <= source.len() {
        take_scan_step(steps)?;
        let header = parse_element_header(source, position, explicit_vr, little_endian)?;
        let value_offset = position + header.header_length;
        // Undefined-length values are scanned ONCE (not once for the length, then again for the
        // next position) - both are derived from the same skip_undefined_length_value call.
        let (length, next_position) = if let Some(length) = header.length {
            (length, value_offset + length)
        } else {
            let end_position =
                skip_undefined_length_value(source, value_offset, explicit_vr, little_endian, steps)?;
            (end_position.saturating_sub(value_offset), end_position + 8)
        };

        if header.tag == target && value_matches(source, value_offset, length, expected_bytes) {
            return Ok(Some(ElementLocation {
                offset: value_offset,
                length,
            }));
        }

        position = next_position;
    }

    Ok(None)
}

fn locate_element_value_by_matching_bytes(
    source: &[u8],
    target: Tag,
    expected: &[u8],
    explicit_vr: bool,
    little_endian: bool,
) -> Result<Option<ElementLocation>, DicomJsonError> {
    if expected.is_empty() || expected.len() > MAX_MATCH_NEEDLE_LEN {
        return Ok(None);
    }

    let mut search_start = 0;
    let mut attempts = MAX_MATCH_SCAN_ATTEMPTS;
    let mut steps = MAX_SCAN_STEPS;
    while search_start + expected.len() <= source.len() {
        take_scan_step(&mut attempts)?;
        let Some(relative_match) = find_subslice(&source[search_start..], expected) else {
            break;
        };
        let value_offset = search_start + relative_match;

        if let Some(location) = try_locate_header_for_value_offset(
            source,
            target,
            value_offset,
            expected.len(),
            explicit_vr,
            little_endian,
            &mut steps,
        )? {
            return Ok(Some(location));
        }

        search_start = value_offset + 1;
    }

    Ok(None)
}

fn try_locate_header_for_value_offset(
    source: &[u8],
    target: Tag,
    value_offset: usize,
    expected_length: usize,
    explicit_vr: bool,
    little_endian: bool,
    steps: &mut usize,
) -> Result<Option<ElementLocation>, DicomJsonError> {
    let header_lengths: &[usize] = if explicit_vr { &[8, 12] } else { &[8] };

    for header_length in header_lengths {
        if value_offset < *header_length {
            continue;
        }

        let header_offset = value_offset - *header_length;
        let Ok(header) = parse_element_header(source, header_offset, explicit_vr, little_endian)
        else {
            continue;
        };

        if header.tag != target {
            continue;
        }

        if header_offset + header.header_length != value_offset {
            continue;
        }

        let length = if let Some(length) = header.length {
            length
        } else {
            skip_undefined_length_value(source, value_offset, explicit_vr, little_endian, steps)?
                .saturating_sub(value_offset)
        };

        if length == expected_length {
            return Ok(Some(ElementLocation {
                offset: value_offset,
                length,
            }));
        }
    }

    Ok(None)
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || needle.len() > haystack.len() {
        return None;
    }

    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn value_matches(
    source: &[u8],
    value_offset: usize,
    value_length: usize,
    expected_bytes: Option<&[u8]>,
) -> bool {
    let Some(expected) = expected_bytes else {
        return true;
    };

    value_length == expected.len()
        && source
            .get(value_offset..value_offset + value_length)
            .map(|bytes| bytes == expected)
            .unwrap_or(false)
}

fn skip_undefined_length_value(
    source: &[u8],
    mut position: usize,
    explicit_vr: bool,
    little_endian: bool,
    steps: &mut usize,
) -> Result<usize, DicomJsonError> {
    while position + 8 <= source.len() {
        take_scan_step(steps)?;
        let tag = read_tag(source, position, little_endian)?;
        if tag == SEQUENCE_DELIMITATION_TAG || tag == ITEM_DELIMITATION_TAG {
            return Ok(position);
        }

        if tag == ITEM_TAG {
            let item_length = read_u32(source, position + 4, little_endian)? as usize;
            position += 8;
            position = if item_length == u32::MAX as usize {
                skip_undefined_length_value(source, position, explicit_vr, little_endian, steps)? + 8
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
            skip_undefined_length_value(source, value_offset, explicit_vr, little_endian, steps)? + 8
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
