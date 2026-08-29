use std::ffi::OsStr;
use std::io::Cursor;
use std::io::Read;
use std::path::Path;
use std::process::Command;
use std::sync::OnceLock;

use crate::perf;
use dcmnorm_core::ops::ApplyOp;
use dcmnorm_core::value::PixelFragmentSequence;
use dcmnorm_core::value::Value;
use dcmnorm_core::{DataElement, PrimitiveValue, Tag, VR};
use dcmnorm_dictionary::{tags, uids};
use dcmnorm_encoding::adapters::EncodeOptions;
use dcmnorm_encoding::transfer_syntax::{Codec, TransferSyntaxIndex};
use dcmnorm_object::ReadPreamble;
use dcmnorm_object::{
    DefaultDicomObject, FileMetaTableBuilder, InMemDicomObject, OpenFileOptions,
};
use dcmnorm_transcode::TransferSyntaxRegistry;
use rayon::prelude::*;

use super::jpeg_ls;
use super::kakadu;
use super::mpeg;
use super::types::{DicomIoError, ReadError, TranscodeError, TransferSyntaxSupport, WriteError};

pub const JPEG2000_DEBUG_ENV_FLAG: &str = "DCMNORM_JPEG2000_DEBUG";
pub const JPEG2000_CODEC_ENV_FLAG: &str = "DCMNORM_JPEG2000_CODEC";
const DICOM_PROBE_CHUNK_SIZE: usize = 64 * 1024;
const ITEM_TAG: Tag = Tag(0xFFFE, 0xE000);
const ITEM_DELIMITATION_TAG: Tag = Tag(0xFFFE, 0xE00D);
const SEQUENCE_DELIMITATION_TAG: Tag = Tag(0xFFFE, 0xE0DD);

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Jpeg2000Backend {
    Kakadu { library_path: String },
    OpenJpeg,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Jpeg2000CodecPreference {
    Auto,
    Kakadu,
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
    match jpeg2000_codec_preference() {
        Jpeg2000CodecPreference::OpenJpeg => Jpeg2000Backend::OpenJpeg,
        Jpeg2000CodecPreference::Auto | Jpeg2000CodecPreference::Kakadu => {
            detect_jpeg2000_backend_from_ld_library_path(std::env::var_os("LD_LIBRARY_PATH").as_deref())
        }
    }
}

fn jpeg2000_codec_preference() -> Jpeg2000CodecPreference {
    let Some(value) = std::env::var_os(JPEG2000_CODEC_ENV_FLAG) else {
        return Jpeg2000CodecPreference::Auto;
    };

    let normalized = value.to_string_lossy().trim().to_ascii_lowercase();
    match normalized.as_str() {
        "openjpeg" => Jpeg2000CodecPreference::OpenJpeg,
        "kakadu" => Jpeg2000CodecPreference::Kakadu,
        _ => Jpeg2000CodecPreference::Auto,
    }
}

pub fn jpeg2000_backend_name() -> &'static str {
    jpeg2000_backend().name()
}

pub fn detect_jpeg2000_backend_from_search_path(search_path: &str) -> Jpeg2000Backend {
    detect_jpeg2000_backend_from_ld_library_path(Some(OsStr::new(search_path)))
}

fn detect_jpeg2000_backend_from_ld_library_path(
    ld_library_path: Option<&OsStr>,
) -> Jpeg2000Backend {
    if !kakadu_ffi_enabled() {
        return Jpeg2000Backend::OpenJpeg;
    }

    if let Some(search_path) = ld_library_path {
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
    }

    // kakadu-ffi builds are linked against Kakadu, so treat Kakadu as active
    // even if the library filename is not discoverable in LD_LIBRARY_PATH.
    Jpeg2000Backend::Kakadu {
        library_path: "linked-via-loader".to_owned(),
    }
}

fn is_kakadu_library_name(file_name: &str) -> bool {
    file_name.starts_with("libkdu") && file_name.contains(".so")
}

fn jpeg2000_debug_enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| {
        std::env::var(JPEG2000_DEBUG_ENV_FLAG)
            .map(|value| {
                let normalized = value.trim().to_ascii_lowercase();
                matches!(normalized.as_str(), "1" | "true" | "yes" | "on")
            })
            .unwrap_or(false)
    })
}

fn jpeg2000_debug_log(message: impl AsRef<str>) {
    if jpeg2000_debug_enabled() {
        eprintln!("[dcmnorm:jpeg2000] {}", message.as_ref());
    }
}

// The single canonical definition - previously duplicated (with a diverging UID list) in
// render.rs. That copy was missing .92/.93 (JPEG 2000 Part 2 Multi-component), which wasn't
// an intentional narrower scope, just drift between the two copies; this list is their union.
pub(super) fn is_jpeg2000_transfer_syntax(uid: &str) -> bool {
    matches!(
        normalize_transfer_syntax_uid(uid),
        "1.2.840.10008.1.2.4.90"
            | "1.2.840.10008.1.2.4.91"
            // JPEG 2000 Part 2 Multi-component Image Compression (Lossless Only / lossy) -
            // same codestream family, needs the same MCT/component-mismatch correction and
            // Kakadu/OpenJPEG dispatch as classic JPEG 2000.
            | "1.2.840.10008.1.2.4.92"
            | "1.2.840.10008.1.2.4.93"
            // High-Throughput JPEG 2000 (Lossless Only / RPCL / lossy) - same
            // codestream format as classic JPEG 2000, decoded by the same
            // OpenJPEG/Kakadu backends, so it needs the same MCT/component
            // correction and Kakadu dispatch logic gated by this check.
            | "1.2.840.10008.1.2.4.201"
            | "1.2.840.10008.1.2.4.202"
            | "1.2.840.10008.1.2.4.203"
    )
}

