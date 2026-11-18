//! The committed binding constants must be what the generator produces from the current
//! dictionary, so a dictionary update can't leave a binding behind.

#[test]
fn generated_sources_are_up_to_date() {
    let stale = dcmnorm_codegen::stale_outputs(&dcmnorm_codegen::repo_root());
    assert!(
        stale.is_empty(),
        "out of date: {stale:?} - run `cargo run -p dcmnorm-codegen` and commit the result"
    );
}

#[test]
fn names_are_the_standard_keywords() {
    let tags = dcmnorm_codegen::tags();
    let find = |keyword: &str| {
        tags.iter()
            .find(|entry| entry.keyword == keyword)
            .map(|entry| entry.tag)
    };
    assert_eq!(find("StudyInstanceUID"), Some(0x0020_000D));
    assert_eq!(find("PixelData"), Some(0x7FE0_0010));
    assert_eq!(find("OverlayData"), Some(0x6000_3000));
    let syntaxes = dcmnorm_codegen::transfer_syntaxes();
    let ts = |keyword: &str| syntaxes.iter().find(|ts| ts.keyword == keyword).unwrap();
    assert!(!ts("ExplicitVRLittleEndian").encapsulated && ts("ExplicitVRLittleEndian").explicit_vr);
    assert!(!ts("ImplicitVRLittleEndian").explicit_vr);
    assert!(ts("ExplicitVRBigEndian").big_endian && ts("ExplicitVRBigEndian").retired);
    assert!(ts("DeflatedExplicitVRLittleEndian").deflated);
    assert!(ts("JPEGBaseline8Bit").lossy && ts("JPEGBaseline8Bit").compressed);
    assert!(!ts("JPEG2000Lossless").lossy && ts("JPEG2000").lossy);
    assert!(ts("RLELossless").compressed && !ts("RLELossless").lossy);
    assert!(ts("EncapsulatedUncompressedExplicitVRLittleEndian").encapsulated);
    assert!(!ts("EncapsulatedUncompressedExplicitVRLittleEndian").compressed);
    assert!(ts("MPEG4HP41").lossy && ts("HEVCMP51").lossy);
    let sop = dcmnorm_codegen::sop_classes();
    assert!(
        sop.iter()
            .any(|entry| entry.keyword == "CTImageStorage"
                && entry.uid == "1.2.840.10008.5.1.4.1.1.2")
    );
    assert!(dcmnorm_codegen::vrs().len() >= 34);
}
