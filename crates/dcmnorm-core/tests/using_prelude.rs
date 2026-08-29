use dcmnorm_core::prelude::*;

#[test]
fn can_use_prelude() {
    // can refer to `DataElement`, `Tag`, `VR`, and `dicom_value!`
    let elem: DataElement<dcmnorm_core::header::EmptyObject, dcmnorm_core::value::InMemFragment> =
        DataElement::new(
            Tag(0x0010, 0x0010),
            VR::PN,
            dicom_value!(Str, "Simões^João"),
        );
    let length = elem.length().0;
    assert_eq!(length as usize, "Simões^João".len());

    // can call `by_tag`
    assert_eq!(
        dcmnorm_core::dictionary::stub::StubDataDictionary.by_tag(Tag(0x0010, 0x0010)),
        None,
    );
}