/// Determines whether the JPEG 2000 codestream for the given frame uses the
/// Multiple Component Transformation (MCT).
///
/// Per DICOM PS3.5, YBR_RCT/YBR_ICT require the codestream to apply MCT, in
/// which case a conformant decoder (openjpeg, Kakadu) reverses it internally
/// as part of standard decompression and hands back genuine RGB samples -
/// re-applying a manual YCbCr/RCT->RGB conversion on that output corrupts the
/// colors. Some non-conformant encoders (seen from certain WSI/pathology
/// scanners) store raw, un-transformed YCbCr component samples without
/// setting the codestream's MCT flag, relying on the DICOM attribute alone;
/// those genuinely need the manual conversion downstream. Returns `None` when
/// the flag can't be determined (falls back to the spec-conformant
/// assumption that MCT was used).
pub(super) fn jpeg2000_frame_uses_mct(object: &DefaultDicomObject, frame_index: usize) -> Option<bool> {
    let fragments = object.element(tags::PIXEL_DATA).ok()?.fragments()?;
    let number_of_frames = object
        .get(tags::NUMBER_OF_FRAMES)
        .and_then(|element| element.to_str().ok())
        .and_then(|value| value.trim().parse::<usize>().ok())
        .unwrap_or(1);

    let bytes = if fragments.len() == number_of_frames {
        fragments.get(frame_index)?
    } else {
        fragments.first()?
    };

    codestream_uses_mct(bytes)
}

/// Scans a raw JPEG 2000 codestream's main header for the COD marker segment
/// and reads its Multiple Component Transformation flag. Marker layout
/// (ITU-T T.800): FF52 (2) + Lcod (2) + Scod (1) + progression order (1) +
/// number of layers (2) + MCT (1) - i.e. the MCT byte sits 8 bytes after the
/// start of the FF52 marker.
fn codestream_uses_mct(codestream: &[u8]) -> Option<bool> {
    let window = &codestream[..codestream.len().min(4096)];
    let marker_start = window.windows(2).position(|pair| pair == [0xFF, 0x52])?;
    window.get(marker_start + 8).map(|&byte| byte != 0)
}

/// Determines the true number of components a JPEG 2000 codestream carries
/// for the given frame, independent of the DICOM SamplesPerPixel attribute.
///
/// Some non-conformant encoders (seen from certain ultrasound modalities)
/// leave SamplesPerPixel=3/PhotometricInterpretation=RGB on a frame whose
/// codestream was actually only ever encoded with a single (grayscale)
/// component - e.g. a machine-UI screen capture saved as frame 1 of a study.
/// A decoder sizing its output buffer from the declared SamplesPerPixel then
/// only fills the first (red) channel and leaves green/blue zeroed, which
/// renders as a solid red image. See jpeg2000_component_mismatch, which uses
/// this to detect and correct that case before decoding.
pub(super) fn jpeg2000_frame_component_count(
    object: &DefaultDicomObject,
    frame_index: usize,
) -> Option<u16> {
    let fragments = object.element(tags::PIXEL_DATA).ok()?.fragments()?;
    let number_of_frames = object
        .get(tags::NUMBER_OF_FRAMES)
        .and_then(|element| element.to_str().ok())
        .and_then(|value| value.trim().parse::<usize>().ok())
        .unwrap_or(1);

    let bytes = if fragments.len() == number_of_frames {
        fragments.get(frame_index)?
    } else {
        fragments.first()?
    };

    codestream_component_count(bytes)
}

/// Scans a raw JPEG 2000 codestream's main header for the SIZ marker segment
/// and reads its Csiz (number of components) field. Marker layout (ITU-T
/// T.800): FF51 (2) + Lsiz (2) + Rsiz (2) + Xsiz/Ysiz/XOsiz/YOsiz/XTsiz/
/// YTsiz/XTOsiz/YTOsiz (4 bytes each, 32 total) + Csiz (2) - i.e. Csiz sits
/// 38 bytes after the start of the FF51 marker.
fn codestream_component_count(codestream: &[u8]) -> Option<u16> {
    let window = &codestream[..codestream.len().min(4096)];
    let marker_start = window.windows(2).position(|pair| pair == [0xFF, 0x51])?;
    let csiz_start = marker_start + 38;
    let bytes = window.get(csiz_start..csiz_start + 2)?;
    Some(u16::from_be_bytes([bytes[0], bytes[1]]))
}

/// Returns the actual component count when it disagrees with a >1 declared
/// SamplesPerPixel, i.e. when applying it via
/// [`apply_jpeg2000_component_correction`] would change anything.
pub(super) fn jpeg2000_component_mismatch(
    object: &DefaultDicomObject,
    frame_index: usize,
) -> Option<u16> {
    let declared_samples_per_pixel = object
        .get(tags::SAMPLES_PER_PIXEL)
        .and_then(|element| element.uint16().ok())
        .unwrap_or(1);

    if declared_samples_per_pixel <= 1 {
        return None;
    }

    let actual_components = jpeg2000_frame_component_count(object, frame_index)?;

    if actual_components == 0 || actual_components >= declared_samples_per_pixel {
        return None;
    }

    Some(actual_components)
}

/// Rewrites SamplesPerPixel (and, when the codestream is single-component,
/// PhotometricInterpretation/PlanarConfiguration) to match a codestream's
/// real component count ahead of decoding, so every backend (OpenJPEG,
/// Kakadu) sizes and fills its output buffer correctly instead of leaving
/// unfilled channels zeroed. Call only when [`jpeg2000_component_mismatch`]
/// found a real mismatch.
pub(super) fn apply_jpeg2000_component_correction(
    object: &mut DefaultDicomObject,
    actual_components: u16,
) {
    jpeg2000_debug_log(format!(
        "codestream carries {actual_components} component(s), correcting SamplesPerPixel before decode"
    ));

    object.put(DataElement::new(
        tags::SAMPLES_PER_PIXEL,
        VR::US,
        PrimitiveValue::from(actual_components),
    ));

    if actual_components == 1 {
        object.put(DataElement::new(
            tags::PHOTOMETRIC_INTERPRETATION,
            VR::CS,
            PrimitiveValue::from("MONOCHROME2".to_owned()),
        ));
        object.remove_element(tags::PLANAR_CONFIGURATION);
    }
}

