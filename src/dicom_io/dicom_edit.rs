use dicom_core::header::Header;
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
