use std::io::{self, ErrorKind};

use dcmnorm_core::dictionary::{DataDictionary, DataDictionaryEntry};
use dcmnorm_core::header::Header;
use dcmnorm_core::{Tag, VR};
use dcmnorm_dictionary::{tags, StandardDataDictionary};
use dcmnorm_object::{DefaultDicomObject, FileMetaTable};

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

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DicomPathSegment {
    Sequence { tag: Tag, index: Option<usize> },
    Leaf(Tag),
}

fn parse_single_tag_token(tok: &str, full_key: &str) -> Result<Tag, io::Error> {
    let clean = tok.trim();
    if let Some(tag) = StandardDataDictionary.parse_tag(clean) {
        return Ok(tag);
    }
    let hex_only: String = clean.chars().filter(|c| c.is_ascii_hexdigit()).collect();
    if hex_only.len() == 8 && (clean.len() == 8 || clean.starts_with('(')) {
        if let (Ok(g), Ok(e)) = (
            u16::from_str_radix(&hex_only[..4], 16),
            u16::from_str_radix(&hex_only[4..], 16),
        ) {
            return Ok(Tag(g, e));
        }
    }
    Err(io::Error::new(
        ErrorKind::InvalidInput,
        format!(
            "invalid DICOM tag/path '{full_key}' (unrecognized segment '{clean}'; use a DICOM keyword like PatientName, nested Seq.0.Leaf / Seq[0].Leaf, or tag like (0010,0010))"
        ),
    ))
}

/// Parses either a flat DICOM tag or a nested sequence selector such as
/// `ReferencedStudySequence.0.ReferencedSOPInstanceUID`,
/// `ReferencedStudySequence[0].ReferencedSOPInstanceUID`, or nested
/// `SeqA.0.SeqB[1].LeafTag`.
pub fn parse_tag_path(key: &str) -> Result<Vec<DicomPathSegment>, io::Error> {
    let trimmed = key.trim();
    if trimmed.is_empty() {
        return Err(io::Error::new(
            ErrorKind::InvalidInput,
            "DICOM attribute key/path cannot be empty",
        ));
    }
    if !trimmed.contains('.') && !trimmed.contains('[') {
        return Ok(vec![DicomPathSegment::Leaf(parse_single_tag_token(
            trimmed, trimmed,
        )?)]);
    }

    let raw_parts: Vec<&str> = trimmed.split('.').collect();
    if raw_parts.iter().any(|p| p.trim().is_empty()) {
        return Err(io::Error::new(
            ErrorKind::InvalidInput,
            format!("invalid DICOM path '{trimmed}': empty segment"),
        ));
    }

    let mut segments = Vec::new();
    let mut i = 0usize;
    while i < raw_parts.len() {
        let part = raw_parts[i].trim();
        let is_last = i + 1 == raw_parts.len();
        if let Some(open_idx) = part.find('[') {
            if !part.ends_with(']') || open_idx == 0 {
                return Err(io::Error::new(
                    ErrorKind::InvalidInput,
                    format!("invalid bracket index in DICOM path '{trimmed}' at '{part}'"),
                ));
            }
            if is_last {
                return Err(io::Error::new(
                    ErrorKind::InvalidInput,
                    format!("invalid DICOM path '{trimmed}': path cannot end with a sequence index '{part}'"),
                ));
            }
            let tag_str = &part[..open_idx];
            let idx_str = part[open_idx + 1..part.len() - 1].trim();
            let index = if idx_str == "*" {
                None
            } else {
                Some(idx_str.parse::<usize>().map_err(|_| {
                    io::Error::new(
                        ErrorKind::InvalidInput,
                        format!(
                            "invalid sequence item index '{idx_str}' in DICOM path '{trimmed}'"
                        ),
                    )
                })?)
            };
            let tag = parse_single_tag_token(tag_str, trimmed)?;
            segments.push(DicomPathSegment::Sequence { tag, index });
            i += 1;
        } else if !is_last {
            let tag = parse_single_tag_token(part, trimmed)?;
            let next_part = raw_parts[i + 1].trim();
            if next_part == "*" {
                if i + 2 >= raw_parts.len() {
                    return Err(io::Error::new(
                        ErrorKind::InvalidInput,
                        format!("invalid DICOM path '{trimmed}': wildcard cannot terminate path"),
                    ));
                }
                segments.push(DicomPathSegment::Sequence { tag, index: None });
                i += 2;
            } else if next_part.chars().all(|c| c.is_ascii_digit()) {
                if i + 2 >= raw_parts.len() {
                    return Err(io::Error::new(
                        ErrorKind::InvalidInput,
                        format!("invalid DICOM path '{trimmed}': path cannot end with numeric item index '{next_part}'"),
                    ));
                }
                let index = next_part.parse::<usize>().map_err(|_| {
                    io::Error::new(
                        ErrorKind::InvalidInput,
                        format!("invalid numeric sequence index '{next_part}' in '{trimmed}'"),
                    )
                })?;
                segments.push(DicomPathSegment::Sequence {
                    tag,
                    index: Some(index),
                });
                i += 2;
            } else {
                segments.push(DicomPathSegment::Sequence { tag, index: None });
                i += 1;
            }
        } else {
            let tag = parse_single_tag_token(part, trimmed)?;
            segments.push(DicomPathSegment::Leaf(tag));
            i += 1;
        }
    }

    if !matches!(segments.last(), Some(DicomPathSegment::Leaf(_))) {
        return Err(io::Error::new(
            ErrorKind::InvalidInput,
            format!("invalid DICOM path '{trimmed}': expected leaf attribute"),
        ));
    }
    Ok(segments)
}

