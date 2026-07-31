use std::io::{self, ErrorKind};

use dicom_core::dictionary::{DataDictionary, DataDictionaryEntry};
use dicom_core::header::Header;
use dicom_core::{Tag, VR};
use dicom_dictionary_std::{tags, StandardDataDictionary};
use dicom_object::{DefaultDicomObject, FileMetaTable};

/// Remove all private tags from a DICOM object in-place
pub fn remove_private_tags_inplace(obj: &mut DefaultDicomObject) {
    let tags_to_remove: Vec<_> = obj
        .iter()
        .filter_map(|el| {
            let tag = el.tag();
            if tag.group() % 2 != 0 {
                Some(tag)
            } else {
                None
            }
        })
        .collect();
    for tag in tags_to_remove {
        let _ = obj.remove_element(tag);
    }
}

/// Parses a DICOM keyword or tag expression (e.g. "PatientName" or
/// "(0010,0010)") for use with [`remove_attribute`].
pub fn parse_tag_key(key: &str) -> Result<Tag, io::Error> {
    let key = key.trim();
    if key.is_empty() {
        return Err(io::Error::new(
            ErrorKind::InvalidInput,
            "--remove KEY cannot be empty",
        ));
    }

    StandardDataDictionary.parse_tag(key).ok_or_else(|| {
        io::Error::new(
            ErrorKind::InvalidInput,
            format!(
                "invalid --remove key '{key}'; use a DICOM keyword like PatientName or a tag expression like (0010,0010)"
            ),
        )
    })
}

/// Parses a `KEY=VALUE` assignment (e.g. "PatientName=DOE^JOHN") into the
/// tag, VR, and raw string value to apply with [`set_attribute`].
pub fn parse_attribute_override(assignment: &str) -> Result<(Tag, VR, String), io::Error> {
    let (raw_key, raw_value) = assignment.split_once('=').ok_or_else(|| {
        io::Error::new(
            ErrorKind::InvalidInput,
            format!(
                "invalid --set value '{assignment}'; expected KEY=VALUE, for example SOPClassUID=1.2.840.10008.5.1.4.1.1.2"
            ),
        )
    })?;

    let key = raw_key.trim();
    if key.is_empty() {
        return Err(io::Error::new(
            ErrorKind::InvalidInput,
            format!("invalid --set value '{assignment}'; KEY cannot be empty"),
        ));
    }

    let tag = StandardDataDictionary.parse_tag(key).ok_or_else(|| {
        io::Error::new(
            ErrorKind::InvalidInput,
            format!(
                "invalid --set key '{key}'; use a DICOM keyword like SOPClassUID or a tag expression like (0008,0016)"
            ),
        )
    })?;

    let vr = StandardDataDictionary
        .by_tag(tag)
        .map(|entry| entry.vr().relaxed())
        .ok_or_else(|| {
            io::Error::new(
                ErrorKind::InvalidInput,
                format!(
                    "could not determine VR for --set key '{key}'; use a standard DICOM attribute"
                ),
            )
        })?;

    if vr == VR::SQ {
        return Err(io::Error::new(
            ErrorKind::InvalidInput,
            format!(
                "--set does not currently support sequence attributes ({key}); set non-sequence elements instead"
            ),
        ));
    }

    Ok((tag, vr, raw_value.to_owned()))
}

/// Sets (inserting or overwriting) a single non-sequence attribute.
///
/// File Meta Information (group 0002) elements live in the object's separate
/// meta table rather than its dataset, so they're routed to
/// [`set_meta_attribute`] instead of being written into the dataset.
pub fn set_attribute(
    object: &mut DefaultDicomObject,
    tag: Tag,
    vr: VR,
    value: String,
) -> Result<(), io::Error> {
    if tag.group() == 0x0002 {
        set_meta_attribute(object.meta_mut(), tag, value)
    } else {
        object.put_str(tag, vr, value);
        Ok(())
    }
}

