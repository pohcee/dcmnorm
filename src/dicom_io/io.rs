use std::io::Cursor;
use std::ffi::OsStr;
use std::path::Path;

use dicom_core::ops::ApplyOp;
use dicom_core::value::PixelFragmentSequence;
use dicom_core::{DataElement, PrimitiveValue, Tag, VR};
use dicom_dictionary_std::{tags, uids};
use dicom_encoding::adapters::EncodeOptions;
use dicom_encoding::transfer_syntax::{Codec, TransferSyntaxIndex};
use dicom_object::file::ReadPreamble;
use dicom_object::{
    open_file, DefaultDicomObject, FileMetaTableBuilder, InMemDicomObject, OpenFileOptions,
};
use dicom_transfer_syntax_registry::TransferSyntaxRegistry;

use super::kakadu;
use super::types::{
    DicomIoError, ReadError, TransferSyntaxSupport, TranscodeError, WriteError,
};

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Jpeg2000Backend {
    Kakadu { library_path: String },
    OpenJpeg,
}

pub fn kakadu_ffi_enabled() -> bool {
    kakadu::kakadu_ffi_enabled()
}

impl Jpeg2000Backend {
    pub fn name(&self) -> &'static str {
        match self {
            Self::Kakadu { .. } => "kakadu",
            Self::OpenJpeg => "openjpeg",
        }
    }
}

pub fn jpeg2000_backend() -> Jpeg2000Backend {
    detect_jpeg2000_backend_from_ld_library_path(std::env::var_os("LD_LIBRARY_PATH").as_deref())
}

pub fn jpeg2000_backend_name() -> &'static str {
    jpeg2000_backend().name()
}

pub fn detect_jpeg2000_backend_from_search_path(search_path: &str) -> Jpeg2000Backend {
    detect_jpeg2000_backend_from_ld_library_path(Some(OsStr::new(search_path)))
}

fn detect_jpeg2000_backend_from_ld_library_path(ld_library_path: Option<&OsStr>) -> Jpeg2000Backend {
    let Some(search_path) = ld_library_path else {
        return Jpeg2000Backend::OpenJpeg;
    };

    for directory in std::env::split_paths(search_path) {
        let Ok(entries) = std::fs::read_dir(&directory) else {
            continue;
        };

        for entry in entries.flatten() {
            let path = entry.path();
            let Some(name) = path.file_name().and_then(OsStr::to_str) else {
                continue;
            };

            if is_kakadu_library_name(name) {
                return Jpeg2000Backend::Kakadu {
                    library_path: path.to_string_lossy().to_string(),
                };
            }
        }
    }

    Jpeg2000Backend::OpenJpeg
}

fn is_kakadu_library_name(file_name: &str) -> bool {
    file_name.starts_with("libkdu") && file_name.contains(".so")
}

fn is_jpeg2000_transfer_syntax(uid: &str) -> bool {
    matches!(
        normalize_transfer_syntax_uid(uid),
        "1.2.840.10008.1.2.4.91" | "1.2.840.10008.1.2.4.90"
    )
}

pub fn read_dicom_file<P>(path: P) -> Result<DefaultDicomObject, ReadError>
where
    P: AsRef<Path>,
{
    open_file(path)
}

pub fn read_dicom_bytes(bytes: impl AsRef<[u8]>) -> Result<DefaultDicomObject, ReadError> {
    OpenFileOptions::new()
        .read_preamble(ReadPreamble::Always)
        .from_reader(Cursor::new(bytes.as_ref()))
}

pub fn write_dicom_file<P>(object: &DefaultDicomObject, path: P) -> Result<(), WriteError>
where
    P: AsRef<Path>,
{
    object.write_to_file(path).map(|_| ())
}

pub fn write_dicom_bytes(object: &DefaultDicomObject) -> Result<Vec<u8>, WriteError> {
    let mut bytes = Vec::new();
    object.write_all(&mut bytes)?;
    Ok(bytes)
}