fn remove_segments_in_dataset(
    ds: &mut dcmnorm_object::InMemDicomObject,
    segments: &[DicomPathSegment],
) -> bool {
    let Some((first, rest)) = segments.split_first() else {
        return false;
    };
    match first {
        DicomPathSegment::Leaf(tag) => ds.remove_element(*tag),
        DicomPathSegment::Sequence { tag, index } => {
            let Some(el) = ds.get_mut(*tag) else {
                return false;
            };
            let Some(items) = el.items_mut() else {
                return false;
            };
            let mut removed_any = false;
            match index {
                Some(idx) => {
                    if let Some(item) = items.get_mut(*idx) {
                        removed_any |= remove_segments_in_dataset(item, rest);
                    }
                }
                None => {
                    for item in items.iter_mut() {
                        removed_any |= remove_segments_in_dataset(item, rest);
                    }
                }
            }
            removed_any
        }
    }
}

fn set_segments_in_dataset(
    ds: &mut dcmnorm_object::InMemDicomObject,
    segments: &[DicomPathSegment],
    vr: VR,
    value: &str,
) -> Result<(), io::Error> {
    let Some((first, rest)) = segments.split_first() else {
        return Ok(());
    };
    match first {
        DicomPathSegment::Leaf(tag) => {
            ds.put_str(*tag, vr, value);
            Ok(())
        }
        DicomPathSegment::Sequence { tag, index } => {
            let Some(el) = ds.get_mut(*tag) else {
                return Ok(());
            };
            let Some(items) = el.items_mut() else {
                return Err(io::Error::new(
                    ErrorKind::InvalidInput,
                    format!("DICOM tag {tag} is not a VR::SQ sequence"),
                ));
            };
            match index {
                Some(idx) => {
                    if let Some(item) = items.get_mut(*idx) {
                        set_segments_in_dataset(item, rest, vr, value)?;
                    }
                }
                None => {
                    for item in items.iter_mut() {
                        set_segments_in_dataset(item, rest, vr, value)?;
                    }
                }
            }
            Ok(())
        }
    }
}