fn is_mpeg_transfer_syntax(uid: &str) -> bool {
    let normalized = normalize_transfer_syntax_uid(uid);
    matches!(
        normalized,
        "1.2.840.10008.1.2.4.100"
            | "1.2.840.10008.1.2.4.101"
            | "1.2.840.10008.1.2.4.102"
            | "1.2.840.10008.1.2.4.103"
            | "1.2.840.10008.1.2.4.104"
            | "1.2.840.10008.1.2.4.105"
            | "1.2.840.10008.1.2.4.106"
            | "1.2.840.10008.1.2.4.107"
            | "1.2.840.10008.1.2.4.108"
    )
}

fn is_jpeg_ls_transfer_syntax(uid: &str) -> bool {
    matches!(
        normalize_transfer_syntax_uid(uid),
        "1.2.840.10008.1.2.4.80" | "1.2.840.10008.1.2.4.81"
    )
}

fn ffmpeg_available() -> bool {
    if let Ok(output) = Command::new("ffmpeg").arg("-version").output() {
        output.status.success()
    } else {
        false
    }
}

fn read_dicom_dataset_without_meta(bytes: &[u8]) -> Option<DefaultDicomObject> {
    let candidate_transfer_syntaxes = [
        uids::IMPLICIT_VR_LITTLE_ENDIAN,
        uids::EXPLICIT_VR_LITTLE_ENDIAN,
        "1.2.840.10008.1.2.2",
    ];

    for transfer_syntax_uid in candidate_transfer_syntaxes {
        let Some(transfer_syntax) = TransferSyntaxRegistry.get(transfer_syntax_uid) else {
            continue;
        };

        let Ok(dataset) = InMemDicomObject::read_dataset_with_ts(Cursor::new(bytes), transfer_syntax)
        else {
            continue;
        };

        if let Ok(file_object) = dataset
            .with_meta(FileMetaTableBuilder::new().transfer_syntax(transfer_syntax_uid))
        {
            return Some(file_object);
        }
    }

    None
}

pub fn read_dicom_file<P>(path: P) -> Result<DefaultDicomObject, ReadError>
where
    P: AsRef<Path>,
{
    let path_ref = path.as_ref();

    OpenFileOptions::new()
        .read_preamble(ReadPreamble::Always)
        .open_file(path_ref)
        .or_else(|error| match std::fs::read(path_ref) {
            Ok(bytes) => read_dicom_dataset_without_meta(&bytes).ok_or(error),
            Err(_) => Err(error),
        })
}

pub fn read_dicom_bytes(bytes: impl AsRef<[u8]>) -> Result<DefaultDicomObject, ReadError> {
    let bytes = bytes.as_ref();

    OpenFileOptions::new()
        .read_preamble(ReadPreamble::Always)
        .from_reader(Cursor::new(bytes))
        .or_else(|error| read_dicom_dataset_without_meta(bytes).ok_or(error))
}

