use dicom_core::value::Value as DicomValue;
use dicom_core::{PrimitiveValue, Tag, VR};
use dicom_dictionary_std::tags;
use dicom_object::mem::InMemElement;
use dicom_object::{DefaultDicomObject, InMemDicomObject};
use serde_json::{Map as JsonMap, Number as JsonNumber, Value as JsonValue};

use super::bulk_data::{
    bulk_representation, is_bulk_value, needs_custom_standard_bulk, raw_bytes_to_dicom_value,
    resolve_standard_bulk_bytes,
};
use super::common::{apply_meta_element, extract_transfer_syntax_from_standard, keyword_for_tag, tag_key};
use super::types::{BulkRepresentation, DicomJsonBulkDataMode, DicomJsonError, DicomJsonWriteOptions};

pub(super) fn write_standard_json_value(
    object: &DefaultDicomObject,
    options: DicomJsonWriteOptions<'_>,
) -> Result<JsonValue, DicomJsonError> {
    let mut json = JsonMap::new();

    for element in object.meta().to_element_iter() {
        if element.header().tag == tags::FILE_META_INFORMATION_GROUP_LENGTH {
            continue;
        }

        let value = if needs_custom_standard_bulk(element.header().tag, element.vr(), element.value()) {
            custom_standard_element_json(
                element.header().tag,
                element.vr(),
                element.value(),
                options,
            )?
        } else {
            let DicomValue::Primitive(primitive) = element.value() else {
                unreachable!();
            };
            let meta_element: InMemElement =
                InMemElement::new(element.header().tag, element.vr(), primitive.clone());
            dicom_json::to_value(meta_element)?
        };

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
        let value = if needs_custom_standard_bulk(element.header().tag, element.vr(), element.value())
            || (options.bulk_data_mode == DicomJsonBulkDataMode::Uri
                && options.bulk_data_source.is_some()
                && is_bulk_value(element.header().tag, element.vr(), element.value()))
        {
            custom_standard_element_json(
                element.header().tag,
                element.vr(),
                element.value(),
                options,
            )?
        } else {
            dicom_json::to_value(element.clone())?
        };

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
        .unwrap_or_else(|| dicom_dictionary_std::uids::EXPLICIT_VR_LITTLE_ENDIAN.to_owned());

    let mut dataset_elements = Vec::new();
    let mut meta_builder = dicom_object::FileMetaTableBuilder::new();

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

        let cleaned = clean_standard_element_json(tag_text, element_json)?;
        let vr = standard_element_vr(tag_text, &cleaned)?;
        let value = standard_json_to_dicom_value(
            tag,
            vr,
            &cleaned,
            bulk_data_source,
            transfer_syntax_uid.as_str(),
        )?;
        let element = dicom_core::DataElement::new(tag, vr, value);

        if tag.group() == 0x0002 {
            apply_meta_element(&mut meta_builder, &element)?;
        } else {
            dataset_elements.push(element);
        }
    }

    let dataset = InMemDicomObject::from_element_iter(dataset_elements);
    Ok(dataset.with_meta(meta_builder.transfer_syntax(transfer_syntax_uid))?)
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

    object.insert("Keyword".to_owned(), JsonValue::String(keyword_for_tag(tag)));
    object.insert("VM".to_owned(), JsonValue::Number(JsonNumber::from(multiplicity)));
    Ok(JsonValue::Object(object))
}

fn custom_standard_element_json<I, P>(
    tag: Tag,
    vr: VR,
    value: &DicomValue<I, P>,
    options: DicomJsonWriteOptions<'_>,
) -> Result<JsonValue, DicomJsonError>
where
    P: AsRef<[u8]>,
{
    let mut object = JsonMap::new();
    object.insert("vr".to_owned(), JsonValue::String(vr.to_string().to_owned()));

    match bulk_representation(tag, vr, value, options)? {
        BulkRepresentation::Uri(uri) => {
            object.insert("BulkDataURI".to_owned(), JsonValue::String(uri));
        }
        BulkRepresentation::InlineBinary(encoded) => {
            object.insert("InlineBinary".to_owned(), JsonValue::String(encoded));
        }
    }

    Ok(JsonValue::Object(object))
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

    vr.parse().map_err(|_| DicomJsonError::InvalidStandardElement {
        tag: tag_text.to_owned(),
        message: format!("invalid VR {vr}"),
    })
}

fn standard_json_to_dicom_value(
    tag: Tag,
    vr: VR,
    element_json: &JsonValue,
    bulk_data_source: Option<&[u8]>,
    transfer_syntax_uid: &str,
) -> Result<DicomValue<InMemDicomObject>, DicomJsonError> {
    let JsonValue::Object(object) = element_json else {
        return Err(DicomJsonError::InvalidStandardElement {
            tag: tag_key(tag),
            message: "expected an object".to_owned(),
        });
    };

    if let Some(bytes) = resolve_standard_bulk_bytes(tag, vr, object, bulk_data_source)? {
        return raw_bytes_to_dicom_value(tag, vr, &bytes, transfer_syntax_uid);
    }

    let mut mini = JsonMap::new();
    mini.insert(tag_key(tag), element_json.clone());
    let object: InMemDicomObject = dicom_json::from_value(JsonValue::Object(mini))?;
    let element = object
        .into_iter()
        .next()
        .ok_or(DicomJsonError::InvalidStandardElement {
            tag: tag_key(tag),
            message: "empty element map".to_owned(),
        })?;
    Ok(element.into_value())
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