/// Removes a flat or nested sequence DICOM attribute (`PatientName`, `Seq.0.Tag`, `Seq[0].Tag`).
pub fn remove_attribute_path(
    object: &mut DefaultDicomObject,
    key: &str,
) -> Result<bool, io::Error> {
    let segments = parse_tag_path(key)?;
    if segments.len() == 1 {
        if let DicomPathSegment::Leaf(tag) = segments[0] {
            return Ok(remove_attribute(object, tag));
        }
    }
    let Some((first, rest)) = segments.split_first() else {
        return Ok(false);
    };
    let DicomPathSegment::Sequence { tag, index } = first else {
        return Ok(false);
    };
    let Some(el) = object.get_mut(*tag) else {
        return Ok(false);
    };
    let Some(items) = el.items_mut() else {
        return Ok(false);
    };
    let mut removed_any = false;
    match index {
        Some(idx) => {
            if let Some(item) = items.get_mut(*idx) {
                removed_any |= remove_segments_in_dataset(item, rest);
            }
        }
        None => {
            for item in items.iter_mut() {
                removed_any |= remove_segments_in_dataset(item, rest);
            }
        }
    }
    Ok(removed_any)
}

/// Sets a flat or nested sequence DICOM attribute (`PatientName`, `Seq.0.Tag`, `Seq[0].Tag`).
pub fn set_attribute_path(
    object: &mut DefaultDicomObject,
    key: &str,
    vr_override: Option<VR>,
    value: String,
) -> Result<(), io::Error> {
    let segments = parse_tag_path(key)?;
    let leaf_tag = match segments.last() {
        Some(DicomPathSegment::Leaf(t)) => *t,
        _ => {
            return Err(io::Error::new(
                ErrorKind::InvalidInput,
                format!("invalid DICOM path '{key}'"),
            ));
        }
    };
    let vr = vr_override.unwrap_or_else(|| {
        StandardDataDictionary
            .by_tag(leaf_tag)
            .map(|entry| entry.vr().relaxed())
            .unwrap_or(VR::LO)
    });
    if vr == VR::SQ {
        return Err(io::Error::new(
            ErrorKind::InvalidInput,
            format!("cannot set VR::SQ sequence attribute '{key}' to scalar string"),
        ));
    }
    if segments.len() == 1 {
        return set_attribute(object, leaf_tag, vr, value);
    }
    let Some((first, rest)) = segments.split_first() else {
        return Ok(());
    };
    let DicomPathSegment::Sequence { tag, index } = first else {
        return Ok(());
    };
    let Some(el) = object.get_mut(*tag) else {
        return Ok(());
    };
    let Some(items) = el.items_mut() else {
        return Err(io::Error::new(
            ErrorKind::InvalidInput,
            format!("DICOM tag {tag} is not a VR::SQ sequence"),
        ));
    };
    match index {
        Some(idx) => {
            if let Some(item) = items.get_mut(*idx) {
                set_segments_in_dataset(item, rest, vr, &value)?;
            }
        }
        None => {
            for item in items.iter_mut() {
                set_segments_in_dataset(item, rest, vr, &value)?;
            }
        }
    }
    Ok(())
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
        tags::SENDING_APPLICATION_ENTITY_TITLE => {
            meta.sending_application_entity_title = Some(value)
        }
        tags::RECEIVING_APPLICATION_ENTITY_TITLE => {
            meta.receiving_application_entity_title = Some(value)
        }
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
        tags::SOURCE_APPLICATION_ENTITY_TITLE => {
            meta.source_application_entity_title.take().is_some()
        }
        tags::SENDING_APPLICATION_ENTITY_TITLE => {
            meta.sending_application_entity_title.take().is_some()
        }
        tags::RECEIVING_APPLICATION_ENTITY_TITLE => {
            meta.receiving_application_entity_title.take().is_some()
        }
        tags::PRIVATE_INFORMATION_CREATOR_UID => {
            meta.private_information_creator_uid.take().is_some()
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dcmnorm_core::value::DataSetSequence;
    use dcmnorm_object::{InMemDicomObject, InMemElement};

    fn build_object_with_sequence() -> DefaultDicomObject {
        let mut item0 = InMemDicomObject::new_empty();
        item0.put_str(tags::PERFORMING_PHYSICIAN_NAME, VR::PN, "DOE^FIRST");
        item0.put_str(tags::SCHEDULED_STATION_NAME, VR::SH, "CT0");

        let mut item1 = InMemDicomObject::new_empty();
        item1.put_str(tags::PERFORMING_PHYSICIAN_NAME, VR::PN, "SMITH^SECOND");
        item1.put_str(tags::SCHEDULED_STATION_NAME, VR::SH, "CT1");

        let mut obj = DefaultDicomObject::new_empty_with_meta(FileMetaTable::default());
        obj.put(InMemElement::new(
            tags::REQUEST_ATTRIBUTES_SEQUENCE,
            VR::SQ,
            DataSetSequence::from(vec![item0, item1]),
        ));
        obj
    }

    #[test]
    fn parse_tag_path_supports_dot_and_bracket_sequence_indices_and_wildcards() {
        let p_dot = parse_tag_path("RequestAttributesSequence.0.PerformingPhysicianName").unwrap();
        assert_eq!(
            p_dot,
            vec![
                DicomPathSegment::Sequence {
                    tag: tags::REQUEST_ATTRIBUTES_SEQUENCE,
                    index: Some(0),
                },
                DicomPathSegment::Leaf(tags::PERFORMING_PHYSICIAN_NAME),
            ]
        );

        let p_bracket =
            parse_tag_path("RequestAttributesSequence[1].PerformingPhysicianName").unwrap();
        assert_eq!(
            p_bracket,
            vec![
                DicomPathSegment::Sequence {
                    tag: tags::REQUEST_ATTRIBUTES_SEQUENCE,
                    index: Some(1),
                },
                DicomPathSegment::Leaf(tags::PERFORMING_PHYSICIAN_NAME),
            ]
        );

        let p_wild = parse_tag_path("RequestAttributesSequence.PerformingPhysicianName").unwrap();
        assert_eq!(
            p_wild,
            vec![
                DicomPathSegment::Sequence {
                    tag: tags::REQUEST_ATTRIBUTES_SEQUENCE,
                    index: None,
                },
                DicomPathSegment::Leaf(tags::PERFORMING_PHYSICIAN_NAME),
            ]
        );

        assert!(parse_tag_path("NotARealDicomAlias").is_err());
    }

    #[test]
    fn set_and_remove_attribute_path_modify_targeted_and_wildcard_sq_items() {
        let mut obj = build_object_with_sequence();

        set_attribute_path(
            &mut obj,
            "RequestAttributesSequence[0].PerformingPhysicianName",
            Some(VR::PN),
            "[REDACTED_PN]".to_string(),
        )
        .unwrap();
        let seq = obj
            .element(tags::REQUEST_ATTRIBUTES_SEQUENCE)
            .unwrap()
            .items()
            .unwrap();
        assert_eq!(
            seq[0]
                .element(tags::PERFORMING_PHYSICIAN_NAME)
                .unwrap()
                .to_str()
                .unwrap(),
            "[REDACTED_PN]"
        );
        assert_eq!(
            seq[1]
                .element(tags::PERFORMING_PHYSICIAN_NAME)
                .unwrap()
                .to_str()
                .unwrap(),
            "SMITH^SECOND"
        );

        assert!(
            remove_attribute_path(&mut obj, "RequestAttributesSequence.ScheduledStationName")
                .unwrap()
        );
        let seq_after = obj
            .element(tags::REQUEST_ATTRIBUTES_SEQUENCE)
            .unwrap()
            .items()
            .unwrap();
        assert!(seq_after[0].element(tags::SCHEDULED_STATION_NAME).is_err());
        assert!(seq_after[1].element(tags::SCHEDULED_STATION_NAME).is_err());
    }
}
