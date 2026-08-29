use std::fs;
use std::io::{self, ErrorKind};
use std::path::Path;

use dcmnorm_core::dictionary::DataDictionary;
use dcmnorm_core::Tag;
use dcmnorm_dictionary::StandardDataDictionary;
use dcmnorm_object::ReadPreamble;
use dcmnorm_object::{DefaultDicomObject, OpenFileOptions};

use super::io::{read_dicom_bytes, read_dicom_file};
use super::types::ReadError;

/// A single `--filter`-style request: keep only this tag's element.
#[derive(Clone, Debug)]
pub struct FilterRequest {
    pub tag: Tag,
}

/// Parses keyword/tag-expression strings (e.g. "StudyInstanceUID" or
/// "(0020,000D)") into filter requests.
pub fn parse_filter_requests(keys: &[String]) -> Result<Vec<FilterRequest>, io::Error> {
    let mut requests = Vec::with_capacity(keys.len());

    for key in keys {
        let display_key = key.trim();
        if display_key.is_empty() {
            return Err(io::Error::new(
                ErrorKind::InvalidInput,
                "--filter KEY cannot be empty",
            ));
        }

        let tag = StandardDataDictionary.parse_tag(display_key).ok_or_else(|| {
            io::Error::new(
                ErrorKind::InvalidInput,
                format!(
                    "invalid --filter key '{display_key}'; use a DICOM keyword like StudyInstanceUID or a tag expression like (0020,000D)"
                ),
            )
        })?;

        requests.push(FilterRequest { tag });
    }

    Ok(requests)
}

/// Returns the tag immediately following `tag` in DICOM (group, element)
/// order, or `None` if `tag` is the last representable tag.
pub fn next_tag(tag: Tag) -> Option<Tag> {
    if tag.element() < u16::MAX {
        return Some(Tag(tag.group(), tag.element() + 1));
    }

    if tag.group() < u16::MAX {
        return Some(Tag(tag.group() + 1, 0));
    }

    None
}

/// Reads only as much of the file as is needed to cover every requested tag,
/// stopping right after the highest one. Falls back to a full read if the
/// fast path fails (e.g. a preambleless/raw dataset).
pub fn read_dcmnorm_object_for_filter(
    input_path: &Path,
    requests: &[FilterRequest],
) -> Result<DefaultDicomObject, ReadError> {
    let Some(max_tag) = requests
        .iter()
        .map(|request| request.tag)
        .max_by_key(|tag| ((tag.group() as u32) << 16) | tag.element() as u32)
    else {
        return read_dicom_file(input_path);
    };

    let Some(stop_tag) = next_tag(max_tag) else {
        return read_dicom_file(input_path);
    };

    OpenFileOptions::new()
        .read_preamble(ReadPreamble::Always)
        .read_until(stop_tag)
        .open_file(input_path)
        .or_else(|error| match fs::read(input_path) {
            Ok(bytes) => read_dicom_bytes(&bytes).map_err(|_| error),
            Err(_) => Err(error),
        })
}

/// Strips every element from `object` that isn't among `requests`.
pub fn apply_filter_to_object(object: &mut DefaultDicomObject, requests: &[FilterRequest]) {
    let keep_tags = requests.iter().map(|request| request.tag).collect::<Vec<_>>();
    let remove_tags = object
        .iter()
        .map(|element| element.header().tag)
        .filter(|tag| !keep_tags.contains(tag))
        .collect::<Vec<_>>();

    for tag in remove_tags {
        object.remove_element(tag);
    }
}