pub fn write_dataset_as_dicom_file<P>(
    dataset: InMemDicomObject,
    path: P,
    transfer_syntax_uid: &str,
) -> Result<(), DicomIoError>
where
    P: AsRef<Path>,
{
    let file_object = dataset
        .with_meta(FileMetaTableBuilder::new().transfer_syntax(transfer_syntax_uid))?;

    file_object.write_to_file(path)?;
    Ok(())
}

pub fn write_dataset_as_dicom_bytes(
    dataset: InMemDicomObject,
    transfer_syntax_uid: &str,
) -> Result<Vec<u8>, DicomIoError> {
    let file_object = dataset
        .with_meta(FileMetaTableBuilder::new().transfer_syntax(transfer_syntax_uid))?;

    let mut bytes = Vec::new();
    file_object.write_all(&mut bytes)?;
    Ok(bytes)
}

pub fn list_transfer_syntax_support() -> Vec<TransferSyntaxSupport> {
    let kakadu_enabled = kakadu_ffi_available_from_backend(&jpeg2000_backend());
    let mut syntaxes = TransferSyntaxRegistry
        .iter()
        .map(|ts| TransferSyntaxSupport {
            uid: ts.uid().to_owned(),
            name: ts.name().to_owned(),
            encapsulated_pixel_data: is_encapsulated_transfer_syntax(ts),
            can_read_dataset: can_read_dataset(ts),
            can_write_dataset: can_write_dataset(ts),
            can_decode_pixel_data: can_decode_pixel_data(ts, kakadu_enabled),
            can_encode_pixel_data: can_encode_pixel_data(ts, kakadu_enabled),
        })
        .collect::<Vec<_>>();

    syntaxes.sort_by(|left, right| left.uid.cmp(&right.uid));
    syntaxes
}

pub fn transcode_dicom_object(
    object: &DefaultDicomObject,
    target_transfer_syntax_uid: &str,
) -> Result<DefaultDicomObject, TranscodeError> {
    let source_uid = normalize_transfer_syntax_uid(object.meta().transfer_syntax());
    let target_uid = normalize_transfer_syntax_uid(target_transfer_syntax_uid);

    if source_uid == target_uid {
        return Ok(object.clone());
    }

    let source_ts = TransferSyntaxRegistry
        .get(source_uid)
        .ok_or_else(|| TranscodeError::UnknownTransferSyntax(source_uid.to_owned()))?;
    let target_ts = TransferSyntaxRegistry
        .get(target_uid)
        .ok_or_else(|| TranscodeError::UnknownTransferSyntax(target_uid.to_owned()))?;

    let mut transcoded = object.clone();
    let pixel_representation = pixel_data_representation(object);

    match pixel_representation {
        PixelDataRepresentation::Absent => {}
        PixelDataRepresentation::Native => {
            if is_encapsulated_transfer_syntax(target_ts) {
                encode_pixel_data(&mut transcoded, target_ts)?;
            }
        }
        PixelDataRepresentation::Encapsulated => {
            decode_pixel_data(&mut transcoded, source_ts)?;

            if is_encapsulated_transfer_syntax(target_ts) {
                encode_pixel_data(&mut transcoded, target_ts)?;
            }
        }
    }

    transcoded.meta_mut().set_transfer_syntax(target_ts);
    Ok(transcoded)
}

pub fn transcode_dicom_bytes(
    bytes: impl AsRef<[u8]>,
    target_transfer_syntax_uid: &str,
) -> Result<Vec<u8>, TranscodeError> {
    let object = read_dicom_bytes(bytes)?;
    let transcoded = transcode_dicom_object(&object, target_transfer_syntax_uid)?;
    Ok(write_dicom_bytes(&transcoded)?)
}

pub fn transcode_dicom_file<P, Q>(
    input_path: P,
    output_path: Q,
    target_transfer_syntax_uid: &str,
) -> Result<(), TranscodeError>
where
    P: AsRef<Path>,
    Q: AsRef<Path>,
{
    let object = read_dicom_file(input_path)?;
    let transcoded = transcode_dicom_object(&object, target_transfer_syntax_uid)?;
    write_dicom_file(&transcoded, output_path)?;
    Ok(())
}

