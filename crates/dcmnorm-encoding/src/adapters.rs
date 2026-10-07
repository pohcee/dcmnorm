//! Core module for building pixel data adapters.
//!
//! This module contains the core types and traits
//! for consumers and implementers of
//! transfer syntaxes with encapsulated pixel data.
//!
//! Complete DICOM object types
//! (such as `FileDicomObject<InMemDicomObject>`)
//! implement the [`PixelDataObject`] trait.
//! Transfer syntaxes which define an encapsulated pixel data encoding
//! need to provide suitable implementations of
//! [`PixelDataReader`] and [`PixelDataWriter`]
//! to be able to decode and encode imaging data, respectively.

use dcmnorm_core::{ops::AttributeOp, value::C};
use snafu::Snafu;
use std::borrow::Cow;

/// The possible error conditions when decoding (reading) pixel data.
///
/// Users of this type are free to handle errors based on their variant,
/// but should not make decisions based on the display message,
/// since that is not considered part of the API
/// and may change on any new release.
///
/// Implementers of transfer syntaxes
/// are recommended to choose the most fitting error variant
/// for the tested condition.
/// When no suitable variant is available,
/// the [`Custom`](DecodeError::Custom) variant may be used.
/// See also [`snafu`] for guidance on using context selectors.
#[derive(Debug, Snafu)]
#[non_exhaustive]
#[snafu(visibility(pub), module)]
pub enum DecodeError {
    /// A custom error occurred when decoding,
    /// reported as a dynamic error value with a message.
    ///
    /// The [`whatever!`](snafu::whatever) macro can be used
    /// to easily create an error of this kind.
    #[snafu(whatever, display("{}", message))]
    Custom {
        /// The error message.
        message: String,
        /// The underlying error cause, if any.
        #[snafu(source(from(Box<dyn std::error::Error + Send + Sync + 'static>, Some)))]
        source: Option<Box<dyn std::error::Error + Send + Sync + 'static>>,
    },

    /// The input pixel data is not encapsulated.
    ///
    /// Either the image needs no decoding
    /// or the compressed imaging data was in a flat pixel data element by mistake.
    NotEncapsulated,

    /// The requested frame range is outside the given object's frame range.
    FrameRangeOutOfBounds,

    /// A required attribute is missing
    /// from the DICOM object representing the image.
    #[snafu(display("Missing required attribute `{}`", name))]
    MissingAttribute { name: &'static str },
}

/// The possible error conditions when encoding (writing) pixel data.
///
/// Users of this type are free to handle errors based on their variant,
/// but should not make decisions based on the display message,
/// since that is not considered part of the API
/// and may change on any new release.
///
/// Implementers of transfer syntaxes
/// are recommended to choose the most fitting error variant
/// for the tested condition.
/// When no suitable variant is available,
/// the [`Custom`](EncodeError::Custom) variant may be used.
/// See also [`snafu`] for guidance on using context selectors.
#[derive(Debug, Snafu)]
#[non_exhaustive]
#[snafu(visibility(pub), module)]
pub enum EncodeError {
    /// A custom error when encoding fails.
    /// Read the `message` and the underlying `source`
    /// for more details.
    #[snafu(whatever, display("{}", message))]
    Custom {
        /// The error message.
        message: String,
        /// The underlying error cause, if any.
        #[snafu(source(from(Box<dyn std::error::Error + Send + Sync + 'static>, Some)))]
        source: Option<Box<dyn std::error::Error + Send + Sync + 'static>>,
    },

    /// Input pixel data is not native, should be decoded first.
    NotNative,

    /// The requested frame range is outside the given object's frame range.
    FrameRangeOutOfBounds,

    /// A required attribute is missing
    /// from the DICOM object representing the image.
    #[snafu(display("Missing required attribute `{}`", name))]
    MissingAttribute { name: &'static str },
}

/// The result of decoding (reading) pixel data
pub type DecodeResult<T, E = DecodeError> = Result<T, E>;

/// The result of encoding (writing) pixel data
pub type EncodeResult<T, E = EncodeError> = Result<T, E>;

#[derive(Debug)]
pub struct RawPixelData {
    /// Either a byte slice/vector if native pixel data
    /// or byte fragments if encapsulated
    pub fragments: C<Vec<u8>>,

