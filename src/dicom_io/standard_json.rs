use dcmnorm_core::header::EmptyObject;
use dcmnorm_core::value::Value as DicomValue;
use dcmnorm_core::{Length, PrimitiveValue, Tag, VR};
use dcmnorm_dictionary::tags;
use dcmnorm_object::{DefaultDicomObject, InMemDicomObject};
use serde_json::{Map as JsonMap, Number as JsonNumber, Value as JsonValue};

use super::bulk_data::{
    bulk_representation, is_bulk_value, primitive_is_bulk, resolve_standard_bulk_bytes,
    raw_bytes_to_dicom_value,
};
use super::common::{
    apply_meta_element, extract_transfer_syntax_from_standard, json_number_from_f32,
    json_number_from_f64, json_value_to_numbers, json_value_to_text, keyword_for_tag, tag_key,
};
use super::types::{
    BulkRepresentation, DicomJsonBulkDataMode, DicomJsonError, DicomJsonWriteOptions,
};

pub(super) fn write_standard_json_value(
    object: &DefaultDicomObject,
    options: DicomJsonWriteOptions<'_>,
) -> Result<JsonValue, DicomJsonError> {
    let mut json = JsonMap::new();

    for element in object.meta().to_element_iter() {
        if element.header().tag == tags::FILE_META_INFORMATION_GROUP_LENGTH {
            continue;
        }

        let value = write_standard_element(
            element.header().tag,
            element.vr(),
            element.value(),
            options,
        )?;

        json.insert(
            tag_key(element.header().tag),
            decorate_standard_element_json(
                value,
                element.header().tag,
                standard_vm(element.header().tag, element.vr(), element.value()),
            )?,
        );
    }

    for element in object.iter() {
        let value = write_standard_element(
            element.header().tag,
            element.vr(),
            element.value(),
            options,
        )?;

        json.insert(
            tag_key(element.header().tag),
            decorate_standard_element_json(
                value,
                element.header().tag,
                standard_vm(element.header().tag, element.vr(), element.value()),
            )?,
        );
    }

    Ok(JsonValue::Object(json))
}

pub(super) fn read_standard_json_value(
    value: &JsonValue,
    bulk_data_source: Option<&[u8]>,
) -> Result<DefaultDicomObject, DicomJsonError> {
    let JsonValue::Object(entries) = value else {
        return Err(DicomJsonError::InvalidJsonRoot);
    };

    let transfer_syntax_uid = extract_transfer_syntax_from_standard(entries)
        .unwrap_or_else(|| dcmnorm_dictionary::uids::EXPLICIT_VR_LITTLE_ENDIAN.to_owned());

    let mut dataset_elements = Vec::new();
    let mut meta_builder = dcmnorm_object::FileMetaTableBuilder::new();

    for (tag_text, element_json) in entries {
        let tag = tag_text
            .parse()
            .map_err(|_| DicomJsonError::InvalidStandardElement {
                tag: tag_text.clone(),
                message: "invalid tag key".to_owned(),
            })?;

        if tag == tags::FILE_META_INFORMATION_GROUP_LENGTH {
            continue;
        }

        let (vr, value) = read_standard_element(
            tag_text,
            tag,
            element_json,
            bulk_data_source,
            transfer_syntax_uid.as_str(),
        )?;
        let element = dcmnorm_core::DataElement::new(tag, vr, value);

        if tag.group() == 0x0002 {
            apply_meta_element(&mut meta_builder, &element)?;
        } else {
            dataset_elements.push(element);
        }
    }

    let dataset = InMemDicomObject::from_element_iter(dataset_elements);
    Ok(dataset.with_meta(meta_builder.transfer_syntax(transfer_syntax_uid))?)
}

