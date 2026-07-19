use std::io::{self, ErrorKind};

use dicom_core::dictionary::{DataDictionary, DataDictionaryEntry};
use dicom_core::header::Header;
use dicom_core::{Tag, VR};
use dicom_dictionary_std::StandardDataDictionary;
use dicom_object::DefaultDicomObject;

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
pub fn set_attribute(object: &mut DefaultDicomObject, tag: Tag, vr: VR, value: String) {
    object.put_str(tag, vr, value);
}

/// Removes a single attribute; returns whether it was present.
pub fn remove_attribute(object: &mut DefaultDicomObject, tag: Tag) -> bool {
    object.remove_element(tag)
}