    /// The offset table for the fragments,
    /// or empty if there is none
    pub offset_table: C<u32>,
}

/// A DICOM object trait to be interpreted as pixel data.
///
/// This trait extends the concept of DICOM object
/// as defined in [`dicom_object`],
/// in order to retrieve important pieces of the object
/// for pixel data decoding into images or multi-dimensional arrays.
///
/// It is defined in this crate so that
/// transfer syntax implementers only have to depend on `dcmnorm_encoding`.
///
/// [`dicom_object`]: https://docs.rs/dicom_object
pub trait PixelDataObject {
    /// Return the object's transfer syntax UID.
    fn transfer_syntax_uid(&self) -> &str;

    /// Return the _Rows_, or `None` if it is not found
    fn rows(&self) -> Option<u16>;

    /// Return the _Columns_, or `None` if it is not found
    fn cols(&self) -> Option<u16>;

    /// Return the _Samples Per Pixel_, or `None` if it is not found
    fn samples_per_pixel(&self) -> Option<u16>;

    /// Return the _Bits Allocated_, or `None` if it is not defined
    fn bits_allocated(&self) -> Option<u16>;

    /// Return the _Bits Stored_, or `None` if it is not defined
    fn bits_stored(&self) -> Option<u16>;

    /// Return the _Photometric Interpretation_,
    /// with trailing whitespace removed,
    /// or `None` if it is not defined
    fn photometric_interpretation(&self) -> Option<&str>;

    /// Return the _Number Of Frames_, or `None` if it is not defined
    fn number_of_frames(&self) -> Option<u32>;

    /// Returns the _Number of Fragments_, or `None` for native pixel data
    fn number_of_fragments(&self) -> Option<u32>;

    /// Return a specific encoded pixel fragment by index
    /// (where 0 is the first fragment after the basic offset table)
    /// as a [`Cow<[u8]>`][1],
    /// or `None` if no such fragment is available.
    ///
    /// In the case of native (non-encapsulated) pixel data,
    /// the whole data may be obtained
    /// by requesting fragment number 0.
    ///
    /// [1]: std::borrow::Cow
    fn fragment(&self, fragment: usize) -> Option<Cow<'_, [u8]>>;

    /// Return the object's offset table,
    /// or `None` if no offset table is available.
    fn offset_table(&self) -> Option<Cow<'_, [u32]>>;

    /// Should return either a byte slice/vector if the pixel data is native
    /// or the list of byte fragments and offset table if encapsulated.
    ///
    /// Returns `None` if no pixel data is found.
    fn raw_pixel_data(&self) -> Option<RawPixelData>;
}

/// Custom options when encoding pixel data into an encapsulated form.
#[derive(Debug, Default, Clone)]
#[non_exhaustive]
pub struct EncodeOptions {
    /// The quality of the output image as a number between 0 and 100,
    /// where 100 is the best quality that the encapsulated form can achieve
    /// and smaller values represent smaller data size
    /// with an increasingly higher error.
    /// It is ignored if the transfer syntax only supports lossless compression.
    /// If it does support lossless compression,
    /// it is expected that a quality of 100 results in
    /// a mathematically lossless encoding.
    ///
    /// If this option is not specified,
    /// the output quality is decided automatically by the underlying adapter.
    pub quality: Option<u8>,