/// Serializes a single element (top-level or nested inside a sequence item)
/// to its standard-JSON representation, recursing into `Value` for `SQ`.
/// Applying the same bulk-data handling at every nesting depth is what keeps
/// bulk data inside sequences consistent with top-level bulk data.
fn write_standard_element<I, P>(
    tag: Tag,
    vr: VR,
    value: &DicomValue<I, P>,
    options: DicomJsonWriteOptions<'_>,
) -> Result<JsonValue, DicomJsonError>
where
    I: StandardJsonItem,
    P: AsRef<[u8]>,
{
    let mut object = JsonMap::new();
    object.insert("vr".to_owned(), JsonValue::String(vr.to_string().to_owned()));

    match value {
        DicomValue::Sequence(sequence) => {
            let items = sequence
                .items()
                .iter()
                .map(|item| item.to_standard_json(options).map(JsonValue::Object))
                .collect::<Result<Vec<_>, DicomJsonError>>()?;
            object.insert("Value".to_owned(), JsonValue::Array(items));
        }
        DicomValue::PixelSequence(_) => {
            insert_bulk_representation(&mut object, tag, vr, value, options)?;
        }
        DicomValue::Primitive(PrimitiveValue::Empty) => {}
        DicomValue::Primitive(_) if primitive_is_bulk(vr) => {
            insert_bulk_representation(&mut object, tag, vr, value, options)?;
        }
        DicomValue::Primitive(primitive) => {
            object.insert("Value".to_owned(), standard_primitive_to_json(vr, primitive));
        }
    }

    Ok(JsonValue::Object(object))
}

/// `WaveformData` is conventionally always inlined rather than referenced by
/// `BulkDataURI`, matching the read side's [`super::bulk_data::is_bulk_value`]
/// exclusion; every other bulk VR is eligible for a URI in URI mode.
fn insert_bulk_representation<I, P>(
    object: &mut JsonMap<String, JsonValue>,
    tag: Tag,
    vr: VR,
    value: &DicomValue<I, P>,
    options: DicomJsonWriteOptions<'_>,
) -> Result<(), DicomJsonError>
where
    P: AsRef<[u8]>,
{
    let effective_options = if tag == tags::WAVEFORM_DATA {
        DicomJsonWriteOptions {
            bulk_data_mode: DicomJsonBulkDataMode::InlineBinary,
            ..options
        }
    } else {
        options
    };

    match bulk_representation(tag, vr, value, effective_options)? {
        BulkRepresentation::Uri(uri) => {
            object.insert("BulkDataURI".to_owned(), JsonValue::String(uri));
        }
        BulkRepresentation::InlineBinary(encoded) => {
            object.insert("InlineBinary".to_owned(), JsonValue::String(encoded));
        }
    }

    Ok(())
}

fn decorate_standard_element_json(
    value: JsonValue,
    tag: Tag,
    multiplicity: u32,
) -> Result<JsonValue, DicomJsonError> {
    let JsonValue::Object(mut object) = value else {
        return Err(DicomJsonError::InvalidStandardElement {
            tag: tag_key(tag),
            message: "expected an object value".to_owned(),
        });
    };

    object.insert(
        "Keyword".to_owned(),
        JsonValue::String(keyword_for_tag(tag)),
    );
    object.insert(
        "VM".to_owned(),
        JsonValue::Number(JsonNumber::from(multiplicity)),
    );
    Ok(JsonValue::Object(object))
}

/// Per PS3.18 Annex F, PN is an array of `{"Alphabetic": ..., "Ideographic":
/// ..., "Phonetic": ...}` objects and numeric VRs prefer JSON numbers (with
/// "NaN"/"inf"/"-inf" string tokens for non-finite floats); everything else
/// is a plain string array.
fn standard_primitive_to_json(vr: VR, primitive: &PrimitiveValue) -> JsonValue {
    match vr {
        VR::PN => JsonValue::Array(
            primitive
                .to_multi_str()
                .iter()
                .map(|name| person_name_to_json(name))
                .collect(),
        ),
        VR::FD | VR::IS | VR::FL | VR::DS | VR::SL | VR::SS | VR::SV | VR::UL | VR::US
        | VR::UV => standard_numbers_json(primitive),
        _ => JsonValue::Array(
            primitive
                .to_multi_str()
                .iter()
                .cloned()
                .map(JsonValue::String)
                .collect(),
        ),
    }
}

fn person_name_to_json(raw: &str) -> JsonValue {
    let mut parts = raw.splitn(3, '=');
    let mut object = JsonMap::new();

    if let Some(alphabetic) = parts.next() {
        if !alphabetic.is_empty() {
            object.insert(
                "Alphabetic".to_owned(),
                JsonValue::String(alphabetic.to_owned()),
            );
        }
    }
    if let Some(ideographic) = parts.next() {
        if !ideographic.is_empty() {
            object.insert(
                "Ideographic".to_owned(),
                JsonValue::String(ideographic.to_owned()),
            );
        }
    }
    if let Some(phonetic) = parts.next() {
        if !phonetic.is_empty() {
            object.insert(
                "Phonetic".to_owned(),
                JsonValue::String(phonetic.to_owned()),
            );
        }
    }

    JsonValue::Object(object)
}