fn normalize_transfer_syntax_uid(uid: &str) -> &str {
    uid.trim_end_matches(|character: char| character.is_whitespace() || character == '\0')
}

fn can_read_dataset<D, R, W>(ts: &dicom_encoding::TransferSyntax<D, R, W>) -> bool {
    !matches!(ts.codec(), Codec::Dataset(None))
}

fn can_write_dataset<D, R, W>(ts: &dicom_encoding::TransferSyntax<D, R, W>) -> bool {
    !matches!(ts.codec(), Codec::Dataset(None))
}

fn can_decode_pixel_data<D, R, W>(
    ts: &dicom_encoding::TransferSyntax<D, R, W>,
    kakadu_enabled: bool,
) -> bool {
    matches!(ts.codec(), Codec::EncapsulatedPixelData(Some(_), _))
        || (kakadu_enabled && is_jpeg2000_transfer_syntax(ts.uid()))
}

fn can_encode_pixel_data<D, R, W>(
    ts: &dicom_encoding::TransferSyntax<D, R, W>,
    kakadu_enabled: bool,
) -> bool {
    matches!(ts.codec(), Codec::EncapsulatedPixelData(_, Some(_)))
        || (kakadu_enabled && is_jpeg2000_transfer_syntax(ts.uid()))
}

fn kakadu_ffi_available_from_backend(backend: &Jpeg2000Backend) -> bool {
    matches!(backend, Jpeg2000Backend::Kakadu { .. }) && kakadu_ffi_enabled()
}

fn decode_jpeg2000_with_kakadu(object: &DefaultDicomObject, _library_path: &str) -> Result<Vec<u8>, String> {
    let rows = object
        .get(tags::ROWS)
        .and_then(|element| element.uint16().ok())
        .ok_or_else(|| "missing Rows attribute".to_owned())? as usize;
    let cols = object
        .get(tags::COLUMNS)
        .and_then(|element| element.uint16().ok())
        .ok_or_else(|| "missing Columns attribute".to_owned())? as usize;
    let samples_per_pixel = object
        .get(tags::SAMPLES_PER_PIXEL)
        .and_then(|element| element.uint16().ok())
        .unwrap_or(1) as usize;
    let bits_stored = object
        .get(tags::BITS_STORED)
        .and_then(|element| element.uint16().ok())
        .or_else(|| object.get(tags::BITS_ALLOCATED).and_then(|element| element.uint16().ok()))
        .ok_or_else(|| "missing BitsStored/BitsAllocated attribute".to_owned())?;
    let is_signed = object
        .get(tags::PIXEL_REPRESENTATION)
        .and_then(|element| element.uint16().ok())
        .unwrap_or(0)
        != 0;
    let number_of_frames = object
        .get(tags::NUMBER_OF_FRAMES)
        .and_then(|element| element.to_str().ok())
        .and_then(|value| value.trim().parse::<usize>().ok())
        .unwrap_or(1);

    if number_of_frames != 1 {
        return Err("Kakadu FFI decode currently supports single-frame datasets only".to_owned());
    }

    let fragments = object
        .element(tags::PIXEL_DATA)
        .map_err(|error| format!("missing PixelData element: {error}"))?
        .fragments()
        .ok_or_else(|| "expected encapsulated JPEG2000 PixelData fragments".to_owned())?;
    let mut codestream = Vec::new();
    for fragment in fragments {
        codestream.extend_from_slice(fragment);
    }

    kakadu::decode_jpeg2000(&codestream, rows, cols, samples_per_pixel, bits_stored, is_signed)
}

