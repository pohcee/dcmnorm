use dicom_core::dictionary::{DataDictionary, DataDictionaryEntry};
use dicom_core::header::Header;
use dicom_core::{DataElement, Tag};
use dicom_dictionary_std::{tags, StandardDataDictionary};
use dicom_object::{FileMetaTableBuilder, InMemDicomObject};
use serde_json::{Map as JsonMap, Number as JsonNumber, Value as JsonValue};

use super::types::{DicomJsonError, DicomJsonKeyStyle};

pub(super) fn extract_transfer_syntax_from_flat(
    entries: &JsonMap<String, JsonValue>,
) -> Option<String> {
    entries.get("00020010").and_then(flat_string_value)
}

pub(super) fn normalize_flat_json_entries(
    entries: &JsonMap<String, JsonValue>,
) -> Result<JsonMap<String, JsonValue>, DicomJsonError> {
    let mut normalized = JsonMap::new();

    for (key, value) in entries {
        let tag = StandardDataDictionary
            .parse_tag(key)
            .ok_or_else(|| DicomJsonError::UnknownAttribute(key.clone()))?;
        normalized.insert(tag_key(tag), value.clone());
    }

    Ok(normalized)
}

pub(super) fn extract_transfer_syntax_from_standard(
    entries: &JsonMap<String, JsonValue>,
) -> Option<String> {
    let JsonValue::Object(element) = entries.get("00020010")? else {
        return None;
    };

    let JsonValue::Array(values) = element.get("Value")? else {
        return None;
    };

    let JsonValue::String(uid) = values.first()? else {
        return None;
    };

    Some(uid.clone())
}

pub(super) fn keyword_for_tag(tag: Tag) -> String {
    StandardDataDictionary
        .by_tag(tag)
        .map(|entry| entry.alias().to_owned())
        .filter(|alias| alias != "PrivateCreator" && alias != "GenericGroupLength")
        .unwrap_or_else(|| format!("({:04X},{:04X})", tag.group(), tag.element()))
}

pub(super) fn flat_key_for_tag(tag: Tag, key_style: DicomJsonKeyStyle) -> String {
    if key_style == DicomJsonKeyStyle::Hex || should_use_hex_key(tag) {
        tag_key(tag)
    } else {
        keyword_for_tag(tag)
    }
}

pub(super) fn should_use_hex_key(tag: Tag) -> bool {
    if tag.group() % 2 == 1 {
        return true;
    }

    match StandardDataDictionary.by_tag(tag) {
        Some(entry) => {
            entry.tag() != tag
                || entry.alias() == "PrivateCreator"
                || entry.alias() == "GenericGroupLength"
        }
        None => true,
    }
}

pub(super) fn should_wrap_flat_element(tag: Tag) -> bool {
    tag.group() % 2 == 1
}

pub(super) fn tag_key(tag: Tag) -> String {
    format!("{:04X}{:04X}", tag.group(), tag.element())
}

pub(super) fn number_or_backslash_string(values: Vec<JsonValue>) -> JsonValue {
    if values.len() == 1 {
        values.into_iter().next().unwrap_or(JsonValue::Null)
    } else {
        JsonValue::String(
            values
                .iter()
                .map(|value| match value {
                    JsonValue::Number(number) => number.to_string(),
                    JsonValue::String(text) => text.clone(),
                    _ => String::new(),
                })
                .collect::<Vec<_>>()
                .join("\\"),
        )
    }
}

pub(super) fn json_value_to_text(
    keyword: &str,
    value: &JsonValue,
) -> Result<String, DicomJsonError> {
    match value {
        JsonValue::String(text) => Ok(text.clone()),
        JsonValue::Number(number) => Ok(number.to_string()),
        JsonValue::Bool(flag) => Ok(flag.to_string()),
        _ => Err(invalid_json_value(
            keyword,
            "expected a string-compatible JSON value",
        )),
    }
}

pub(super) fn json_value_to_numbers<T>(
    keyword: &str,
    value: &JsonValue,
) -> Result<Vec<T>, DicomJsonError>
where
    T: std::str::FromStr,
{
    match value {
        JsonValue::Number(number) => parse_scalar_number(keyword, &number.to_string()),
        JsonValue::String(text) => parse_split_numbers(keyword, text),
        JsonValue::Array(items) => items
            .iter()
            .map(|item| match item {
                JsonValue::Number(number) => parse_single_number(keyword, &number.to_string()),
                JsonValue::String(text) => parse_single_number(keyword, text),
                _ => Err(invalid_json_value(keyword, "expected numeric strings or numbers")),
            })
            .collect(),
        _ => Err(invalid_json_value(
            keyword,
            "expected a number, string, or array",
        )),
    }
}

fn parse_scalar_number<T>(keyword: &str, text: &str) -> Result<Vec<T>, DicomJsonError>
where
    T: std::str::FromStr,
{
    Ok(vec![parse_single_number(keyword, text)?])
}

fn parse_split_numbers<T>(keyword: &str, text: &str) -> Result<Vec<T>, DicomJsonError>
where
    T: std::str::FromStr,
{
    split_multi_value(text)
        .into_iter()
        .map(|part| parse_single_number(keyword, &part))
        .collect()
}

fn parse_single_number<T>(keyword: &str, text: &str) -> Result<T, DicomJsonError>
where
    T: std::str::FromStr,
{
    text.trim()
        .parse()
        .map_err(|_| invalid_json_value(keyword, "failed to parse numeric value"))
}

