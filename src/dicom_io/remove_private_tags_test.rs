use dicom_core::{DataElement, PrimitiveValue, Tag, VR};
use dicom_dictionary_std::tags;
use dcmnorm::dicom_io::{read_dicom_bytes, write_dicom_bytes};

#[test]
fn remove_private_tags_removes_all_private_tags() {
    // Create a DICOM object with private tags
    let mut object = read_dicom_bytes(include_bytes!("../../test/files/dx.dcm")).unwrap();
    // Insert a private tag
    let private_tag = Tag(0x0013, 0x1010); // group 0x0013 is private
    object.put(DataElement::new(private_tag, VR::LO, PrimitiveValue::from("PRIVATE")));
    // Insert a standard tag
    let std_tag = tags::PATIENT_NAME;
    object.put(DataElement::new(std_tag, VR::PN, PrimitiveValue::from("John^Doe")));

    // Remove private tags
    super::remove_private_tags_inplace(&mut object);

    // Private tag should be gone
    assert!(object.element(private_tag).is_err(), "Private tag was not removed");
    // Standard tag should remain
    assert_eq!(object.element(std_tag).unwrap().to_str().unwrap(), "John^Doe");
}