    /// The amount of effort that the encoder may take to encode the pixel data,
    /// as a number between 0 and 100.
    /// If supported, higher values result in better compression,
    /// at the expense of more processing time.
    /// Encoders are not required to support this option.
    /// If this option is not specified,
    /// the actual effort is decided by the underlying adapter.
    pub effort: Option<u8>,
}

impl EncodeOptions {
    pub fn new() -> Self {
        Self::default()
    }
}

/// Trait object responsible for decoding
/// pixel data based on the transfer syntax.
///
/// A transfer syntax with support for decoding encapsulated pixel data
/// would implement these methods.
pub trait PixelDataReader {
    /// Decode the given DICOM object
    /// containing encapsulated pixel data
    /// into native pixel data as a byte stream in little endian,
    /// appending these bytes to the given vector `dst`.
    ///
    /// It is a necessary precondition that the object's pixel data
    /// is encoded in accordance to the transfer syntax(es)
    /// supported by this adapter.
    /// A `NotEncapsulated` error is returned otherwise.
    ///
    /// The output is a sequence of native pixel values
    /// which follow the image properties of the given object
    /// _save for the photometric interpretation and planar configuration_.
    /// If the image has 3 samples per pixel,
    /// the output must be in RGB with each pixel contiguous in memory
    /// (planar configuration of 0).
    /// However, if the image is monochrome,
    /// the output should retain the photometric interpretation of the source object
    /// (so that images in _MONOCHROME1_ continue to be in _MONOCHROME1_
    /// and images in _MONOCHROME2_ continue to be in _MONOCHROME2_).
    fn decode(&self, src: &dyn PixelDataObject, dst: &mut Vec<u8>) -> DecodeResult<()> {
        let frames = src.number_of_frames().unwrap_or(1);
        for frame in 0..frames {
            self.decode_frame(src, frame, dst)?;
        }
        Ok(())
    }

    /// Decode the given DICOM object
    /// containing encapsulated pixel data
    /// into native pixel data of a single frame
    /// as a byte stream in little endian,
    /// appending these bytes to the given vector `dst`.
    ///
    /// The frame index is 0-based.
    ///
    /// It is a necessary precondition that the object's pixel data
    /// is encoded in accordance to the transfer syntax(es)
    /// supported by this adapter.
    /// A `NotEncapsulated` error is returned otherwise.
    ///
    /// The output is a sequence of native pixel values of a frame
    /// which follow the image properties of the given object
    /// _save for the photometric interpretation and planar configuration_.
    /// If the image has 3 samples per pixel,
    /// the output must be in RGB with each pixel contiguous in memory
    /// (planar configuration of 0).
    /// For pixel data with a single sample per pixel,
    /// the output shall retain the photometric interpretation
    /// declared in the original object
    /// if it is one of _MONOCHROME1_, _MONOCHROME2_, or _PALETTE COLOR_.
    /// For any other photometric interpretation,
    /// the output shall be assumed to be in _MONOCHROME2_.
    fn decode_frame(
        &self,
        src: &dyn PixelDataObject,
        frame: u32,
        dst: &mut Vec<u8>,
    ) -> DecodeResult<()>;
}

/// Trait object responsible for encoding
/// pixel data based on a certain transfer syntax.
///
/// A transfer syntax with support for creating compressed pixel data
/// would implement these methods.
pub trait PixelDataWriter {
    /// Encode a DICOM object's image into the format supported by this adapter,
    /// writing a byte stream of pixel data fragment values
    /// to the given vector `dst`
    /// and the offsets to each decoded frame into `offset_table`.
    ///
    /// New data is appended to `dst` and `offset_table`,
    /// which are not cleared before writing.
    ///
    /// All implementations are required to support
    /// writing the object's pixel data when it is in a _native encoding_.
    /// If the given pixel data object is not in a native encoding,
    /// and this writer does not support transcoding
    /// from that encoding to the target transfer syntax,
    /// a `NotNative` error is returned instead.
    ///
    /// When the operation is successful,
    /// a listing of attribute changes is returned,
    /// comprising the sequence of operations that the DICOM object
    /// should consider upon assuming the new encoding.
    fn encode(
        &self,
        src: &dyn PixelDataObject,
        options: EncodeOptions,
        dst: &mut Vec<Vec<u8>>,
        offset_table: &mut Vec<u32>,
    ) -> EncodeResult<Vec<AttributeOp>> {
        let frames = src.number_of_frames().unwrap_or(1);
        let mut out = Vec::new();
        let mut frame_offset = 0;
        let mut basic_offset = true;
        let prev_offset_table_len = offset_table.len();
        for frame in 0..frames {
            let mut frame_data = Vec::new();
            out = self.encode_frame(src, frame, options.clone(), &mut frame_data)?;
            if basic_offset {
                // push frame offset to basic offset table
                offset_table.push(frame_offset);

                if frame < frames - 1 {
                    // and calculate the next one (ensuring that offset fits in a u32).
                    // Each offset is measured from the first byte of the first fragment's item
                    // header, so it advances by the fragment's item header (8 bytes) plus its
                    // value length *as written*: an odd-length fragment is padded to even length
                    // when serialized (PS3.5 §7.1), so count that padding byte too.
                    let written_len = frame_data.len() + (frame_data.len() & 1);
                    let new_frame_offset = <u32 as std::convert::TryFrom<usize>>::try_from(written_len)
                        .ok()
                        .and_then(|delta| delta.checked_add(8))
                        .and_then(|delta| frame_offset.checked_add(delta));
                    if let Some(v) = new_frame_offset {
                        // update the offset for the next frame and continue normally
                        frame_offset = v;
                    } else {
                        // cannot build a basic offset table,
                        // clear everything done so far and leave it empty
                        offset_table.truncate(prev_offset_table_len);
                        basic_offset = false;
                    }
                }
            }
            dst.push(frame_data);
        }
        Ok(out)
    }