fn encode_jpeg2000_with_kakadu(
    object: &DefaultDicomObject,
    target_uid: &str,
    _library_path: &str,
) -> Result<Vec<Vec<u8>>, String> {
    let rows = object
        .get(tags::ROWS)
        .and_then(|element| element.uint16().ok())
        .ok_or_else(|| "missing Rows attribute".to_owned())? as usize;
    let cols = object
        .get(tags::COLUMNS)
        .and_then(|element| element.uint16().ok())
        .ok_or_else(|| "missing Columns attribute".to_owned())? as usize;
    let samples_per_pixel = object
        .get(tags::SAMPLES_PER_PIXEL)
        .and_then(|element| element.uint16().ok())
        .unwrap_or(1) as usize;
    let bits_stored = object
        .get(tags::BITS_STORED)
        .and_then(|element| element.uint16().ok())
        .or_else(|| object.get(tags::BITS_ALLOCATED).and_then(|element| element.uint16().ok()))
        .ok_or_else(|| "missing BitsStored/BitsAllocated attribute".to_owned())?;
    let is_signed = object
        .get(tags::PIXEL_REPRESENTATION)
        .and_then(|element| element.uint16().ok())
        .unwrap_or(0)
        != 0;
    let number_of_frames = object
        .get(tags::NUMBER_OF_FRAMES)
        .and_then(|element| element.to_str().ok())
        .and_then(|value| value.trim().parse::<usize>().ok())
        .unwrap_or(1);
    let planar_configuration = object
        .get(tags::PLANAR_CONFIGURATION)
        .and_then(|element| element.uint16().ok())
        .unwrap_or(0);

    if number_of_frames != 1 {
        return Err("Kakadu FFI encode currently supports single-frame datasets only".to_owned());
    }
    if samples_per_pixel > 1 && planar_configuration != 0 {
        return Err("Kakadu FFI encode currently supports only planar configuration 0".to_owned());
    }

    let pixels = object
        .element(tags::PIXEL_DATA)
        .map_err(|error| format!("missing PixelData element: {error}"))?
        .to_bytes()
        .map_err(|error| format!("failed to access native PixelData bytes: {error}"))?
        .to_vec();
    let reversible = normalize_transfer_syntax_uid(target_uid) == "1.2.840.10008.1.2.4.90";
    let codestream = kakadu::encode_jpeg2000(
        &pixels,
        rows,
        cols,
        samples_per_pixel,
        bits_stored,
        is_signed,
        reversible,
    )?;
    Ok(vec![codestream])
}

