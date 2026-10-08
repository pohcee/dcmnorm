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
    DefaultDicomObject, DeferredValues, FileMetaTableBuilder, InMemDicomObject, OpenFileOptions,
};
use dcmnorm_transcode::TransferSyntaxRegistry;
use rayon::prelude::*;

use super::jpeg2000_openjpeg;
use super::jpeg_ls;
use super::jpeg_xl;
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

/// The engine that decodes/encodes `uid`'s pixel data when it's a JPEG 2000-family transfer
/// syntax: always OpenHTJ2K for HTJ2K (`.201`-`.203`) - Kakadu is only ever used for classic
/// JPEG 2000, see `decode_pixel_data` - otherwise [`jpeg2000_backend_name`].
pub fn jpeg2000_engine_name_for(uid: &str) -> &'static str {
    if is_htj2k_transfer_syntax(uid) {
        "openhtj2k"
    } else {
        jpeg2000_backend_name()
    }
}

/// One-line summary of the JPEG 2000 codec setup, for diagnostics: classic backend, the OpenHTJ2K
/// CPU variant in use, and the per-call thread count.
pub fn jpeg2000_codec_summary() -> String {
    format!(
        "classic={} htj2k=openhtj2k({}) threads={}",
        jpeg2000_backend_name(),
        dcmnorm_jpeg2000::htj2k::variant_name(),
        dcmnorm_jpeg2000::threads::configured_threads()
    )
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

/// The High-Throughput JPEG 2000 (Part-15) subset of [`is_jpeg2000_transfer_syntax`]. HTJ2K is
/// always decoded and encoded with OpenHTJ2K, never Kakadu - see `decode_pixel_data`.
pub(super) fn is_htj2k_transfer_syntax(uid: &str) -> bool {
    matches!(
        normalize_transfer_syntax_uid(uid),
        "1.2.840.10008.1.2.4.201" | "1.2.840.10008.1.2.4.202" | "1.2.840.10008.1.2.4.203"
    )
}

/// HTJ2K transfer syntaxes this build can *encode* (losslessly, via OpenHTJ2K - see
/// `encode_htj2k_pixel_data`). `.202` (RPCL Options) is decode-only: its DICOM profile also
/// requires PLT/TLM marker segments, which OpenHTJ2K's encoder doesn't write.
fn is_htj2k_encode_transfer_syntax(uid: &str) -> bool {
    matches!(
        normalize_transfer_syntax_uid(uid),
        "1.2.840.10008.1.2.4.201" | "1.2.840.10008.1.2.4.203"
    )
}

/// Classic JPEG 2000 (Lossless Only / may-be-lossy), the two classic JPEG2000 transfer syntaxes
/// this build can *encode* - see `encode_jpeg2000_pixel_data`'s doc comment for why Part 2
/// multi-component (`.92`/`.93`) isn't included even though it's grouped together for *decode*
/// by [`is_jpeg2000_transfer_syntax`]. HTJ2K encode is [`is_htj2k_encode_transfer_syntax`].
fn is_classic_jpeg2000_encode_transfer_syntax(uid: &str) -> bool {
    matches!(
        normalize_transfer_syntax_uid(uid),
        "1.2.840.10008.1.2.4.90" | "1.2.840.10008.1.2.4.91"
    )
}

/// DICOM doesn't mandate a specific compression ratio for `.91` (JPEG 2000 Image Compression,
/// as opposed to `.90` Lossless Only); this picks a conservative, commonly-cited "visually
/// lossless" target for radiology (see e.g. ACR guidance on lossy JPEG2000 for CT/MR/CR, which
/// typically cites ratios in the 10:1-15:1 range) rather than an arbitrary value, mirroring
/// `jpeg_ls::NEAR_LOSSLESS_STEP`'s reasoning for its own default.
const JPEG2000_LOSSY_COMPRESSION_RATIO: f32 = 10.0;

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

fn is_jpeg_xl_transfer_syntax(uid: &str) -> bool {
    matches!(
        normalize_transfer_syntax_uid(uid),
        "1.2.840.10008.1.2.4.110" | "1.2.840.10008.1.2.4.111" | "1.2.840.10008.1.2.4.112"
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

// Both readers fall back in turn when a strict read fails, keeping the strict read's error if
// every fallback fails too: first to detecting explicit vs implicit VR from the data set itself
// (`flexible_decoding`, for files that declare Explicit VR Little Endian but write Implicit VR -
// a strict read of those fails at the first element), then to a data set without a meta group.
// A file the strict read accepts is never reinterpreted.
pub fn read_dicom_file<P>(path: P) -> Result<DefaultDicomObject, ReadError>
where
    P: AsRef<Path>,
{
    let path_ref = path.as_ref();
    let options = OpenFileOptions::new().read_preamble(ReadPreamble::Always);

    options
        .clone()
        .open_file(path_ref)
        .or_else(|error| options.flexible_decoding(true).open_file(path_ref).map_err(|_| error))
        .or_else(|error| match std::fs::read(path_ref) {
            Ok(bytes) => read_dicom_dataset_without_meta(&bytes).ok_or(error),
            Err(_) => Err(error),
        })
}

pub fn read_dicom_bytes(bytes: impl AsRef<[u8]>) -> Result<DefaultDicomObject, ReadError> {
    let bytes = bytes.as_ref();
    let options = OpenFileOptions::new().read_preamble(ReadPreamble::Always);

    options
        .clone()
        .from_reader(Cursor::new(bytes))
        .or_else(|error| options.flexible_decoding(true).from_reader(Cursor::new(bytes)).map_err(|_| error))
        .or_else(|error| read_dicom_dataset_without_meta(bytes).ok_or(error))
}

/// Read `path` for writing DICOM JSON with `BulkDataURI` references, without reading the bulk
/// values themselves: every value that would be written as a `BulkDataURI` anyway - PixelData
/// (native or encapsulated), and any other bulk-VR value (OB/OW/OF/OD/OL/OV/UN) larger than
/// the inline threshold, at any depth - is skipped by seeking past it, and its file location
/// returned in [`DeferredValues`]. Pass that as `DicomJsonWriteOptions::deferred_bulk_data`
/// (with `bulk_data_mode: Uri`) to write the same JSON a full read + `bulk_data_source` scan
/// produces, at a cost independent of the bulk data's size: a 29MB multi-frame ultrasound
/// reads ~2KB instead of 29MB (three times over: file buffer, parsed copy, verifying scan).
///
/// The returned object's deferred elements hold empty placeholders (see [`DeferredValues`]):
/// it is only fit for writing that JSON, not for rendering, transcoding or writing DICOM.
///
/// Returns `Ok(None)` when the shortcut doesn't apply - Explicit VR Big Endian (its bulk bytes
/// in the file aren't the little-endian bytes a URI consumer reads back), a deflated data set,
/// or a file too short for the ranges it declares - and the caller should read in full.
pub fn read_dicom_file_deferring_bulk_data<P>(
    path: P,
) -> Result<Option<(DefaultDicomObject, DeferredValues)>, ReadError>
where
    P: AsRef<Path>,
{
    let path = path.as_ref();
    let _scope = perf::scope("io.read_dicom_file_deferring_bulk_data");
    let mut file = std::io::BufReader::new(std::fs::File::open(path).map_err(|source| ReadError::Io {
        source,
        context: "opening file",
    })?);
    let Some(meta) = dcmnorm_object::FileMetaTable::read_from(&mut file)? else {
        return Ok(None);
    };
    if normalize_transfer_syntax_uid(&meta.transfer_syntax) == "1.2.840.10008.1.2.2" {
        return Ok(None);
    }

    // Exactly the values the JSON writer would emit as a BulkDataURI: a bulk VR (WaveformData
    // excepted - always inlined), more than INLINE_BINARY_URI_THRESHOLD bytes, and a whole
    // number of that VR's elements (a value with a trailing partial element doesn't parse to
    // its file bytes, so a scan of the source never referenced it either).
    let defer = |header: &dcmnorm_core::header::DataElementHeader| {
        let Some(length) = header.len.get() else { return false };
        let element_size = match header.vr {
            VR::OB | VR::UN => 1,
            VR::OW => 2,
            VR::OF | VR::OL => 4,
            VR::OD | VR::OV => 8,
            _ => return false,
        };
        header.tag != tags::WAVEFORM_DATA
            && length as usize > super::bulk_data::INLINE_BINARY_URI_THRESHOLD
            && length % element_size == 0
    };

    OpenFileOptions::new()
        .read_preamble(ReadPreamble::Always)
        .open_file_deferring(path, &defer, true)
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
    transcode_dcmnorm_object_owned(object.clone(), target_transfer_syntax_uid)
}

/// [`transcode_dcmnorm_object`], taking ownership of `object` so callers that don't need the
/// source afterwards skip a deep clone of it - for a native multi-frame object that clone is a
/// full copy of PixelData (e.g. ~29MB for a 227-frame ultrasound cine), and when the source
/// already has the target transfer syntax it's the only work there is to do.
pub fn transcode_dcmnorm_object_owned(
    object: DefaultDicomObject,
    target_transfer_syntax_uid: &str,
) -> Result<DefaultDicomObject, TranscodeError> {
    let _scope = perf::scope("transcode.transcode_dcmnorm_object");
    let source_uid = normalize_transfer_syntax_uid(object.meta().transfer_syntax());
    let target_uid = normalize_transfer_syntax_uid(target_transfer_syntax_uid);

    if source_uid == target_uid {
        return Ok(object);
    }

    let source_ts = TransferSyntaxRegistry
        .get(source_uid)
        .ok_or_else(|| TranscodeError::UnknownTransferSyntax(source_uid.to_owned()))?;
    let target_ts = TransferSyntaxRegistry
        .get(target_uid)
        .ok_or_else(|| TranscodeError::UnknownTransferSyntax(target_uid.to_owned()))?;

    let pixel_representation = pixel_data_representation(&object);
    let mut transcoded = object;

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
    let mut transcoded = transcode_dcmnorm_object_owned(object, target_transfer_syntax_uid)?;
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
    let mut transcoded = transcode_dcmnorm_object_owned(object, target_transfer_syntax_uid)?;
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
        || (cfg!(feature = "jpeg-xl-codec") && is_jpeg_xl_transfer_syntax(uid))
}

fn can_encode_pixel_data<D, R, W>(
    ts: &dcmnorm_encoding::TransferSyntax<D, R, W>,
    _kakadu_enabled: bool,
    _ffmpeg_enabled: bool,
) -> bool {
    let uid = ts.uid();
    if is_htj2k_transfer_syntax(uid) {
        return is_htj2k_encode_transfer_syntax(uid);
    }
    if is_jpeg2000_transfer_syntax(uid) {
        // Only classic JPEG2000 (.90/.91) can be encoded, and only via the OpenJPEG backend -
        // see `encode_jpeg2000_pixel_data`'s doc comment for why Kakadu and Part 2
        // multi-component (.92/.93) aren't included.
        return cfg!(feature = "jpeg2000-openjpeg-encode") && is_classic_jpeg2000_encode_transfer_syntax(uid);
    }

    matches!(ts.codec(), Codec::EncapsulatedPixelData(_, Some(_)))
        || (cfg!(feature = "ffmpeg-codec") && is_mpeg_transfer_syntax(uid))
        || (cfg!(feature = "jpeg-ls-codec") && is_jpeg_ls_transfer_syntax(uid))
}

fn kakadu_ffi_available_from_backend(backend: &Jpeg2000Backend) -> bool {
    matches!(backend, Jpeg2000Backend::Kakadu { .. }) && kakadu_ffi_enabled()
}

/// Encodes native pixel data to classic JPEG 2000 (`.90` Lossless Only / `.91`), one fragment
/// per frame (PS3.5's standard encapsulation convention for JPEG-family codecs, same as
/// `jpeg_ls::encode_jpeg_ls_pixel_data`).
///
/// Always uses the raw-FFI OpenJPEG backend (`jpeg2000_openjpeg::encode_jpeg2000_with_openjpeg`),
/// never Kakadu, regardless of `jpeg2000_backend()`/`DCMNORM_JPEG2000_CODEC` (which only govern
/// *decode* preference): a Kakadu encoder exists (`kakadu::encode_jpeg2000`) but was found to
/// produce incorrect pixel data on the currently-licensed Kakadu v7.8 SDK - see that function's
/// doc comment. `.92`/`.93` (Part 2 multi-component) aren't handled here at all: Part 2
/// multi-component encode was never implemented on either backend. HTJ2K is
/// [`encode_htj2k_pixel_data`].
fn encode_jpeg2000_pixel_data(
    object: &DefaultDicomObject,
    target_uid: &str,
) -> Result<Vec<Vec<u8>>, String> {
    let pixel_data = object
        .element(tags::PIXEL_DATA)
        .map_err(|e| format!("missing PixelData: {e}"))?
        .to_bytes()
        .map_err(|e| format!("failed to access pixel data: {e}"))?
        .to_vec();

    let rows = object
        .get(tags::ROWS)
        .and_then(|e| e.uint16().ok())
        .ok_or_else(|| "missing Rows attribute".to_owned())?;
    let cols = object
        .get(tags::COLUMNS)
        .and_then(|e| e.uint16().ok())
        .ok_or_else(|| "missing Columns attribute".to_owned())?;
    let samples_per_pixel = object.get(tags::SAMPLES_PER_PIXEL).and_then(|e| e.uint16().ok()).unwrap_or(1);
    let bits_allocated = object
        .get(tags::BITS_ALLOCATED)
        .and_then(|e| e.uint16().ok())
        .ok_or_else(|| "missing BitsAllocated attribute".to_owned())?;
    let bits_stored = object.get(tags::BITS_STORED).and_then(|e| e.uint16().ok()).unwrap_or(bits_allocated);
    let is_signed = object
        .get(tags::PIXEL_REPRESENTATION)
        .and_then(|e| e.uint16().ok())
        .unwrap_or(0)
        != 0;
    let number_of_frames = object
        .get(tags::NUMBER_OF_FRAMES)
        .and_then(|e| e.to_str().ok())
        .and_then(|s| s.trim().parse::<usize>().ok())
        .unwrap_or(1)
        .max(1);

    if bits_allocated != 8 && bits_allocated != 16 {
        return Err(format!("unsupported BitsAllocated for JPEG2000 encoding: {bits_allocated}"));
    }
    let bytes_per_sample = if bits_allocated <= 8 { 1usize } else { 2usize };
    let frame_len =
        usize::from(rows) * usize::from(cols) * usize::from(samples_per_pixel) * bytes_per_sample;
    let expected_len = frame_len * number_of_frames;
    if pixel_data.len() < expected_len {
        return Err(format!(
            "PixelData too short for {number_of_frames} frame(s) of {rows}x{cols}x{samples_per_pixel} \
             at {bits_allocated} bits allocated: expected at least {expected_len} bytes, got {}",
            pixel_data.len()
        ));
    }

    let lossless = normalize_transfer_syntax_uid(target_uid) == "1.2.840.10008.1.2.4.90";

    let mut fragments = Vec::with_capacity(number_of_frames);
    for frame_index in 0..number_of_frames {
        let start = frame_index * frame_len;
        let frame_bytes = &pixel_data[start..start + frame_len];
        let encoded = jpeg2000_openjpeg::encode_jpeg2000_with_openjpeg(
            frame_bytes,
            u32::from(rows),
            u32::from(cols),
            u32::from(samples_per_pixel),
            u32::from(bits_stored),
            is_signed,
            lossless,
            JPEG2000_LOSSY_COMPRESSION_RATIO,
        )
        .map_err(|e| format!("JPEG2000 encode failed for frame {frame_index}: {e}"))?;
        fragments.push(encoded);
    }

    Ok(fragments)
}

/// Encodes native pixel data to HTJ2K (`.201` / `.203`) with OpenHTJ2K, one fragment per frame.
/// Always lossless (reversible 5/3, single quality layer) - valid for `.203` as well, which allows
/// but doesn't require lossy compression - and never applies a colour transform, so RGB stays RGB
/// and PhotometricInterpretation doesn't change. Multi-component frames are deinterleaved into
/// planes; signed samples are sign-extended from BitsStored.
fn encode_htj2k_pixel_data(object: &DefaultDicomObject) -> Result<Vec<Vec<u8>>, String> {
    let pixel_data = object
        .element(tags::PIXEL_DATA)
        .map_err(|e| format!("missing PixelData: {e}"))?
        .to_bytes()
        .map_err(|e| format!("failed to access pixel data: {e}"))?;

    let rows = object
        .get(tags::ROWS)
        .and_then(|e| e.uint16().ok())
        .ok_or_else(|| "missing Rows attribute".to_owned())?;
    let cols = object
        .get(tags::COLUMNS)
        .and_then(|e| e.uint16().ok())
        .ok_or_else(|| "missing Columns attribute".to_owned())?;
    let samples_per_pixel = object.get(tags::SAMPLES_PER_PIXEL).and_then(|e| e.uint16().ok()).unwrap_or(1);
    let bits_allocated = object
        .get(tags::BITS_ALLOCATED)
        .and_then(|e| e.uint16().ok())
        .ok_or_else(|| "missing BitsAllocated attribute".to_owned())?;
    let bits_stored = object.get(tags::BITS_STORED).and_then(|e| e.uint16().ok()).unwrap_or(bits_allocated);
    let is_signed = object
        .get(tags::PIXEL_REPRESENTATION)
        .and_then(|e| e.uint16().ok())
        .unwrap_or(0)
        != 0;
    let number_of_frames = object
        .get(tags::NUMBER_OF_FRAMES)
        .and_then(|e| e.to_str().ok())
        .and_then(|s| s.trim().parse::<usize>().ok())
        .unwrap_or(1)
        .max(1);

    if bits_allocated != 8 && bits_allocated != 16 {
        return Err(format!("unsupported BitsAllocated for HTJ2K encoding: {bits_allocated}"));
    }
    if bits_stored == 0 || bits_stored > bits_allocated {
        return Err(format!("unsupported BitsStored {bits_stored} for BitsAllocated {bits_allocated}"));
    }
    let bytes_per_sample = usize::from(bits_allocated / 8);
    let spp = usize::from(samples_per_pixel);
    let pixels = usize::from(rows) * usize::from(cols);
    let frame_len = pixels * spp * bytes_per_sample;
    let expected_len = frame_len * number_of_frames;
    if pixel_data.len() < expected_len {
        return Err(format!(
            "PixelData too short for {number_of_frames} frame(s) of {rows}x{cols}x{samples_per_pixel} \
             at {bits_allocated} bits allocated: expected at least {expected_len} bytes, got {}",
            pixel_data.len()
        ));
    }

    // Masks off any bits above BitsStored (overlay bits in the high bits are allowed by the
    // standard) and sign-extends from BitsStored for signed data.
    let shift = 32 - u32::from(bits_stored);
    let sample = |raw: u32| -> i32 {
        if is_signed {
            ((raw << shift) as i32) >> shift
        } else {
            ((raw << shift) >> shift) as i32
        }
    };

    let mut planes = vec![vec![0i32; pixels]; spp];
    let mut fragments = Vec::with_capacity(number_of_frames);
    for frame_index in 0..number_of_frames {
        let frame = &pixel_data[frame_index * frame_len..(frame_index + 1) * frame_len];
        for (i, pixel) in frame.chunks_exact(spp * bytes_per_sample).enumerate() {
            for (c, value) in pixel.chunks_exact(bytes_per_sample).enumerate() {
                let raw = if bytes_per_sample == 2 {
                    u32::from(u16::from_le_bytes([value[0], value[1]]))
                } else {
                    u32::from(value[0])
                };
                planes[c][i] = sample(raw);
            }
        }
        let plane_refs: Vec<&[i32]> = planes.iter().map(Vec::as_slice).collect();
        let encoded = dcmnorm_jpeg2000::htj2k::encode_lossless(
            &plane_refs,
            u32::from(cols),
            u32::from(rows),
            bits_stored as u8,
            is_signed,
            dcmnorm_jpeg2000::htj2k::Progression::Lrcp,
        )
        .map_err(|e| format!("HTJ2K encode failed for frame {frame_index}: {e}"))?;
        fragments.push(encoded);
    }

    Ok(fragments)
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

    // Kakadu is only ever an override for classic JPEG 2000. HTJ2K always goes to OpenHTJ2K (the
    // registry's Htj2kAdapter below): the licensed Kakadu v7.8 predates HTJ2K, and handing it an
    // HT-coded codestream doesn't fail cleanly - `kdu_codestream::create()` can hang indefinitely
    // (verified empirically) - and OpenHTJ2K is the faster HTJ2K decoder regardless (see
    // docs/jpeg2000-codec-evaluation.md).
    let is_htj2k = is_htj2k_transfer_syntax(source_ts.uid());
    let kakadu_can_decode = kakadu_ffi_enabled() && !is_htj2k;

    if is_jpeg2000_transfer_syntax(source_ts.uid())
        && kakadu_can_decode
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
        let reason = if is_htj2k {
            "HTJ2K: decoding with OpenHTJ2K (Kakadu is only used for classic JPEG 2000)"
        } else if codec_preference == Jpeg2000CodecPreference::OpenJpeg {
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

    // Try JPEG XL decoding
    if is_jpeg_xl_transfer_syntax(source_ts.uid()) {
        match jpeg_xl::decode_jpeg_xl_pixel_data(object) {
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
            // JPEG 2000 (not HTJ2K: OpenHTJ2K serializes its calls and spreads each frame across its
            // own pool, so frame-parallel workers would only block on its lock)
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
    if is_htj2k_encode_transfer_syntax(target_ts.uid()) {
        match encode_htj2k_pixel_data(object) {
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
    } else if is_classic_jpeg2000_encode_transfer_syntax(target_ts.uid()) {
        match encode_jpeg2000_pixel_data(object, target_ts.uid()) {
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
    } else if is_jpeg2000_transfer_syntax(target_ts.uid()) {
        return Err(TranscodeError::UnsupportedTargetTransferSyntax {
            uid: target_ts.uid().to_owned(),
            name: target_ts.name().to_owned(),
            reason: "JPEG2000 Part 2 multi-component and HTJ2K with RPCL Options (.202) encoding \
                     are not supported in this build (classic JPEG2000 .90/.91 and HTJ2K \
                     .201/.203 are)"
                .to_owned(),
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
    use super::{is_jpeg2000_transfer_syntax, read_dicom_file, write_dicom_file};
    use std::path::PathBuf;

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

    /// HTJ2K must always decode through OpenHTJ2K (the registry's Htj2kAdapter), never Kakadu:
    /// verified empirically against a real Kakadu v7.8 SDK that `decode_jpeg2000_with_kakadu`
    /// hangs `kdu_codestream::create()` indefinitely when handed a genuine HT-coded (Part-15)
    /// codestream, rather than failing cleanly - v7.8 predates HTJ2K support entirely. This
    /// exercises the real end-to-end `transcode_dcmnorm_object` path with the default `Auto`
    /// codec preference against a real HT codestream (produced by `openjph-core`, an independent
    /// HTJ2K encoder, not this codebase's own code). In a `kakadu-ffi` build a regression here
    /// hangs rather than fails, which is the correct failure mode for a hang bug: a fast, wrong
    /// answer would be a worse regression to miss silently.
    #[test]
    fn htj2k_decodes_with_openhtj2k_and_never_reaches_kakadu() {
        use dcmnorm_core::value::PixelFragmentSequence;
        use dcmnorm_core::{DataElement, PrimitiveValue, VR};
        use dcmnorm_object::{DefaultDicomObject, FileMetaTableBuilder};
        use openjph_core::codestream::Codestream;
        use openjph_core::file::MemOutfile;
        use openjph_core::types::{Point, Size};

        let (width, height) = (16u32, 16u32);
        let pixels: Vec<i32> = (0..width * height).map(|i| (i % 251) as i32).collect();

        let mut cs = Codestream::new();
        cs.access_siz_mut().set_image_extent(Point::new(width, height));
        cs.access_siz_mut().set_num_components(1);
        cs.access_siz_mut().set_comp_info(0, Point::new(1, 1), 8, false);
        cs.access_siz_mut().set_tile_size(Size::new(width, height));
        cs.access_cod_mut().set_num_decomposition(0);
        cs.access_cod_mut().set_reversible(true);
        cs.access_cod_mut().set_color_transform(false);
        cs.set_planar(0);

        let mut outfile = MemOutfile::new();
        cs.write_headers(&mut outfile, &[]).unwrap();
        for y in 0..height as usize {
            let start = y * width as usize;
            cs.exchange(&pixels[start..start + width as usize], 0).unwrap();
        }
        cs.flush(&mut outfile).unwrap();
        let encoded = outfile.get_data().to_vec();

        let meta = FileMetaTableBuilder::new()
            .transfer_syntax("1.2.840.10008.1.2.4.201")
            .build()
            .unwrap();
        let mut object = DefaultDicomObject::new_empty_with_meta(meta);
        for element in [
            DataElement::new(dcmnorm_dictionary::tags::ROWS, VR::US, PrimitiveValue::from(height as u16)),
            DataElement::new(dcmnorm_dictionary::tags::COLUMNS, VR::US, PrimitiveValue::from(width as u16)),
            DataElement::new(dcmnorm_dictionary::tags::SAMPLES_PER_PIXEL, VR::US, PrimitiveValue::from(1u16)),
            DataElement::new(dcmnorm_dictionary::tags::BITS_ALLOCATED, VR::US, PrimitiveValue::from(8u16)),
            DataElement::new(dcmnorm_dictionary::tags::BITS_STORED, VR::US, PrimitiveValue::from(8u16)),
            DataElement::new(dcmnorm_dictionary::tags::HIGH_BIT, VR::US, PrimitiveValue::from(7u16)),
            DataElement::new(dcmnorm_dictionary::tags::PIXEL_REPRESENTATION, VR::US, PrimitiveValue::from(0u16)),
            DataElement::new(dcmnorm_dictionary::tags::PHOTOMETRIC_INTERPRETATION, VR::CS, PrimitiveValue::from("MONOCHROME2".to_owned())),
            DataElement::new(dcmnorm_dictionary::tags::PIXEL_DATA, VR::OB, PixelFragmentSequence::new(vec![0], vec![encoded])),
        ] {
            object.put(element);
        }

        // Default `Auto` preference - the same path a real caller with no special configuration
        // would take. Must complete (not hang) and must decode correctly via OpenHTJ2K.
        let transcoded = super::transcode_dcmnorm_object(&object, dcmnorm_dictionary::uids::EXPLICIT_VR_LITTLE_ENDIAN)
            .expect("HTJ2K decode should succeed via OpenHTJ2K");
        let decoded = transcoded
            .element(dcmnorm_dictionary::tags::PIXEL_DATA)
            .unwrap()
            .to_bytes()
            .unwrap();
        let expected: Vec<u8> = (0..width * height).map(|i| (i % 251) as u8).collect();
        assert_eq!(decoded.as_ref(), expected.as_slice());
    }

    /// Regression test for the deflate wiring fix: Deflated Explicit VR Little Endian
    /// (1.2.840.10008.1.2.1.99) is registered in the transfer syntax table with a real
    /// `FlateAdapter`, but until `InMemDicomObject::read_dataset_with_ts`/
    /// `write_dataset_with_ts` actually applied it, the adapter was dead code - reading a real
    /// deflated file misinterpreted the still-compressed bytes as plain data set elements. Runs
    /// in this crate (rather than dcmnorm-object's own test suite) because only here is
    /// dcmnorm-transcode's `deflate` Cargo feature actually enabled, so `FlateAdapter` is really
    /// compiled in. Uses a real third-party fixture, not one this codebase wrote itself, so the
    /// test can't accidentally "pass" against a file built with the same bug.
    #[test]
    fn deflated_explicit_vr_little_endian_reads_and_round_trips() {
        let fixture = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("test/files/deflated.dcm");
        let mut object = read_dicom_file(&fixture).expect("deflated.dcm should read");
        assert_eq!(object.meta().transfer_syntax.trim_end_matches('\0'), "1.2.840.10008.1.2.1.99");
        assert_eq!(
            object.get(dcmnorm_dictionary::tags::MODALITY).and_then(|e| e.to_str().ok()).as_deref(),
            Some("OT")
        );

        let out = std::env::temp_dir().join(format!(
            "dcmnorm-deflate-roundtrip-{}.dcm",
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
        ));
        write_dicom_file(&mut object, &out).expect("writing a deflated object should succeed");
        let reread = read_dicom_file(&out).expect("re-reading the just-written deflated file should succeed");
        assert_eq!(
            reread.get(dcmnorm_dictionary::tags::MODALITY).and_then(|e| e.to_str().ok()).as_deref(),
            Some("OT")
        );
        let _ = std::fs::remove_file(&out);
    }

    /// Regression test for the same wiring gap on the `read_until` fast path (behind
    /// `readTags`/`--filter`): `read_dataset_until` didn't apply the deflate adapter, so a
    /// deflated file came back with an empty data set or an error. Calls `OpenFileOptions`
    /// directly - `read_dcmnorm_object_for_filter`'s full-read fallback on error would hide a
    /// failure with this fixture (real customer files instead stopped early with no error).
    #[test]
    fn deflated_explicit_vr_little_endian_reads_until_stop_tag() {
        let fixture = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("test/files/deflated.dcm");
        let object = super::OpenFileOptions::new()
            .read_preamble(super::ReadPreamble::Always)
            .read_until(dcmnorm_core::Tag(0x0020, 0x000F))
            .open_file(&fixture)
            .expect("deflated.dcm should read up to the stop tag");
        assert!(object.get(dcmnorm_dictionary::tags::SOP_INSTANCE_UID).is_some());
        assert!(object.get(dcmnorm_dictionary::tags::STUDY_INSTANCE_UID).is_some());
        assert!(object.get(dcmnorm_dictionary::tags::SERIES_INSTANCE_UID).is_some());
        assert!(object.get(dcmnorm_dictionary::tags::PIXEL_DATA).is_none(), "should stop before PixelData");
    }

    fn htj2k_test_object(
        transfer_syntax: &str,
        rows: u16,
        cols: u16,
        samples_per_pixel: u16,
        bits_allocated: u16,
        bits_stored: u16,
        signed: bool,
        frames: usize,
        pixel_data: dcmnorm_core::value::Value<dcmnorm_object::InMemDicomObject, Vec<u8>>,
    ) -> dcmnorm_object::DefaultDicomObject {
        use dcmnorm_core::{DataElement, PrimitiveValue, VR};
        use dcmnorm_dictionary::tags;

        let meta = dcmnorm_object::FileMetaTableBuilder::new()
            .transfer_syntax(transfer_syntax)
            .media_storage_sop_class_uid("1.2.840.10008.5.1.4.1.1.7")
            .media_storage_sop_instance_uid("2.25.1234567890")
            .build()
            .unwrap();
        let mut object = dcmnorm_object::DefaultDicomObject::new_empty_with_meta(meta);
        let photometric = if samples_per_pixel == 3 { "RGB" } else { "MONOCHROME2" };
        let vr = if bits_allocated > 8 { VR::OW } else { VR::OB };
        for element in [
            DataElement::new(tags::ROWS, VR::US, PrimitiveValue::from(rows)),
            DataElement::new(tags::COLUMNS, VR::US, PrimitiveValue::from(cols)),
            DataElement::new(tags::SAMPLES_PER_PIXEL, VR::US, PrimitiveValue::from(samples_per_pixel)),
            DataElement::new(tags::BITS_ALLOCATED, VR::US, PrimitiveValue::from(bits_allocated)),
            DataElement::new(tags::BITS_STORED, VR::US, PrimitiveValue::from(bits_stored)),
            DataElement::new(tags::HIGH_BIT, VR::US, PrimitiveValue::from(bits_stored - 1)),
            DataElement::new(tags::PIXEL_REPRESENTATION, VR::US, PrimitiveValue::from(u16::from(signed))),
            DataElement::new(tags::PHOTOMETRIC_INTERPRETATION, VR::CS, PrimitiveValue::from(photometric.to_owned())),
            DataElement::new(tags::NUMBER_OF_FRAMES, VR::IS, PrimitiveValue::from(frames.to_string())),
        ] {
            object.put(element);
        }
        if samples_per_pixel == 3 {
            object.put(DataElement::new(tags::PLANAR_CONFIGURATION, VR::US, PrimitiveValue::from(0u16)));
        }
        object.put(DataElement::new(tags::PIXEL_DATA, vr, pixel_data));
        object
    }

    fn fnv1a64(bytes: &[u8]) -> u64 {
        bytes.iter().fold(0xcbf2_9ce4_8422_2325u64, |hash, &byte| {
            (hash ^ u64::from(byte)).wrapping_mul(0x0000_0100_0000_01b3)
        })
    }

    /// The failure mode behind pohcee/safebridge#406: real HTJ2K that OpenJPEG can't decode. These
    /// are ISO/IEC 15444-4 conformance codestreams (test/files/htj2k_conformance/README.md); the
    /// expected checksums come from an independent decoder (Grok), not from dcmnorm.
    #[test]
    fn htj2k_conformance_codestreams_decode_where_openjpeg_cannot() {
        use dcmnorm_core::value::PixelFragmentSequence;

        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("test/files/htj2k_conformance");
        for (file, rows, cols, spp, expected) in [
            ("ds0_ht_02_b11.j2k", 126u16, 64u16, 1u16, 0x3406_5ee0_6095_3dedu64),
            ("ds0_ht_10_b11.j2k", 64, 64, 3, 0x3dcd_ad4d_ac4e_848e),
        ] {
            let codestream = std::fs::read(dir.join(file)).unwrap();
            assert!(
                dcmnorm_jpeg2000::openjpeg::decode(&codestream).is_err(),
                "{file}: OpenJPEG unexpectedly decoded this - pick a fixture that still exercises the gap"
            );
            let object = htj2k_test_object(
                "1.2.840.10008.1.2.4.201",
                rows,
                cols,
                spp,
                8,
                8,
                false,
                1,
                PixelFragmentSequence::new(vec![0], vec![codestream]).into(),
            );
            let decoded = super::transcode_dcmnorm_object(&object, dcmnorm_dictionary::uids::EXPLICIT_VR_LITTLE_ENDIAN)
                .unwrap_or_else(|e| panic!("{file}: {e}"));
            let bytes = decoded.element(dcmnorm_dictionary::tags::PIXEL_DATA).unwrap().to_bytes().unwrap();
            assert_eq!(bytes.len(), usize::from(rows) * usize::from(cols) * usize::from(spp), "{file}");
            assert_eq!(fnv1a64(&bytes), expected, "{file}");
        }
    }

    /// Native -> HTJ2K (.201 and .203) -> native must be bit-exact across bit depths, signedness,
    /// colour and multiple frames, and must leave PhotometricInterpretation alone (no colour
    /// transform is applied).
    #[test]
    fn htj2k_encode_round_trips_through_transcode() {
        use dcmnorm_core::PrimitiveValue;
        use dcmnorm_dictionary::{tags, uids};

        for (rows, cols, spp, bits_allocated, bits_stored, signed, frames) in [
            (29u16, 37u16, 1u16, 8u16, 8u16, false, 1usize),
            (128, 96, 1, 16, 12, false, 1),
            (64, 80, 1, 16, 16, true, 1),
            (48, 64, 3, 8, 8, false, 1),
            (32, 40, 1, 16, 10, false, 3),
        ] {
            let samples = usize::from(rows) * usize::from(cols) * usize::from(spp) * frames;
            let mask = (1u32 << bits_stored) - 1;
            let values: Vec<u32> = (0..samples as u32)
                .map(|i| (i.wrapping_mul(2_654_435_761) >> 9) & mask)
                .collect();
            let (pixel_data, raw_bytes): (dcmnorm_core::value::Value<dcmnorm_object::InMemDicomObject, Vec<u8>>, Vec<u8>) = if bits_allocated == 8 {
                let bytes: Vec<u8> = values.iter().map(|&v| v as u8).collect();
                (PrimitiveValue::from(bytes.clone()).into(), bytes)
            } else {
                // Signed samples: sign-extend from BitsStored into the 16-bit container.
                let words: Vec<u16> = values
                    .iter()
                    .map(|&v| {
                        if signed {
                            let shift = 32 - u32::from(bits_stored);
                            (((v << shift) as i32 >> shift) as i16) as u16
                        } else {
                            v as u16
                        }
                    })
                    .collect();
                let bytes = words.iter().flat_map(|w| w.to_le_bytes()).collect();
                (PrimitiveValue::U16(words.into()).into(), bytes)
            };
            let native = htj2k_test_object(
                uids::EXPLICIT_VR_LITTLE_ENDIAN,
                rows,
                cols,
                spp,
                bits_allocated,
                bits_stored,
                signed,
                frames,
                pixel_data,
            );
            for target in ["1.2.840.10008.1.2.4.201", "1.2.840.10008.1.2.4.203"] {
                let label = format!("{rows}x{cols}x{spp} {bits_stored}/{bits_allocated}-bit signed={signed} frames={frames} -> {target}");
                let encoded = super::transcode_dcmnorm_object(&native, target).unwrap_or_else(|e| panic!("{label}: {e}"));
                let fragments = encoded.element(tags::PIXEL_DATA).unwrap().fragments().map(|f| f.len());
                assert_eq!(fragments, Some(frames), "{label}: one fragment per frame");
                assert_eq!(
                    encoded.element(tags::PHOTOMETRIC_INTERPRETATION).unwrap().to_str().unwrap().trim(),
                    if spp == 3 { "RGB" } else { "MONOCHROME2" },
                    "{label}"
                );
                let decoded = super::transcode_dcmnorm_object(&encoded, uids::EXPLICIT_VR_LITTLE_ENDIAN)
                    .unwrap_or_else(|e| panic!("{label}: decode: {e}"));
                let bytes = decoded.element(tags::PIXEL_DATA).unwrap().to_bytes().unwrap();
                assert_eq!(bytes.as_ref(), raw_bytes.as_slice(), "{label}");
            }
        }
    }

    #[test]
    fn htj2k_capabilities_are_reported_accurately() {
        let support = super::list_transfer_syntax_support();
        let entry = |uid: &str| support.iter().find(|entry| entry.uid == uid).unwrap().clone();
        for uid in ["1.2.840.10008.1.2.4.201", "1.2.840.10008.1.2.4.203"] {
            let entry = entry(uid);
            assert!(entry.can_decode_pixel_data && entry.can_encode_pixel_data && entry.can_transcode_to(), "{uid}");
            assert!(super::can_encode_transfer_syntax(uid), "{uid}");
        }
        // RPCL Options: decode only (its profile needs PLT/TLM markers OpenHTJ2K doesn't write).
        let rpcl = entry("1.2.840.10008.1.2.4.202");
        assert!(rpcl.can_decode_pixel_data && !rpcl.can_encode_pixel_data);
        assert_eq!(super::jpeg2000_engine_name_for("1.2.840.10008.1.2.4.201"), "openhtj2k");
    }
}
