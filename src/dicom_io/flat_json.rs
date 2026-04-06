use dicom_core::dictionary::{DataDictionary, DataDictionaryEntry};
use dicom_core::header::EmptyObject;
use dicom_core::value::Value as DicomValue;
use dicom_core::{DataElement, Length, PrimitiveValue, Tag, VR};
use dicom_dictionary_std::{tags, StandardDataDictionary};
use dicom_object::{DefaultDicomObject, InMemDicomObject};
use serde_json::{Map as JsonMap, Number as JsonNumber, Value as JsonValue};

use super::bulk_data::{bulk_json_value, primitive_is_bulk, resolve_flat_bulk_bytes, raw_bytes_to_dicom_value};
use super::common::{
    apply_meta_element, extract_transfer_syntax_from_flat, flat_key_for_tag, invalid_json_value,
    json_number_from_f32, json_number_from_f64, json_value_to_numbers, json_value_to_text,
    normalize_flat_json_entries, number_or_backslash_string, parse_tag_values,
    should_wrap_flat_element, split_multi_value,
};
use super::types::{DicomJsonError, DicomJsonWriteOptions};

pub(super) fn write_flat_json_value(
    object: &DefaultDicomObject,
    options: DicomJsonWriteOptions<'_>,
) -> Result<JsonValue, DicomJsonError> {
    let mut json = JsonMap::new();

    for element in object.meta().to_element_iter() {
        if element.header().tag == dicom_dictionary_std::tags::FILE_META_INFORMATION_GROUP_LENGTH {
            continue;
        }

        let key = flat_key_for_tag(element.header().tag, options.key_style);
        json.insert(
            key.clone(),
            flat_element_to_json(
                key.as_str(),
                element.header().tag,
                element.vr(),
                element.value(),
                options,
            )?,
        );
    }

    for element in object.iter() {
        let key = flat_key_for_tag(element.header().tag, options.key_style);
        json.insert(
            key.clone(),
            flat_element_to_json(
                key.as_str(),
                element.header().tag,
                element.vr(),
                element.value(),
                options,
            )?,
        );
    }

    Ok(JsonValue::Object(json))
}

pub(super) fn read_flat_json_value(
    value: &JsonValue,
    bulk_data_source: Option<&[u8]>,
) -> Result<DefaultDicomObject, DicomJsonError> {
    let JsonValue::Object(entries) = value else {
        return Err(DicomJsonError::InvalidJsonRoot);
    };

    let entries = normalize_flat_json_entries(entries)?;

    let transfer_syntax_uid = extract_transfer_syntax_from_flat(&entries)
        .unwrap_or_else(|| dicom_dictionary_std::uids::EXPLICIT_VR_LITTLE_ENDIAN.to_owned());

    let mut dataset_elements = Vec::new();
    let mut meta_builder = dicom_object::FileMetaTableBuilder::new();

    for (keyword, json_value) in &entries {
        let tag = StandardDataDictionary
            .parse_tag(keyword)
            .ok_or_else(|| DicomJsonError::UnknownAttribute(keyword.clone()))?;
        let default_vr = StandardDataDictionary
            .by_tag(tag)
            .map(|entry| entry.vr().relaxed())
            .unwrap_or(VR::UN);

        if tag == dicom_dictionary_std::tags::FILE_META_INFORMATION_GROUP_LENGTH {
            continue;
        }

        let (vr, value) = flat_json_element_to_dicom_parts(
            keyword,
            tag,
            json_value,
            default_vr,
            bulk_data_source,
            transfer_syntax_uid.as_str(),
        )?;
        let element = DataElement::new(tag, vr, value);

        if tag.group() == 0x0002 {
            apply_meta_element(&mut meta_builder, &element)?;
        } else {
            dataset_elements.push(element);
        }
    }

    let dataset = InMemDicomObject::from_element_iter(dataset_elements);
    Ok(dataset.with_meta(meta_builder.transfer_syntax(transfer_syntax_uid))?)
}