/// Mirrors the actual stored [`PrimitiveValue`] variant (not just the VR),
/// since values read from a file often keep DS/IS as text (`Strs`) while
/// values built programmatically may already be typed numbers.
fn standard_numbers_json(primitive: &PrimitiveValue) -> JsonValue {
    match primitive {
        PrimitiveValue::Empty => JsonValue::Array(Vec::new()),
        PrimitiveValue::Str(text) => JsonValue::Array(vec![JsonValue::String(text.clone())]),
        PrimitiveValue::Strs(strings) => {
            JsonValue::Array(strings.iter().cloned().map(JsonValue::String).collect())
        }
        PrimitiveValue::U8(values) => {
            JsonValue::Array(values.iter().map(|v| JsonValue::Number((*v).into())).collect())
        }
        PrimitiveValue::I16(values) => JsonValue::Array(
            values
                .iter()
                .map(|v| JsonValue::Number(JsonNumber::from(*v)))
                .collect(),
        ),
        PrimitiveValue::U16(values) => {
            JsonValue::Array(values.iter().map(|v| JsonValue::Number((*v).into())).collect())
        }
        PrimitiveValue::I32(values) => JsonValue::Array(
            values
                .iter()
                .map(|v| JsonValue::Number(JsonNumber::from(*v)))
                .collect(),
        ),
        PrimitiveValue::U32(values) => {
            JsonValue::Array(values.iter().map(|v| JsonValue::Number((*v).into())).collect())
        }
        PrimitiveValue::I64(values) => JsonValue::Array(
            values
                .iter()
                .map(|v| match i32::try_from(*v) {
                    Ok(narrowed) => JsonValue::Number(JsonNumber::from(narrowed)),
                    Err(_) => JsonValue::String(v.to_string()),
                })
                .collect(),
        ),
        PrimitiveValue::U64(values) => JsonValue::Array(
            values
                .iter()
                .map(|v| match i32::try_from(*v) {
                    Ok(narrowed) => JsonValue::Number(JsonNumber::from(narrowed)),
                    Err(_) => JsonValue::String(v.to_string()),
                })
                .collect(),
        ),
        PrimitiveValue::F32(values) => JsonValue::Array(
            values
                .iter()
                .map(|v| match json_number_from_f32(*v) {
                    Some(number) => number,
                    None if v.is_nan() => JsonValue::String("NaN".to_owned()),
                    None if v.is_sign_positive() => JsonValue::String("inf".to_owned()),
                    None => JsonValue::String("-inf".to_owned()),
                })
                .collect(),
        ),
        PrimitiveValue::F64(values) => JsonValue::Array(
            values
                .iter()
                .map(|v| match json_number_from_f64(*v) {
                    Some(number) => number,
                    None if v.is_nan() => JsonValue::String("NaN".to_owned()),
                    None if v.is_sign_positive() => JsonValue::String("inf".to_owned()),
                    None => JsonValue::String("-inf".to_owned()),
                })
                .collect(),
        ),
        _ => JsonValue::Array(Vec::new()),
    }
}

/// Mirror of [`write_standard_element`] for reading: recurses into `Value`
/// for `SQ` and requires `InlineBinary`/`BulkDataURI` for bulk VRs at every
/// nesting depth, so a sequence-nested bulk element round-trips the same way
/// a top-level one does.
fn read_standard_element(
    tag_text: &str,
    tag: Tag,
    element_json: &JsonValue,
    bulk_data_source: Option<&[u8]>,
    transfer_syntax_uid: &str,
) -> Result<(VR, DicomValue<InMemDicomObject>), DicomJsonError> {
    let cleaned = clean_standard_element_json(tag_text, element_json)?;
    let vr = standard_element_vr(tag_text, &cleaned)?;

    let JsonValue::Object(object) = &cleaned else {
        return Err(DicomJsonError::InvalidStandardElement {
            tag: tag_text.to_owned(),
            message: "expected an object".to_owned(),
        });
    };

    if vr == VR::SQ {
        let items = match object.get("Value") {
            Some(JsonValue::Array(items)) => items.as_slice(),
            None | Some(JsonValue::Null) => &[],
            _ => {
                return Err(DicomJsonError::InvalidStandardElement {
                    tag: tag_text.to_owned(),
                    message: "expected an array of sequence items".to_owned(),
                })
            }
        };

        let mut sequence_items = Vec::with_capacity(items.len());
        for item in items {
            let JsonValue::Object(item_object) = item else {
                return Err(DicomJsonError::InvalidStandardElement {
                    tag: tag_text.to_owned(),
                    message: "expected each sequence item to be a JSON object".to_owned(),
                });
            };

            sequence_items.push(read_standard_dataset_object(
                item_object,
                bulk_data_source,
                transfer_syntax_uid,
            )?);
        }

        return Ok((
            VR::SQ,
            DicomValue::new_sequence(sequence_items, Length::UNDEFINED),
        ));
    }

    if primitive_is_bulk(vr) {
        return match resolve_standard_bulk_bytes(tag, vr, object, bulk_data_source)? {
            Some(bytes) => Ok((
                vr,
                raw_bytes_to_dicom_value(tag, vr, &bytes, transfer_syntax_uid)?,
            )),
            None => Err(DicomJsonError::InvalidStandardElement {
                tag: tag_text.to_owned(),
                message: "bulk data requires InlineBinary or BulkDataURI".to_owned(),
            }),
        };
    }

    let value_json = object.get("Value").cloned().unwrap_or(JsonValue::Null);
    let primitive = standard_json_to_primitive(tag_text, vr, &value_json)?;
    Ok((vr, primitive.into()))
}