pub(super) fn parse_tag_values(
    keyword: &str,
    value: &JsonValue,
) -> Result<Vec<Tag>, DicomJsonError> {
    match value {
        JsonValue::String(text) => split_multi_value(text)
            .into_iter()
            .map(|part| {
                StandardDataDictionary
                    .parse_tag(part.as_str())
                    .ok_or_else(|| invalid_json_value(keyword, "failed to parse tag reference"))
            })
            .collect(),
        JsonValue::Array(items) => items
            .iter()
            .map(|item| {
                let JsonValue::String(text) = item else {
                    return Err(invalid_json_value(keyword, "expected string tag expressions"));
                };
                StandardDataDictionary
                    .parse_tag(text)
                    .ok_or_else(|| invalid_json_value(keyword, "failed to parse tag reference"))
            })
            .collect(),
        _ => Err(invalid_json_value(
            keyword,
            "expected a string or array of strings",
        )),
    }
}

pub(super) fn split_multi_value(text: &str) -> Vec<String> {
    if text.is_empty() {
        Vec::new()
    } else {
        text.split('\\').map(|part| part.to_owned()).collect()
    }
}

pub(super) fn invalid_json_value(keyword: &str, message: &str) -> DicomJsonError {
    DicomJsonError::InvalidJsonValue {
        keyword: keyword.to_owned(),
        message: message.to_owned(),
    }
}

pub(super) fn apply_meta_element(
    meta_builder: &mut FileMetaTableBuilder,
    element: &DataElement<InMemDicomObject>,
) -> Result<(), DicomJsonError> {
    match element.header().tag() {
        tags::FILE_META_INFORMATION_VERSION => {
            let bytes = element.to_bytes().map_err(|_| DicomJsonError::InvalidJsonValue {
                keyword: keyword_for_tag(element.header().tag()),
                message: "expected binary file meta information version".to_owned(),
            })?;
            if bytes.len() >= 2 {
                *meta_builder = meta_builder.clone().information_version([bytes[0], bytes[1]]);
            }
        }
        tags::MEDIA_STORAGE_SOP_CLASS_UID => {
            *meta_builder = meta_builder.clone().media_storage_sop_class_uid(
                element
                    .to_str()
                    .map_err(|_| {
                        invalid_json_value("MediaStorageSOPClassUID", "expected a string value")
                    })?
                    .into_owned(),
            );
        }
        tags::MEDIA_STORAGE_SOP_INSTANCE_UID => {
            *meta_builder = meta_builder.clone().media_storage_sop_instance_uid(
                element
                    .to_str()
                    .map_err(|_| {
                        invalid_json_value(
                            "MediaStorageSOPInstanceUID",
                            "expected a string value",
                        )
                    })?
                    .into_owned(),
            );
        }
        tags::TRANSFER_SYNTAX_UID => {
            *meta_builder = meta_builder.clone().transfer_syntax(
                element
                    .to_str()
                    .map_err(|_| invalid_json_value("TransferSyntaxUID", "expected a string value"))?
                    .into_owned(),
            );
        }
        tags::IMPLEMENTATION_CLASS_UID => {
            *meta_builder = meta_builder.clone().implementation_class_uid(
                element
                    .to_str()
                    .map_err(|_| {
                        invalid_json_value("ImplementationClassUID", "expected a string value")
                    })?
                    .into_owned(),
            );
        }
        tags::IMPLEMENTATION_VERSION_NAME => {
            *meta_builder = meta_builder.clone().implementation_version_name(
                element
                    .to_str()
                    .map_err(|_| {
                        invalid_json_value("ImplementationVersionName", "expected a string value")
                    })?
                    .into_owned(),
            );
        }
        tags::SOURCE_APPLICATION_ENTITY_TITLE => {
            *meta_builder = meta_builder.clone().source_application_entity_title(
                element
                    .to_str()
                    .map_err(|_| {
                        invalid_json_value(
                            "SourceApplicationEntityTitle",
                            "expected a string value",
                        )
                    })?
                    .into_owned(),
            );
        }
        tags::SENDING_APPLICATION_ENTITY_TITLE => {
            *meta_builder = meta_builder.clone().sending_application_entity_title(
                element
                    .to_str()
                    .map_err(|_| {
                        invalid_json_value(
                            "SendingApplicationEntityTitle",
                            "expected a string value",
                        )
                    })?
                    .into_owned(),
            );
        }
        tags::RECEIVING_APPLICATION_ENTITY_TITLE => {
            *meta_builder = meta_builder.clone().receiving_application_entity_title(
                element
                    .to_str()
                    .map_err(|_| {
                        invalid_json_value(
                            "ReceivingApplicationEntityTitle",
                            "expected a string value",
                        )
                    })?
                    .into_owned(),
            );
        }
        tags::PRIVATE_INFORMATION_CREATOR_UID => {
            *meta_builder = meta_builder.clone().private_information_creator_uid(
                element
                    .to_str()
                    .map_err(|_| {
                        invalid_json_value(
                            "PrivateInformationCreatorUID",
                            "expected a string value",
                        )
                    })?
                    .into_owned(),
            );
        }
        tags::PRIVATE_INFORMATION => {
            *meta_builder = meta_builder.clone().private_information(
                element
                    .to_bytes()
                    .map_err(|_| invalid_json_value("PrivateInformation", "expected binary data"))?
                    .to_vec(),
            );
        }
        _ => {}
    }

    Ok(())
}

pub(super) fn json_number_from_f32(value: f32) -> Option<JsonValue> {
    JsonNumber::from_f64(value as f64).map(JsonValue::Number)
}

pub(super) fn json_number_from_f64(value: f64) -> Option<JsonValue> {
    JsonNumber::from_f64(value).map(JsonValue::Number)
}

fn flat_string_value(value: &JsonValue) -> Option<String> {
    match value {
        JsonValue::String(text) => Some(text.clone()),
        JsonValue::Object(object) => match object.get("Value") {
            Some(JsonValue::String(text)) => Some(text.clone()),
            _ => None,
        },
        _ => None,
    }
}