fn flat_element_to_json<I, P>(
    keyword: &str,
    tag: Tag,
    vr: VR,
    value: &DicomValue<I, P>,
    options: DicomJsonWriteOptions<'_>,
) -> Result<JsonValue, DicomJsonError>
where
    I: FlatJsonItem,
    P: AsRef<[u8]>,
{
    let plain_value = flat_value_to_json(keyword, tag, vr, value, options)?;
    if should_wrap_flat_element(tag) {
        Ok(wrap_flat_value(vr, plain_value))
    } else {
        Ok(plain_value)
    }
}

fn flat_value_to_json<I, P>(
    _keyword: &str,
    tag: Tag,
    vr: VR,
    value: &DicomValue<I, P>,
    options: DicomJsonWriteOptions<'_>,
) -> Result<JsonValue, DicomJsonError>
where
    I: FlatJsonItem,
    P: AsRef<[u8]>,
{
    match value {
        DicomValue::Sequence(sequence) => Ok(JsonValue::Array(
            sequence
                .items()
                .iter()
                .map(|item| item.to_flat_json(options))
                .map(JsonValue::Object)
                .collect(),
        )),
        DicomValue::PixelSequence(_) => bulk_json_value(tag, vr, value, options),
        DicomValue::Primitive(_) if primitive_is_bulk(vr) => bulk_json_value(tag, vr, value, options),
        DicomValue::Primitive(primitive) => Ok(flat_primitive_to_json(vr, primitive)),
    }
}

fn wrap_flat_value(vr: VR, value: JsonValue) -> JsonValue {
    let mut object = JsonMap::new();
    object.insert("vr".to_owned(), JsonValue::String(vr.to_string().to_owned()));

    if let JsonValue::Object(map) = value {
        if map.contains_key("InlineBinary") || map.contains_key("BulkDataURI") {
            object.extend(map);
            return JsonValue::Object(object);
        }
        object.insert("Value".to_owned(), JsonValue::Object(map));
        return JsonValue::Object(object);
    }

    object.insert("Value".to_owned(), value);
    JsonValue::Object(object)
}

fn flat_primitive_to_json(vr: VR, primitive: &PrimitiveValue) -> JsonValue {
    match primitive {
        PrimitiveValue::Empty => JsonValue::Null,
        PrimitiveValue::Str(_)
        | PrimitiveValue::Strs(_)
        | PrimitiveValue::Date(_)
        | PrimitiveValue::DateTime(_)
        | PrimitiveValue::Time(_) => JsonValue::String(primitive.to_multi_str().join("\\")),
        PrimitiveValue::Tags(values) => JsonValue::String(
            values
                .iter()
                .map(|value| format!("({:04X},{:04X})", value.group(), value.element()))
                .collect::<Vec<_>>()
                .join("\\"),
        ),
        PrimitiveValue::U8(values) => number_or_backslash_string(
            values
                .iter()
                .map(|value| JsonValue::Number((*value).into()))
                .collect(),
        ),
        PrimitiveValue::I16(values) => number_or_backslash_string(
            values
                .iter()
                .map(|value| JsonValue::Number(JsonNumber::from(*value)))
                .collect(),
        ),
        PrimitiveValue::U16(values) => number_or_backslash_string(
            values
                .iter()
                .map(|value| JsonValue::Number((*value).into()))
                .collect(),
        ),
        PrimitiveValue::I32(values) => {
            if vr == VR::IS {
                JsonValue::String(values.iter().map(ToString::to_string).collect::<Vec<_>>().join("\\"))
            } else {
                number_or_backslash_string(
                    values
                        .iter()
                        .map(|value| JsonValue::Number(JsonNumber::from(*value)))
                        .collect(),
                )
            }
        }
        PrimitiveValue::U32(values) => number_or_backslash_string(
            values
                .iter()
                .map(|value| JsonValue::Number((*value).into()))
                .collect(),
        ),
        PrimitiveValue::I64(values) => number_or_backslash_string(
            values
                .iter()
                .map(|value| JsonValue::Number(JsonNumber::from(*value)))
                .collect(),
        ),
        PrimitiveValue::U64(values) => number_or_backslash_string(
            values
                .iter()
                .map(|value| JsonValue::Number(JsonNumber::from(*value)))
                .collect(),
        ),
        PrimitiveValue::F32(values) => {
            if vr == VR::DS {
                JsonValue::String(values.iter().map(ToString::to_string).collect::<Vec<_>>().join("\\"))
            } else {
                number_or_backslash_string(
                    values
                        .iter()
                        .filter_map(|value| json_number_from_f32(*value))
                        .collect(),
                )
            }
        }
        PrimitiveValue::F64(values) => {
            if vr == VR::DS {
                JsonValue::String(values.iter().map(ToString::to_string).collect::<Vec<_>>().join("\\"))
            } else {
                number_or_backslash_string(
                    values
                        .iter()
                        .filter_map(|value| json_number_from_f64(*value))
                        .collect(),
                )
            }
        }
    }
}