fn read_standard_dataset_object(
    object: &JsonMap<String, JsonValue>,
    bulk_data_source: Option<&[u8]>,
    transfer_syntax_uid: &str,
) -> Result<InMemDicomObject, DicomJsonError> {
    let mut elements = Vec::with_capacity(object.len());

    for (tag_text, element_json) in object {
        let tag = tag_text
            .parse()
            .map_err(|_| DicomJsonError::InvalidStandardElement {
                tag: tag_text.clone(),
                message: "invalid tag key".to_owned(),
            })?;

        let (vr, value) = read_standard_element(
            tag_text,
            tag,
            element_json,
            bulk_data_source,
            transfer_syntax_uid,
        )?;
        elements.push(dcmnorm_core::DataElement::new(tag, vr, value));
    }

    Ok(InMemDicomObject::from_element_iter(elements))
}

fn clean_standard_element_json(
    tag_text: &str,
    element_json: &JsonValue,
) -> Result<JsonValue, DicomJsonError> {
    let JsonValue::Object(object) = element_json else {
        return Err(DicomJsonError::InvalidStandardElement {
            tag: tag_text.to_owned(),
            message: "expected an object".to_owned(),
        });
    };

    let mut cleaned = object.clone();
    cleaned.remove("Keyword");
    cleaned.remove("keyword");
    cleaned.remove("VM");
    cleaned.remove("vm");
    Ok(JsonValue::Object(cleaned))
}

fn standard_element_vr(tag_text: &str, element_json: &JsonValue) -> Result<VR, DicomJsonError> {
    let JsonValue::Object(object) = element_json else {
        return Err(DicomJsonError::InvalidStandardElement {
            tag: tag_text.to_owned(),
            message: "expected an object".to_owned(),
        });
    };

    let Some(JsonValue::String(vr)) = object.get("vr") else {
        return Err(DicomJsonError::InvalidStandardElement {
            tag: tag_text.to_owned(),
            message: "missing vr field".to_owned(),
        });
    };

    vr.parse()
        .map_err(|_| DicomJsonError::InvalidStandardElement {
            tag: tag_text.to_owned(),
            message: format!("invalid VR {vr}"),
        })
}