fn is_encapsulated_transfer_syntax<D, R, W>(
    ts: &dicom_encoding::TransferSyntax<D, R, W>,
) -> bool {
    matches!(ts.codec(), Codec::EncapsulatedPixelData(_, _))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PixelDataRepresentation {
    Absent,
    Native,
    Encapsulated,
}

fn pixel_data_representation(object: &DefaultDicomObject) -> PixelDataRepresentation {
    let Some(element) = object.get(tags::PIXEL_DATA) else {
        return PixelDataRepresentation::Absent;
    };

    match element.value() {
        dicom_core::value::Value::Primitive(_) => PixelDataRepresentation::Native,
        dicom_core::value::Value::PixelSequence(_) => PixelDataRepresentation::Encapsulated,
        dicom_core::value::Value::Sequence(_) => PixelDataRepresentation::Absent,
    }
}

fn decode_pixel_data(
    object: &mut DefaultDicomObject,
    source_ts: &dicom_encoding::TransferSyntax,
) -> Result<(), TranscodeError> {
    if is_jpeg2000_transfer_syntax(source_ts.uid()) {
        if let Jpeg2000Backend::Kakadu { library_path } = jpeg2000_backend() {
            if let Ok(decoded) = decode_jpeg2000_with_kakadu(object, &library_path) {
                replace_with_native_pixel_data(object, decoded)?;
                normalize_decoded_pixel_data_attributes(object);
                object.meta_mut().set_transfer_syntax(
                    TransferSyntaxRegistry
                        .get(uids::EXPLICIT_VR_LITTLE_ENDIAN)
                        .expect("explicit VR little endian transfer syntax must exist"),
                );
                return Ok(());
            }
        }
    }

    let reader = match source_ts.codec() {
        Codec::EncapsulatedPixelData(Some(reader), _) => reader,
        _ => {
            let reason = match jpeg2000_backend() {
                Jpeg2000Backend::Kakadu { library_path } if is_jpeg2000_transfer_syntax(source_ts.uid()) => format!(
                    "Kakadu detected at {library_path}, but neither Kakadu nor OpenJPEG decoder could be used for this dataset"
                ),
                _ => "pixel data decoding is not available in this build".to_owned(),
            };
            return Err(TranscodeError::UnsupportedSourceTransferSyntax {
                uid: source_ts.uid().to_owned(),
                name: source_ts.name().to_owned(),
                reason,
            });
        }
    };

    let mut decoded = Vec::new();
    reader
        .decode(object, &mut decoded)
        .map_err(|error| TranscodeError::DecodePixelData {
            uid: source_ts.uid().to_owned(),
            name: source_ts.name().to_owned(),
            message: error.to_string(),
        })?;

    replace_with_native_pixel_data(object, decoded)?;
    normalize_decoded_pixel_data_attributes(object);
    object.meta_mut().set_transfer_syntax(
        TransferSyntaxRegistry
            .get(uids::EXPLICIT_VR_LITTLE_ENDIAN)
            .expect("explicit VR little endian transfer syntax must exist"),
    );

    Ok(())
}

fn encode_pixel_data(
    object: &mut DefaultDicomObject,
    target_ts: &dicom_encoding::TransferSyntax,
) -> Result<(), TranscodeError> {
    if is_jpeg2000_transfer_syntax(target_ts.uid()) {
        if let Jpeg2000Backend::Kakadu { library_path } = jpeg2000_backend() {
            let fragments = encode_jpeg2000_with_kakadu(object, target_ts.uid(), &library_path)
                .map_err(|error| TranscodeError::EncodePixelData {
                    uid: target_ts.uid().to_owned(),
                    name: target_ts.name().to_owned(),
                    message: format!("Kakadu FFI encode failed: {error}"),
                })?;
            replace_with_encapsulated_pixel_data(object, vec![0], fragments);
            return Ok(());
        }
    }

    let Codec::EncapsulatedPixelData(_, Some(writer)) = target_ts.codec() else {
        let reason = match jpeg2000_backend() {
            Jpeg2000Backend::Kakadu { library_path } if is_jpeg2000_transfer_syntax(target_ts.uid()) => format!(
                "Kakadu detected at {library_path}, but Kakadu tools were not available for JPEG2000 encoding"
            ),
            _ => "pixel data encoding is not available in this build".to_owned(),
        };
        return Err(TranscodeError::UnsupportedTargetTransferSyntax {
            uid: target_ts.uid().to_owned(),
            name: target_ts.name().to_owned(),
            reason,
        });
    };

    let mut fragments = Vec::new();
    let mut offset_table = Vec::new();
    let operations = writer
        .encode(object, EncodeOptions::default(), &mut fragments, &mut offset_table)
        .map_err(|error| TranscodeError::EncodePixelData {
            uid: target_ts.uid().to_owned(),
            name: target_ts.name().to_owned(),
            message: error.to_string(),
        })?;

    replace_with_encapsulated_pixel_data(object, offset_table, fragments);

    for operation in operations {
        object
            .apply(operation)
            .map_err(|error| TranscodeError::ApplyAttribute(error.to_string()))?;
    }

    Ok(())
}

fn replace_with_native_pixel_data(
    object: &mut DefaultDicomObject,
    decoded: Vec<u8>,
) -> Result<(), TranscodeError> {
    let bits_allocated = object
        .get(tags::BITS_ALLOCATED)
        .and_then(|element| element.uint16().ok())
        .ok_or(TranscodeError::MissingImageAttribute("BitsAllocated"))?;
    let value = native_pixel_value_from_little_endian_bytes(decoded, bits_allocated)?;
    let vr = native_pixel_vr(bits_allocated);

    remove_encapsulation_sidecar_attributes(object);
    object.put(DataElement::new(tags::PIXEL_DATA, vr, value));
    Ok(())
}

fn replace_with_encapsulated_pixel_data(
    object: &mut DefaultDicomObject,
    offset_table: Vec<u32>,
    fragments: Vec<Vec<u8>>,
) {
    remove_encapsulation_sidecar_attributes(object);
    object.put(DataElement::new(
        tags::PIXEL_DATA,
        VR::OB,
        PixelFragmentSequence::new(offset_table, fragments),
    ));
}

fn remove_encapsulation_sidecar_attributes(object: &mut DefaultDicomObject) {
    object.remove_element(Tag(0x7FE0, 0x0001));
    object.remove_element(Tag(0x7FE0, 0x0002));
    object.remove_element(Tag(0x7FE0, 0x0003));
}

fn native_pixel_value_from_little_endian_bytes(
    bytes: Vec<u8>,
    bits_allocated: u16,
) -> Result<PrimitiveValue, TranscodeError> {
    match bits_allocated {
        0 => Err(TranscodeError::UnsupportedBitsAllocated(bits_allocated)),
        1..=8 => Ok(PrimitiveValue::from(bytes)),
        9..=16 => {
            let words = bytes_to_words::<2, u16>(bytes, u16::from_le_bytes, bits_allocated)?;
            Ok(PrimitiveValue::U16(words.into_iter().collect()))
        }
        17..=32 => {
            let words = bytes_to_words::<4, u32>(bytes, u32::from_le_bytes, bits_allocated)?;
            Ok(PrimitiveValue::U32(words.into_iter().collect()))
        }
        33..=64 => {
            let words = bytes_to_words::<8, u64>(bytes, u64::from_le_bytes, bits_allocated)?;
            Ok(PrimitiveValue::U64(words.into_iter().collect()))
        }
        _ => Err(TranscodeError::UnsupportedBitsAllocated(bits_allocated)),
    }
}

fn bytes_to_words<const N: usize, T>(
    bytes: Vec<u8>,
    convert: fn([u8; N]) -> T,
    bits_allocated: u16,
) -> Result<Vec<T>, TranscodeError> {
    if bytes.len() % N != 0 {
        return Err(TranscodeError::InvalidDecodedPixelDataLength {
            bits_allocated,
            length: bytes.len(),
        });
    }

    let mut values = Vec::with_capacity(bytes.len() / N);
    for chunk in bytes.chunks_exact(N) {
        let mut buffer = [0u8; N];
        buffer.copy_from_slice(chunk);
        values.push(convert(buffer));
    }
    Ok(values)
}

fn native_pixel_vr(bits_allocated: u16) -> VR {
    if bits_allocated <= 8 {
        VR::OB
    } else {
        VR::OW
    }
}

fn normalize_decoded_pixel_data_attributes(object: &mut DefaultDicomObject) {
    let samples_per_pixel = object
        .get(tags::SAMPLES_PER_PIXEL)
        .and_then(|element| element.uint16().ok())
        .unwrap_or(1);

    if samples_per_pixel > 1 {
        object.put(DataElement::new(
            tags::PHOTOMETRIC_INTERPRETATION,
            VR::CS,
            PrimitiveValue::from("RGB"),
        ));
        object.put(DataElement::new(
            tags::PLANAR_CONFIGURATION,
            VR::US,
            PrimitiveValue::from(0u16),
        ));
        return;
    }

    let normalized_photometric = object
        .get(tags::PHOTOMETRIC_INTERPRETATION)
        .and_then(|element| element.to_str().ok())
        .map(|value| value.trim().to_owned())
        .filter(|value| matches!(value.as_str(), "MONOCHROME1" | "MONOCHROME2" | "PALETTE COLOR"))
        .unwrap_or_else(|| "MONOCHROME2".to_owned());

    object.put(DataElement::new(
        tags::PHOTOMETRIC_INTERPRETATION,
        VR::CS,
        PrimitiveValue::from(normalized_photometric),
    ));
    object.remove_element(tags::PLANAR_CONFIGURATION);
}