fn flat_json_to_dicom_value(
    keyword: &str,
    tag: Tag,
    vr: VR,
    json: &JsonValue,
    bulk_data_source: Option<&[u8]>,
    transfer_syntax_uid: &str,
) -> Result<DicomValue<InMemDicomObject>, DicomJsonError> {
    if vr == VR::SQ {
        let JsonValue::Array(items) = json else {
            return Err(invalid_json_value(keyword, "expected an array of sequence items"));
        };

        let mut sequence_items = Vec::with_capacity(items.len());
        for item in items {
            let JsonValue::Object(object) = item else {
                return Err(invalid_json_value(
                    keyword,
                    "expected each sequence item to be a JSON object",
                ));
            };

            sequence_items.push(read_flat_dataset_object(
                object,
                bulk_data_source,
                transfer_syntax_uid,
            )?);
        }

        return Ok(DicomValue::new_sequence(sequence_items, Length::UNDEFINED));
    }

    if let Some(bytes) = resolve_flat_bulk_bytes(keyword, json, bulk_data_source)? {
        return raw_bytes_to_dicom_value(tag, vr, &bytes, transfer_syntax_uid);
    }

    if json.is_null() {
        return Ok(PrimitiveValue::Empty.into());
    }

    let primitive = match vr {
        VR::AE
        | VR::AS
        | VR::CS
        | VR::DA
        | VR::DS
        | VR::DT
        | VR::IS
        | VR::LO
        | VR::PN
        | VR::SH
        | VR::TM
        | VR::UC
        | VR::UI => PrimitiveValue::Strs(split_multi_value(&json_value_to_text(keyword, json)?).into()),
        VR::LT | VR::ST | VR::UR | VR::UT => PrimitiveValue::Str(json_value_to_text(keyword, json)?),
        VR::AT => PrimitiveValue::Tags(parse_tag_values(keyword, json)?.into()),
        VR::SS => PrimitiveValue::I16(json_value_to_numbers::<i16>(keyword, json)?.into()),
        VR::US => PrimitiveValue::U16(json_value_to_numbers::<u16>(keyword, json)?.into()),
        VR::SL => PrimitiveValue::I32(json_value_to_numbers::<i32>(keyword, json)?.into()),
        VR::UL => PrimitiveValue::U32(json_value_to_numbers::<u32>(keyword, json)?.into()),
        VR::SV => PrimitiveValue::I64(json_value_to_numbers::<i64>(keyword, json)?.into()),
        VR::UV => PrimitiveValue::U64(json_value_to_numbers::<u64>(keyword, json)?.into()),
        VR::FL => PrimitiveValue::F32(json_value_to_numbers::<f32>(keyword, json)?.into()),
        VR::FD => PrimitiveValue::F64(json_value_to_numbers::<f64>(keyword, json)?.into()),
        VR::OB | VR::OD | VR::OF | VR::OL | VR::OV | VR::OW | VR::UN => {
            return Err(invalid_json_value(
                keyword,
                "bulk data requires InlineBinary, BulkDataURI, or numeric data",
            ))
        }
        VR::SQ => unreachable!(),
    };

    Ok(primitive.into())
}

