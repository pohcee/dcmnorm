//! This crate implements standard DICOM dictionaries and constants.
//!
//! ## Run-time dictinaries
//!
//! The following modules provide definitions for dictionaries
//! which can be queried during a program's lifetime:
//!  
//! - [`data_element`]: Contains all information about the
//!   DICOM attributes specified in the standard,
//!   and it will be used by default in most other abstractions available.
//!   When not using private tags, this dictionary should suffice.
//! - `sop_class` (requires Cargo feature **sop-class**):
//!   Contains information about DICOM Service-Object Pair (SOP) classes
//!   and their respective unique identifiers.
//!
//! The records in these dictionaries are typically collected
//! from [DICOM PS3.6] directly,
//! but they may be obtained through other sources.
//! Each dictionary is provided as a singleton
//! behind a unit type for efficiency and ease of use.
//!
//! [DICOM PS3.6]: https://dicom.nema.org/medical/dicom/current/output/chtml/part06/ps3.6.html
//!
//! ## Constants
//!
//! The following modules contain constant declarations,
//! which perform an equivalent mapping at compile time,
//! thus without incurring a look-up cost:
//!
//! - [`tags`], which map an attribute alias to a DICOM tag
//! - [`uids`], for various normative DICOM unique identifiers
pub mod data_element;

#[cfg(feature = "sop-class")]
pub mod sop_class;
pub mod tags;
pub mod uids;

pub use data_element::{StandardDataDictionary, StandardDataDictionaryRegistry};
#[cfg(feature = "sop-class")]
pub use sop_class::StandardSopClassDictionary;

#[cfg(test)]
mod tests {
    use dcmnorm_core::Tag;

    /// tests for just a few attributes to make sure that the tag constants
    /// were well installed into the crate
    #[test]
    fn tags_constants_available() {
        use crate::tags::*;
        assert_eq!(PATIENT_NAME, Tag(0x0010, 0x0010));
        assert_eq!(MODALITY, Tag(0x0008, 0x0060));
        assert_eq!(PIXEL_DATA, Tag(0x7FE0, 0x0010));
        assert_eq!(STATUS, Tag(0x0000, 0x0900));
    }

    /// tests for the presence of a few UID constants
    #[test]
    fn uids_constants_available() {
        use crate::uids::*;
        assert_eq!(EXPLICIT_VR_LITTLE_ENDIAN, "1.2.840.10008.1.2.1");
        assert_eq!(VERIFICATION, "1.2.840.10008.1.1");
        assert_eq!(HOT_IRON_PALETTE, "1.2.840.10008.1.5.1");
        assert_eq!(
            PATIENT_ROOT_QUERY_RETRIEVE_INFORMATION_MODEL_FIND,
            "1.2.840.10008.5.1.4.1.2.1.1"
        );
        assert_eq!(
            STUDY_ROOT_QUERY_RETRIEVE_INFORMATION_MODEL_MOVE,
            "1.2.840.10008.5.1.4.1.2.2.2"
        );
    }

    /// This project regression test (not from upstream dicom-rs): walks every entry in the
    /// mechanically-ported tag table and confirms `parse_tag(keyword)` resolves to the
    /// entry's tag, and that looking that tag back up (`by_tag`) returns to the same
    /// alias - i.e. the keyword <-> tag mapping round-trips both directions for the whole
    /// table, not just the handful of attributes spot-checked above. This is the acceptance
    /// test for Phase 2 of the dicom-rs removal plan: a mechanical port should preserve
    /// every entry's identity exactly, and this is cheap insurance against a transcription
    /// slip (e.g. a truncated copy) going unnoticed.
    #[test]
    fn every_entry_round_trips_through_parse_tag_and_by_tag() {
        use crate::data_element::StandardDataDictionary;
        use crate::tags::ENTRIES;
        use dcmnorm_core::dictionary::{DataDictionary, DataDictionaryEntry};

        let dict = StandardDataDictionary;
        let mut checked = 0usize;

        for entry in ENTRIES {
            let tag = entry.tag_range().inner();

            let parsed = dict
                .parse_tag(entry.alias)
                .unwrap_or_else(|| panic!("parse_tag({:?}) returned None", entry.alias));
            assert_eq!(
                parsed, tag,
                "parse_tag({:?}) resolved to {:?}, expected {:?}",
                entry.alias, parsed, tag
            );

            let looked_up = dict
                .by_tag(tag)
                .unwrap_or_else(|| panic!("by_tag({tag:?}) (for {:?}) returned None", entry.alias));
            assert_eq!(
                looked_up.alias, entry.alias,
                "by_tag({tag:?}) round-tripped to alias {:?}, expected {:?}",
                looked_up.alias, entry.alias
            );

            checked += 1;
        }

        // A sanity floor on table size, so a catastrophic transcription failure (e.g. an
        // empty or truncated ENTRIES array) fails loudly here instead of this test
        // trivially "passing" over zero entries.
        assert!(
            checked > 3000,
            "expected the full ~4000-entry standard dictionary, only walked {}",
            checked
        );
    }
}