pub fn probe_dicom_file_for_sop_class_uid<P>(path: P) -> Result<bool, std::io::Error>
where
    P: AsRef<Path>,
{
    let path_ref = path.as_ref();
    let metadata = std::fs::metadata(path_ref)?;
    if !metadata.is_file() {
        return Ok(false);
    }

    let mut file = std::fs::File::open(path_ref)?;
    let mut bytes = Vec::with_capacity(DICOM_PROBE_CHUNK_SIZE * 2);
    let mut chunk = vec![0u8; DICOM_PROBE_CHUNK_SIZE];

    loop {
        let read = file.read(&mut chunk)?;
        if read > 0 {
            bytes.extend_from_slice(&chunk[..read]);
        }
        let eof = read == 0;

        match probe_dicom_bytes_for_sop_class_uid(&bytes, eof) {
            DicomProbeStatus::Found => return Ok(true),
            DicomProbeStatus::NeedMore if !eof => continue,
            DicomProbeStatus::NeedMore | DicomProbeStatus::NotFound | DicomProbeStatus::Invalid => {
                return Ok(false)
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DicomProbeStatus {
    Found,
    NeedMore,
    NotFound,
    Invalid,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ProbeParseError {
    NeedMore,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ProbeTransferSyntax {
    explicit_vr: bool,
    little_endian: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ProbeHeader {
    tag: Tag,
    header_length: usize,
    length: Option<usize>,
}

fn probe_dicom_bytes_for_sop_class_uid(source: &[u8], eof: bool) -> DicomProbeStatus {
    if source.len() >= 132 && &source[128..132] == b"DICM" {
        return probe_part10_for_sop_class_uid(source, eof);
    }

    probe_dataset_without_meta_for_sop_class_uid(source, eof)
}

fn probe_part10_for_sop_class_uid(source: &[u8], eof: bool) -> DicomProbeStatus {
    let mut position = 132;
    let mut transfer_syntax = ProbeTransferSyntax {
        explicit_vr: true,
        little_endian: true,
    };

    while position + 8 <= source.len() {
        let header = match parse_probe_element_header(source, position, true, true) {
            Ok(header) => header,
            Err(ProbeParseError::NeedMore) => return DicomProbeStatus::NeedMore,
        };

        if header.tag.group() != 0x0002 {
            break;
        }

        let value_offset = position + header.header_length;
        let Some(value_length) = header.length else {
            return DicomProbeStatus::Invalid;
        };

        if value_offset + value_length > source.len() {
            return DicomProbeStatus::NeedMore;
        }

        if header.tag == tags::TRANSFER_SYNTAX_UID {
            let uid = decode_probe_text(&source[value_offset..value_offset + value_length]);
            transfer_syntax = probe_transfer_syntax_from_uid(uid.as_str());
        }

        position = value_offset + value_length;
    }

    probe_dataset_for_sop_class_uid(
        source,
        position,
        transfer_syntax.explicit_vr,
        transfer_syntax.little_endian,
        eof,
    )
}

fn probe_dataset_without_meta_for_sop_class_uid(source: &[u8], eof: bool) -> DicomProbeStatus {
    let mut any_need_more = false;
    let syntaxes = [
        ProbeTransferSyntax {
            explicit_vr: false,
            little_endian: true,
        },
        ProbeTransferSyntax {
            explicit_vr: true,
            little_endian: true,
        },
        ProbeTransferSyntax {
            explicit_vr: true,
            little_endian: false,
        },
    ];

    for syntax in syntaxes {
        match probe_dataset_for_sop_class_uid(source, 0, syntax.explicit_vr, syntax.little_endian, eof)
        {
            DicomProbeStatus::Found => return DicomProbeStatus::Found,
            DicomProbeStatus::NeedMore => any_need_more = true,
            DicomProbeStatus::NotFound | DicomProbeStatus::Invalid => {}
        }
    }

    if any_need_more && !eof {
        DicomProbeStatus::NeedMore
    } else {
        DicomProbeStatus::Invalid
    }
}

fn probe_dataset_for_sop_class_uid(
    source: &[u8],
    mut position: usize,
    explicit_vr: bool,
    little_endian: bool,
    eof: bool,
) -> DicomProbeStatus {
    while position + 8 <= source.len() {
        let header = match parse_probe_element_header(source, position, explicit_vr, little_endian) {
            Ok(header) => header,
            Err(ProbeParseError::NeedMore) => return DicomProbeStatus::NeedMore,
        };
        let value_offset = position + header.header_length;

        if header.tag == tags::SOP_CLASS_UID {
            let Some(length) = header.length else {
                return DicomProbeStatus::Invalid;
            };

            if length == 0 || value_offset + length > source.len() {
                return DicomProbeStatus::NeedMore;
            }

            let value = decode_probe_text(&source[value_offset..value_offset + length]);
            if value.is_empty() {
                return DicomProbeStatus::Invalid;
            }

            return DicomProbeStatus::Found;
        }

        position = if let Some(length) = header.length {
            let end = value_offset.saturating_add(length);
            if end > source.len() {
                return DicomProbeStatus::NeedMore;
            }
            end
        } else {
            match skip_probe_undefined_length_value(source, value_offset, explicit_vr, little_endian) {
                Ok(end_of_value) => end_of_value + 8,
                Err(ProbeParseError::NeedMore) => return DicomProbeStatus::NeedMore,
            }
        };
    }

    if eof {
        DicomProbeStatus::NotFound
    } else {
        DicomProbeStatus::NeedMore
    }
}

fn skip_probe_undefined_length_value(
    source: &[u8],
    mut position: usize,
    explicit_vr: bool,
    little_endian: bool,
) -> Result<usize, ProbeParseError> {
    while position + 8 <= source.len() {
        let tag = read_probe_tag(source, position, little_endian)?;
        if tag == SEQUENCE_DELIMITATION_TAG || tag == ITEM_DELIMITATION_TAG {
            return Ok(position);
        }

        if tag == ITEM_TAG {
            let item_length = read_probe_u32(source, position + 4, little_endian)? as usize;
            position += 8;
            position = if item_length == u32::MAX as usize {
                skip_probe_undefined_length_value(source, position, explicit_vr, little_endian)? + 8
            } else {
                let end = position.saturating_add(item_length);
                if end > source.len() {
                    return Err(ProbeParseError::NeedMore);
                }
                end
            };
            continue;
        }

        let header = parse_probe_element_header(source, position, explicit_vr, little_endian)?;
        let value_offset = position + header.header_length;
        position = if let Some(length) = header.length {
            let end = value_offset.saturating_add(length);
            if end > source.len() {
                return Err(ProbeParseError::NeedMore);
            }
            end
        } else {
            skip_probe_undefined_length_value(source, value_offset, explicit_vr, little_endian)? + 8
        };
    }

    Err(ProbeParseError::NeedMore)
}

fn parse_probe_element_header(
    source: &[u8],
    position: usize,
    explicit_vr: bool,
    little_endian: bool,
) -> Result<ProbeHeader, ProbeParseError> {
    let tag = read_probe_tag(source, position, little_endian)?;

    if explicit_vr {
        if position + 8 > source.len() {
            return Err(ProbeParseError::NeedMore);
        }

        let vr_bytes = [source[position + 4], source[position + 5]];
        let vr = VR::from_binary(vr_bytes).unwrap_or(VR::UN);
        if matches!(
            vr,
            VR::OB
                | VR::OD
                | VR::OF
                | VR::OL
                | VR::OV
                | VR::OW
                | VR::SQ
                | VR::UC
                | VR::UR
                | VR::UT
                | VR::UN
        ) {
            if position + 12 > source.len() {
                return Err(ProbeParseError::NeedMore);
            }

            let length = read_probe_u32(source, position + 8, little_endian)?;
            Ok(ProbeHeader {
                tag,
                header_length: 12,
                length: if length == u32::MAX {
                    None
                } else {
                    Some(length as usize)
                },
            })
        } else {
            let length = read_probe_u16(source, position + 6, little_endian)? as usize;
            Ok(ProbeHeader {
                tag,
                header_length: 8,
                length: Some(length),
            })
        }
    } else {
        if position + 8 > source.len() {
            return Err(ProbeParseError::NeedMore);
        }

        let length = read_probe_u32(source, position + 4, little_endian)?;
        Ok(ProbeHeader {
            tag,
            header_length: 8,
            length: if length == u32::MAX {
                None
            } else {
                Some(length as usize)
            },
        })
    }
}

fn read_probe_tag(source: &[u8], position: usize, little_endian: bool) -> Result<Tag, ProbeParseError> {
    Ok(Tag(
        read_probe_u16(source, position, little_endian)?,
        read_probe_u16(source, position + 2, little_endian)?,
    ))
}

fn read_probe_u16(source: &[u8], position: usize, little_endian: bool) -> Result<u16, ProbeParseError> {
    let Some(bytes) = source.get(position..position + 2) else {
        return Err(ProbeParseError::NeedMore);
    };

    Ok(if little_endian {
        u16::from_le_bytes([bytes[0], bytes[1]])
    } else {
        u16::from_be_bytes([bytes[0], bytes[1]])
    })
}

fn read_probe_u32(source: &[u8], position: usize, little_endian: bool) -> Result<u32, ProbeParseError> {
    let Some(bytes) = source.get(position..position + 4) else {
        return Err(ProbeParseError::NeedMore);
    };

    Ok(if little_endian {
        u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
    } else {
        u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
    })
}

fn decode_probe_text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes)
        .trim_matches(char::from(0))
        .trim_end()
        .to_owned()
}

fn probe_transfer_syntax_from_uid(uid: &str) -> ProbeTransferSyntax {
    match uid {
        uids::IMPLICIT_VR_LITTLE_ENDIAN => ProbeTransferSyntax {
            explicit_vr: false,
            little_endian: true,
        },
        "1.2.840.10008.1.2.2" => ProbeTransferSyntax {
            explicit_vr: true,
            little_endian: false,
        },
        _ => ProbeTransferSyntax {
            // Most transfer syntaxes use explicit VR little endian.
            explicit_vr: true,
            little_endian: true,
        },
    }
}

pub fn write_dicom_file<P>(object: &mut DefaultDicomObject, path: P) -> Result<(), WriteError>
where
    P: AsRef<Path>,
{
    object.write_to_file(path).map(|_| ())
}

pub fn write_dicom_bytes(object: &mut DefaultDicomObject) -> Result<Vec<u8>, WriteError> {
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
    let file_object =
        dataset.with_meta(FileMetaTableBuilder::new().transfer_syntax(transfer_syntax_uid))?;

    file_object.write_to_file(path)?;
    Ok(())
}

pub fn write_dataset_as_dicom_bytes(
    dataset: InMemDicomObject,
    transfer_syntax_uid: &str,
) -> Result<Vec<u8>, DicomIoError> {
    let file_object =
        dataset.with_meta(FileMetaTableBuilder::new().transfer_syntax(transfer_syntax_uid))?;

    let mut bytes = Vec::new();
    file_object.write_all(&mut bytes)?;
    Ok(bytes)
}

pub fn list_transfer_syntax_support() -> Vec<TransferSyntaxSupport> {
    let kakadu_enabled = kakadu_ffi_available_from_backend(&jpeg2000_backend());
    let ffmpeg_enabled = ffmpeg_available();
    let mut syntaxes = TransferSyntaxRegistry
        .iter()
        .map(|ts| TransferSyntaxSupport {
            uid: ts.uid().to_owned(),
            name: ts.name().to_owned(),
            encapsulated_pixel_data: is_encapsulated_transfer_syntax(ts),
            can_read_dataset: can_read_dataset(ts),
            can_write_dataset: can_write_dataset(ts),
            can_decode_pixel_data: can_decode_pixel_data(ts, kakadu_enabled, ffmpeg_enabled),
            can_encode_pixel_data: can_encode_pixel_data(ts, kakadu_enabled, ffmpeg_enabled),
        })
        .collect::<Vec<_>>();

    syntaxes.sort_by(|left, right| left.uid.cmp(&right.uid));
    syntaxes
}

/// Whether this build can write dataset content out under `uid` (e.g. for presentation-context
/// negotiation, where offering a transfer syntax we can only decode risks the peer accepting it
/// as the single transfer syntax for a context, then requiring an encode we can't perform). For a
/// non-encapsulated (native) transfer syntax like Explicit/Implicit VR Little Endian there's no
/// pixel data codec involved at all, so `can_encode_pixel_data` alone would always say `false`
/// here - mirrors `TransferSyntaxSupport::can_transcode_to`'s condition rather than reusing
/// `can_encode_pixel_data` directly.
pub fn can_encode_transfer_syntax(uid: &str) -> bool {
    let Some(ts) = TransferSyntaxRegistry.get(uid) else {
        return false;
    };
    can_write_dataset(ts)
        && (!is_encapsulated_transfer_syntax(ts)
            || can_encode_pixel_data(
                ts,
                kakadu_ffi_available_from_backend(&jpeg2000_backend()),
                ffmpeg_available(),
            ))
}

pub fn transcode_dcmnorm_object(
    object: &DefaultDicomObject,
    target_transfer_syntax_uid: &str,
) -> Result<DefaultDicomObject, TranscodeError> {
    let _scope = perf::scope("transcode.transcode_dcmnorm_object");
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
    let mut transcoded = transcode_dcmnorm_object(&object, target_transfer_syntax_uid)?;
    Ok(write_dicom_bytes(&mut transcoded)?)
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
    let mut transcoded = transcode_dcmnorm_object(&object, target_transfer_syntax_uid)?;
    write_dicom_file(&mut transcoded, output_path)?;
    Ok(())
}

pub(super) fn normalize_transfer_syntax_uid(uid: &str) -> &str {
    uid.trim_end_matches(|character: char| character.is_whitespace() || character == '\0')
}

fn can_read_dataset<D, R, W>(ts: &dcmnorm_encoding::TransferSyntax<D, R, W>) -> bool {
    !matches!(ts.codec(), Codec::Dataset(None))
}

fn can_write_dataset<D, R, W>(ts: &dcmnorm_encoding::TransferSyntax<D, R, W>) -> bool {
    !matches!(ts.codec(), Codec::Dataset(None))
}

fn can_decode_pixel_data<D, R, W>(
    ts: &dcmnorm_encoding::TransferSyntax<D, R, W>,
    kakadu_enabled: bool,
    _ffmpeg_enabled: bool,
) -> bool {
    let uid = ts.uid();
    matches!(ts.codec(), Codec::EncapsulatedPixelData(Some(_), _))
        || (kakadu_enabled && is_jpeg2000_transfer_syntax(uid))
        || (cfg!(feature = "ffmpeg-codec") && is_mpeg_transfer_syntax(uid))
        || (cfg!(feature = "jpeg-ls-codec") && is_jpeg_ls_transfer_syntax(uid))
}

fn can_encode_pixel_data<D, R, W>(
    ts: &dcmnorm_encoding::TransferSyntax<D, R, W>,
    _kakadu_enabled: bool,
    _ffmpeg_enabled: bool,
) -> bool {
    let uid = ts.uid();
    if is_jpeg2000_transfer_syntax(uid) {
        return false;
    }

    matches!(ts.codec(), Codec::EncapsulatedPixelData(_, Some(_)))
        || (cfg!(feature = "ffmpeg-codec") && is_mpeg_transfer_syntax(uid))
        || (cfg!(feature = "jpeg-ls-codec") && is_jpeg_ls_transfer_syntax(uid))
}

fn kakadu_ffi_available_from_backend(backend: &Jpeg2000Backend) -> bool {
    matches!(backend, Jpeg2000Backend::Kakadu { .. }) && kakadu_ffi_enabled()
}

fn decode_jpeg2000_with_kakadu(object: &DefaultDicomObject) -> Result<Vec<u8>, String> {
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
        .or_else(|| {
            object
                .get(tags::BITS_ALLOCATED)
                .and_then(|element| element.uint16().ok())
        })
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

    kakadu::decode_jpeg2000(
        &codestream,
        rows,
        cols,
        samples_per_pixel,
        bits_stored,
        is_signed,
    )
}

fn is_encapsulated_transfer_syntax<D, R, W>(ts: &dcmnorm_encoding::TransferSyntax<D, R, W>) -> bool {
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
        dcmnorm_core::value::Value::Primitive(_) => PixelDataRepresentation::Native,
        dcmnorm_core::value::Value::PixelSequence(_) => PixelDataRepresentation::Encapsulated,
        dcmnorm_core::value::Value::Sequence(_) => PixelDataRepresentation::Absent,
    }
}

fn decode_pixel_data(
    object: &mut DefaultDicomObject,
    source_ts: &dcmnorm_encoding::TransferSyntax,
) -> Result<(), TranscodeError> {
    let _scope = perf::scope("transcode.decode_pixel_data");
    let codec_preference = jpeg2000_codec_preference();

    if is_jpeg2000_transfer_syntax(source_ts.uid()) {
        if let Some(actual_components) = jpeg2000_component_mismatch(object, 0) {
            apply_jpeg2000_component_correction(object, actual_components);
        }
    }

    let jpeg2000_uses_mct = is_jpeg2000_transfer_syntax(source_ts.uid())
        .then(|| jpeg2000_frame_uses_mct(object, 0))
        .flatten();

    if is_jpeg2000_transfer_syntax(source_ts.uid()) {
        let backend = jpeg2000_backend();
        let backend_name = backend.name();
        let number_of_frames = object
            .get(tags::NUMBER_OF_FRAMES)
            .and_then(|element| element.to_str().ok())
            .and_then(|value| value.trim().parse::<usize>().ok())
            .unwrap_or(1);
        jpeg2000_debug_log(format!(
            "decode start: uid={} name={} backend={} preference={:?} kakadu_ffi_enabled={} frames={}",
            source_ts.uid(),
            source_ts.name(),
            backend_name,
            codec_preference,
            kakadu_ffi_enabled(),
            number_of_frames
        ));
    }

    if is_jpeg2000_transfer_syntax(source_ts.uid())
        && kakadu_ffi_enabled()
        && codec_preference != Jpeg2000CodecPreference::OpenJpeg
    {
        jpeg2000_debug_log("attempting Kakadu decode");
        match decode_jpeg2000_with_kakadu(object) {
            Ok(decoded) => {
                jpeg2000_debug_log(format!("Kakadu decode succeeded ({} decoded bytes)", decoded.len()));
                replace_with_native_pixel_data(object, decoded)?;
                normalize_decoded_pixel_data_attributes(object, source_ts.uid(), jpeg2000_uses_mct);
                object.meta_mut().set_transfer_syntax(
                    TransferSyntaxRegistry
                        .get(uids::EXPLICIT_VR_LITTLE_ENDIAN)
                        .expect("explicit VR little endian transfer syntax must exist"),
                );
                return Ok(());
            }
            Err(error) => {
                jpeg2000_debug_log(format!("Kakadu decode failed: {error}"));
                if codec_preference == Jpeg2000CodecPreference::Kakadu {
                    return Err(TranscodeError::DecodePixelData {
                        uid: source_ts.uid().to_owned(),
                        name: source_ts.name().to_owned(),
                        message: format!("forced kakadu decode failed: {error}"),
                    });
                }
            }
        }
    } else if is_jpeg2000_transfer_syntax(source_ts.uid()) {
        let reason = if codec_preference == Jpeg2000CodecPreference::OpenJpeg {
            "Kakadu decode not attempted because DCMNORM_JPEG2000_CODEC=openjpeg"
        } else {
            "Kakadu decode not attempted because kakadu-ffi feature is disabled"
        };
        jpeg2000_debug_log(reason);
    }

    // Try MPEG decoding
    if is_mpeg_transfer_syntax(source_ts.uid()) {
        match mpeg::decode_mpeg_pixel_data(object) {
            Ok(decoded) => {
                replace_with_native_pixel_data(object, decoded)?;
                normalize_decoded_pixel_data_attributes(object, source_ts.uid(), jpeg2000_uses_mct);
                object.meta_mut().set_transfer_syntax(
                    TransferSyntaxRegistry
                        .get(uids::EXPLICIT_VR_LITTLE_ENDIAN)
                        .expect("explicit VR little endian transfer syntax must exist"),
                );
                return Ok(());
            }
            Err(error) => {
                return Err(TranscodeError::DecodePixelData {
                    uid: source_ts.uid().to_owned(),
                    name: source_ts.name().to_owned(),
                    message: error,
                });
            }
        }
    }

    // Try JPEG-LS decoding
    if is_jpeg_ls_transfer_syntax(source_ts.uid()) {
        match jpeg_ls::decode_jpeg_ls_pixel_data(object) {
            Ok(decoded) => {
                replace_with_native_pixel_data(object, decoded)?;
                normalize_decoded_pixel_data_attributes(object, source_ts.uid(), jpeg2000_uses_mct);
                object.meta_mut().set_transfer_syntax(
                    TransferSyntaxRegistry
                        .get(uids::EXPLICIT_VR_LITTLE_ENDIAN)
                        .expect("explicit VR little endian transfer syntax must exist"),
                );
                return Ok(());
            }
            Err(error) => {
                return Err(TranscodeError::DecodePixelData {
                    uid: source_ts.uid().to_owned(),
                    name: source_ts.name().to_owned(),
                    message: error,
                });
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
    let number_of_frames = object
        .get(tags::NUMBER_OF_FRAMES)
        .and_then(|element| element.to_str().ok())
        .and_then(|value| value.trim().parse::<usize>().ok())
        .unwrap_or(1);

    if should_parallel_frame_decode(object, source_ts.uid(), number_of_frames) {
        let _parallel_scope = perf::scope("transcode.decode_pixel_data_parallel_frames");
        match decode_pixel_data_parallel_frames(reader.as_ref(), object, number_of_frames) {
            Ok(bytes) => {
                decoded = bytes;
            }
            Err(error) => {
                if perf::enabled() {
                    eprintln!(
                        "[dcmnorm:perf] transcode.decode_pixel_data_parallel_fallback: {}",
                        error
                    );
                }
                if is_jpeg2000_transfer_syntax(source_ts.uid()) {
                    jpeg2000_debug_log(format!(
                        "parallel frame decode fallback to serial reader decode: {error}"
                    ));
                }
                reader
                    .decode(object, &mut decoded)
                    .map_err(|decode_error| TranscodeError::DecodePixelData {
                        uid: source_ts.uid().to_owned(),
                        name: source_ts.name().to_owned(),
                        message: decode_error.to_string(),
                    })?;
            }
        }
    } else {
        if is_jpeg2000_transfer_syntax(source_ts.uid()) {
            jpeg2000_debug_log("using codec registry reader decode path");
        }
        reader
            .decode(object, &mut decoded)
            .map_err(|error| TranscodeError::DecodePixelData {
                uid: source_ts.uid().to_owned(),
                name: source_ts.name().to_owned(),
                message: error.to_string(),
            })?;
    }

    if is_jpeg2000_transfer_syntax(source_ts.uid()) {
        jpeg2000_debug_log(format!("codec registry reader decode succeeded ({} decoded bytes)", decoded.len()));
    }

    replace_with_native_pixel_data(object, decoded)?;
    normalize_decoded_pixel_data_attributes(object, source_ts.uid(), jpeg2000_uses_mct);
    object.meta_mut().set_transfer_syntax(
        TransferSyntaxRegistry
            .get(uids::EXPLICIT_VR_LITTLE_ENDIAN)
            .expect("explicit VR little endian transfer syntax must exist"),
    );

    Ok(())
}

fn decode_pixel_data_parallel_frames(
    reader: &(dyn dcmnorm_encoding::adapters::PixelDataReader + Send + Sync),
    object: &DefaultDicomObject,
    number_of_frames: usize,
) -> Result<Vec<u8>, String> {
    let frames = (0..number_of_frames)
        .into_par_iter()
        .map(|frame_index| {
            let mut frame_bytes = Vec::new();
            reader
                .decode_frame(object, frame_index as u32, &mut frame_bytes)
                .map_err(|error| format!("frame {} decode failed: {}", frame_index, error))?;
            Ok(frame_bytes)
        })
        .collect::<Vec<Result<Vec<u8>, String>>>();

    let mut decoded = Vec::new();
    for frame in frames {
        decoded.extend(frame?);
    }

    Ok(decoded)
}

fn should_parallel_frame_decode(
    object: &DefaultDicomObject,
    source_transfer_syntax_uid: &str,
    number_of_frames: usize,
) -> bool {
    if number_of_frames <= 1 {
        return false;
    }

    if !is_parallel_decode_transfer_syntax(source_transfer_syntax_uid) {
        return false;
    }

    let Some(pixel_data) = object.get(tags::PIXEL_DATA) else {
        return false;
    };

    let Value::PixelSequence(pixel_sequence) = pixel_data.value() else {
        return false;
    };

    let fragment_count = pixel_sequence.fragments().len();
    let offset_count = pixel_sequence.offset_table().len();

    fragment_count == number_of_frames || offset_count >= number_of_frames.saturating_sub(1)
}

fn is_parallel_decode_transfer_syntax(uid: &str) -> bool {
    matches!(
        normalize_transfer_syntax_uid(uid),
        // JPEG Baseline and Extended
        "1.2.840.10008.1.2.4.50"
            | "1.2.840.10008.1.2.4.51"
            // JPEG Lossless and SV1
            | "1.2.840.10008.1.2.4.57"
            | "1.2.840.10008.1.2.4.70"
            // JPEG-LS
            | "1.2.840.10008.1.2.4.80"
            | "1.2.840.10008.1.2.4.81"
            // JPEG 2000
            | "1.2.840.10008.1.2.4.90"
            | "1.2.840.10008.1.2.4.91"
            // RLE Lossless
            | "1.2.840.10008.1.2.5"
    )
}

fn encode_pixel_data(
    object: &mut DefaultDicomObject,
    target_ts: &dcmnorm_encoding::TransferSyntax,
) -> Result<(), TranscodeError> {
    let _scope = perf::scope("transcode.encode_pixel_data");
    if is_jpeg2000_transfer_syntax(target_ts.uid()) {
        return Err(TranscodeError::UnsupportedTargetTransferSyntax {
            uid: target_ts.uid().to_owned(),
            name: target_ts.name().to_owned(),
            reason: "JPEG2000 encoding is disabled in this build".to_owned(),
        });
    }

    // Try MPEG encoding
    if is_mpeg_transfer_syntax(target_ts.uid()) {
        match mpeg::encode_mpeg_pixel_data(object, target_ts.uid()) {
            Ok(fragments) => {
                replace_with_encapsulated_pixel_data(object, vec![0], fragments);
                return Ok(());
            }
            Err(error) => {
                return Err(TranscodeError::EncodePixelData {
                    uid: target_ts.uid().to_owned(),
                    name: target_ts.name().to_owned(),
                    message: error,
                });
            }
        }
    }

    // Try JPEG-LS encoding
    if is_jpeg_ls_transfer_syntax(target_ts.uid()) {
        let lossless = target_ts.uid() == "1.2.840.10008.1.2.4.80";
        match jpeg_ls::encode_jpeg_ls_pixel_data(object, lossless) {
            Ok(fragments) => {
                replace_with_encapsulated_pixel_data(object, vec![0], fragments);
                return Ok(());
            }
            Err(error) => {
                return Err(TranscodeError::EncodePixelData {
                    uid: target_ts.uid().to_owned(),
                    name: target_ts.name().to_owned(),
                    message: error,
                });
            }
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
        .encode(
            object,
            EncodeOptions::default(),
            &mut fragments,
            &mut offset_table,
        )
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

fn normalize_decoded_pixel_data_attributes(
    object: &mut DefaultDicomObject,
    source_ts_uid: &str,
    jpeg2000_uses_mct: Option<bool>,
) {
    let samples_per_pixel = object
        .get(tags::SAMPLES_PER_PIXEL)
        .and_then(|element| element.uint16().ok())
        .unwrap_or(1);

    if samples_per_pixel > 1 {
        let current_photometric = object
            .get(tags::PHOTOMETRIC_INTERPRETATION)
            .and_then(|element| element.to_str().ok())
            .map(|value| value.trim().to_owned())
            .unwrap_or_default();
        let is_rle = normalize_transfer_syntax_uid(source_ts_uid) == uids::RLE_LOSSLESS;
        // Only RLE Lossless, and JPEG 2000 codestreams that did not apply the
        // Multiple Component Transformation, hand back raw un-converted YBR
        // component samples. See jpeg2000_frame_uses_mct for why JPEG 2000
        // otherwise already produces RGB after standard decompression.
        let preserves_ybr_on_decode =
            is_rle || (is_jpeg2000_transfer_syntax(source_ts_uid) && jpeg2000_uses_mct == Some(false));
        let is_ybr = current_photometric.starts_with("YBR_");
        let target_photometric = if preserves_ybr_on_decode && is_ybr {
            current_photometric
        } else {
            "RGB".to_owned()
        };

        object.put(DataElement::new(
            tags::PHOTOMETRIC_INTERPRETATION,
            VR::CS,
            PrimitiveValue::from(target_photometric),
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
        .filter(|value| {
            matches!(
                value.as_str(),
                "MONOCHROME1" | "MONOCHROME2" | "PALETTE COLOR"
            )
        })
        .unwrap_or_else(|| "MONOCHROME2".to_owned());

    object.put(DataElement::new(
        tags::PHOTOMETRIC_INTERPRETATION,
        VR::CS,
        PrimitiveValue::from(normalized_photometric),
    ));
    object.remove_element(tags::PLANAR_CONFIGURATION);
}

#[cfg(test)]
mod tests {
    use super::is_jpeg2000_transfer_syntax;

    #[test]
    fn recognizes_high_throughput_jpeg2000_as_jpeg2000() {
        assert!(is_jpeg2000_transfer_syntax("1.2.840.10008.1.2.4.90"));
        assert!(is_jpeg2000_transfer_syntax("1.2.840.10008.1.2.4.91"));
        assert!(is_jpeg2000_transfer_syntax("1.2.840.10008.1.2.4.92"));
        assert!(is_jpeg2000_transfer_syntax("1.2.840.10008.1.2.4.93"));
        assert!(is_jpeg2000_transfer_syntax("1.2.840.10008.1.2.4.201"));
        assert!(is_jpeg2000_transfer_syntax("1.2.840.10008.1.2.4.202"));
        assert!(is_jpeg2000_transfer_syntax("1.2.840.10008.1.2.4.203"));
        // JPIP HTJ2K Referenced (Deflate) - no embedded codestream, not a
        // JPEG2000 decode-path case.
        assert!(!is_jpeg2000_transfer_syntax("1.2.840.10008.1.2.4.204"));
        assert!(!is_jpeg2000_transfer_syntax("1.2.840.10008.1.2.4.205"));
    }
}