fn flat_json_element_to_dicom_parts(
    keyword: &str,
    tag: Tag,
    json: &JsonValue,
    default_vr: VR,
    bulk_data_source: Option<&[u8]>,
    transfer_syntax_uid: &str,
) -> Result<(VR, DicomValue<InMemDicomObject>), DicomJsonError> {
    let (parsed_vr, inner_value) = if let Some((vr, inner_value)) = extract_flat_typed_value(keyword, json)? {
        (vr, inner_value)
    } else {
        (default_vr, json.clone())
    };

    let value = flat_json_to_dicom_value(
        keyword,
        tag,
        parsed_vr,
        &inner_value,
        bulk_data_source,
        transfer_syntax_uid,
    )?;

    let vr = match &value {
        DicomValue::Sequence(_) => VR::SQ,
        DicomValue::PixelSequence(_) if tag == tags::PIXEL_DATA => VR::OB,
        DicomValue::PixelSequence(_) | DicomValue::Primitive(_) => parsed_vr,
    };

    Ok((vr, value))
}

fn extract_flat_typed_value(
    keyword: &str,
    json: &JsonValue,
) -> Result<Option<(VR, JsonValue)>, DicomJsonError> {
    let JsonValue::Object(object) = json else {
        return Ok(None);
    };

    let Some(JsonValue::String(vr_text)) = object.get("vr") else {
        return Ok(None);
    };

    let vr = vr_text
        .parse()
        .map_err(|_| invalid_json_value(keyword, "invalid VR in flattened element wrapper"))?;

    if let Some(value) = object.get("InlineBinary") {
        let mut bulk = JsonMap::new();
        bulk.insert("InlineBinary".to_owned(), value.clone());
        return Ok(Some((vr, JsonValue::Object(bulk))));
    }

    if let Some(value) = object.get("BulkDataURI") {
        let mut bulk = JsonMap::new();
        bulk.insert("BulkDataURI".to_owned(), value.clone());
        return Ok(Some((vr, JsonValue::Object(bulk))));
    }

    Ok(Some((
        vr,
        object.get("Value").cloned().unwrap_or(JsonValue::Null),
    )))
}

fn read_flat_dataset_object(
    object: &JsonMap<String, JsonValue>,
    bulk_data_source: Option<&[u8]>,
    transfer_syntax_uid: &str,
) -> Result<InMemDicomObject, DicomJsonError> {
    let object = normalize_flat_json_entries(object)?;
    let mut elements = Vec::new();

    for (keyword, json_value) in &object {
        let tag = StandardDataDictionary
            .parse_tag(keyword)
            .ok_or_else(|| DicomJsonError::UnknownAttribute(keyword.clone()))?;
        let default_vr = StandardDataDictionary
            .by_tag(tag)
            .map(|entry| entry.vr().relaxed())
            .unwrap_or(VR::UN);
        let (vr, value) = flat_json_element_to_dicom_parts(
            keyword,
            tag,
            json_value,
            default_vr,
            bulk_data_source,
            transfer_syntax_uid,
        )?;
        elements.push(DataElement::new(tag, vr, value));
    }

    Ok(InMemDicomObject::from_element_iter(elements))
}

trait FlatJsonItem {
    fn to_flat_json(&self, options: DicomJsonWriteOptions<'_>) -> JsonMap<String, JsonValue>;
}

impl FlatJsonItem for InMemDicomObject {
    fn to_flat_json(&self, options: DicomJsonWriteOptions<'_>) -> JsonMap<String, JsonValue> {
        let mut object = JsonMap::new();

        for element in self.iter() {
            let key = flat_key_for_tag(element.header().tag, options.key_style);
            let value = flat_element_to_json(
                key.as_str(),
                element.header().tag,
                element.vr(),
                element.value(),
                options,
            )
            .unwrap_or(JsonValue::Null);
            object.insert(key, value);
        }

        object
    }
}

impl FlatJsonItem for EmptyObject {
    fn to_flat_json(&self, _options: DicomJsonWriteOptions<'_>) -> JsonMap<String, JsonValue> {
        JsonMap::new()
    }
}