fn standard_json_to_primitive(
    tag_text: &str,
    vr: VR,
    value: &JsonValue,
) -> Result<PrimitiveValue, DicomJsonError> {
    if value.is_null() {
        return Ok(PrimitiveValue::Empty);
    }

    if vr == VR::AT {
        return Ok(PrimitiveValue::Tags(
            standard_json_to_tags(tag_text, value)?.into(),
        ));
    }

    match vr {
        VR::SS => return Ok(PrimitiveValue::I16(json_value_to_numbers(tag_text, value)?.into())),
        VR::US => return Ok(PrimitiveValue::U16(json_value_to_numbers(tag_text, value)?.into())),
        VR::SL => return Ok(PrimitiveValue::I32(json_value_to_numbers(tag_text, value)?.into())),
        VR::UL => return Ok(PrimitiveValue::U32(json_value_to_numbers(tag_text, value)?.into())),
        VR::SV => return Ok(PrimitiveValue::I64(json_value_to_numbers(tag_text, value)?.into())),
        VR::UV => return Ok(PrimitiveValue::U64(json_value_to_numbers(tag_text, value)?.into())),
        VR::FL => return Ok(PrimitiveValue::F32(json_value_to_numbers(tag_text, value)?.into())),
        VR::FD => return Ok(PrimitiveValue::F64(json_value_to_numbers(tag_text, value)?.into())),
        _ => {}
    }

    let JsonValue::Array(items) = value else {
        return Err(DicomJsonError::InvalidStandardElement {
            tag: tag_text.to_owned(),
            message: "expected \"Value\" to be a JSON array".to_owned(),
        });
    };

    if items.is_empty() {
        return Ok(PrimitiveValue::Empty);
    }

    if vr == VR::PN {
        let names = items
            .iter()
            .map(|item| standard_json_to_person_name(tag_text, item))
            .collect::<Result<Vec<_>, _>>()?;
        return Ok(PrimitiveValue::Strs(names.into()));
    }

    let strings = items
        .iter()
        .map(|item| json_value_to_text(tag_text, item))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(PrimitiveValue::Strs(strings.into()))
}

fn standard_json_to_person_name(tag_text: &str, item: &JsonValue) -> Result<String, DicomJsonError> {
    let JsonValue::Object(object) = item else {
        return Err(DicomJsonError::InvalidStandardElement {
            tag: tag_text.to_owned(),
            message: "expected a person name object".to_owned(),
        });
    };

    let alphabetic = match object.get("Alphabetic") {
        Some(JsonValue::String(text)) => text.as_str(),
        _ => "",
    };
    let ideographic = match object.get("Ideographic") {
        Some(JsonValue::String(text)) => Some(text.as_str()),
        _ => None,
    };
    let phonetic = match object.get("Phonetic") {
        Some(JsonValue::String(text)) => Some(text.as_str()),
        _ => None,
    };

    Ok(match (ideographic, phonetic) {
        (None, None) => alphabetic.to_owned(),
        (Some(ideographic), None) => format!("{alphabetic}={ideographic}"),
        (None, Some(phonetic)) => format!("{alphabetic}=={phonetic}"),
        (Some(ideographic), Some(phonetic)) => {
            format!("{alphabetic}={ideographic}={phonetic}")
        }
    })
}

fn standard_json_to_tags(tag_text: &str, value: &JsonValue) -> Result<Vec<Tag>, DicomJsonError> {
    let JsonValue::Array(items) = value else {
        return Err(DicomJsonError::InvalidStandardElement {
            tag: tag_text.to_owned(),
            message: "expected \"Value\" to be a JSON array".to_owned(),
        });
    };

    items
        .iter()
        .map(|item| {
            let JsonValue::String(text) = item else {
                return Err(DicomJsonError::InvalidStandardElement {
                    tag: tag_text.to_owned(),
                    message: "expected a hexadecimal tag string".to_owned(),
                });
            };
            text.parse()
                .map_err(|_| DicomJsonError::InvalidStandardElement {
                    tag: tag_text.to_owned(),
                    message: format!("invalid tag reference: {text}"),
                })
        })
        .collect()
}

fn standard_vm<I, P>(tag: Tag, vr: VR, value: &DicomValue<I, P>) -> u32 {
    if is_bulk_value(tag, vr, value) {
        match value {
            DicomValue::Primitive(PrimitiveValue::Empty) => 0,
            _ => 1,
        }
    } else {
        value.multiplicity()
    }
}

trait StandardJsonItem {
    fn to_standard_json(
        &self,
        options: DicomJsonWriteOptions<'_>,
    ) -> Result<JsonMap<String, JsonValue>, DicomJsonError>;
}

impl StandardJsonItem for InMemDicomObject {
    fn to_standard_json(
        &self,
        options: DicomJsonWriteOptions<'_>,
    ) -> Result<JsonMap<String, JsonValue>, DicomJsonError> {
        let mut object = JsonMap::new();

        for element in self.iter() {
            let value = write_standard_element(
                element.header().tag,
                element.vr(),
                element.value(),
                options,
            )?;
            object.insert(tag_key(element.header().tag), value);
        }

        Ok(object)
    }
}

impl StandardJsonItem for EmptyObject {
    fn to_standard_json(
        &self,
        _options: DicomJsonWriteOptions<'_>,
    ) -> Result<JsonMap<String, JsonValue>, DicomJsonError> {
        Ok(JsonMap::new())
    }
}