/// Sets a single File Meta Information (group 0002) element directly on the
/// meta table. Covers the same elements `common::apply_meta_element` supports
/// when building a table from JSON, minus Transfer Syntax UID: changing the
/// on-disk meta transfer syntax here would desync it from the dataset's
/// actual pixel encoding, since this path never transcodes pixel data - use
/// the dedicated transcode operation for that instead.
fn set_meta_attribute(meta: &mut FileMetaTable, tag: Tag, value: String) -> Result<(), io::Error> {
    match tag {
        tags::MEDIA_STORAGE_SOP_CLASS_UID => meta.media_storage_sop_class_uid = value,
        tags::MEDIA_STORAGE_SOP_INSTANCE_UID => meta.media_storage_sop_instance_uid = value,
        tags::IMPLEMENTATION_CLASS_UID => meta.implementation_class_uid = value,
        tags::IMPLEMENTATION_VERSION_NAME => meta.implementation_version_name = Some(value),
        tags::SOURCE_APPLICATION_ENTITY_TITLE => meta.source_application_entity_title = Some(value),
        tags::SENDING_APPLICATION_ENTITY_TITLE => meta.sending_application_entity_title = Some(value),
        tags::RECEIVING_APPLICATION_ENTITY_TITLE => meta.receiving_application_entity_title = Some(value),
        tags::PRIVATE_INFORMATION_CREATOR_UID => meta.private_information_creator_uid = Some(value),
        _ => {
            return Err(io::Error::new(
                ErrorKind::InvalidInput,
                format!("tag {tag} is not a settable File Meta Information element"),
            ));
        }
    }
    Ok(())
}

/// Removes a single attribute; returns whether it was present.
///
/// File Meta Information (group 0002) elements live in the object's separate
/// meta table rather than its dataset, so they're routed to
/// [`remove_meta_attribute`] instead of `remove_element`, which is a no-op
/// for group-0002 tags (they were never in the dataset to begin with).
pub fn remove_attribute(object: &mut DefaultDicomObject, tag: Tag) -> bool {
    if tag.group() == 0x0002 {
        remove_meta_attribute(object.meta_mut(), tag)
    } else {
        object.remove_element(tag)
    }
}

/// Clears a single File Meta Information (group 0002) element on the meta
/// table, returning whether it was previously present. Mandatory String
/// fields (Media Storage SOP Class/Instance UID, Implementation Class UID)
/// have no "absent" representation, so removing them clears to an empty
/// string rather than deleting the field outright.
fn remove_meta_attribute(meta: &mut FileMetaTable, tag: Tag) -> bool {
    match tag {
        tags::MEDIA_STORAGE_SOP_CLASS_UID => {
            let was_present = !meta.media_storage_sop_class_uid.is_empty();
            meta.media_storage_sop_class_uid = String::new();
            was_present
        }
        tags::MEDIA_STORAGE_SOP_INSTANCE_UID => {
            let was_present = !meta.media_storage_sop_instance_uid.is_empty();
            meta.media_storage_sop_instance_uid = String::new();
            was_present
        }
        tags::IMPLEMENTATION_CLASS_UID => {
            let was_present = !meta.implementation_class_uid.is_empty();
            meta.implementation_class_uid = String::new();
            was_present
        }
        tags::IMPLEMENTATION_VERSION_NAME => meta.implementation_version_name.take().is_some(),
        tags::SOURCE_APPLICATION_ENTITY_TITLE => meta.source_application_entity_title.take().is_some(),
        tags::SENDING_APPLICATION_ENTITY_TITLE => meta.sending_application_entity_title.take().is_some(),
        tags::RECEIVING_APPLICATION_ENTITY_TITLE => meta.receiving_application_entity_title.take().is_some(),
        tags::PRIVATE_INFORMATION_CREATOR_UID => meta.private_information_creator_uid.take().is_some(),
        _ => false,
    }
}
