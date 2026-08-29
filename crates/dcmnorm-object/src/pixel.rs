//! [`PixelDataObject`] and [`ApplyOp`] implementations for [`DefaultDicomObject`] - the two
//! traits `dcmnorm-encoding`'s/`dcmnorm-core`'s pixel data codec adapters need to read
//! image-shape attributes from an object and, for the encapsulated-pixel-data writer, apply
//! required post-transcode attribute patches (e.g. Photometric Interpretation adjustments)
//! back onto it.

use std::borrow::Cow;

use dcmnorm_core::header::Tag;
use dcmnorm_core::ops::{AttributeAction, AttributeOp, AttributeSelectorStep, ApplyOp};
use dcmnorm_encoding::adapters::{PixelDataObject, RawPixelData};

use crate::file::{DefaultDicomObject, FileDicomObject};
use crate::mem::{InMemDicomObject, MissingElementError};

const ROWS: Tag = Tag(0x0028, 0x0010);
const COLUMNS: Tag = Tag(0x0028, 0x0011);
const SAMPLES_PER_PIXEL: Tag = Tag(0x0028, 0x0002);
const BITS_ALLOCATED: Tag = Tag(0x0028, 0x0100);
const BITS_STORED: Tag = Tag(0x0028, 0x0101);
const PHOTOMETRIC_INTERPRETATION: Tag = Tag(0x0028, 0x0004);
const NUMBER_OF_FRAMES: Tag = Tag(0x0028, 0x0008);
const PIXEL_DATA: Tag = Tag(0x7FE0, 0x0010);

impl PixelDataObject for FileDicomObject<InMemDicomObject> {
    fn transfer_syntax_uid(&self) -> &str {
        self.meta.transfer_syntax()
    }

    fn rows(&self) -> Option<u16> {
        self.object.get(ROWS)?.to_int().ok()
    }

    fn cols(&self) -> Option<u16> {
        self.object.get(COLUMNS)?.to_int().ok()
    }

    fn samples_per_pixel(&self) -> Option<u16> {
        self.object.get(SAMPLES_PER_PIXEL)?.to_int().ok()
    }

    fn bits_allocated(&self) -> Option<u16> {
        self.object.get(BITS_ALLOCATED)?.to_int().ok()
    }

    fn bits_stored(&self) -> Option<u16> {
        self.object.get(BITS_STORED)?.to_int().ok()
    }

    fn photometric_interpretation(&self) -> Option<&str> {
        // Extract the inner `&str` by matching rather than calling `.trim_end()` straight on
        // the `Cow` - the latter would borrow from the temporary `Cow` value itself rather
        // than from `self`, and not compile. PhotometricInterpretation is a CS-type VR (pure
        // ASCII per PS3.5), so `Cow::Owned` (a lossy non-UTF8 conversion) is not valid DICOM
        // for this tag and treated as absent rather than allocating just to satisfy the
        // signature.
        match self.object.get(PHOTOMETRIC_INTERPRETATION)?.to_str().ok()? {
            Cow::Borrowed(s) => Some(s.trim_end()),
            Cow::Owned(_) => None,
        }
    }

    fn number_of_frames(&self) -> Option<u32> {
        self.object
            .get(NUMBER_OF_FRAMES)
            .and_then(|e| e.to_str().ok())
            .and_then(|s| s.trim().parse().ok())
    }

    fn number_of_fragments(&self) -> Option<u32> {
        match self.object.get(PIXEL_DATA)?.value() {
            dcmnorm_core::value::Value::PixelSequence(seq) => Some(seq.fragments().len() as u32),
            _ => None,
        }
    }

    fn fragment(&self, fragment: usize) -> Option<Cow<'_, [u8]>> {
        match self.object.get(PIXEL_DATA)?.value() {
            dcmnorm_core::value::Value::PixelSequence(seq) => {
                seq.fragments().get(fragment).map(|f| Cow::Borrowed(f.as_slice()))
            }
            dcmnorm_core::value::Value::Primitive(pv) if fragment == 0 => {
                Some(pv.to_bytes())
            }
            _ => None,
        }
    }

    fn offset_table(&self) -> Option<Cow<'_, [u32]>> {
        match self.object.get(PIXEL_DATA)?.value() {
            dcmnorm_core::value::Value::PixelSequence(seq) => Some(Cow::Borrowed(seq.offset_table())),
            _ => None,
        }
    }

    fn raw_pixel_data(&self) -> Option<RawPixelData> {
        match self.object.get(PIXEL_DATA)?.value() {
            dcmnorm_core::value::Value::PixelSequence(seq) => Some(RawPixelData {
                fragments: seq.fragments().iter().cloned().collect(),
                offset_table: seq.offset_table().iter().copied().collect(),
            }),
            dcmnorm_core::value::Value::Primitive(pv) => Some(RawPixelData {
                fragments: std::iter::once(pv.to_bytes().into_owned()).collect(),
                offset_table: Default::default(),
            }),
            _ => None,
        }
    }
}

impl ApplyOp for DefaultDicomObject {
    type Err = MissingElementError;

    fn apply(&mut self, op: AttributeOp) -> Result<(), Self::Err> {
        // Surface a real error only for the one failure mode dcmnorm's own callers actually
        // hit in practice - replacing an attribute that must already exist - matching the
        // `ApplyOp` trait's contract ("no changes to the receiver are made" on error). Every
        // other action already treats absence as a valid outcome per `AttributeAction`'s own
        // documented semantics (e.g. Remove on a missing tag is a no-op, not an error).
        if let AttributeAction::Replace(_) | AttributeAction::ReplaceStr(_) = &op.action {
            let AttributeSelectorStep::Tag(tag) = *op.selector.first_step() else {
                return Ok(());
            };
            if self.object.get(tag).is_none() {
                return Err(MissingElementError { tag });
            }
        }
        self.object.apply(op);
        Ok(())
    }
}