    /// Encode a single frame of a DICOM object's image
    /// into the format supported by this adapter,
    /// by writing a byte stream of pixel data values
    /// into the given destination.
    /// The bytes written comprise a single pixel data fragment
    /// in its entirety.
    ///
    /// New data is appended to `dst`,
    /// keeping all bytes previously present before writing.
    ///
    /// All implementations are required to support
    /// writing the object's pixel data when it is in a _native encoding_.
    /// If the given pixel data object is not in a native encoding,
    /// and this writer does not support transcoding
    /// from that encoding to the target transfer syntax,
    /// a `NotNative` error is returned instead.
    ///
    /// When the operation is successful,
    /// a listing of attribute changes is returned,
    /// comprising the sequence of operations that the DICOM object
    /// should consider upon assuming the new encoding.
    fn encode_frame(
        &self,
        src: &dyn PixelDataObject,
        frame: u32,
        options: EncodeOptions,
        dst: &mut Vec<u8>,
    ) -> EncodeResult<Vec<AttributeOp>>;
}

/// Alias type for a dynamically dispatched pixel data reader.
pub type DynPixelDataReader = Box<dyn PixelDataReader + Send + Sync + 'static>;

/// Alias type for a dynamically dispatched pixel data writer.
pub type DynPixelDataWriter = Box<dyn PixelDataWriter + Send + Sync + 'static>;

/// An immaterial type representing an adapter which is never provided.
///
/// This type may be used as the type parameters `R` and `W`
/// of [`TransferSyntax`](crate::transfer_syntax::TransferSyntax)
/// when representing a transfer syntax which
/// either does not support reading and writing imaging data,
/// or when such support is not needed in the first place.
#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum NeverPixelAdapter {}

impl PixelDataReader for NeverPixelAdapter {
    fn decode(&self, _src: &dyn PixelDataObject, _dst: &mut Vec<u8>) -> DecodeResult<()> {
        unreachable!()
    }

    fn decode_frame(
        &self,
        _src: &dyn PixelDataObject,
        _frame: u32,
        _dst: &mut Vec<u8>,
    ) -> DecodeResult<()> {
        unreachable!()
    }
}

impl PixelDataWriter for NeverPixelAdapter {
    fn encode(
        &self,
        _src: &dyn PixelDataObject,
        _options: EncodeOptions,
        _dst: &mut Vec<Vec<u8>>,
        _offset_table: &mut Vec<u32>,
    ) -> EncodeResult<Vec<AttributeOp>> {
        unreachable!()
    }

    fn encode_frame(
        &self,
        _src: &dyn PixelDataObject,
        _frame: u32,
        _options: EncodeOptions,
        _dst: &mut Vec<u8>,
    ) -> EncodeResult<Vec<AttributeOp>> {
        unreachable!()
    }
}

impl PixelDataReader for crate::transfer_syntax::NeverAdapter {
    fn decode(&self, _src: &dyn PixelDataObject, _dst: &mut Vec<u8>) -> DecodeResult<()> {
        unreachable!()
    }

    fn decode_frame(
        &self,
        _src: &dyn PixelDataObject,
        _frame: u32,
        _dst: &mut Vec<u8>,
    ) -> DecodeResult<()> {
        unreachable!()
    }
}

impl PixelDataWriter for crate::transfer_syntax::NeverAdapter {
    fn encode(
        &self,
        _src: &dyn PixelDataObject,
        _options: EncodeOptions,
        _dst: &mut Vec<Vec<u8>>,
        _offset_table: &mut Vec<u32>,
    ) -> EncodeResult<Vec<AttributeOp>> {
        unreachable!()
    }

    fn encode_frame(
        &self,
        _src: &dyn PixelDataObject,
        _frame: u32,
        _options: EncodeOptions,
        _dst: &mut Vec<u8>,
    ) -> EncodeResult<Vec<AttributeOp>> {
        unreachable!()
    }
}

#[cfg(test)]
mod tests {
    use super::{EncodeOptions, EncodeResult, PixelDataObject, PixelDataWriter, RawPixelData};
    use dcmnorm_core::ops::AttributeOp;
    use std::borrow::Cow;

    /// Only `number_of_frames` matters to the default `PixelDataWriter::encode` implementation.
    struct Frames(u32);

    impl PixelDataObject for Frames {
        fn transfer_syntax_uid(&self) -> &str { "1.2.840.10008.1.2.1" }
        fn rows(&self) -> Option<u16> { Some(1) }
        fn cols(&self) -> Option<u16> { Some(1) }
        fn samples_per_pixel(&self) -> Option<u16> { Some(1) }
        fn bits_allocated(&self) -> Option<u16> { Some(8) }
        fn bits_stored(&self) -> Option<u16> { Some(8) }
        fn photometric_interpretation(&self) -> Option<&str> { Some("MONOCHROME2") }
        fn number_of_frames(&self) -> Option<u32> { Some(self.0) }
        fn number_of_fragments(&self) -> Option<u32> { Some(1) }
        fn fragment(&self, _fragment: usize) -> Option<Cow<'_, [u8]>> { None }
        fn offset_table(&self) -> Option<Cow<'_, [u32]>> { None }
        fn raw_pixel_data(&self) -> Option<RawPixelData> { None }
    }

    /// Writes frame `i` as a fragment of `lengths[i]` bytes.
    struct FixedLengths(Vec<usize>);

    impl PixelDataWriter for FixedLengths {
        fn encode_frame(
            &self,
            _src: &dyn PixelDataObject,
            frame: u32,
            _options: EncodeOptions,
            dst: &mut Vec<u8>,
        ) -> EncodeResult<Vec<AttributeOp>> {
            dst.resize(dst.len() + self.0[frame as usize], 0xAB);
            Ok(Vec::new())
        }
    }

    /// Regression test for upstream dicom-rs 59ab4edb, plus odd-length padding: the basic offset
    /// table holds, for each frame, the byte offset of its fragment's item header relative to the
    /// first fragment's. Before the fix every entry was `len(frame) + 8 * (frame + 1)` (not even
    /// starting at 0), so readers that seek through the table landed in the wrong place. Offsets
    /// must also count the padding byte an odd-length fragment gets when written.
    #[test]
    fn default_encode_builds_cumulative_basic_offset_table() {
        let writer = FixedLengths(vec![6865, 314, 1000, 7]);
        let mut fragments = Vec::new();
        let mut offset_table = Vec::new();
        writer
            .encode(&Frames(4), EncodeOptions::default(), &mut fragments, &mut offset_table)
            .unwrap();

        assert_eq!(fragments.iter().map(Vec::len).collect::<Vec<_>>(), vec![6865, 314, 1000, 7]);
        assert_eq!(
            offset_table,
            vec![
                0,
                6866 + 8,                      // 6865 padded to 6866
                6866 + 8 + 314 + 8,
                6866 + 8 + 314 + 8 + 1000 + 8, // the last fragment's own length is never needed
            ]
        );
    }

    #[test]
    fn default_encode_of_a_single_frame_has_a_single_zero_offset() {
        let mut fragments = Vec::new();
        let mut offset_table = Vec::new();
        FixedLengths(vec![101])
            .encode(&Frames(1), EncodeOptions::default(), &mut fragments, &mut offset_table)
            .unwrap();
        assert_eq!(offset_table, vec![0]);
        assert_eq!(fragments.len(), 1);
    }
}
