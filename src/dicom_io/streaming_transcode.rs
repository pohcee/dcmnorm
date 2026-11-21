//! Streaming frame-by-frame file-to-file DICOM transcoder.
//!
//! Reads only the dataset metadata (deferring top-level `(7FE0,0010) PixelData` on disk via
//! [`OpenFileOptions::open_file_deferring_pixel_data`]), writes the Part 10 header and
//! pre-PixelData elements, then streams PixelData frame-by-frame (or in small memory-bounded
//! Rayon batches) directly into a temporary output file before atomically renaming it into place.
//!
//! For multi-hundred-megabyte multi-frame datasets (such as 638–718 MiB Breast Tomosynthesis
//! volumes), this keeps peak memory bounded to O(batch_size × single_frame) instead of holding
//! the entire uncompressed volume plus intermediate copies in RAM.
//!
//! Any input/target combination not supported by streaming (meta-less dumps, deflated datasets,
//! MPEG video, multi-fragment frames without an offset table, or target codecs whose
//! pre-PixelData attributes depend on post-encode compressed sizes such as JPEG Baseline)
//! returns `Ok(false)` so the caller falls back cleanly to the in-memory transcoder.

use std::borrow::Cow;
use std::fs::{File, OpenOptions};
use std::io::{BufWriter, Read, Seek, SeekFrom, Write};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use dcmnorm_core::header::{DataElementHeader, Length};
use dcmnorm_core::VR;
use dcmnorm_dictionary::{tags, uids};
use dcmnorm_encoding::adapters::{PixelDataObject, PixelDataReader, RawPixelData};
use dcmnorm_encoding::transfer_syntax::{Codec, TransferSyntax, TransferSyntaxIndex};
use dcmnorm_object::{
    DataSetWriter, DataToken, DefaultDicomObject, DeferredPixelData, FileMetaTableBuilder,
    FragmentLocation, InMemDicomObject, OpenFileOptions, PixelDataStreamStop, ReadPreamble,
    WriteError,
};
use dcmnorm_transcode::TransferSyntaxRegistry;
use rayon::prelude::*;

use crate::perf;

use super::io::{
    apply_jpeg2000_component_correction, apply_jpeg2000_precision_correction,
    codestream_component_count, codestream_precision, codestream_uses_mct, encode_htj2k_frame,
    encode_jpeg2000_frame, is_classic_jpeg2000_encode_transfer_syntax,
    is_encapsulated_transfer_syntax, is_htj2k_encode_transfer_syntax, is_htj2k_transfer_syntax,
    is_jpeg2000_transfer_syntax, is_jpeg_ls_transfer_syntax, is_jpeg_xl_lossless_transfer_syntax,
    is_jpeg_xl_transfer_syntax, is_mpeg_transfer_syntax, is_parallel_decode_transfer_syntax,
    is_planar, jpeg2000_codec_preference, jpeg2000_debug_log, kakadu_ffi_enabled, native_pixel_vr,
    normalize_decoded_pixel_data_attributes, normalize_transfer_syntax_uid, number_of_frames,
    remove_encapsulation_sidecar_attributes, set_planar_configuration_interleaved,
    Jpeg2000CodecPreference,
};
use super::jpeg_ls;
use super::jpeg_xl;
use super::types::TranscodeError;

const EXPLICIT_VR_BIG_ENDIAN_UID: &str = "1.2.840.10008.1.2.2";
const COPY_CHUNK_SIZE: usize = 1024 * 1024;
const STREAM_BATCH_MEM_BUDGET: usize = 64 * 1024 * 1024;

static TEMP_FILE_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Try to transcode `input_path` -> `output_path` without loading `PixelData` in full.
///
/// Returns:
/// - `Ok(true)` if streaming handled the transcode and wrote `output_path`;
/// - `Ok(false)` if this input/target combination is not streamable and the caller should fall
///   back to the in-memory read -> transcode -> write path;
/// - `Err(err)` if a real transcode/IO error occurred.
pub(super) fn try_transcode_dicom_file_streaming(
    input_path: &Path,
    output_path: &Path,
    target_transfer_syntax_uid: &str,
) -> Result<bool, TranscodeError> {
    let _scope = perf::scope("transcode.streaming_file");

    // Never rename a temp file over a non-regular file (e.g. /dev/null or a FIFO).
    if let Ok(meta) = std::fs::symlink_metadata(output_path) {
        if !meta.file_type().is_file() {
            return Ok(false);
        }
    }

    // Preserve the refusal to overwrite an existing read-only file (and reject invalid paths
    // such as interior NULs) before doing any transcode work or creating a staging file.
    ensure_existing_output_writable(output_path)?;

    // Never consume a non-regular input (e.g. a FIFO or process substitution) before falling back.
    if !std::fs::metadata(input_path).is_ok_and(|meta| meta.file_type().is_file()) {
        return Ok(false);
    }

    let target_uid = normalize_transfer_syntax_uid(target_transfer_syntax_uid);
    let target_ts = TransferSyntaxRegistry
        .get(target_uid)
        .ok_or_else(|| TranscodeError::UnknownTransferSyntax(target_uid.to_owned()))?;

    // Dataset-level codecs (Deflated Explicit VR LE) cannot be seek-patched and are rare; let
    // the in-memory path handle them.
    if matches!(target_ts.codec(), Codec::Dataset(_)) {
        return Ok(false);
    }

    let open_result = OpenFileOptions::new()
        .read_preamble(ReadPreamble::Always)
        .open_file_deferring_pixel_data(input_path);
    let (mut object, deferred_pixel_data, mut input_file) = match open_result {
        Ok(Some(triple)) => triple,
        // Deflated source, truncated file, meta-less file, or non-standard syntax that needs
        // flexible_decoding: fall back so read_dicom_file applies all fallbacks/canonical errors.
        Ok(None) | Err(_) => return Ok(false),
    };

    let source_uid = normalize_transfer_syntax_uid(object.meta().transfer_syntax()).to_owned();
    let source_ts = TransferSyntaxRegistry
        .get(&source_uid)
        .ok_or_else(|| TranscodeError::UnknownTransferSyntax(source_uid.clone()))?;

    // Encapsulated nested PixelData (e.g. compressed IconImageSequence) shares the dataset's
    // transfer syntax per PS3.5 Annex A.4 and cannot stay compressed under a different codec or
    // native transfer syntax.
    if source_uid != target_uid && has_nested_encapsulated_pixel_data(&object) {
        return Err(TranscodeError::EncodePixelData {
            uid: target_uid.to_owned(),
            name: target_ts.name().to_owned(),
            message: "cannot transcode dataset containing encapsulated nested PixelData (e.g. IconImageSequence) to a different transfer syntax".to_owned(),
        });
    }

    // Absent PixelData: metadata-only file, nothing to stream.
    if matches!(deferred_pixel_data, DeferredPixelData::Absent) {
        object.meta_mut().set_transfer_syntax(target_ts);
        let Some(temp) = try_create_temp_output(input_path, output_path)? else {
            return Ok(false);
        };
        {
            let mut writer = BufWriter::new(temp.file());
            object.write_all(&mut writer)?;
            writer.flush().map_err(|source| WriteError::Io {
                source,
                context: "flushing transcoded file",
            })?;
        }
        temp.persist(output_path)?;
        return Ok(true);
    }

    // Same transfer syntax: stream PixelData verbatim without touching codecs.
    if source_uid == target_uid {
        return stream_passthrough(
            &object,
            &deferred_pixel_data,
            &mut input_file,
            input_path,
            output_path,
        );
    }

    // Native -> Native (e.g. Implicit VR LE <-> Explicit VR LE <-> Explicit VR BE):
    // stream raw bytes (word-swapping OW when endianness flips) without framing or decoding.
    if let DeferredPixelData::Native { vr, offset, length } = deferred_pixel_data {
        if !is_encapsulated_transfer_syntax(target_ts) {
            let source_be = source_uid == EXPLICIT_VR_BIG_ENDIAN_UID;
            let target_be = target_uid == EXPLICIT_VR_BIG_ENDIAN_UID;
            if vr == VR::OW && source_be != target_be && length % 2 != 0 {
                return Ok(false);
            }
            object.meta_mut().set_transfer_syntax(target_ts);
            let Some(temp) = try_create_temp_output(input_path, output_path)? else {
                return Ok(false);
            };
            {
                let mut writer = BufWriter::new(temp.file());
                object.write_all_with_pixel_data(
                    &mut writer,
                    |ds_writer, _| -> Result<(), TranscodeError> {
                        stream_native_bytes(
                            &mut input_file,
                            offset,
                            length,
                            vr,
                            vr == VR::OW && source_be != target_be,
                            ds_writer,
                        )
                    },
                )?;
                writer.flush().map_err(|source| WriteError::Io {
                    source,
                    context: "flushing transcoded file",
                })?;
            }
            temp.persist(output_path)?;
            return Ok(true);
        }
    }

    // Determine streamable TargetMode.
    let Some(target_mode) = resolve_target_mode(target_ts) else {
        return Ok(false);
    };

    // Determine streamable SourceMode (and inspect J2K header if needed).
    let nr_frames = number_of_frames(&object);
    let (source_mode, jpeg2000_uses_mct) = match deferred_pixel_data {
        DeferredPixelData::Absent => unreachable!(),
        DeferredPixelData::Native { vr, offset, length } => (
            SourceMode::Native {
                vr,
                offset,
                length,
                big_endian: source_uid == EXPLICIT_VR_BIG_ENDIAN_UID,
            },
            None,
        ),
        DeferredPixelData::Encapsulated {
            offset_table,
            fragments,
            ..
        } => {
            if is_mpeg_transfer_syntax(&source_uid) || fragments.is_empty() {
                return Ok(false);
            }
            let Some(frame_ranges) =
                compute_frame_fragment_ranges(&object, &offset_table, &fragments, nr_frames)
            else {
                return Ok(false);
            };
            if source_uid == uids::RLE_LOSSLESS && fragments.len() != nr_frames {
                return Ok(false);
            }
            if is_jpeg_ls_transfer_syntax(&source_uid) && nr_frames == 1 && fragments.len() > 1 {
                return Ok(false);
            }
            if is_jpeg_xl_transfer_syntax(&source_uid) && fragments.len() != nr_frames {
                return Ok(false);
            }

            let is_j2k = is_jpeg2000_transfer_syntax(&source_uid);
            let is_htj2k = is_htj2k_transfer_syntax(&source_uid);
            let j2k_pref = jpeg2000_codec_preference();
            if is_j2k
                && !is_htj2k
                && kakadu_ffi_enabled()
                && j2k_pref != Jpeg2000CodecPreference::OpenJpeg
                && (nr_frames == 1 || j2k_pref == Jpeg2000CodecPreference::Kakadu)
            {
                // Explicit Kakadu preference (any frame count) or single-frame default Kakadu runs
                // on the standard path with its full Kakadu dispatch.
                return Ok(false);
            }

            let mut j2k_uses_mct = None;
            if is_j2k {
                let first_frag = fragments[frame_ranges[0].start];
                let probe = read_j2k_header_probe(&mut input_file, first_frag)?;

                let declared_spp = object
                    .get(tags::SAMPLES_PER_PIXEL)
                    .and_then(|e| e.uint16().ok())
                    .unwrap_or(1);
                if declared_spp > 1 {
                    if let Some(actual_components) = codestream_component_count(&probe) {
                        if actual_components > 0 && actual_components < declared_spp {
                            apply_jpeg2000_component_correction(&mut object, actual_components);
                        }
                    }
                }
                if let (Some(precision), Some(bits_allocated)) = (
                    codestream_precision(&probe),
                    object
                        .get(tags::BITS_ALLOCATED)
                        .and_then(|e| e.uint16().ok()),
                ) {
                    let bits_stored = object
                        .get(tags::BITS_STORED)
                        .and_then(|e| e.uint16().ok())
                        .unwrap_or(bits_allocated);
                    if precision > bits_stored {
                        apply_jpeg2000_precision_correction(&mut object, precision).map_err(
                            |message| TranscodeError::DecodePixelData {
                                uid: source_ts.uid().to_owned(),
                                name: source_ts.name().to_owned(),
                                message,
                            },
                        )?;
                    }
                }
                j2k_uses_mct = codestream_uses_mct(&probe);
                jpeg2000_debug_log(format!(
                    "streaming decode start: uid={} name={} frames={}",
                    source_ts.uid(),
                    source_ts.name(),
                    nr_frames
                ));
            }

            let decoder = if is_jpeg_ls_transfer_syntax(&source_uid) {
                if !cfg!(feature = "jpeg-ls-codec") {
                    return Ok(false);
                }
                EncapsulatedDecoder::JpegLs
            } else if is_jpeg_xl_transfer_syntax(&source_uid) {
                if !cfg!(feature = "jpeg-xl-codec") {
                    return Ok(false);
                }
                let pixel_rep = object
                    .get(tags::PIXEL_REPRESENTATION)
                    .and_then(|e| e.uint16().ok())
                    .unwrap_or(0);
                if pixel_rep != 0 {
                    return Ok(false);
                }
                EncapsulatedDecoder::JpegXl
            } else {
                let Codec::EncapsulatedPixelData(Some(reader), _) = source_ts.codec() else {
                    return Ok(false);
                };
                EncapsulatedDecoder::Registry(reader.as_ref())
            };

            (
                SourceMode::Encapsulated {
                    fragments,
                    frame_ranges,
                    decoder,
                },
                j2k_uses_mct,
            )
        }
    };

    // Validate image geometry before modifying metadata or opening the output file.
    let Some(rows) = object.get(tags::ROWS).and_then(|e| e.uint16().ok()) else {
        return Ok(false);
    };
    let Some(cols) = object.get(tags::COLUMNS).and_then(|e| e.uint16().ok()) else {
        return Ok(false);
    };
    let samples_per_pixel = object
        .get(tags::SAMPLES_PER_PIXEL)
        .and_then(|e| e.uint16().ok())
        .unwrap_or(1);
    let Some(bits_allocated) = object
        .get(tags::BITS_ALLOCATED)
        .and_then(|e| e.uint16().ok())
    else {
        return Ok(false);
    };
    let bits_stored = object
        .get(tags::BITS_STORED)
        .and_then(|e| e.uint16().ok())
        .unwrap_or(bits_allocated);
    let is_signed = object
        .get(tags::PIXEL_REPRESENTATION)
        .and_then(|e| e.uint16().ok())
        .unwrap_or(0)
        != 0;

    if bits_allocated != 8 && bits_allocated != 16 {
        return Ok(false);
    }
    if bits_stored == 0 || bits_stored > bits_allocated {
        return Ok(false);
    }
    let bytes_per_sample = usize::from(bits_allocated / 8);
    let Some(frame_len) = usize::from(rows)
        .checked_mul(usize::from(cols))
        .and_then(|n| n.checked_mul(usize::from(samples_per_pixel)))
        .and_then(|n| n.checked_mul(bytes_per_sample))
    else {
        return Ok(false);
    };
    if frame_len == 0 {
        return Ok(false);
    }

    let Some(total_native_bytes) = (frame_len as u64).checked_mul(nr_frames as u64) else {
        return Ok(false);
    };

    if let SourceMode::Native {
        vr,
        length,
        big_endian,
        ..
    } = &source_mode
    {
        if u64::from(*length) < total_native_bytes {
            return Ok(false);
        }
        // Big-endian OW stores 16-bit words continuously across the entire PixelData value; if
        // individual 8-bit frames have odd byte length, a 16-bit word straddles two frames, so
        // frame-splitting before byte-swapping would corrupt samples. Fall back to in-memory.
        if *big_endian && *vr == VR::OW && (frame_len % 2 != 0 || length % 2 != 0) {
            return Ok(false);
        }
    }
    if matches!(target_mode, TargetMode::Native { .. }) {
        let padded = total_native_bytes + (total_native_bytes % 2);
        if padded > u64::from(u32::MAX) {
            return Ok(false);
        }
    }
    if matches!(target_mode, TargetMode::JpegLs { .. }) && bits_allocated == 16 && bits_stored <= 8
    {
        return Ok(false);
    }
    if matches!(target_mode, TargetMode::JpegXlLossless)
        && jpeg_xl::validate_jpeg_xl_encode_params(
            rows,
            cols,
            samples_per_pixel,
            bits_allocated,
            bits_stored,
            is_signed,
        )
        .is_err()
    {
        return Ok(false);
    }

    // Capture pre-normalization source attributes for frame decoders (e.g. JPEG's RGB vs YCbCr
    // default color transform depends on the source's PhotometricInterpretation before
    // normalize_decoded_pixel_data_attributes rewrites it to RGB).
    let decode_meta = FrameDecodeMeta {
        transfer_syntax_uid: source_uid.clone(),
        rows,
        cols,
        samples_per_pixel,
        bits_allocated,
        bits_stored,
        photometric_interpretation: object
            .get(tags::PHOTOMETRIC_INTERPRETATION)
            .and_then(|e| e.to_str().ok())
            .map(|s| s.trim().to_owned()),
    };

    // Capture planar configuration of the native frame handed to the encoder:
    // - for native input, it's whatever the source declares;
    // - for encapsulated input, decoders always produce interleaved pixels (PlanarConfiguration=0).
    let encode_planar_configuration = match &source_mode {
        SourceMode::Native { .. } => object
            .get(tags::PLANAR_CONFIGURATION)
            .and_then(|e| e.uint16().ok())
            .unwrap_or(0),
        SourceMode::Encapsulated { .. } => 0,
    };
    let encode_is_planar = match &source_mode {
        SourceMode::Native { .. } => is_planar(&object, samples_per_pixel),
        SourceMode::Encapsulated { .. } => false,
    };

    // Apply all metadata updates BEFORE writing pre-PixelData elements.
    if matches!(source_mode, SourceMode::Encapsulated { .. }) {
        normalize_decoded_pixel_data_attributes(&mut object, &source_uid, jpeg2000_uses_mct);
    }
    remove_encapsulation_sidecar_attributes(&mut object);
    if matches!(
        target_mode,
        TargetMode::Htj2k | TargetMode::ClassicJpeg2000 { .. } | TargetMode::JpegXlLossless
    ) {
        set_planar_configuration_interleaved(&mut object);
    }
    object.meta_mut().set_transfer_syntax(target_ts);

    let encode_meta = FrameEncodeMeta {
        rows,
        cols,
        samples_per_pixel,
        bits_allocated,
        bits_stored,
        is_signed,
        planar: encode_is_planar,
        planar_configuration: encode_planar_configuration,
    };

    let batch_size = choose_batch_size(
        &source_uid,
        &source_mode,
        &target_mode,
        nr_frames,
        frame_len,
    );

    let Some(mut temp) = try_create_temp_output(input_path, output_path)? else {
        return Ok(false);
    };
    let mut bot_patch: Option<(u64, Vec<u32>)> = None;

    {
        let mut writer = BufWriter::new(temp.file());
        object.write_all_with_pixel_data(
            &mut writer,
            |ds_writer, dataset_start| -> Result<(), TranscodeError> {
                match target_mode {
                    TargetMode::Native { big_endian } => {
                        let out_vr = native_pixel_vr(bits_allocated);
                        let padded_len = (total_native_bytes + (total_native_bytes % 2)) as u32;
                        ds_writer
                            .write_element_header(DataElementHeader::new(
                                tags::PIXEL_DATA,
                                out_vr,
                                Length::defined(padded_len),
                            ))
                            .map_err(|source| WriteError::Dataset { source })?;

                        let swap_out = big_endian && out_vr == VR::OW;
                        let mut batch_start = 0;
                        while batch_start < nr_frames {
                            let batch_end = next_batch_end(
                                &source_mode,
                                batch_start,
                                nr_frames,
                                batch_size,
                                frame_len,
                            );
                            let inputs = read_frame_batch(
                                &mut input_file,
                                &source_mode,
                                batch_start..batch_end,
                                frame_len,
                            )?;
                            let outputs = transform_batch_to_native(
                                inputs,
                                &source_mode,
                                &decode_meta,
                                source_ts,
                                frame_len,
                                swap_out,
                                batch_start,
                            )?;
                            for frame in outputs {
                                ds_writer
                                    .write_raw_bytes(&frame)
                                    .map_err(|source| WriteError::Dataset { source })?;
                            }
                            batch_start = batch_end;
                        }

                        if total_native_bytes % 2 == 1 {
                            ds_writer
                                .write_raw_bytes(&[0])
                                .map_err(|source| WriteError::Dataset { source })?;
                        }
                        Ok(())
                    }
                    TargetMode::Htj2k
                    | TargetMode::ClassicJpeg2000 { .. }
                    | TargetMode::JpegLs { .. }
                    | TargetMode::JpegXlLossless => {
                        ds_writer
                            .write(DataToken::PixelSequenceStart)
                            .map_err(|source| WriteError::Dataset { source })?;

                        // Reserve the Basic Offset Table item (4 bytes per frame) and patch it
                        // after all frames have been written.
                        let bot_byte_len = nr_frames
                            .checked_mul(4)
                            .and_then(|n| u32::try_from(n).ok())
                            .ok_or_else(|| TranscodeError::EncodePixelData {
                                uid: target_ts.uid().to_owned(),
                                name: target_ts.name().to_owned(),
                                message: "too many frames for Basic Offset Table".to_owned(),
                            })?;
                        ds_writer
                            .write(DataToken::ItemStart {
                                len: Length::defined(bot_byte_len),
                            })
                            .map_err(|source| WriteError::Dataset { source })?;
                        let bot_file_offset = dataset_start + ds_writer.bytes_written();
                        if bot_byte_len > 0 {
                            ds_writer
                                .write_raw_bytes(&vec![0u8; bot_byte_len as usize])
                                .map_err(|source| WriteError::Dataset { source })?;
                        }
                        ds_writer
                            .write(DataToken::ItemEnd)
                            .map_err(|source| WriteError::Dataset { source })?;

                        let mut offsets = Vec::with_capacity(nr_frames);
                        let mut rel_pos = 0u64;
                        // Allocated lazily and fallibly only after frame 0 decodes to `frame_len`,
                        // so untrusted metadata declaring huge dimensions cannot OOM pre-decode.
                        let mut htj2k_planes: Option<Vec<Vec<i32>>> = None;
                        let mut write_fragment = |mut frag: Vec<u8>| -> Result<(), TranscodeError> {
                            if frag.len() % 2 == 1 {
                                frag.push(0);
                            }
                            let offset = u32::try_from(rel_pos).map_err(|_| {
                                TranscodeError::EncodePixelData {
                                    uid: target_ts.uid().to_owned(),
                                    name: target_ts.name().to_owned(),
                                    message: "encapsulated output exceeded 4 GiB Basic Offset Table limit"
                                        .to_owned(),
                                }
                            })?;
                            offsets.push(offset);
                            let frag_len = u32::try_from(frag.len()).map_err(|_| {
                                TranscodeError::EncodePixelData {
                                    uid: target_ts.uid().to_owned(),
                                    name: target_ts.name().to_owned(),
                                    message: "encoded frame fragment exceeds u32::MAX bytes"
                                        .to_owned(),
                                }
                            })?;
                            ds_writer
                                .write(DataToken::ItemStart {
                                    len: Length::defined(frag_len),
                                })
                                .map_err(|source| WriteError::Dataset { source })?;
                            ds_writer
                                .write_item_value(&frag)
                                .map_err(|source| WriteError::Dataset { source })?;
                            ds_writer
                                .write(DataToken::ItemEnd)
                                .map_err(|source| WriteError::Dataset { source })?;
                            rel_pos += 8 + frag.len() as u64;
                            Ok(())
                        };

                        let mut batch_start = 0;
                        while batch_start < nr_frames {
                            let batch_end = next_batch_end(
                                &source_mode,
                                batch_start,
                                nr_frames,
                                batch_size,
                                frame_len,
                            );
                            let inputs = read_frame_batch(
                                &mut input_file,
                                &source_mode,
                                batch_start..batch_end,
                                frame_len,
                            )?;
                            if matches!(target_mode, TargetMode::Htj2k) {
                                // Decode the bounded batch in parallel (when source supports it),
                                // then encode each decoded frame sequentially into OpenHTJ2K
                                // (which parallelizes internally per frame behind a mutex),
                                // reusing one plane buffer and dropping each frame immediately.
                                let decoded_frames = decode_batch(
                                    inputs,
                                    &source_mode,
                                    &decode_meta,
                                    source_ts,
                                    frame_len,
                                    batch_start,
                                )?;
                                let planes = match htj2k_planes.as_mut() {
                                    Some(planes) => planes,
                                    None => htj2k_planes.insert(allocate_htj2k_planes(
                                        &encode_meta,
                                        target_ts,
                                    )?),
                                };
                                for (offset, native) in decoded_frames.into_iter().enumerate() {
                                    let frame_index = batch_start + offset;
                                    let frag = encode_htj2k_frame(
                                        &native,
                                        encode_meta.rows,
                                        encode_meta.cols,
                                        encode_meta.samples_per_pixel,
                                        encode_meta.bits_allocated,
                                        encode_meta.bits_stored,
                                        encode_meta.is_signed,
                                        encode_meta.planar,
                                        planes,
                                    )
                                    .map_err(|e| TranscodeError::EncodePixelData {
                                        uid: target_ts.uid().to_owned(),
                                        name: target_ts.name().to_owned(),
                                        message: format!("HTJ2K encode failed for frame {frame_index}: {e}"),
                                    })?;
                                    drop(native);
                                    write_fragment(frag)?;
                                }
                            } else {
                                let fragments = transform_batch_to_encapsulated(
                                    inputs,
                                    &source_mode,
                                    &decode_meta,
                                    source_ts,
                                    target_mode,
                                    &encode_meta,
                                    target_ts,
                                    frame_len,
                                    batch_start,
                                )?;
                                for frag in fragments {
                                    write_fragment(frag)?;
                                }
                            }
                            batch_start = batch_end;
                        }

                        ds_writer
                            .write(DataToken::SequenceEnd)
                            .map_err(|source| WriteError::Dataset { source })?;

                        if bot_byte_len > 0 {
                            bot_patch = Some((bot_file_offset, offsets));
                        }
                        Ok(())
                    }
                }
            },
        )?;
        writer.flush().map_err(|source| WriteError::Io {
            source,
            context: "flushing transcoded file",
        })?;
    }

    if let Some((bot_offset, offsets)) = bot_patch {
        let mut bot_bytes = Vec::with_capacity(offsets.len() * 4);
        for offset in offsets {
            bot_bytes.extend_from_slice(&offset.to_le_bytes());
        }
        let file = temp.file_mut();
        file.seek(SeekFrom::Start(bot_offset))
            .map_err(|source| WriteError::Io {
                source,
                context: "seeking to patch Basic Offset Table",
            })?;
        file.write_all(&bot_bytes)
            .map_err(|source| WriteError::Io {
                source,
                context: "patching Basic Offset Table",
            })?;
        file.flush().map_err(|source| WriteError::Io {
            source,
            context: "flushing Basic Offset Table",
        })?;
    }

    temp.persist(output_path)?;
    Ok(true)
}

#[derive(Clone, Copy)]
enum TargetMode {
    Native { big_endian: bool },
    Htj2k,
    ClassicJpeg2000 { lossless: bool },
    JpegLs { lossless: bool },
    JpegXlLossless,
}

fn resolve_target_mode(target_ts: &TransferSyntax) -> Option<TargetMode> {
    let uid = normalize_transfer_syntax_uid(target_ts.uid());
    if !is_encapsulated_transfer_syntax(target_ts) {
        return Some(TargetMode::Native {
            big_endian: uid == EXPLICIT_VR_BIG_ENDIAN_UID,
        });
    }
    if is_htj2k_encode_transfer_syntax(uid) {
        return Some(TargetMode::Htj2k);
    }
    if is_classic_jpeg2000_encode_transfer_syntax(uid) && cfg!(feature = "jpeg2000-openjpeg-encode")
    {
        return Some(TargetMode::ClassicJpeg2000 {
            lossless: uid == "1.2.840.10008.1.2.4.90",
        });
    }
    if is_jpeg_ls_transfer_syntax(uid) && cfg!(feature = "jpeg-ls-codec") {
        return Some(TargetMode::JpegLs {
            lossless: uid == "1.2.840.10008.1.2.4.80",
        });
    }
    if is_jpeg_xl_lossless_transfer_syntax(uid) && cfg!(feature = "jpeg-xl-codec") {
        return Some(TargetMode::JpegXlLossless);
    }
    None
}

enum EncapsulatedDecoder<'a> {
    Registry(&'a (dyn PixelDataReader + Send + Sync)),
    JpegLs,
    JpegXl,
}

enum SourceMode<'a> {
    Native {
        vr: VR,
        offset: u64,
        length: u32,
        big_endian: bool,
    },
    Encapsulated {
        fragments: Vec<FragmentLocation>,
        frame_ranges: Vec<Range<usize>>,
        decoder: EncapsulatedDecoder<'a>,
    },
}

struct FrameDecodeMeta {
    transfer_syntax_uid: String,
    rows: u16,
    cols: u16,
    samples_per_pixel: u16,
    bits_allocated: u16,
    bits_stored: u16,
    photometric_interpretation: Option<String>,
}

struct FrameEncodeMeta {
    rows: u16,
    cols: u16,
    samples_per_pixel: u16,
    bits_allocated: u16,
    bits_stored: u16,
    is_signed: bool,
    planar: bool,
    planar_configuration: u16,
}

/// Lightweight single-frame view implementing [`PixelDataObject`] for registry adapters so a
/// single frame's codestream can be decoded without materializing the full multi-frame object.
struct SingleFramePixelDataObject<'a> {
    meta: &'a FrameDecodeMeta,
    codestream: &'a [u8],
}

impl PixelDataObject for SingleFramePixelDataObject<'_> {
    fn transfer_syntax_uid(&self) -> &str {
        &self.meta.transfer_syntax_uid
    }
    fn rows(&self) -> Option<u16> {
        Some(self.meta.rows)
    }
    fn cols(&self) -> Option<u16> {
        Some(self.meta.cols)
    }
    fn samples_per_pixel(&self) -> Option<u16> {
        Some(self.meta.samples_per_pixel)
    }
    fn bits_allocated(&self) -> Option<u16> {
        Some(self.meta.bits_allocated)
    }
    fn bits_stored(&self) -> Option<u16> {
        Some(self.meta.bits_stored)
    }
    fn photometric_interpretation(&self) -> Option<&str> {
        self.meta.photometric_interpretation.as_deref()
    }
    fn number_of_frames(&self) -> Option<u32> {
        Some(1)
    }
    fn number_of_fragments(&self) -> Option<u32> {
        Some(1)
    }
    fn fragment(&self, fragment: usize) -> Option<Cow<'_, [u8]>> {
        (fragment == 0).then_some(Cow::Borrowed(self.codestream))
    }
    fn offset_table(&self) -> Option<Cow<'_, [u32]>> {
        Some(Cow::Borrowed(&[0]))
    }
    fn raw_pixel_data(&self) -> Option<RawPixelData> {
        Some(RawPixelData {
            fragments: std::iter::once(self.codestream.to_vec()).collect(),
            offset_table: std::iter::once(0).collect(),
        })
    }
}

fn choose_batch_size(
    source_uid: &str,
    source_mode: &SourceMode<'_>,
    target_mode: &TargetMode,
    nr_frames: usize,
    frame_len: usize,
) -> usize {
    if nr_frames <= 1 {
        return 1;
    }
    // OpenHTJ2K already parallelizes internally per frame behind a mutex, so only batch when the
    // *other* side (parallel-decodable source or parallel-encodable target) benefits from Rayon.
    let source_can_parallel = match source_mode {
        SourceMode::Encapsulated { .. } => {
            !is_htj2k_transfer_syntax(source_uid)
                && (is_parallel_decode_transfer_syntax(source_uid)
                    || is_jpeg_xl_transfer_syntax(source_uid))
        }
        SourceMode::Native { .. } => false,
    };
    let target_can_parallel = !matches!(target_mode, TargetMode::Native { .. } | TargetMode::Htj2k);
    if !source_can_parallel && !target_can_parallel {
        return 1;
    }
    let max_by_mem = (STREAM_BATCH_MEM_BUDGET / frame_len.max(1)).max(1);
    let max_by_cpu = rayon::current_num_threads().clamp(1, 8);
    max_by_mem.min(max_by_cpu).min(nr_frames)
}

/// Computes the exclusive upper frame index for the batch starting at `batch_start`, budgeting
/// both decoded (`frame_len`) and encoded input bytes against `STREAM_BATCH_MEM_BUDGET`.
/// Always advances by at least one frame so a single oversized frame still makes progress.
fn next_batch_end(
    source_mode: &SourceMode<'_>,
    batch_start: usize,
    nr_frames: usize,
    max_batch_frames: usize,
    frame_len: usize,
) -> usize {
    let limit = batch_start
        .saturating_add(max_batch_frames.max(1))
        .min(nr_frames);
    if limit <= batch_start + 1 {
        return limit;
    }
    let budget = STREAM_BATCH_MEM_BUDGET as u64;
    let decoded_bytes = frame_len as u64;
    let frame_total = |frame_index: usize| -> Option<u64> {
        let encoded_bytes = match source_mode {
            SourceMode::Native { .. } => decoded_bytes,
            SourceMode::Encapsulated {
                fragments,
                frame_ranges,
                ..
            } => {
                let mut total = 0u64;
                for frag in &fragments[frame_ranges[frame_index].clone()] {
                    total = total.checked_add(u64::from(frag.length))?;
                }
                total
            }
        };
        decoded_bytes.checked_add(encoded_bytes)
    };

    let Some(mut acc) = frame_total(batch_start) else {
        return batch_start + 1;
    };
    let mut end = batch_start + 1;
    while end < limit {
        let Some(cost) = frame_total(end) else {
            break;
        };
        let Some(next_acc) = acc.checked_add(cost) else {
            break;
        };
        if next_acc > budget {
            break;
        }
        acc = next_acc;
        end += 1;
    }
    end
}

const MAX_HTJ2K_SCRATCH_BYTES: usize = 512 * 1024 * 1024;

fn allocate_htj2k_planes(
    meta: &FrameEncodeMeta,
    target_ts: &TransferSyntax,
) -> Result<Vec<Vec<i32>>, TranscodeError> {
    let pixels = usize::from(meta.rows)
        .checked_mul(usize::from(meta.cols))
        .ok_or_else(|| TranscodeError::EncodePixelData {
            uid: target_ts.uid().to_owned(),
            name: target_ts.name().to_owned(),
            message: "frame dimensions overflow HTJ2K scratch plane size".to_owned(),
        })?;
    let spp = usize::from(meta.samples_per_pixel);
    let total_bytes = pixels
        .checked_mul(spp)
        .and_then(|samples| samples.checked_mul(std::mem::size_of::<i32>()))
        .ok_or_else(|| TranscodeError::EncodePixelData {
            uid: target_ts.uid().to_owned(),
            name: target_ts.name().to_owned(),
            message: "frame dimensions overflow HTJ2K scratch plane size".to_owned(),
        })?;
    if total_bytes > MAX_HTJ2K_SCRATCH_BYTES {
        return Err(TranscodeError::EncodePixelData {
            uid: target_ts.uid().to_owned(),
            name: target_ts.name().to_owned(),
            message: format!(
                "HTJ2K scratch plane size ({total_bytes} bytes) exceeds limit ({MAX_HTJ2K_SCRATCH_BYTES} bytes)"
            ),
        });
    }
    let mut planes = Vec::new();
    planes
        .try_reserve_exact(spp)
        .map_err(|_| TranscodeError::EncodePixelData {
            uid: target_ts.uid().to_owned(),
            name: target_ts.name().to_owned(),
            message: "could not allocate HTJ2K scratch plane list".to_owned(),
        })?;
    for _ in 0..spp {
        let mut plane = Vec::new();
        plane
            .try_reserve_exact(pixels)
            .map_err(|_| TranscodeError::EncodePixelData {
                uid: target_ts.uid().to_owned(),
                name: target_ts.name().to_owned(),
                message: "could not allocate HTJ2K scratch plane".to_owned(),
            })?;
        plane.resize(pixels, 0);
        planes.push(plane);
    }
    Ok(planes)
}

fn compute_frame_fragment_ranges(
    object: &DefaultDicomObject,
    offset_table: &[u32],
    fragments: &[FragmentLocation],
    nr_frames: usize,
) -> Option<Vec<Range<usize>>> {
    let number_of_fragments = fragments.len();
    if number_of_fragments == 0 || nr_frames == 0 {
        return None;
    }
    if number_of_fragments == nr_frames {
        return Some((0..nr_frames).map(|i| i..i + 1).collect());
    }
    if nr_frames == 1 {
        return Some(std::iter::once(0..number_of_fragments).collect());
    }

    let extended_offsets = object
        .get(tags::EXTENDED_OFFSET_TABLE)
        .and_then(|element| element.to_bytes().ok())
        .filter(|bytes| nr_frames.checked_mul(8) == Some(bytes.len()))
        .map(|bytes| {
            bytes
                .chunks_exact(8)
                .map(|chunk| u64::from_le_bytes(chunk.try_into().expect("8-byte chunk")))
                .collect::<Vec<u64>>()
        });
    let frame_offsets: Vec<u64> = match extended_offsets {
        Some(offsets) => offsets,
        None => {
            if offset_table.len() != nr_frames {
                return None;
            }
            offset_table.iter().map(|&o| u64::from(o)).collect()
        }
    };

    let mut fragment_starts = Vec::with_capacity(number_of_fragments);
    let mut position = 0u64;
    for fragment in fragments {
        fragment_starts.push(position);
        position += u64::from(fragment.length) + 8;
    }
    let fragment_at = |offset: u64| fragment_starts.binary_search(&offset).ok();

    let mut ranges = Vec::with_capacity(nr_frames);
    for frame_index in 0..nr_frames {
        let start = fragment_at(frame_offsets[frame_index])?;
        let end = match frame_offsets.get(frame_index + 1) {
            Some(&next) => fragment_at(next)?,
            None => number_of_fragments,
        };
        if start >= end {
            return None;
        }
        ranges.push(start..end);
    }
    Some(ranges)
}

fn read_frame_batch(
    input_file: &mut File,
    source_mode: &SourceMode<'_>,
    range: Range<usize>,
    frame_len: usize,
) -> Result<Vec<Vec<u8>>, TranscodeError> {
    let mut batch = Vec::with_capacity(range.len());
    match source_mode {
        SourceMode::Native {
            vr,
            offset,
            big_endian,
            ..
        } => {
            let start_offset = offset + (range.start as u64) * (frame_len as u64);
            input_file
                .seek(SeekFrom::Start(start_offset))
                .map_err(|source| dcmnorm_object::ReadError::Io {
                    source,
                    context: "seeking to native frame",
                })?;
            let swap_in = *big_endian && *vr == VR::OW;
            for _ in range {
                let mut buf = vec![0u8; frame_len];
                input_file.read_exact(&mut buf).map_err(|source| {
                    dcmnorm_object::ReadError::Io {
                        source,
                        context: "reading native frame",
                    }
                })?;
                if swap_in {
                    swap_u16_bytes(&mut buf);
                }
                batch.push(buf);
            }
        }
        SourceMode::Encapsulated {
            fragments,
            frame_ranges,
            ..
        } => {
            for frame_index in range {
                let frag_range = &frame_ranges[frame_index];
                let total: usize = fragments[frag_range.clone()]
                    .iter()
                    .map(|f| f.length as usize)
                    .sum();
                let mut codestream = Vec::with_capacity(total);
                for frag in &fragments[frag_range.clone()] {
                    input_file
                        .seek(SeekFrom::Start(frag.offset))
                        .map_err(|source| dcmnorm_object::ReadError::Io {
                            source,
                            context: "seeking to encapsulated fragment",
                        })?;
                    let start = codestream.len();
                    codestream.resize(start + frag.length as usize, 0);
                    input_file
                        .read_exact(&mut codestream[start..])
                        .map_err(|source| dcmnorm_object::ReadError::Io {
                            source,
                            context: "reading encapsulated fragment",
                        })?;
                }
                batch.push(codestream);
            }
        }
    }
    Ok(batch)
}

fn decode_one_frame(
    input: Vec<u8>,
    source_mode: &SourceMode<'_>,
    decode_meta: &FrameDecodeMeta,
    source_ts: &TransferSyntax,
    frame_len: usize,
    frame_index: usize,
) -> Result<Vec<u8>, TranscodeError> {
    match source_mode {
        SourceMode::Native { .. } => Ok(input),
        SourceMode::Encapsulated { decoder, .. } => {
            let decoded = match decoder {
                EncapsulatedDecoder::Registry(reader) => {
                    let view = SingleFramePixelDataObject {
                        meta: decode_meta,
                        codestream: &input,
                    };
                    let mut out = Vec::new();
                    reader.decode_frame(&view, 0, &mut out).map_err(|error| {
                        TranscodeError::DecodePixelData {
                            uid: source_ts.uid().to_owned(),
                            name: source_ts.name().to_owned(),
                            message: format!("frame {frame_index} decode failed: {error}"),
                        }
                    })?;
                    out
                }
                EncapsulatedDecoder::JpegLs => jpeg_ls::decode_jpeg_ls_codestream(
                    &input,
                    u32::from(decode_meta.rows),
                    u32::from(decode_meta.cols),
                )
                .map_err(|message| TranscodeError::DecodePixelData {
                    uid: source_ts.uid().to_owned(),
                    name: source_ts.name().to_owned(),
                    message: format!("frame {frame_index} decode failed: {message}"),
                })?,
                EncapsulatedDecoder::JpegXl => {
                    // Defer buffer allocation until `decode_jpeg_xl_fragment_into` validates the
                    // JXL frame dimensions against declared metadata.
                    let mut out = Vec::new();
                    jpeg_xl::decode_jpeg_xl_fragment_into(
                        &input,
                        usize::from(decode_meta.rows),
                        usize::from(decode_meta.cols),
                        usize::from(decode_meta.samples_per_pixel),
                        decode_meta.bits_allocated,
                        &mut out,
                    )
                    .map_err(|message| TranscodeError::DecodePixelData {
                        uid: source_ts.uid().to_owned(),
                        name: source_ts.name().to_owned(),
                        message: format!("frame {frame_index} decode failed: {message}"),
                    })?;
                    out
                }
            };
            if decoded.len() != frame_len {
                return Err(TranscodeError::DecodePixelData {
                    uid: source_ts.uid().to_owned(),
                    name: source_ts.name().to_owned(),
                    message: format!(
                        "frame {frame_index} decoded to {} bytes, expected {frame_len}",
                        decoded.len()
                    ),
                });
            }
            Ok(decoded)
        }
    }
}

fn encode_one_frame(
    native_frame: &[u8],
    target_mode: TargetMode,
    meta: &FrameEncodeMeta,
    target_ts: &TransferSyntax,
    frame_index: usize,
) -> Result<Vec<u8>, TranscodeError> {
    let encoded = match target_mode {
        TargetMode::Native { .. } => unreachable!(),
        TargetMode::Htj2k => {
            let pixels = usize::from(meta.rows) * usize::from(meta.cols);
            let mut planes = vec![vec![0i32; pixels]; usize::from(meta.samples_per_pixel)];
            encode_htj2k_frame(
                native_frame,
                meta.rows,
                meta.cols,
                meta.samples_per_pixel,
                meta.bits_allocated,
                meta.bits_stored,
                meta.is_signed,
                meta.planar,
                &mut planes,
            )
            .map_err(|e| format!("HTJ2K encode failed for frame {frame_index}: {e}"))
        }
        TargetMode::ClassicJpeg2000 { lossless } => encode_jpeg2000_frame(
            native_frame,
            meta.rows,
            meta.cols,
            meta.samples_per_pixel,
            meta.bits_allocated,
            meta.bits_stored,
            meta.is_signed,
            meta.planar,
            lossless,
        )
        .map_err(|e| format!("JPEG2000 encode failed for frame {frame_index}: {e}")),
        TargetMode::JpegLs { lossless } => jpeg_ls::encode_jpeg_ls_frame(
            native_frame,
            meta.rows,
            meta.cols,
            meta.samples_per_pixel,
            meta.bits_stored,
            meta.planar_configuration,
            lossless,
        )
        .map_err(|e| format!("JPEG-LS encode failed for frame {frame_index}: {e}")),
        TargetMode::JpegXlLossless => {
            let mut scratch = Vec::new();
            jpeg_xl::encode_jpeg_xl_lossless_frame(
                native_frame,
                meta.rows,
                meta.cols,
                meta.samples_per_pixel,
                meta.bits_allocated,
                meta.bits_stored,
                meta.planar,
                &mut scratch,
            )
            .map_err(|e| format!("JPEG XL encode failed for frame {frame_index}: {e}"))
        }
    };
    encoded.map_err(|message| TranscodeError::EncodePixelData {
        uid: target_ts.uid().to_owned(),
        name: target_ts.name().to_owned(),
        message,
    })
}

fn decode_batch(
    inputs: Vec<Vec<u8>>,
    source_mode: &SourceMode<'_>,
    decode_meta: &FrameDecodeMeta,
    source_ts: &TransferSyntax,
    frame_len: usize,
    batch_start: usize,
) -> Result<Vec<Vec<u8>>, TranscodeError> {
    let process = |(offset, input): (usize, Vec<u8>)| -> Result<Vec<u8>, TranscodeError> {
        decode_one_frame(
            input,
            source_mode,
            decode_meta,
            source_ts,
            frame_len,
            batch_start + offset,
        )
    };
    if inputs.len() == 1 || matches!(source_mode, SourceMode::Native { .. }) {
        inputs.into_iter().enumerate().map(process).collect()
    } else {
        inputs.into_par_iter().enumerate().map(process).collect()
    }
}

fn transform_batch_to_native(
    inputs: Vec<Vec<u8>>,
    source_mode: &SourceMode<'_>,
    decode_meta: &FrameDecodeMeta,
    source_ts: &TransferSyntax,
    frame_len: usize,
    swap_out: bool,
    batch_start: usize,
) -> Result<Vec<Vec<u8>>, TranscodeError> {
    let process = |(offset, input): (usize, Vec<u8>)| -> Result<Vec<u8>, TranscodeError> {
        let mut decoded = decode_one_frame(
            input,
            source_mode,
            decode_meta,
            source_ts,
            frame_len,
            batch_start + offset,
        )?;
        if swap_out {
            swap_u16_bytes(&mut decoded);
        }
        Ok(decoded)
    };
    if inputs.len() == 1 {
        inputs.into_iter().enumerate().map(process).collect()
    } else {
        inputs.into_par_iter().enumerate().map(process).collect()
    }
}

#[allow(clippy::too_many_arguments)]
fn transform_batch_to_encapsulated(
    inputs: Vec<Vec<u8>>,
    source_mode: &SourceMode<'_>,
    decode_meta: &FrameDecodeMeta,
    source_ts: &TransferSyntax,
    target_mode: TargetMode,
    encode_meta: &FrameEncodeMeta,
    target_ts: &TransferSyntax,
    frame_len: usize,
    batch_start: usize,
) -> Result<Vec<Vec<u8>>, TranscodeError> {
    let process = |(offset, input): (usize, Vec<u8>)| -> Result<Vec<u8>, TranscodeError> {
        let frame_index = batch_start + offset;
        let native = decode_one_frame(
            input,
            source_mode,
            decode_meta,
            source_ts,
            frame_len,
            frame_index,
        )?;
        encode_one_frame(&native, target_mode, encode_meta, target_ts, frame_index)
    };
    if inputs.len() == 1 {
        inputs.into_iter().enumerate().map(process).collect()
    } else {
        inputs.into_par_iter().enumerate().map(process).collect()
    }
}

fn rebuild_passthrough_offset_table(
    offset_table: &[u32],
    fragments: &[FragmentLocation],
) -> Option<Vec<u32>> {
    if offset_table.is_empty() || !fragments.iter().any(|f| f.length % 2 == 1) {
        return Some(offset_table.to_vec());
    }
    let mut mapping = std::collections::HashMap::with_capacity(fragments.len());
    let mut old_rel: u64 = 0;
    let mut new_rel: u64 = 0;
    for frag in fragments {
        mapping.insert(old_rel, new_rel);
        old_rel = old_rel.checked_add(8 + u64::from(frag.length))?;
        let padded = u64::from(frag.length) + u64::from(frag.length % 2);
        new_rel = new_rel.checked_add(8 + padded)?;
    }
    let mut rebuilt = Vec::with_capacity(offset_table.len());
    for &entry in offset_table {
        let &mapped = mapping.get(&u64::from(entry))?;
        rebuilt.push(u32::try_from(mapped).ok()?);
    }
    Some(rebuilt)
}

pub(crate) fn has_nested_encapsulated_pixel_data(
    object: &dcmnorm_object::InMemDicomObject,
) -> bool {
    for elem in object.iter() {
        if let dcmnorm_core::value::Value::Sequence(seq) = elem.value() {
            for item in seq.items() {
                if item_has_encapsulated_pixel_data(item) {
                    return true;
                }
            }
        }
    }
    false
}

fn item_has_encapsulated_pixel_data(item: &dcmnorm_object::InMemDicomObject) -> bool {
    for elem in item.iter() {
        match elem.value() {
            dcmnorm_core::value::Value::PixelSequence(_) => return true,
            dcmnorm_core::value::Value::Sequence(seq)
                if seq.items().iter().any(item_has_encapsulated_pixel_data) =>
            {
                return true;
            }
            _ => {}
        }
    }
    false
}

fn stream_passthrough(
    object: &DefaultDicomObject,
    deferred_pixel_data: &DeferredPixelData,
    input_file: &mut File,
    input_path: &Path,
    output_path: &Path,
) -> Result<bool, TranscodeError> {
    let rebuilt_bot = match deferred_pixel_data {
        DeferredPixelData::Encapsulated {
            offset_table,
            fragments,
            ..
        } => match rebuild_passthrough_offset_table(offset_table, fragments) {
            Some(bot) => Some(bot),
            None => return Ok(false),
        },
        _ => None,
    };

    let Some(temp) = try_create_temp_output(input_path, output_path)? else {
        return Ok(false);
    };
    {
        let mut writer = BufWriter::new(temp.file());
        object.write_all_with_pixel_data(
            &mut writer,
            |ds_writer, _| -> Result<(), TranscodeError> {
                match deferred_pixel_data {
                    DeferredPixelData::Absent => Ok(()),
                    DeferredPixelData::Native { vr, offset, length } => {
                        stream_native_bytes(input_file, *offset, *length, *vr, false, ds_writer)
                    }
                    DeferredPixelData::Encapsulated { fragments, .. } => {
                        let offset_table = rebuilt_bot.as_deref().unwrap_or(&[]);
                        ds_writer
                            .write(DataToken::PixelSequenceStart)
                            .map_err(|source| WriteError::Dataset { source })?;
                        ds_writer
                            .write(DataToken::ItemStart {
                                len: Length::defined((offset_table.len() * 4) as u32),
                            })
                            .map_err(|source| WriteError::Dataset { source })?;
                        if !offset_table.is_empty() {
                            ds_writer
                                .write(DataToken::OffsetTable(offset_table.to_vec()))
                                .map_err(|source| WriteError::Dataset { source })?;
                        }
                        ds_writer
                            .write(DataToken::ItemEnd)
                            .map_err(|source| WriteError::Dataset { source })?;

                        let mut chunk = vec![0u8; COPY_CHUNK_SIZE];
                        for frag in fragments {
                            let padded_len = frag.length + (frag.length % 2);
                            ds_writer
                                .write(DataToken::ItemStart {
                                    len: Length::defined(padded_len),
                                })
                                .map_err(|source| WriteError::Dataset { source })?;
                            copy_file_range(
                                input_file,
                                frag.offset,
                                u64::from(frag.length),
                                false,
                                &mut chunk,
                                ds_writer,
                            )?;
                            if frag.length % 2 == 1 {
                                ds_writer
                                    .write_raw_bytes(&[0])
                                    .map_err(|source| WriteError::Dataset { source })?;
                            }
                            ds_writer
                                .write(DataToken::ItemEnd)
                                .map_err(|source| WriteError::Dataset { source })?;
                        }
                        ds_writer
                            .write(DataToken::SequenceEnd)
                            .map_err(|source| WriteError::Dataset { source })?;
                        Ok(())
                    }
                }
            },
        )?;
        writer.flush().map_err(|source| WriteError::Io {
            source,
            context: "flushing transcoded file",
        })?;
    }
    temp.persist(output_path)?;
    Ok(true)
}

fn stream_native_bytes<W, E>(
    input_file: &mut File,
    offset: u64,
    length: u32,
    vr: VR,
    swap_words: bool,
    ds_writer: &mut DataSetWriter<W, E>,
) -> Result<(), TranscodeError>
where
    W: Write,
    E: dcmnorm_encoding::encode::EncodeTo<W>,
{
    let padded_len = length.saturating_add(length % 2);
    ds_writer
        .write_element_header(DataElementHeader::new(
            tags::PIXEL_DATA,
            vr,
            Length::defined(padded_len),
        ))
        .map_err(|source| WriteError::Dataset { source })?;
    let mut chunk = vec![0u8; COPY_CHUNK_SIZE];
    copy_file_range(
        input_file,
        offset,
        u64::from(length),
        swap_words,
        &mut chunk,
        ds_writer,
    )?;
    if length % 2 == 1 {
        ds_writer
            .write_raw_bytes(&[0])
            .map_err(|source| WriteError::Dataset { source })?;
    }
    Ok(())
}

fn copy_file_range<W, E>(
    input_file: &mut File,
    offset: u64,
    mut remaining: u64,
    swap_words: bool,
    chunk: &mut [u8],
    ds_writer: &mut DataSetWriter<W, E>,
) -> Result<(), TranscodeError>
where
    W: Write,
    E: dcmnorm_encoding::encode::EncodeTo<W>,
{
    debug_assert!(chunk.len().is_multiple_of(2));
    input_file
        .seek(SeekFrom::Start(offset))
        .map_err(|source| dcmnorm_object::ReadError::Io {
            source,
            context: "seeking to PixelData",
        })?;
    while remaining > 0 {
        let to_read = (remaining as usize).min(chunk.len());
        input_file
            .read_exact(&mut chunk[..to_read])
            .map_err(|source| dcmnorm_object::ReadError::Io {
                source,
                context: "reading PixelData chunk",
            })?;
        if swap_words {
            swap_u16_bytes(&mut chunk[..to_read]);
        }
        ds_writer
            .write_raw_bytes(&chunk[..to_read])
            .map_err(|source| WriteError::Dataset { source })?;
        remaining -= to_read as u64;
    }
    Ok(())
}

fn swap_u16_bytes(bytes: &mut [u8]) {
    for pair in bytes.chunks_exact_mut(2) {
        pair.swap(0, 1);
    }
}

/// Temporary file created alongside `output_path` and atomically renamed into place on success.
/// Removes itself automatically on drop if an error occurs before [`Self::persist`], and makes
/// in-place `--overwrite` (`input_path == output_path`) completely safe.
struct TempOutputFile {
    path: PathBuf,
    file: Option<File>,
    persisted: bool,
}

impl TempOutputFile {
    fn file(&self) -> &File {
        self.file.as_ref().expect("temp file still open")
    }

    fn file_mut(&mut self) -> &mut File {
        self.file.as_mut().expect("temp file still open")
    }

    fn create_next_to(output_path: &Path) -> Result<Self, WriteError> {
        #[cfg(windows)]
        {
            use std::os::windows::ffi::OsStrExt;
            if output_path.as_os_str().encode_wide().any(|unit| unit == 0) {
                return Err(WriteError::Io {
                    source: std::io::Error::new(
                        std::io::ErrorKind::InvalidInput,
                        "output path contains interior NUL character",
                    ),
                    context: "creating temporary output file",
                });
            }
        }
        let dir = output_path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        let pid = std::process::id();
        for _ in 0..64 {
            let seq = TEMP_FILE_COUNTER.fetch_add(1, Ordering::Relaxed);
            let candidate = dir.join(format!(".dcmnorm-transcode-{pid}-{seq}.tmp"));
            let mut opts = OpenOptions::new();
            opts.read(true).write(true).create_new(true);
            #[cfg(unix)]
            if output_path.exists() {
                // Keep staged bytes private until `persist` applies the destination's mode.
                use std::os::unix::fs::OpenOptionsExt;
                opts.mode(0o600);
            }
            match opts.open(&candidate) {
                Ok(file) => {
                    return Ok(Self {
                        path: candidate,
                        file: Some(file),
                        persisted: false,
                    });
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(source) => {
                    return Err(WriteError::Io {
                        source,
                        context: "creating temporary output file",
                    });
                }
            }
        }
        Err(WriteError::Io {
            source: std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                "could not allocate unique temporary output file",
            ),
            context: "creating temporary output file",
        })
    }

    fn persist(mut self, target: &Path) -> Result<(), WriteError> {
        // Close the file handle before renaming (required on Windows).
        let temp_path = self.path.clone();
        self.persisted = true;
        drop(self.file.take());

        // Preserve existing destination permissions (e.g. 0600 on --overwrite) before replacing,
        // while avoiding granting group access if a setgid directory assigned a different GID.
        #[cfg(unix)]
        if let (Ok(existing_meta), Ok(temp_meta)) =
            (std::fs::metadata(target), std::fs::metadata(&temp_path))
        {
            use std::os::unix::fs::{MetadataExt, PermissionsExt};
            let mut mode = existing_meta.permissions().mode() & 0o777;
            if temp_meta.gid() != existing_meta.gid() {
                mode &= !0o070;
            }
            let _ = std::fs::set_permissions(&temp_path, std::fs::Permissions::from_mode(mode));
        }

        if let Err(source) = replace_file_atomically(&temp_path, target) {
            let _ = std::fs::remove_file(&temp_path);
            return Err(WriteError::Io {
                source,
                context: "renaming transcoded output file into place",
            });
        }
        Ok(())
    }
}

fn ensure_existing_output_writable(output_path: &Path) -> Result<(), WriteError> {
    match OpenOptions::new().write(true).open(output_path) {
        Ok(_) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(WriteError::Io {
            source,
            context: "checking write access to existing output file",
        }),
    }
}

fn try_create_temp_output(
    input_path: &Path,
    output_path: &Path,
) -> Result<Option<TempOutputFile>, WriteError> {
    match TempOutputFile::create_next_to(output_path) {
        Ok(temp) => Ok(Some(temp)),
        Err(WriteError::Io { source, .. })
            if source.kind() == std::io::ErrorKind::PermissionDenied
                && output_path.exists()
                && !same_file(input_path, output_path) =>
        {
            Ok(None)
        }
        Err(err) => Err(err),
    }
}

fn same_file(a: &Path, b: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        match (std::fs::metadata(a), std::fs::metadata(b)) {
            (Ok(ma), Ok(mb)) => ma.dev() == mb.dev() && ma.ino() == mb.ino(),
            _ => false,
        }
    }
    #[cfg(not(unix))]
    {
        match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
            (Ok(ca), Ok(cb)) => ca == cb,
            _ => false,
        }
    }
}

/// Reads up to 64 KiB of a JPEG 2000 codestream header from `frag`. When `frag` is wrapped in a
/// JP2 container, walks top-level boxes by seeking until `jp2c` so arbitrarily large leading boxes
/// (e.g. ICC profiles or metadata boxes past 64 KiB) never hide the codestream's SIZ/COD markers.
fn read_j2k_header_probe(
    input_file: &mut File,
    frag: FragmentLocation,
) -> Result<Vec<u8>, dcmnorm_object::ReadError> {
    const MAX_PROBE_BYTES: usize = 65536;
    let frag_len = u64::from(frag.length);
    let frag_end = frag.offset.saturating_add(frag_len);

    let head_len = (frag_len as usize).min(12);
    let mut head = [0u8; 12];
    input_file
        .seek(SeekFrom::Start(frag.offset))
        .map_err(|source| dcmnorm_object::ReadError::Io {
            source,
            context: "seeking to JPEG2000 codestream header",
        })?;
    input_file
        .read_exact(&mut head[..head_len])
        .map_err(|source| dcmnorm_object::ReadError::Io {
            source,
            context: "reading JPEG2000 codestream header",
        })?;

    if head_len == 12 && head == dcmnorm_jpeg2000::codestream::JP2_SIGNATURE {
        let mut pos = frag.offset;
        while pos.saturating_add(8) <= frag_end {
            input_file.seek(SeekFrom::Start(pos)).map_err(|source| {
                dcmnorm_object::ReadError::Io {
                    source,
                    context: "seeking JPEG2000 JP2 box header",
                }
            })?;
            let mut box_hdr = [0u8; 8];
            input_file.read_exact(&mut box_hdr).map_err(|source| {
                dcmnorm_object::ReadError::Io {
                    source,
                    context: "reading JPEG2000 JP2 box header",
                }
            })?;
            let lbox = u64::from(u32::from_be_bytes([
                box_hdr[0], box_hdr[1], box_hdr[2], box_hdr[3],
            ]));
            let kind = &box_hdr[4..8];
            let (header_len, box_len) = match lbox {
                0 => (8u64, frag_end - pos),
                1 => {
                    if pos.saturating_add(16) > frag_end {
                        break;
                    }
                    let mut xl = [0u8; 8];
                    input_file.read_exact(&mut xl).map_err(|source| {
                        dcmnorm_object::ReadError::Io {
                            source,
                            context: "reading JPEG2000 JP2 extended box length",
                        }
                    })?;
                    (16u64, u64::from_be_bytes(xl))
                }
                2..=7 => break,
                n => (8u64, n),
            };
            if box_len < header_len {
                break;
            }
            let payload_start = pos.saturating_add(header_len);
            if kind == b"jp2c" {
                let payload_end = pos.saturating_add(box_len).min(frag_end);
                let to_read =
                    (payload_end.saturating_sub(payload_start) as usize).min(MAX_PROBE_BYTES);
                let mut probe = vec![0u8; to_read];
                input_file.read_exact(&mut probe).map_err(|source| {
                    dcmnorm_object::ReadError::Io {
                        source,
                        context: "reading JPEG2000 jp2c codestream header",
                    }
                })?;
                return Ok(probe);
            }
            let Some(next_pos) = pos.checked_add(box_len) else {
                break;
            };
            pos = next_pos;
        }
        return Ok(Vec::new());
    }

    let probe_len = (frag_len as usize).min(MAX_PROBE_BYTES);
    let mut probe = vec![0u8; probe_len];
    probe[..head_len].copy_from_slice(&head[..head_len]);
    if probe_len > head_len {
        input_file
            .read_exact(&mut probe[head_len..])
            .map_err(|source| dcmnorm_object::ReadError::Io {
                source,
                context: "reading JPEG2000 codestream header",
            })?;
    }
    Ok(probe)
}

pub(crate) fn replace_file_atomically(temp_path: &Path, target: &Path) -> std::io::Result<()> {
    std::fs::rename(temp_path, target)
}

impl Drop for TempOutputFile {
    fn drop(&mut self) {
        if !self.persisted {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

// =============================================================================================
// DIMSE C-STORE SCU streaming: transcode a file directly into a forward-only Write sink
// (PDataWriter) without loading PixelData in full or buffering the serialized dataset.
// =============================================================================================

/// A validated streaming transcode plan for sending a bare DICOM dataset in a native transfer
/// syntax (`1.2.840.10008.1.2`, `.1.2.1`, `.1.2.2`) directly into a forward-only `Write` sink
/// such as `dcmnorm_dimse::PDataWriter`.
pub(super) struct PreparedNativeDatasetStream {
    object: DefaultDicomObject,
    input_file: File,
    target_ts: &'static TransferSyntax,
    plan: NativeStreamPlan,
}

enum NativeStreamPlan {
    Absent,
    DirectNative {
        vr: VR,
        offset: u64,
        length: u32,
        swap_words: bool,
    },
    DecodeEncapsulated {
        source_mode: SourceMode<'static>,
        decode_meta: FrameDecodeMeta,
        source_ts: &'static TransferSyntax,
        nr_frames: usize,
        frame_len: usize,
        total_native_bytes: u64,
        out_vr: VR,
        swap_out: bool,
        batch_size: usize,
    },
}

/// Prepare to stream `input_path` transcoded into a native (uncompressed) `target_uid` directly
/// to a forward-only sink (e.g. DIMSE `PDataWriter`).
///
/// Validates everything upfront *before* the caller sends the C-STORE-RQ command PDU, returning
/// `Ok(None)` when streaming does not apply (non-native target, deflated/meta-less/MPEG source,
/// single-frame Kakadu, or missing offset table) so the caller can fall back cleanly.
pub(super) fn prepare_streaming_dataset_for_dimse(
    input_path: &Path,
    target_uid: &str,
) -> Result<Option<PreparedNativeDatasetStream>, TranscodeError> {
    let target_uid = normalize_transfer_syntax_uid(target_uid);
    let target_ts = TransferSyntaxRegistry
        .get(target_uid)
        .ok_or_else(|| TranscodeError::UnknownTransferSyntax(target_uid.to_owned()))?;
    if is_encapsulated_transfer_syntax(target_ts) || matches!(target_ts.codec(), Codec::Dataset(_))
    {
        return Ok(None);
    }

    if !std::fs::metadata(input_path).is_ok_and(|meta| meta.file_type().is_file()) {
        return Ok(None);
    }

    let open_result = OpenFileOptions::new()
        .read_preamble(ReadPreamble::Always)
        .open_file_deferring_pixel_data(input_path);
    let (mut object, deferred_pixel_data, mut input_file) = match open_result {
        Ok(Some(triple)) => triple,
        Ok(None) | Err(_) => return Ok(None),
    };

    let source_uid = normalize_transfer_syntax_uid(object.meta().transfer_syntax()).to_owned();
    let source_ts = TransferSyntaxRegistry
        .get(&source_uid)
        .ok_or_else(|| TranscodeError::UnknownTransferSyntax(source_uid.clone()))?;

    if source_uid != target_uid && has_nested_encapsulated_pixel_data(&object) {
        return Err(TranscodeError::EncodePixelData {
            uid: target_uid.to_owned(),
            name: target_ts.name().to_owned(),
            message: "cannot transcode dataset containing encapsulated nested PixelData (e.g. IconImageSequence) to a different transfer syntax".to_owned(),
        });
    }

    let target_be = target_uid == EXPLICIT_VR_BIG_ENDIAN_UID;

    match deferred_pixel_data {
        DeferredPixelData::Absent => Ok(Some(PreparedNativeDatasetStream {
            object,
            input_file,
            target_ts,
            plan: NativeStreamPlan::Absent,
        })),
        DeferredPixelData::Native { vr, offset, length } => {
            let source_be = source_uid == EXPLICIT_VR_BIG_ENDIAN_UID;
            let swap_words = vr == VR::OW && source_be != target_be;
            if swap_words && length % 2 != 0 {
                return Ok(None);
            }
            Ok(Some(PreparedNativeDatasetStream {
                object,
                input_file,
                target_ts,
                plan: NativeStreamPlan::DirectNative {
                    vr,
                    offset,
                    length,
                    swap_words,
                },
            }))
        }
        DeferredPixelData::Encapsulated {
            offset_table,
            fragments,
            ..
        } => {
            let nr_frames = number_of_frames(&object);
            if is_mpeg_transfer_syntax(&source_uid) || fragments.is_empty() {
                return Ok(None);
            }
            let Some(frame_ranges) =
                compute_frame_fragment_ranges(&object, &offset_table, &fragments, nr_frames)
            else {
                return Ok(None);
            };
            if source_uid == uids::RLE_LOSSLESS && fragments.len() != nr_frames {
                return Ok(None);
            }
            if is_jpeg_ls_transfer_syntax(&source_uid) && nr_frames == 1 && fragments.len() > 1 {
                return Ok(None);
            }
            if is_jpeg_xl_transfer_syntax(&source_uid) && fragments.len() != nr_frames {
                return Ok(None);
            }

            let is_j2k = is_jpeg2000_transfer_syntax(&source_uid);
            let is_htj2k = is_htj2k_transfer_syntax(&source_uid);
            let j2k_pref = jpeg2000_codec_preference();
            if is_j2k
                && !is_htj2k
                && kakadu_ffi_enabled()
                && j2k_pref != Jpeg2000CodecPreference::OpenJpeg
                && (nr_frames == 1 || j2k_pref == Jpeg2000CodecPreference::Kakadu)
            {
                return Ok(None);
            }

            let mut j2k_uses_mct = None;
            if is_j2k {
                let first_frag = fragments[frame_ranges[0].start];
                let probe = read_j2k_header_probe(&mut input_file, first_frag)?;
                j2k_uses_mct = apply_j2k_probe_corrections(&mut object, &probe, source_ts)?;
            }

            let decoder = if is_jpeg_ls_transfer_syntax(&source_uid) {
                if !cfg!(feature = "jpeg-ls-codec") {
                    return Ok(None);
                }
                EncapsulatedDecoder::JpegLs
            } else if is_jpeg_xl_transfer_syntax(&source_uid) {
                if !cfg!(feature = "jpeg-xl-codec") {
                    return Ok(None);
                }
                let pixel_rep = object
                    .get(tags::PIXEL_REPRESENTATION)
                    .and_then(|e| e.uint16().ok())
                    .unwrap_or(0);
                if pixel_rep != 0 {
                    return Ok(None);
                }
                EncapsulatedDecoder::JpegXl
            } else {
                let Codec::EncapsulatedPixelData(Some(reader), _) = source_ts.codec() else {
                    return Ok(None);
                };
                EncapsulatedDecoder::Registry(reader.as_ref())
            };

            let Some(rows) = object.get(tags::ROWS).and_then(|e| e.uint16().ok()) else {
                return Ok(None);
            };
            let Some(cols) = object.get(tags::COLUMNS).and_then(|e| e.uint16().ok()) else {
                return Ok(None);
            };
            let samples_per_pixel = object
                .get(tags::SAMPLES_PER_PIXEL)
                .and_then(|e| e.uint16().ok())
                .unwrap_or(1);
            let Some(bits_allocated) = object
                .get(tags::BITS_ALLOCATED)
                .and_then(|e| e.uint16().ok())
            else {
                return Ok(None);
            };
            let bits_stored = object
                .get(tags::BITS_STORED)
                .and_then(|e| e.uint16().ok())
                .unwrap_or(bits_allocated);
            if (bits_allocated != 8 && bits_allocated != 16)
                || bits_stored == 0
                || bits_stored > bits_allocated
            {
                return Ok(None);
            }
            let bytes_per_sample = usize::from(bits_allocated / 8);
            let Some(frame_len) = usize::from(rows)
                .checked_mul(usize::from(cols))
                .and_then(|n| n.checked_mul(usize::from(samples_per_pixel)))
                .and_then(|n| n.checked_mul(bytes_per_sample))
            else {
                return Ok(None);
            };
            if frame_len == 0 {
                return Ok(None);
            }
            let Some(total_native_bytes) = (frame_len as u64).checked_mul(nr_frames as u64) else {
                return Ok(None);
            };
            if total_native_bytes + (total_native_bytes % 2) > u64::from(u32::MAX) {
                return Ok(None);
            }

            let decode_meta = FrameDecodeMeta {
                transfer_syntax_uid: source_uid.clone(),
                rows,
                cols,
                samples_per_pixel,
                bits_allocated,
                bits_stored,
                photometric_interpretation: object
                    .get(tags::PHOTOMETRIC_INTERPRETATION)
                    .and_then(|e| e.to_str().ok())
                    .map(|s| s.trim().to_owned()),
            };

            normalize_decoded_pixel_data_attributes(&mut object, &source_uid, j2k_uses_mct);
            remove_encapsulation_sidecar_attributes(&mut object);

            let source_mode = SourceMode::Encapsulated {
                fragments,
                frame_ranges,
                decoder,
            };
            let target_mode = TargetMode::Native {
                big_endian: target_be,
            };
            let batch_size = choose_batch_size(
                &source_uid,
                &source_mode,
                &target_mode,
                nr_frames,
                frame_len,
            );
            let out_vr = native_pixel_vr(bits_allocated);
            let swap_out = target_be && out_vr == VR::OW;

            Ok(Some(PreparedNativeDatasetStream {
                object,
                input_file,
                target_ts,
                plan: NativeStreamPlan::DecodeEncapsulated {
                    source_mode,
                    decode_meta,
                    source_ts,
                    nr_frames,
                    frame_len,
                    total_native_bytes,
                    out_vr,
                    swap_out,
                    batch_size,
                },
            }))
        }
    }
}

impl PreparedNativeDatasetStream {
    pub(super) fn object(&self) -> &DefaultDicomObject {
        &self.object
    }

    /// Stream the transcoded bare DICOM dataset directly into `out`.
    pub(super) fn write_to<W: Write>(mut self, out: W) -> Result<(), TranscodeError> {
        let target_ts = self.target_ts;
        match self.plan {
            NativeStreamPlan::Absent => {
                self.object.write_dataset_with_ts(out, target_ts)?;
                Ok(())
            }
            NativeStreamPlan::DirectNative {
                vr,
                offset,
                length,
                swap_words,
            } => self.object.write_dataset_with_ts_and_pixel_data(
                out,
                target_ts,
                |ds_writer| -> Result<(), TranscodeError> {
                    stream_native_bytes(
                        &mut self.input_file,
                        offset,
                        length,
                        vr,
                        swap_words,
                        ds_writer,
                    )
                },
            ),
            NativeStreamPlan::DecodeEncapsulated {
                source_mode,
                decode_meta,
                source_ts,
                nr_frames,
                frame_len,
                total_native_bytes,
                out_vr,
                swap_out,
                batch_size,
            } => self.object.write_dataset_with_ts_and_pixel_data(
                out,
                target_ts,
                |ds_writer| -> Result<(), TranscodeError> {
                    let padded_len = (total_native_bytes + (total_native_bytes % 2)) as u32;
                    ds_writer
                        .write_element_header(DataElementHeader::new(
                            tags::PIXEL_DATA,
                            out_vr,
                            Length::defined(padded_len),
                        ))
                        .map_err(|source| WriteError::Dataset { source })?;

                    let mut batch_start = 0;
                    while batch_start < nr_frames {
                        let batch_end = next_batch_end(
                            &source_mode,
                            batch_start,
                            nr_frames,
                            batch_size,
                            frame_len,
                        );
                        let inputs = read_frame_batch(
                            &mut self.input_file,
                            &source_mode,
                            batch_start..batch_end,
                            frame_len,
                        )?;
                        let outputs = transform_batch_to_native(
                            inputs,
                            &source_mode,
                            &decode_meta,
                            source_ts,
                            frame_len,
                            swap_out,
                            batch_start,
                        )?;
                        for frame in outputs {
                            ds_writer
                                .write_raw_bytes(&frame)
                                .map_err(|source| WriteError::Dataset { source })?;
                        }
                        batch_start = batch_end;
                    }
                    if total_native_bytes % 2 == 1 {
                        ds_writer
                            .write_raw_bytes(&[0])
                            .map_err(|source| WriteError::Dataset { source })?;
                    }
                    Ok(())
                },
            ),
        }
    }
}

fn apply_j2k_probe_corrections(
    object: &mut DefaultDicomObject,
    probe: &[u8],
    source_ts: &TransferSyntax,
) -> Result<Option<bool>, TranscodeError> {
    let declared_spp = object
        .get(tags::SAMPLES_PER_PIXEL)
        .and_then(|e| e.uint16().ok())
        .unwrap_or(1);
    if declared_spp > 1 {
        if let Some(actual_components) = codestream_component_count(probe) {
            if actual_components > 0 && actual_components < declared_spp {
                apply_jpeg2000_component_correction(object, actual_components);
            }
        }
    }
    if let (Some(precision), Some(bits_allocated)) = (
        codestream_precision(probe),
        object
            .get(tags::BITS_ALLOCATED)
            .and_then(|e| e.uint16().ok()),
    ) {
        let bits_stored = object
            .get(tags::BITS_STORED)
            .and_then(|e| e.uint16().ok())
            .unwrap_or(bits_allocated);
        if precision > bits_stored {
            apply_jpeg2000_precision_correction(object, precision).map_err(|message| {
                TranscodeError::DecodePixelData {
                    uid: source_ts.uid().to_owned(),
                    name: source_ts.name().to_owned(),
                    message,
                }
            })?;
        }
    }
    Ok(codestream_uses_mct(probe))
}

// =============================================================================================
// DIMSE C-STORE SCP streaming: receive a dataset from a forward-only Read stream (PDataReader)
// and transcode on the fly directly into a Part 10 File on disk (e.g. uncompressed -> HTJ2K).
// Non-images or ineligible syntaxes fall back seamlessly to raw wire passthrough into the same
// Part 10 file with O(PDU) memory and a single sequential disk write.
// =============================================================================================

const MAX_SCP_BOT_FRAMES: usize = 1_000_000;
const MAX_SCP_BOT_BYTES: u32 = (MAX_SCP_BOT_FRAMES as u32) * 4;
const MAX_SCP_TRANSCODE_FRAME_BYTES: usize = 256 * 1024 * 1024;
const MAX_SCP_ENCODED_FRAME_BYTES: usize = 128 * 1024 * 1024;

fn is_lossy_target_mode(mode: TargetMode) -> bool {
    matches!(
        mode,
        TargetMode::ClassicJpeg2000 { lossless: false } | TargetMode::JpegLs { lossless: false }
    )
}

fn write_zero_bytes<W: Write, E: dcmnorm_encoding::encode::EncodeTo<W>>(
    ds_writer: &mut DataSetWriter<W, E>,
    mut remaining: usize,
) -> Result<(), WriteError> {
    const ZEROS: [u8; 4096] = [0u8; 4096];
    while remaining > 0 {
        let chunk = remaining.min(ZEROS.len());
        ds_writer
            .write_raw_bytes(&ZEROS[..chunk])
            .map_err(|source| WriteError::Dataset { source })?;
        remaining -= chunk;
    }
    Ok(())
}

/// Validates an SCP `storage_transfer_syntax` option up front (at `start_scp` / CLI startup)
/// so typos, unsupported UIDs, or lossy transfer syntaxes fail immediately instead of on the
/// first incoming C-STORE.
pub(crate) fn validate_storage_transfer_syntax(
    target_transfer_syntax_uid: &str,
) -> Result<(), String> {
    let target_uid = normalize_transfer_syntax_uid(target_transfer_syntax_uid);
    let target_ts = TransferSyntaxRegistry
        .get(target_uid)
        .ok_or_else(|| format!("unknown storage transfer syntax UID: {target_uid:?}"))?;
    if matches!(target_ts.codec(), Codec::Dataset(_)) {
        return Err(format!(
            "unsupported storage transfer syntax for on-the-fly SCP transcoding: {} ({})",
            target_ts.uid(),
            target_ts.name()
        ));
    }
    let Some(mode) = resolve_target_mode(target_ts) else {
        return Err(format!(
            "unsupported storage transfer syntax for on-the-fly SCP transcoding: {} ({})",
            target_ts.uid(),
            target_ts.name()
        ));
    };
    if is_lossy_target_mode(mode) {
        return Err(format!(
            "lossy storage transfer syntax {} ({}) is not supported for on-the-fly SCP transcoding (would modify pixels without minting a derived SOPInstanceUID)",
            target_ts.uid(),
            target_ts.name()
        ));
    }
    Ok(())
}

/// Receive one C-STORE dataset from `pdata` (a forward-only stream ending at the last Data PDV)
/// and write a complete Part 10 file to `output_file`, transcoding on the fly to `target_uid`
/// whenever the incoming object is eligible (otherwise preserving `negotiated_ts` verbatim).
///
/// Returns `(study_instance_uid, modality)` extracted from the dataset header.
#[allow(clippy::too_many_arguments)]
pub(super) fn receive_and_transcode_stream_to_file<R: Read>(
    pdata: &mut R,
    negotiated_ts: &'static TransferSyntax,
    target_uid: &str,
    affected_sop_class_uid: &str,
    affected_sop_instance_uid: &str,
    output_file: &mut File,
    output_path: &Path,
) -> Result<(String, String), String> {
    let _scope = perf::scope("scp.receive_and_transcode_stream");
    let source_uid = normalize_transfer_syntax_uid(negotiated_ts.uid()).to_owned();
    let target_uid = normalize_transfer_syntax_uid(target_uid);
    let target_ts = TransferSyntaxRegistry
        .get(target_uid)
        .ok_or_else(|| format!("unknown storage transfer syntax UID: {target_uid}"))?;

    let (dataset, stop) =
        InMemDicomObject::read_dataset_until_pixel_data(&mut *pdata, negotiated_ts)
            .map_err(|e| format!("failed to parse incoming C-STORE dataset header: {e}"))?;

    // Reject datasets whose embedded SOP Class or SOP Instance UID disagrees with the
    // C-STORE-RQ command so a mismatched payload cannot overwrite another stored instance.
    if let Some(ds_sop_class) = dataset
        .get(tags::SOP_CLASS_UID)
        .and_then(|e| e.to_str().ok())
        .map(|s| s.trim_end_matches(['\0', ' ']).to_owned())
        .filter(|s| !s.is_empty())
    {
        if ds_sop_class != affected_sop_class_uid.trim_end_matches(['\0', ' ']) {
            return Err(format!(
                "dataset SOPClassUID ({ds_sop_class}) does not match C-STORE-RQ AffectedSOPClassUID ({affected_sop_class_uid})"
            ));
        }
    }
    if let Some(ds_sop_instance) = dataset
        .get(tags::SOP_INSTANCE_UID)
        .and_then(|e| e.to_str().ok())
        .map(|s| s.trim_end_matches(['\0', ' ']).to_owned())
        .filter(|s| !s.is_empty())
    {
        if ds_sop_instance != affected_sop_instance_uid.trim_end_matches(['\0', ' ']) {
            return Err(format!(
                "dataset SOPInstanceUID ({ds_sop_instance}) does not match C-STORE-RQ AffectedSOPInstanceUID ({affected_sop_instance_uid})"
            ));
        }
    }

    let study_instance_uid = dataset
        .get(tags::STUDY_INSTANCE_UID)
        .and_then(|e| e.to_str().ok())
        .map(|s| s.trim().to_owned())
        .unwrap_or_default();
    let modality = dataset
        .get(tags::MODALITY)
        .and_then(|e| e.to_str().ok())
        .map(|s| s.trim().to_owned())
        .unwrap_or_else(|| "UN".to_owned());

    let meta = FileMetaTableBuilder::new()
        .transfer_syntax(negotiated_ts.uid())
        .media_storage_sop_class_uid(affected_sop_class_uid)
        .media_storage_sop_instance_uid(affected_sop_instance_uid);
    let mut object = dataset
        .with_meta(meta)
        .map_err(|e| format!("failed to build File Meta Information: {e}"))?;

    let target_mode = (source_uid != target_uid
        && !matches!(target_ts.codec(), Codec::Dataset(_))
        && !has_nested_encapsulated_pixel_data(&object))
    .then(|| resolve_target_mode(target_ts))
    .flatten()
    .filter(|&mode| !is_lossy_target_mode(mode));

    match (stop, target_mode) {
        (PixelDataStreamStop::Native { vr, length }, Some(target_mode)) => {
            if let Some(plan) =
                prepare_wire_native_plan(&object, &source_uid, vr, length, target_mode)
            {
                stream_wire_native_to_file(
                    pdata,
                    &mut object,
                    negotiated_ts,
                    target_ts,
                    plan,
                    output_file,
                )?;
                return Ok((study_instance_uid, modality));
            }
            write_wire_passthrough_to_file(
                pdata,
                &object,
                PixelDataStreamStop::Native { vr, length },
                &[],
                output_file,
                output_path,
            )?;
            Ok((study_instance_uid, modality))
        }
        (PixelDataStreamStop::Encapsulated, Some(target_mode)) => {
            if is_wire_encapsulated_metadata_eligible(&object, target_mode) {
                if let Some(decoder) =
                    resolve_wire_encapsulated_decoder(&object, &source_uid, negotiated_ts)
                {
                    stream_wire_encapsulated_to_file(
                        pdata,
                        &mut object,
                        &source_uid,
                        negotiated_ts,
                        decoder,
                        target_ts,
                        target_mode,
                        output_file,
                        output_path,
                    )?;
                    return Ok((study_instance_uid, modality));
                }
            }
            write_wire_passthrough_to_file(
                pdata,
                &object,
                PixelDataStreamStop::Encapsulated,
                &[],
                output_file,
                output_path,
            )?;
            Ok((study_instance_uid, modality))
        }
        (stop, _) => {
            write_wire_passthrough_to_file(pdata, &object, stop, &[], output_file, output_path)?;
            Ok((study_instance_uid, modality))
        }
    }
}

struct WireNativePlan {
    vr: VR,
    length: u32,
    source_be: bool,
    target_mode: TargetMode,
    nr_frames: usize,
    frame_len: usize,
    total_native_bytes: u64,
    encode_meta: FrameEncodeMeta,
    batch_size: usize,
}

fn prepare_wire_native_plan(
    object: &DefaultDicomObject,
    source_uid: &str,
    vr: VR,
    length: u32,
    target_mode: TargetMode,
) -> Option<WireNativePlan> {
    let source_be = source_uid == EXPLICIT_VR_BIG_ENDIAN_UID;
    if matches!(target_mode, TargetMode::Native { big_endian } if vr == VR::OW && source_be != big_endian && !length.is_multiple_of(2))
    {
        return None;
    }

    let rows = object.get(tags::ROWS).and_then(|e| e.uint16().ok())?;
    let cols = object.get(tags::COLUMNS).and_then(|e| e.uint16().ok())?;
    let samples_per_pixel = object
        .get(tags::SAMPLES_PER_PIXEL)
        .and_then(|e| e.uint16().ok())
        .unwrap_or(1);
    let bits_allocated = object
        .get(tags::BITS_ALLOCATED)
        .and_then(|e| e.uint16().ok())?;
    let bits_stored = object
        .get(tags::BITS_STORED)
        .and_then(|e| e.uint16().ok())
        .unwrap_or(bits_allocated);
    let is_signed = object
        .get(tags::PIXEL_REPRESENTATION)
        .and_then(|e| e.uint16().ok())
        .unwrap_or(0)
        != 0;
    let nr_frames = number_of_frames(object);

    if (bits_allocated != 8 && bits_allocated != 16)
        || bits_stored == 0
        || bits_stored > bits_allocated
    {
        return None;
    }
    let bytes_per_sample = usize::from(bits_allocated / 8);
    let frame_len = usize::from(rows)
        .checked_mul(usize::from(cols))?
        .checked_mul(usize::from(samples_per_pixel))?
        .checked_mul(bytes_per_sample)?;
    if frame_len == 0 {
        return None;
    }
    if !matches!(target_mode, TargetMode::Native { .. })
        && (frame_len > MAX_SCP_TRANSCODE_FRAME_BYTES || nr_frames > MAX_SCP_BOT_FRAMES)
    {
        return None;
    }
    let total_native_bytes = (frame_len as u64).checked_mul(nr_frames as u64)?;
    if u64::from(length) < total_native_bytes {
        return None;
    }
    // Big-endian OW stores 16-bit words continuously across the entire PixelData value; if
    // individual 8-bit frames have odd byte length, a 16-bit word straddles two frames, so
    // frame-splitting before byte-swapping would corrupt samples. Fall back to wire passthrough.
    if source_be && vr == VR::OW && (!frame_len.is_multiple_of(2) || !length.is_multiple_of(2)) {
        return None;
    }
    // CharLS treats bits_per_sample <= 8 as 1 byte/sample; decline streaming JPEG-LS when
    // native pixels are stored in 16-bit allocated containers with <= 8 stored bits.
    if matches!(target_mode, TargetMode::JpegLs { .. }) && bits_allocated == 16 && bits_stored <= 8
    {
        return None;
    }
    if matches!(target_mode, TargetMode::JpegXlLossless)
        && jpeg_xl::validate_jpeg_xl_encode_params(
            rows,
            cols,
            samples_per_pixel,
            bits_allocated,
            bits_stored,
            is_signed,
        )
        .is_err()
    {
        return None;
    }

    let planar_configuration = object
        .get(tags::PLANAR_CONFIGURATION)
        .and_then(|e| e.uint16().ok())
        .unwrap_or(0);
    let encode_meta = FrameEncodeMeta {
        rows,
        cols,
        samples_per_pixel,
        bits_allocated,
        bits_stored,
        is_signed,
        planar: is_planar(object, samples_per_pixel),
        planar_configuration,
    };

    let target_can_parallel = !matches!(target_mode, TargetMode::Native { .. } | TargetMode::Htj2k);
    let batch_size = if nr_frames <= 1 || !target_can_parallel {
        1
    } else {
        let max_by_mem = (STREAM_BATCH_MEM_BUDGET / frame_len.max(1)).max(1);
        let max_by_cpu = rayon::current_num_threads().clamp(1, 8);
        max_by_mem.min(max_by_cpu).min(nr_frames)
    };

    Some(WireNativePlan {
        vr,
        length,
        source_be,
        target_mode,
        nr_frames,
        frame_len,
        total_native_bytes,
        encode_meta,
        batch_size,
    })
}

fn stream_wire_native_to_file<R: Read>(
    pdata: &mut R,
    object: &mut DefaultDicomObject,
    negotiated_ts: &TransferSyntax,
    target_ts: &TransferSyntax,
    plan: WireNativePlan,
    output_file: &mut File,
) -> Result<(), String> {
    if !matches!(plan.target_mode, TargetMode::Native { .. }) {
        remove_encapsulation_sidecar_attributes(object);
        if matches!(
            plan.target_mode,
            TargetMode::Htj2k | TargetMode::ClassicJpeg2000 { .. } | TargetMode::JpegXlLossless
        ) {
            set_planar_configuration_interleaved(object);
        }
    }
    object.meta_mut().set_transfer_syntax(target_ts);

    let mut bot_patch: Option<(u64, Vec<u32>)> = None;
    {
        let mut writer = BufWriter::new(&*output_file);
        object
            .write_all_with_pixel_data(
                &mut writer,
                |ds_writer, dataset_start| -> Result<(), TranscodeError> {
                    match plan.target_mode {
                        TargetMode::Native { big_endian } => {
                            let padded_len = plan.length.saturating_add(plan.length % 2);
                            ds_writer
                                .write_element_header(DataElementHeader::new(
                                    tags::PIXEL_DATA,
                                    plan.vr,
                                    Length::defined(padded_len),
                                ))
                                .map_err(|source| WriteError::Dataset { source })?;
                            let swap_words = plan.vr == VR::OW && plan.source_be != big_endian;
                            let mut remaining = u64::from(plan.length);
                            let mut chunk = vec![0u8; COPY_CHUNK_SIZE];
                            while remaining > 0 {
                                let to_read = (remaining as usize).min(chunk.len());
                                pdata
                                    .read_exact(&mut chunk[..to_read])
                                    .map_err(|source| dcmnorm_object::ReadError::Io {
                                        source,
                                        context: "reading incoming native PixelData",
                                    })?;
                                if swap_words {
                                    swap_u16_bytes(&mut chunk[..to_read]);
                                }
                                ds_writer
                                    .write_raw_bytes(&chunk[..to_read])
                                    .map_err(|source| WriteError::Dataset { source })?;
                                remaining -= to_read as u64;
                            }
                            if plan.length % 2 == 1 {
                                ds_writer
                                    .write_raw_bytes(&[0])
                                    .map_err(|source| WriteError::Dataset { source })?;
                            }
                        }
                        TargetMode::Htj2k
                        | TargetMode::ClassicJpeg2000 { .. }
                        | TargetMode::JpegLs { .. }
                        | TargetMode::JpegXlLossless => {
                            ds_writer
                                .write(DataToken::PixelSequenceStart)
                                .map_err(|source| WriteError::Dataset { source })?;
                            let bot_byte_len = plan
                                .nr_frames
                                .checked_mul(4)
                                .and_then(|n| u32::try_from(n).ok())
                                .ok_or_else(|| TranscodeError::EncodePixelData {
                                    uid: target_ts.uid().to_owned(),
                                    name: target_ts.name().to_owned(),
                                    message: "too many frames for Basic Offset Table".to_owned(),
                                })?;
                            ds_writer
                                .write(DataToken::ItemStart {
                                    len: Length::defined(bot_byte_len),
                                })
                                .map_err(|source| WriteError::Dataset { source })?;
                            let bot_file_offset = dataset_start + ds_writer.bytes_written();
                            if bot_byte_len > 0 {
                                write_zero_bytes(ds_writer, bot_byte_len as usize)?;
                            }
                            ds_writer
                                .write(DataToken::ItemEnd)
                                .map_err(|source| WriteError::Dataset { source })?;

                            let mut offsets = Vec::new();
                            offsets.try_reserve_exact(plan.nr_frames).map_err(|_| {
                                TranscodeError::EncodePixelData {
                                    uid: target_ts.uid().to_owned(),
                                    name: target_ts.name().to_owned(),
                                    message: "could not allocate Basic Offset Table buffer"
                                        .to_owned(),
                                }
                            })?;
                            let mut rel_pos = 0u64;
                            let mut htj2k_planes: Option<Vec<Vec<i32>>> = None;
                            let swap_in = plan.source_be && plan.vr == VR::OW;
                            let mut batch_start = 0;
                            while batch_start < plan.nr_frames {
                                let batch_end = (batch_start + plan.batch_size).min(plan.nr_frames);
                                let mut batch = Vec::with_capacity(batch_end - batch_start);
                                for _ in batch_start..batch_end {
                                    let mut buf = Vec::new();
                                    buf.try_reserve_exact(plan.frame_len).map_err(|_| {
                                        TranscodeError::DecodePixelData {
                                            uid: negotiated_ts.uid().to_owned(),
                                            name: negotiated_ts.name().to_owned(),
                                            message: format!(
                                                "could not allocate {} bytes for incoming native frame",
                                                plan.frame_len
                                            ),
                                        }
                                    })?;
                                    buf.resize(plan.frame_len, 0);
                                    pdata.read_exact(&mut buf).map_err(|source| {
                                        dcmnorm_object::ReadError::Io {
                                            source,
                                            context: "reading incoming native frame",
                                        }
                                    })?;
                                    if swap_in {
                                        swap_u16_bytes(&mut buf);
                                    }
                                    batch.push(buf);
                                }
                                let fragments: Vec<Vec<u8>> =
                                    if matches!(plan.target_mode, TargetMode::Htj2k) {
                                        let planes = match htj2k_planes.as_mut() {
                                            Some(planes) => planes,
                                            None => htj2k_planes.insert(allocate_htj2k_planes(
                                                &plan.encode_meta,
                                                target_ts,
                                            )?),
                                        };
                                        let mut frags = Vec::with_capacity(batch.len());
                                        for (i, native) in batch.into_iter().enumerate() {
                                            let frame_index = batch_start + i;
                                            let frag = encode_htj2k_frame(
                                                &native,
                                                plan.encode_meta.rows,
                                                plan.encode_meta.cols,
                                                plan.encode_meta.samples_per_pixel,
                                                plan.encode_meta.bits_allocated,
                                                plan.encode_meta.bits_stored,
                                                plan.encode_meta.is_signed,
                                                plan.encode_meta.planar,
                                                planes,
                                            )
                                            .map_err(|e| TranscodeError::EncodePixelData {
                                                uid: target_ts.uid().to_owned(),
                                                name: target_ts.name().to_owned(),
                                                message: format!(
                                                    "HTJ2K encode failed for frame {frame_index}: {e}"
                                                ),
                                            })?;
                                            frags.push(frag);
                                        }
                                        frags
                                    } else if batch.len() == 1 {
                                        vec![encode_one_frame(
                                            &batch[0],
                                            plan.target_mode,
                                            &plan.encode_meta,
                                            target_ts,
                                            batch_start,
                                        )?]
                                    } else {
                                        batch
                                            .into_par_iter()
                                            .enumerate()
                                            .map(|(i, native)| {
                                                encode_one_frame(
                                                    &native,
                                                    plan.target_mode,
                                                    &plan.encode_meta,
                                                    target_ts,
                                                    batch_start + i,
                                                )
                                            })
                                            .collect::<Result<Vec<_>, _>>()?
                                    };

                                for mut frag in fragments {
                                    if frag.len() % 2 == 1 {
                                        frag.push(0);
                                    }
                                    let offset = u32::try_from(rel_pos).map_err(|_| {
                                        TranscodeError::EncodePixelData {
                                            uid: target_ts.uid().to_owned(),
                                            name: target_ts.name().to_owned(),
                                            message:
                                                "encapsulated output exceeded 4 GiB Basic Offset Table limit"
                                                    .to_owned(),
                                        }
                                    })?;
                                    offsets.push(offset);
                                    let frag_len = u32::try_from(frag.len()).map_err(|_| {
                                        TranscodeError::EncodePixelData {
                                            uid: target_ts.uid().to_owned(),
                                            name: target_ts.name().to_owned(),
                                            message: "encoded frame fragment exceeds u32::MAX bytes"
                                                .to_owned(),
                                        }
                                    })?;
                                    ds_writer
                                        .write(DataToken::ItemStart {
                                            len: Length::defined(frag_len),
                                        })
                                        .map_err(|source| WriteError::Dataset { source })?;
                                    ds_writer
                                        .write_item_value(&frag)
                                        .map_err(|source| WriteError::Dataset { source })?;
                                    ds_writer
                                        .write(DataToken::ItemEnd)
                                        .map_err(|source| WriteError::Dataset { source })?;
                                    rel_pos += 8 + frag.len() as u64;
                                }
                                batch_start = batch_end;
                            }

                            // Drain any odd/trailing padding bytes at the end of native PixelData.
                            let extra = u64::from(plan.length) - plan.total_native_bytes;
                            if extra > 0 {
                                let copied =
                                    std::io::copy(&mut pdata.take(extra), &mut std::io::sink())
                                        .map_err(|source| dcmnorm_object::ReadError::Io {
                                            source,
                                            context: "draining trailing native PixelData padding",
                                        })?;
                                if copied != extra {
                                    return Err(dcmnorm_object::ReadError::Io {
                                        source: std::io::Error::new(
                                            std::io::ErrorKind::UnexpectedEof,
                                            format!(
                                                "truncated native PixelData padding: expected {extra} bytes, got {copied}"
                                            ),
                                        ),
                                        context: "draining trailing native PixelData padding",
                                    }
                                    .into());
                                }
                            }

                            ds_writer
                                .write(DataToken::SequenceEnd)
                                .map_err(|source| WriteError::Dataset { source })?;
                            if bot_byte_len > 0 {
                                bot_patch = Some((bot_file_offset, offsets));
                            }
                        }
                    }

                    // Read and write any trailing elements after (7FE0,0010) until EOF using the
                    // header's declared SpecificCharacterSet.
                    let trailing_cs = object.specific_character_set();
                    let trailing = InMemDicomObject::read_dataset_with_ts_cs(
                        &mut *pdata,
                        negotiated_ts,
                        trailing_cs,
                    )?;
                    trailing.write_elements_to_writer(ds_writer)?;
                    Ok(())
                },
            )
            .map_err(|e| e.to_string())?;
        writer
            .flush()
            .map_err(|e| format!("flushing output file: {e}"))?;
    }

    patch_bot(output_file, bot_patch)?;
    Ok(())
}

fn resolve_wire_encapsulated_decoder(
    object: &DefaultDicomObject,
    source_uid: &str,
    source_ts: &'static TransferSyntax,
) -> Option<EncapsulatedDecoder<'static>> {
    if is_mpeg_transfer_syntax(source_uid) {
        return None;
    }
    if is_jpeg_ls_transfer_syntax(source_uid) {
        return cfg!(feature = "jpeg-ls-codec").then_some(EncapsulatedDecoder::JpegLs);
    }
    if is_jpeg_xl_transfer_syntax(source_uid) {
        if !cfg!(feature = "jpeg-xl-codec") {
            return None;
        }
        let pixel_rep = object
            .get(tags::PIXEL_REPRESENTATION)
            .and_then(|e| e.uint16().ok())
            .unwrap_or(0);
        return (pixel_rep == 0).then_some(EncapsulatedDecoder::JpegXl);
    }
    let Codec::EncapsulatedPixelData(Some(reader), _) = source_ts.codec() else {
        return None;
    };
    Some(EncapsulatedDecoder::Registry(reader.as_ref()))
}

/// Sequential reader over an encapsulated PixelData item stream on a non-seekable `Read`.
struct WireEncapsulatedFrames<'a, R> {
    reader: &'a mut R,
    frame_offsets: Option<Vec<u64>>,
    nr_frames: usize,
    next_frame: usize,
    rel_pos: u64,
    pending_header: Option<(u16, u16, u32)>,
    finished: bool,
}

fn is_wire_encapsulated_metadata_eligible(
    object: &DefaultDicomObject,
    target_mode: TargetMode,
) -> bool {
    let Some(rows) = object.get(tags::ROWS).and_then(|e| e.uint16().ok()) else {
        return false;
    };
    let Some(cols) = object.get(tags::COLUMNS).and_then(|e| e.uint16().ok()) else {
        return false;
    };
    let samples_per_pixel = object
        .get(tags::SAMPLES_PER_PIXEL)
        .and_then(|e| e.uint16().ok())
        .unwrap_or(1);
    let Some(bits_allocated) = object
        .get(tags::BITS_ALLOCATED)
        .and_then(|e| e.uint16().ok())
    else {
        return false;
    };
    let bits_stored = object
        .get(tags::BITS_STORED)
        .and_then(|e| e.uint16().ok())
        .unwrap_or(bits_allocated);
    let is_signed = object
        .get(tags::PIXEL_REPRESENTATION)
        .and_then(|e| e.uint16().ok())
        .unwrap_or(0)
        != 0;
    if (bits_allocated != 8 && bits_allocated != 16)
        || bits_stored == 0
        || bits_stored > bits_allocated
    {
        return false;
    }
    let Some(frame_len) = usize::from(rows)
        .checked_mul(usize::from(cols))
        .and_then(|n| n.checked_mul(usize::from(samples_per_pixel)))
        .and_then(|n| n.checked_mul(usize::from(bits_allocated / 8)))
        .filter(|&n| n > 0)
    else {
        return false;
    };
    let nr_frames = number_of_frames(object);
    if !matches!(target_mode, TargetMode::Native { .. }) && nr_frames > MAX_SCP_BOT_FRAMES {
        return false;
    }
    let Some(total_native_bytes) = (frame_len as u64).checked_mul(nr_frames as u64) else {
        return false;
    };
    if matches!(target_mode, TargetMode::Native { .. })
        && total_native_bytes + (total_native_bytes % 2) > u64::from(u32::MAX)
    {
        return false;
    }
    if matches!(target_mode, TargetMode::JpegLs { .. }) && bits_allocated == 16 && bits_stored <= 8
    {
        return false;
    }
    if matches!(target_mode, TargetMode::JpegXlLossless)
        && jpeg_xl::validate_jpeg_xl_encode_params(
            rows,
            cols,
            samples_per_pixel,
            bits_allocated,
            bits_stored,
            is_signed,
        )
        .is_err()
    {
        return false;
    }
    true
}

enum WireEncapsulatedOpen<'a, R: Read> {
    Frames(WireEncapsulatedFrames<'a, R>),
    /// Multi-frame encapsulated stream with an empty BOT and no Extended Offset Table: frame
    /// boundaries cannot be established safely without buffering the stream (JPEG-family frames
    /// may span multiple fragments), so fall back to wire passthrough preserving the BOT item
    /// bytes already read from `pdata`.
    PassThrough {
        bot_item_bytes: Vec<u8>,
    },
}

impl<'a, R: Read> WireEncapsulatedFrames<'a, R> {
    fn open(
        reader: &'a mut R,
        object: &DefaultDicomObject,
        nr_frames: usize,
    ) -> Result<WireEncapsulatedOpen<'a, R>, String> {
        // First item is always the Basic Offset Table.
        let mut bot_hdr = [0u8; 8];
        reader
            .read_exact(&mut bot_hdr)
            .map_err(|e| format!("reading Basic Offset Table header: {e}"))?;
        let g = u16::from_le_bytes([bot_hdr[0], bot_hdr[1]]);
        let e = u16::from_le_bytes([bot_hdr[2], bot_hdr[3]]);
        let bot_len = u32::from_le_bytes([bot_hdr[4], bot_hdr[5], bot_hdr[6], bot_hdr[7]]);
        if (g, e) != (0xFFFE, 0xE000) || bot_len == u32::MAX {
            return Err(
                "invalid Basic Offset Table item header in encapsulated PixelData".to_owned(),
            );
        }
        if bot_len % 4 != 0 {
            return Err(format!("invalid Basic Offset Table length {bot_len}"));
        }

        let extended_offsets = object
            .get(tags::EXTENDED_OFFSET_TABLE)
            .and_then(|element| element.to_bytes().ok())
            .filter(|bytes| nr_frames.checked_mul(8) == Some(bytes.len()))
            .map(|bytes| {
                bytes
                    .chunks_exact(8)
                    .map(|chunk| u64::from_le_bytes(chunk.try_into().expect("8-byte chunk")))
                    .collect::<Vec<u64>>()
            });

        // Validate declared BOT length against NumberOfFrames and MAX_SCP_BOT_BYTES *before*
        // buffering any BOT bytes in RAM. Ineligible or oversized tables fall back to streaming
        // wire passthrough with only the 8-byte header already consumed.
        let bot_matches_frames = bot_len > 0 && nr_frames.checked_mul(4) == Some(bot_len as usize);
        if bot_len > MAX_SCP_BOT_BYTES
            || (nr_frames > 1 && extended_offsets.is_none() && !bot_matches_frames)
        {
            return Ok(WireEncapsulatedOpen::PassThrough {
                bot_item_bytes: bot_hdr.to_vec(),
            });
        }

        let mut bot = Vec::new();
        if bot_len > 0 {
            let mut raw_bot = Vec::new();
            raw_bot.try_reserve_exact(bot_len as usize).map_err(|_| {
                format!("could not allocate {bot_len} bytes for Basic Offset Table")
            })?;
            reader
                .take(u64::from(bot_len))
                .read_to_end(&mut raw_bot)
                .map_err(|err| format!("reading Basic Offset Table: {err}"))?;
            if raw_bot.len() != bot_len as usize {
                return Err("truncated Basic Offset Table".to_owned());
            }
            if extended_offsets.is_none() && bot_matches_frames {
                bot.try_reserve_exact(raw_bot.len() / 4)
                    .map_err(|_| "could not allocate Basic Offset Table offsets".to_owned())?;
                for chunk in raw_bot.chunks_exact(4) {
                    bot.push(u32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]));
                }
            }
        }

        let frame_offsets = match extended_offsets {
            Some(offsets) => Some(offsets),
            None if bot_matches_frames => Some(bot.into_iter().map(u64::from).collect()),
            None => None,
        };
        if let Some(ref offsets) = frame_offsets {
            if !offsets.is_empty() && (offsets[0] != 0 || !offsets.windows(2).all(|w| w[1] > w[0]))
            {
                return Err(
                    "invalid encapsulated frame offset table: offsets must start at 0 and strictly increase"
                        .to_owned(),
                );
            }
        }

        Ok(WireEncapsulatedOpen::Frames(Self {
            reader,
            frame_offsets,
            nr_frames,
            next_frame: 0,
            rel_pos: 0,
            pending_header: None,
            finished: false,
        }))
    }

    fn next_item_header(&mut self) -> Result<(u16, u16, u32), String> {
        if let Some(hdr) = self.pending_header.take() {
            return Ok(hdr);
        }
        read_item_header_le(self.reader)
    }

    fn read_next_frame(&mut self) -> Result<Vec<u8>, String> {
        if self.next_frame >= self.nr_frames {
            return Err("requested frame past NumberOfFrames".to_owned());
        }
        let frame_index = self.next_frame;
        self.next_frame += 1;

        let next_frame_offset = self
            .frame_offsets
            .as_ref()
            .and_then(|offsets| offsets.get(frame_index + 1).copied());
        let read_until_delimiter =
            self.nr_frames == 1 || (self.frame_offsets.is_some() && next_frame_offset.is_none());

        let mut codestream = Vec::new();
        loop {
            let (g, e, len) = self.next_item_header()?;
            if (g, e) == (0xFFFE, 0xE0DD) {
                if len != 0 {
                    return Err(format!(
                        "SequenceDelimiter in encapsulated PixelData has nonzero length {len}"
                    ));
                }
                self.finished = true;
                if codestream.is_empty() {
                    return Err(format!(
                        "unexpected SequenceDelimiter before frame {frame_index}"
                    ));
                }
                break;
            }
            if (g, e) != (0xFFFE, 0xE000) || len == u32::MAX {
                return Err(format!(
                    "unexpected item header ({g:04X},{e:04X}) len={len} in encapsulated PixelData"
                ));
            }
            let new_len = codestream
                .len()
                .checked_add(len as usize)
                .ok_or_else(|| format!("frame {frame_index} encoded byte length overflow"))?;
            if new_len > MAX_SCP_ENCODED_FRAME_BYTES {
                return Err(format!(
                    "encapsulated frame {frame_index} exceeds maximum encoded byte limit ({MAX_SCP_ENCODED_FRAME_BYTES} bytes)"
                ));
            }
            let start = codestream.len();
            codestream.try_reserve(len as usize).map_err(|_| {
                format!("could not allocate {new_len} bytes for encapsulated frame {frame_index}")
            })?;
            let read = self
                .reader
                .take(u64::from(len))
                .read_to_end(&mut codestream)
                .map_err(|err| {
                    format!("reading encapsulated fragment for frame {frame_index}: {err}")
                })?;
            if read != len as usize {
                return Err(format!(
                    "truncated encapsulated fragment for frame {frame_index}: expected {len}, got {}",
                    codestream.len() - start
                ));
            }
            self.rel_pos += 8 + u64::from(len);

            if !read_until_delimiter && next_frame_offset.is_none() {
                // 1 fragment per frame (no offset table, multi-frame)
                break;
            }
            if let Some(boundary) = next_frame_offset {
                if self.rel_pos == boundary {
                    break;
                }
                if self.rel_pos > boundary {
                    return Err(format!(
                        "frame {} offset {boundary} does not align with an encapsulated item boundary (crossed to {})",
                        frame_index + 1,
                        self.rel_pos
                    ));
                }
            }
        }
        Ok(codestream)
    }

    fn finish_sequence(mut self) -> Result<&'a mut R, String> {
        if self.finished {
            return Ok(self.reader);
        }
        let (g, e, len) = self.next_item_header()?;
        if (g, e) == (0xFFFE, 0xE0DD) && len == 0 {
            return Ok(self.reader);
        }
        Err(format!(
            "expected SequenceDelimiter (len=0) at end of encapsulated PixelData, got ({g:04X},{e:04X}) len={len}"
        ))
    }
}

fn read_item_header_le<R: Read>(reader: &mut R) -> Result<(u16, u16, u32), String> {
    let mut hdr = [0u8; 8];
    reader
        .read_exact(&mut hdr)
        .map_err(|e| format!("reading encapsulated item header: {e}"))?;
    Ok((
        u16::from_le_bytes([hdr[0], hdr[1]]),
        u16::from_le_bytes([hdr[2], hdr[3]]),
        u32::from_le_bytes([hdr[4], hdr[5], hdr[6], hdr[7]]),
    ))
}

#[allow(clippy::too_many_arguments)]
fn stream_wire_encapsulated_to_file<R: Read>(
    pdata: &mut R,
    object: &mut DefaultDicomObject,
    source_uid: &str,
    source_ts: &'static TransferSyntax,
    decoder: EncapsulatedDecoder<'static>,
    target_ts: &'static TransferSyntax,
    target_mode: TargetMode,
    output_file: &mut File,
    output_path: &Path,
) -> Result<(), String> {
    let nr_frames = number_of_frames(object);
    let mut frames_in = match WireEncapsulatedFrames::open(pdata, object, nr_frames)? {
        WireEncapsulatedOpen::Frames(frames) => frames,
        WireEncapsulatedOpen::PassThrough { bot_item_bytes } => {
            return write_wire_passthrough_to_file(
                pdata,
                object,
                PixelDataStreamStop::Encapsulated,
                &bot_item_bytes,
                output_file,
                output_path,
            );
        }
    };
    let frame0 = frames_in.read_next_frame()?;

    let j2k_uses_mct = if is_jpeg2000_transfer_syntax(source_uid) {
        apply_j2k_probe_corrections(object, &frame0, source_ts).map_err(|e| e.to_string())?
    } else {
        None
    };

    let rows = object
        .get(tags::ROWS)
        .and_then(|e| e.uint16().ok())
        .ok_or_else(|| "missing Rows attribute".to_owned())?;
    let cols = object
        .get(tags::COLUMNS)
        .and_then(|e| e.uint16().ok())
        .ok_or_else(|| "missing Columns attribute".to_owned())?;
    let samples_per_pixel = object
        .get(tags::SAMPLES_PER_PIXEL)
        .and_then(|e| e.uint16().ok())
        .unwrap_or(1);
    let bits_allocated = object
        .get(tags::BITS_ALLOCATED)
        .and_then(|e| e.uint16().ok())
        .ok_or_else(|| "missing BitsAllocated attribute".to_owned())?;
    let bits_stored = object
        .get(tags::BITS_STORED)
        .and_then(|e| e.uint16().ok())
        .unwrap_or(bits_allocated);
    let is_signed = object
        .get(tags::PIXEL_REPRESENTATION)
        .and_then(|e| e.uint16().ok())
        .unwrap_or(0)
        != 0;
    if (bits_allocated != 8 && bits_allocated != 16)
        || bits_stored == 0
        || bits_stored > bits_allocated
    {
        return Err(format!(
            "unsupported BitsAllocated/BitsStored ({bits_allocated}/{bits_stored}) for streaming transcode"
        ));
    }
    let frame_len = usize::from(rows)
        .checked_mul(usize::from(cols))
        .and_then(|n| n.checked_mul(usize::from(samples_per_pixel)))
        .and_then(|n| n.checked_mul(usize::from(bits_allocated / 8)))
        .filter(|&n| n > 0)
        .ok_or_else(|| "invalid frame geometry".to_owned())?;
    let total_native_bytes = (frame_len as u64)
        .checked_mul(nr_frames as u64)
        .ok_or_else(|| "frame byte length overflow".to_owned())?;
    if matches!(target_mode, TargetMode::Native { .. })
        && total_native_bytes + (total_native_bytes % 2) > u64::from(u32::MAX)
    {
        return Err("decoded native PixelData exceeds 4 GiB defined length".to_owned());
    }
    if matches!(target_mode, TargetMode::JpegLs { .. }) && bits_allocated == 16 && bits_stored <= 8
    {
        return Err(
            "JPEG-LS streaming does not support 16-bit allocated pixels with <= 8 bits stored"
                .to_owned(),
        );
    }
    if matches!(target_mode, TargetMode::JpegXlLossless) {
        jpeg_xl::validate_jpeg_xl_encode_params(
            rows,
            cols,
            samples_per_pixel,
            bits_allocated,
            bits_stored,
            is_signed,
        )?;
    }

    let decode_meta = FrameDecodeMeta {
        transfer_syntax_uid: source_uid.to_owned(),
        rows,
        cols,
        samples_per_pixel,
        bits_allocated,
        bits_stored,
        photometric_interpretation: object
            .get(tags::PHOTOMETRIC_INTERPRETATION)
            .and_then(|e| e.to_str().ok())
            .map(|s| s.trim().to_owned()),
    };
    let encode_meta = FrameEncodeMeta {
        rows,
        cols,
        samples_per_pixel,
        bits_allocated,
        bits_stored,
        is_signed,
        planar: false,
        planar_configuration: 0,
    };

    normalize_decoded_pixel_data_attributes(object, source_uid, j2k_uses_mct);
    remove_encapsulation_sidecar_attributes(object);
    if matches!(
        target_mode,
        TargetMode::Htj2k | TargetMode::ClassicJpeg2000 { .. } | TargetMode::JpegXlLossless
    ) {
        set_planar_configuration_interleaved(object);
    }
    object.meta_mut().set_transfer_syntax(target_ts);

    let source_mode = SourceMode::Encapsulated {
        fragments: Vec::new(),
        frame_ranges: Vec::new(),
        decoder,
    };
    let batch_size =
        choose_batch_size(source_uid, &source_mode, &target_mode, nr_frames, frame_len);

    let mut frame0_slot = Some(frame0);
    let mut bot_patch: Option<(u64, Vec<u32>)> = None;
    {
        let mut writer = BufWriter::new(&*output_file);
        object
            .write_all_with_pixel_data(
                &mut writer,
                |ds_writer, dataset_start| -> Result<(), TranscodeError> {
                    match target_mode {
                        TargetMode::Native { big_endian } => {
                            let out_vr = native_pixel_vr(bits_allocated);
                            let padded_len = (total_native_bytes + (total_native_bytes % 2)) as u32;
                            ds_writer
                                .write_element_header(DataElementHeader::new(
                                    tags::PIXEL_DATA,
                                    out_vr,
                                    Length::defined(padded_len),
                                ))
                                .map_err(|source| WriteError::Dataset { source })?;
                            let swap_out = big_endian && out_vr == VR::OW;
                            let mut batch_start = 0;
                            while batch_start < nr_frames {
                                let max_end = (batch_start + batch_size).min(nr_frames);
                                let mut inputs = Vec::with_capacity(max_end - batch_start);
                                let mut batch_mem = 0u64;
                                for _ in batch_start..max_end {
                                    let frame = match frame0_slot.take() {
                                        Some(f) => f,
                                        None => frames_in.read_next_frame().map_err(|message| {
                                            TranscodeError::DecodePixelData {
                                                uid: source_ts.uid().to_owned(),
                                                name: source_ts.name().to_owned(),
                                                message,
                                            }
                                        })?,
                                    };
                                    batch_mem = batch_mem
                                        .saturating_add(frame.len() as u64)
                                        .saturating_add(frame_len as u64);
                                    inputs.push(frame);
                                    if batch_mem >= STREAM_BATCH_MEM_BUDGET as u64 {
                                        break;
                                    }
                                }
                                let count = inputs.len();
                                let outputs = transform_batch_to_native(
                                    inputs,
                                    &source_mode,
                                    &decode_meta,
                                    source_ts,
                                    frame_len,
                                    swap_out,
                                    batch_start,
                                )?;
                                for frame in outputs {
                                    ds_writer
                                        .write_raw_bytes(&frame)
                                        .map_err(|source| WriteError::Dataset { source })?;
                                }
                                batch_start += count;
                            }
                            if total_native_bytes % 2 == 1 {
                                ds_writer
                                    .write_raw_bytes(&[0])
                                    .map_err(|source| WriteError::Dataset { source })?;
                            }
                        }
                        TargetMode::Htj2k
                        | TargetMode::ClassicJpeg2000 { .. }
                        | TargetMode::JpegLs { .. }
                        | TargetMode::JpegXlLossless => {
                            ds_writer
                                .write(DataToken::PixelSequenceStart)
                                .map_err(|source| WriteError::Dataset { source })?;
                            let bot_byte_len = nr_frames
                                .checked_mul(4)
                                .and_then(|n| u32::try_from(n).ok())
                                .ok_or_else(|| TranscodeError::EncodePixelData {
                                    uid: target_ts.uid().to_owned(),
                                    name: target_ts.name().to_owned(),
                                    message: "too many frames for Basic Offset Table".to_owned(),
                                })?;
                            ds_writer
                                .write(DataToken::ItemStart {
                                    len: Length::defined(bot_byte_len),
                                })
                                .map_err(|source| WriteError::Dataset { source })?;
                            let bot_file_offset = dataset_start + ds_writer.bytes_written();
                            if bot_byte_len > 0 {
                                write_zero_bytes(ds_writer, bot_byte_len as usize)?;
                            }
                            ds_writer
                                .write(DataToken::ItemEnd)
                                .map_err(|source| WriteError::Dataset { source })?;

                            let mut offsets = Vec::new();
                            offsets.try_reserve_exact(nr_frames).map_err(|_| {
                                TranscodeError::EncodePixelData {
                                    uid: target_ts.uid().to_owned(),
                                    name: target_ts.name().to_owned(),
                                    message: "could not allocate Basic Offset Table buffer"
                                        .to_owned(),
                                }
                            })?;
                            let mut rel_pos = 0u64;
                            let mut htj2k_planes: Option<Vec<Vec<i32>>> = None;
                            let mut write_frag = |mut frag: Vec<u8>| -> Result<(), TranscodeError> {
                                if frag.len() % 2 == 1 {
                                    frag.push(0);
                                }
                                let offset = u32::try_from(rel_pos).map_err(|_| {
                                    TranscodeError::EncodePixelData {
                                        uid: target_ts.uid().to_owned(),
                                        name: target_ts.name().to_owned(),
                                        message:
                                            "encapsulated output exceeded 4 GiB Basic Offset Table limit"
                                                .to_owned(),
                                    }
                                })?;
                                offsets.push(offset);
                                let frag_len = u32::try_from(frag.len()).map_err(|_| {
                                    TranscodeError::EncodePixelData {
                                        uid: target_ts.uid().to_owned(),
                                        name: target_ts.name().to_owned(),
                                        message: "encoded frame fragment exceeds u32::MAX bytes"
                                            .to_owned(),
                                    }
                                })?;
                                ds_writer
                                    .write(DataToken::ItemStart {
                                        len: Length::defined(frag_len),
                                    })
                                    .map_err(|source| WriteError::Dataset { source })?;
                                ds_writer
                                    .write_item_value(&frag)
                                    .map_err(|source| WriteError::Dataset { source })?;
                                ds_writer
                                    .write(DataToken::ItemEnd)
                                    .map_err(|source| WriteError::Dataset { source })?;
                                rel_pos += 8 + frag.len() as u64;
                                Ok(())
                            };

                            let mut batch_start = 0;
                            while batch_start < nr_frames {
                                let max_end = (batch_start + batch_size).min(nr_frames);
                                let mut inputs = Vec::with_capacity(max_end - batch_start);
                                let mut batch_mem = 0u64;
                                for _ in batch_start..max_end {
                                    let frame = match frame0_slot.take() {
                                        Some(f) => f,
                                        None => frames_in.read_next_frame().map_err(|message| {
                                            TranscodeError::DecodePixelData {
                                                uid: source_ts.uid().to_owned(),
                                                name: source_ts.name().to_owned(),
                                                message,
                                            }
                                        })?,
                                    };
                                    batch_mem = batch_mem
                                        .saturating_add(frame.len() as u64)
                                        .saturating_add(frame_len as u64);
                                    inputs.push(frame);
                                    if batch_mem >= STREAM_BATCH_MEM_BUDGET as u64 {
                                        break;
                                    }
                                }
                                let count = inputs.len();
                                if matches!(target_mode, TargetMode::Htj2k) {
                                    let decoded = decode_batch(
                                        inputs,
                                        &source_mode,
                                        &decode_meta,
                                        source_ts,
                                        frame_len,
                                        batch_start,
                                    )?;
                                    let planes = match htj2k_planes.as_mut() {
                                        Some(planes) => planes,
                                        None => htj2k_planes.insert(allocate_htj2k_planes(
                                            &encode_meta,
                                            target_ts,
                                        )?),
                                    };
                                    for (offset, native) in decoded.into_iter().enumerate() {
                                        let frame_index = batch_start + offset;
                                        let frag = encode_htj2k_frame(
                                            &native,
                                            encode_meta.rows,
                                            encode_meta.cols,
                                            encode_meta.samples_per_pixel,
                                            encode_meta.bits_allocated,
                                            encode_meta.bits_stored,
                                            encode_meta.is_signed,
                                            encode_meta.planar,
                                            planes,
                                        )
                                        .map_err(|e| TranscodeError::EncodePixelData {
                                            uid: target_ts.uid().to_owned(),
                                            name: target_ts.name().to_owned(),
                                            message: format!(
                                                "HTJ2K encode failed for frame {frame_index}: {e}"
                                            ),
                                        })?;
                                        drop(native);
                                        write_frag(frag)?;
                                    }
                                } else {
                                    let fragments = transform_batch_to_encapsulated(
                                        inputs,
                                        &source_mode,
                                        &decode_meta,
                                        source_ts,
                                        target_mode,
                                        &encode_meta,
                                        target_ts,
                                        frame_len,
                                        batch_start,
                                    )?;
                                    for frag in fragments {
                                        write_frag(frag)?;
                                    }
                                }
                                batch_start += count;
                            }

                            ds_writer
                                .write(DataToken::SequenceEnd)
                                .map_err(|source| WriteError::Dataset { source })?;
                            if bot_byte_len > 0 {
                                bot_patch = Some((bot_file_offset, offsets));
                            }
                        }
                    }

                    let pdata = frames_in.finish_sequence().map_err(|message| {
                        TranscodeError::DecodePixelData {
                            uid: source_ts.uid().to_owned(),
                            name: source_ts.name().to_owned(),
                            message,
                        }
                    })?;
                    let trailing_cs = object.specific_character_set();
                    let trailing =
                        InMemDicomObject::read_dataset_with_ts_cs(pdata, source_ts, trailing_cs)?;
                    trailing.write_elements_to_writer(ds_writer)?;
                    Ok(())
                },
            )
            .map_err(|e| e.to_string())?;
        writer
            .flush()
            .map_err(|e| format!("flushing output file: {e}"))?;
    }

    patch_bot(output_file, bot_patch)?;
    Ok(())
}

fn write_wire_passthrough_to_file<R: Read>(
    pdata: &mut R,
    object: &DefaultDicomObject,
    stop: PixelDataStreamStop,
    already_read_prefix: &[u8],
    output_file: &mut File,
    output_path: &Path,
) -> Result<(), String> {
    let needs_structural_validation = !matches!(stop, PixelDataStreamStop::Absent);
    {
        let mut writer = BufWriter::new(&*output_file);
        match stop {
            PixelDataStreamStop::Absent => {
                object
                    .write_all(&mut writer)
                    .map_err(|e| format!("writing DICOM file: {e}"))?;
            }
            PixelDataStreamStop::Native { vr, length } => {
                object
                    .write_all_with_pixel_data(
                        &mut writer,
                        |ds_writer, _| -> Result<(), WriteError> {
                            ds_writer
                                .write_element_header(DataElementHeader::new(
                                    tags::PIXEL_DATA,
                                    vr,
                                    Length::defined(length),
                                ))
                                .map_err(|source| WriteError::Dataset { source })?;
                            let mut buf = vec![0u8; 256 * 1024];
                            let mut copied = 0u64;
                            loop {
                                let n = pdata.read(&mut buf).map_err(|source| WriteError::Io {
                                    source,
                                    context: "copying incoming PixelData stream",
                                })?;
                                if n == 0 {
                                    break;
                                }
                                ds_writer
                                    .write_raw_bytes(&buf[..n])
                                    .map_err(|source| WriteError::Dataset { source })?;
                                copied += n as u64;
                            }
                            if copied < u64::from(length) {
                                return Err(WriteError::Io {
                                    source: std::io::Error::new(
                                        std::io::ErrorKind::UnexpectedEof,
                                        format!(
                                            "truncated native PixelData: declared {length} bytes, received {copied}"
                                        ),
                                    ),
                                    context: "validating incoming PixelData length",
                                });
                            }
                            Ok(())
                        },
                    )
                    .map_err(|e| format!("writing passthrough DICOM file: {e}"))?;
            }
            PixelDataStreamStop::Encapsulated => {
                object
                    .write_all_with_pixel_data(
                        &mut writer,
                        |ds_writer, _| -> Result<(), WriteError> {
                            ds_writer
                                .write(DataToken::PixelSequenceStart)
                                .map_err(|source| WriteError::Dataset { source })?;
                            if !already_read_prefix.is_empty() {
                                ds_writer
                                    .write_raw_bytes(already_read_prefix)
                                    .map_err(|source| WriteError::Dataset { source })?;
                            }
                            let mut buf = vec![0u8; 256 * 1024];
                            loop {
                                let n = pdata.read(&mut buf).map_err(|source| WriteError::Io {
                                    source,
                                    context: "copying incoming encapsulated PixelData stream",
                                })?;
                                if n == 0 {
                                    break;
                                }
                                ds_writer
                                    .write_raw_bytes(&buf[..n])
                                    .map_err(|source| WriteError::Dataset { source })?;
                            }
                            Ok(())
                        },
                    )
                    .map_err(|e| format!("writing passthrough DICOM file: {e}"))?;
            }
        }
        writer
            .flush()
            .map_err(|e| format!("flushing output file: {e}"))?;
    }

    // Validate passthrough structure (native PixelData + trailing elements, or encapsulated item
    // headers + Sequence Delimitation Item (FFFE,E0DD) + trailing elements) in O(1) RAM before
    // publishing the file.
    if needs_structural_validation {
        OpenFileOptions::new()
            .read_preamble(ReadPreamble::Always)
            .open_file_deferring_pixel_data(output_path)
            .map_err(|e| format!("validating passthrough dataset: {e}"))?;
    }
    Ok(())
}

fn patch_bot(output_file: &mut File, bot_patch: Option<(u64, Vec<u32>)>) -> Result<(), String> {
    if let Some((bot_offset, offsets)) = bot_patch {
        let mut bot_bytes = Vec::with_capacity(offsets.len() * 4);
        for offset in offsets {
            bot_bytes.extend_from_slice(&offset.to_le_bytes());
        }
        output_file
            .seek(SeekFrom::Start(bot_offset))
            .map_err(|e| format!("seeking to patch Basic Offset Table: {e}"))?;
        output_file
            .write_all(&bot_bytes)
            .map_err(|e| format!("patching Basic Offset Table: {e}"))?;
        output_file
            .flush()
            .map_err(|e| format!("flushing Basic Offset Table: {e}"))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::try_transcode_dicom_file_streaming;
    use crate::dicom_io::{
        read_dicom_file, transcode_dcmnorm_object_owned, transcode_dicom_file, write_dicom_bytes,
        write_dicom_file,
    };
    use dcmnorm_core::header::Tag;
    use dcmnorm_core::value::{DataSetSequence, Value};
    use dcmnorm_core::{DataElement, PrimitiveValue, VR};
    use dcmnorm_dictionary::{tags, uids};
    use dcmnorm_object::{DefaultDicomObject, FileMetaTableBuilder, InMemDicomObject};
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEST_COUNTER: AtomicU64 = AtomicU64::new(0);

    fn temp_path(label: &str) -> PathBuf {
        let id = TEST_COUNTER.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "dcmnorm-stream-test-{}-{}-{id}.dcm",
            std::process::id(),
            label
        ))
    }

    fn build_multiframe_u16_object(
        transfer_syntax: &str,
        vr: VR,
        rows: u16,
        cols: u16,
        frames: usize,
        bits_stored: u16,
        with_icon_and_trailing: bool,
    ) -> DefaultDicomObject {
        let meta = FileMetaTableBuilder::new()
            .transfer_syntax(transfer_syntax)
            .media_storage_sop_class_uid("1.2.840.10008.5.1.4.1.1.13.1.3")
            .media_storage_sop_instance_uid("2.25.9999999999")
            .build()
            .unwrap();
        let mut obj = DefaultDicomObject::new_empty_with_meta(meta);
        let mask = (1u16 << bits_stored) - 1;
        let pixels_per_frame = usize::from(rows) * usize::from(cols);
        let mut raw_le = Vec::with_capacity(pixels_per_frame * frames * 2);
        let mut raw_words = Vec::with_capacity(pixels_per_frame * frames);
        for f in 0..frames {
            for p in 0..pixels_per_frame {
                let sample = ((f * 97 + p * 13) as u16) & mask;
                raw_words.push(sample);
                raw_le.extend_from_slice(&sample.to_le_bytes());
            }
        }

        for elem in [
            DataElement::new(
                tags::MODALITY,
                VR::CS,
                PrimitiveValue::from("MG".to_owned()),
            ),
            DataElement::new(tags::ROWS, VR::US, PrimitiveValue::from(rows)),
            DataElement::new(tags::COLUMNS, VR::US, PrimitiveValue::from(cols)),
            DataElement::new(tags::SAMPLES_PER_PIXEL, VR::US, PrimitiveValue::from(1u16)),
            DataElement::new(tags::BITS_ALLOCATED, VR::US, PrimitiveValue::from(16u16)),
            DataElement::new(tags::BITS_STORED, VR::US, PrimitiveValue::from(bits_stored)),
            DataElement::new(
                tags::HIGH_BIT,
                VR::US,
                PrimitiveValue::from(bits_stored - 1),
            ),
            DataElement::new(
                tags::PIXEL_REPRESENTATION,
                VR::US,
                PrimitiveValue::from(0u16),
            ),
            DataElement::new(
                tags::PHOTOMETRIC_INTERPRETATION,
                VR::CS,
                PrimitiveValue::from("MONOCHROME2".to_owned()),
            ),
            DataElement::new(
                tags::NUMBER_OF_FRAMES,
                VR::IS,
                PrimitiveValue::from(frames.to_string()),
            ),
        ] {
            obj.put(elem);
        }

        if with_icon_and_trailing {
            // Nested IconImageSequence (0088,0200) carrying its own tiny (7FE0,0010) PixelData
            let mut icon = InMemDicomObject::new_empty();
            icon.put(DataElement::new(
                tags::ROWS,
                VR::US,
                PrimitiveValue::from(2u16),
            ));
            icon.put(DataElement::new(
                tags::COLUMNS,
                VR::US,
                PrimitiveValue::from(2u16),
            ));
            icon.put(DataElement::new(
                tags::PIXEL_DATA,
                VR::OB,
                PrimitiveValue::from(vec![10u8, 20, 30, 40]),
            ));
            obj.put(DataElement::new(
                Tag(0x0088, 0x0200),
                VR::SQ,
                Value::Sequence(DataSetSequence::from(vec![icon])),
            ));
            // Trailing element after PixelData: DataSetTrailingPadding (FFFC,FFFC)
            obj.put(DataElement::new(
                Tag(0xFFFC, 0xFFFC),
                VR::OB,
                PrimitiveValue::from(vec![0xAAu8, 0xBB, 0xCC, 0xDD]),
            ));
        }

        let pixel_val = if vr == VR::OB {
            PrimitiveValue::from(raw_le)
        } else {
            PrimitiveValue::U16(raw_words.into())
        };
        obj.put(DataElement::new(tags::PIXEL_DATA, vr, pixel_val));
        obj
    }

    fn assert_streaming_matches_in_memory(
        input_obj: &DefaultDicomObject,
        target_ts: &str,
        label: &str,
    ) {
        let input_path = temp_path(&format!("{label}-in"));
        let stream_out = temp_path(&format!("{label}-stream"));

        let mut input_clone = input_obj.clone();
        write_dicom_file(&mut input_clone, &input_path).unwrap();

        let used_streaming =
            try_transcode_dicom_file_streaming(&input_path, &stream_out, target_ts)
                .expect("streaming transcode should succeed");
        assert!(
            used_streaming,
            "expected {label} -> {target_ts} to use streaming path"
        );

        let mut expected_obj = transcode_dcmnorm_object_owned(input_obj.clone(), target_ts)
            .expect("in-memory transcode");
        let expected_bytes = write_dicom_bytes(&mut expected_obj).unwrap();
        let actual_bytes = std::fs::read(&stream_out).unwrap();

        assert_eq!(
            actual_bytes, expected_bytes,
            "streaming output differed from in-memory output for {label} -> {target_ts}"
        );

        let _ = std::fs::remove_file(&input_path);
        let _ = std::fs::remove_file(&stream_out);
    }

    #[test]
    fn streaming_matches_in_memory_for_multiframe_native_to_compressed_and_back() {
        // Mirrors the 638-718 MiB uncompressed 10-bit/16-bit OB tomos and Clunie OW tomos,
        // plus nested IconImageSequence PixelData and trailing DataSetTrailingPadding.
        let native_ob = build_multiframe_u16_object(
            uids::EXPLICIT_VR_LITTLE_ENDIAN,
            VR::OB,
            24,
            32,
            5,
            10,
            true,
        );
        let native_ow = build_multiframe_u16_object(
            uids::EXPLICIT_VR_LITTLE_ENDIAN,
            VR::OW,
            24,
            32,
            5,
            10,
            true,
        );
        let native_be =
            build_multiframe_u16_object("1.2.840.10008.1.2.2", VR::OW, 24, 32, 5, 10, true);

        let targets: &[&str] = &[
            uids::EXPLICIT_VR_LITTLE_ENDIAN,
            uids::IMPLICIT_VR_LITTLE_ENDIAN,
            "1.2.840.10008.1.2.2",
            "1.2.840.10008.1.2.4.201", // HTJ2K Lossless
            #[cfg(feature = "jpeg2000-openjpeg-encode")]
            "1.2.840.10008.1.2.4.90", // Classic JPEG 2000 Lossless
            #[cfg(feature = "jpeg-ls-codec")]
            "1.2.840.10008.1.2.4.80", // JPEG-LS Lossless
            #[cfg(feature = "jpeg-xl-codec")]
            "1.2.840.10008.1.2.4.110", // JPEG XL Lossless
        ];

        for (label, obj) in [
            ("native_ob", &native_ob),
            ("native_ow", &native_ow),
            ("native_be", &native_be),
        ] {
            for &target in targets {
                assert_streaming_matches_in_memory(obj, target, label);
            }
        }
    }

    #[test]
    fn streaming_matches_in_memory_for_encapsulated_to_native_and_encapsulated_to_encapsulated() {
        let base = build_multiframe_u16_object(
            uids::EXPLICIT_VR_LITTLE_ENDIAN,
            VR::OW,
            24,
            32,
            4,
            10,
            true,
        );

        let sources: &[(&str, &str)] = &[
            ("htj2k", "1.2.840.10008.1.2.4.201"),
            #[cfg(feature = "jpeg2000-openjpeg-encode")]
            ("j2k", "1.2.840.10008.1.2.4.90"),
            #[cfg(feature = "jpeg-ls-codec")]
            ("jpegls", "1.2.840.10008.1.2.4.80"),
            #[cfg(feature = "jpeg-xl-codec")]
            ("jxl", "1.2.840.10008.1.2.4.110"),
        ];

        for &(src_label, src_ts) in sources {
            let compressed = transcode_dcmnorm_object_owned(base.clone(), src_ts).unwrap();
            let targets: &[&str] = &[
                src_ts, // passthrough encapsulated
                uids::EXPLICIT_VR_LITTLE_ENDIAN,
                "1.2.840.10008.1.2.2",
                "1.2.840.10008.1.2.4.201", // compressed -> HTJ2K
                #[cfg(feature = "jpeg-ls-codec")]
                "1.2.840.10008.1.2.4.80", // compressed -> JPEG-LS
            ];
            for &target in targets {
                assert_streaming_matches_in_memory(&compressed, target, src_label);
            }
        }
    }

    #[test]
    fn streaming_supports_safe_in_place_overwrite() {
        let obj = build_multiframe_u16_object(
            uids::EXPLICIT_VR_LITTLE_ENDIAN,
            VR::OB,
            16,
            16,
            3,
            10,
            true,
        );
        let expected_pixels = obj
            .element(tags::PIXEL_DATA)
            .unwrap()
            .to_bytes()
            .unwrap()
            .into_owned();
        let path = temp_path("overwrite");
        let mut writable = obj;
        write_dicom_file(&mut writable, &path).unwrap();

        // Transcode in place to HTJ2K, then back to Explicit VR LE in place.
        transcode_dicom_file(&path, &path, "1.2.840.10008.1.2.4.201").unwrap();
        let compressed = read_dicom_file(&path).unwrap();
        assert_eq!(
            compressed.meta().transfer_syntax(),
            "1.2.840.10008.1.2.4.201"
        );
        assert_eq!(
            compressed
                .element(tags::PIXEL_DATA)
                .unwrap()
                .fragments()
                .unwrap()
                .len(),
            3
        );
        assert_eq!(
            compressed
                .element(tags::PIXEL_DATA)
                .unwrap()
                .offset_table()
                .unwrap()
                .len(),
            3
        );

        transcode_dicom_file(&path, &path, uids::EXPLICIT_VR_LITTLE_ENDIAN).unwrap();
        let roundtripped = read_dicom_file(&path).unwrap();
        assert_eq!(
            roundtripped
                .element(tags::PIXEL_DATA)
                .unwrap()
                .to_bytes()
                .unwrap()
                .as_ref(),
            expected_pixels.as_slice()
        );
        // Verify nested IconImageSequence PixelData and trailing padding survived intact.
        let icon_items = roundtripped
            .element(Tag(0x0088, 0x0200))
            .unwrap()
            .items()
            .unwrap();
        assert_eq!(
            icon_items[0]
                .element(tags::PIXEL_DATA)
                .unwrap()
                .to_bytes()
                .unwrap()
                .as_ref(),
            &[10u8, 20, 30, 40]
        );
        assert_eq!(
            roundtripped
                .element(Tag(0xFFFC, 0xFFFC))
                .unwrap()
                .to_bytes()
                .unwrap()
                .as_ref(),
            &[0xAAu8, 0xBB, 0xCC, 0xDD]
        );

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn streaming_preserves_ybr_jpeg_color_transform_on_decode() {
        // Real RGB fixture encoded to JPEG Baseline (YBR_FULL_422) then decoded via streaming
        // must match the in-memory decoder byte-for-byte.
        let fixture = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("test/files/rgb_planar.dcm");
        let obj = read_dicom_file(&fixture).unwrap();
        let jpeg = transcode_dcmnorm_object_owned(obj, "1.2.840.10008.1.2.4.50").unwrap();
        assert_streaming_matches_in_memory(&jpeg, uids::EXPLICIT_VR_LITTLE_ENDIAN, "jpeg_ybr");
        assert_streaming_matches_in_memory(&jpeg, "1.2.840.10008.1.2.4.201", "jpeg_ybr_to_htj2k");
    }

    #[test]
    fn streaming_handles_planar_rgb_native_to_encapsulated() {
        let fixture = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("test/files/rgb_planar.dcm");
        let obj = read_dicom_file(&fixture).unwrap();
        assert_streaming_matches_in_memory(&obj, "1.2.840.10008.1.2.4.201", "planar_to_htj2k");
        #[cfg(feature = "jpeg2000-openjpeg-encode")]
        assert_streaming_matches_in_memory(&obj, "1.2.840.10008.1.2.4.90", "planar_to_j2k");
        #[cfg(feature = "jpeg-xl-codec")]
        assert_streaming_matches_in_memory(&obj, "1.2.840.10008.1.2.4.110", "planar_to_jxl");
        #[cfg(feature = "jpeg-ls-codec")]
        assert_streaming_matches_in_memory(&obj, "1.2.840.10008.1.2.4.80", "planar_to_jpegls");
    }

    #[test]
    fn big_endian_ow_odd_sized_8bit_frames_transcode_without_corruption() {
        // Two 1x1 8-bit frames packed into one 16-bit big-endian OW word: frame0=0x11, frame1=0x22.
        // In big-endian OW, the two bytes are stored as a single u16 word (0x2211 in LE -> bytes [0x22, 0x11] on disk).
        let build_be_ow_8bit = |rows: u16, cols: u16, frames: usize, words: Vec<u16>| {
            let meta = FileMetaTableBuilder::new()
                .transfer_syntax(super::EXPLICIT_VR_BIG_ENDIAN_UID)
                .media_storage_sop_class_uid("1.2.840.10008.5.1.4.1.1.7")
                .media_storage_sop_instance_uid("1.2.826.0.1.3680043.8.498.88888.1");
            let mut obj = InMemDicomObject::new_empty().with_meta(meta).unwrap();
            for elem in [
                DataElement::new(tags::ROWS, VR::US, PrimitiveValue::from(rows)),
                DataElement::new(tags::COLUMNS, VR::US, PrimitiveValue::from(cols)),
                DataElement::new(tags::SAMPLES_PER_PIXEL, VR::US, PrimitiveValue::from(1u16)),
                DataElement::new(tags::BITS_ALLOCATED, VR::US, PrimitiveValue::from(8u16)),
                DataElement::new(tags::BITS_STORED, VR::US, PrimitiveValue::from(8u16)),
                DataElement::new(tags::HIGH_BIT, VR::US, PrimitiveValue::from(7u16)),
                DataElement::new(
                    tags::PIXEL_REPRESENTATION,
                    VR::US,
                    PrimitiveValue::from(0u16),
                ),
                DataElement::new(
                    tags::PHOTOMETRIC_INTERPRETATION,
                    VR::CS,
                    PrimitiveValue::from("MONOCHROME2".to_owned()),
                ),
                DataElement::new(
                    tags::NUMBER_OF_FRAMES,
                    VR::IS,
                    PrimitiveValue::from(frames.to_string()),
                ),
                DataElement::new(tags::PIXEL_DATA, VR::OW, PrimitiveValue::U16(words.into())),
            ] {
                obj.put(elem);
            }
            obj
        };

        // Case A: two 1x1 8-bit frames [0x11] and [0x22] -> u16 LE word 0x2211.
        let mut two_frames = build_be_ow_8bit(1, 1, 2, vec![u16::from_le_bytes([0x11, 0x22])]);
        let in_a = temp_path("be-ow-2x1x1-in");
        let out_a = temp_path("be-ow-2x1x1-htj2k");
        write_dicom_file(&mut two_frames, &in_a).unwrap();
        assert!(
            !try_transcode_dicom_file_streaming(&in_a, &out_a, "1.2.840.10008.1.2.4.201").unwrap()
        );
        transcode_dicom_file(&in_a, &out_a, "1.2.840.10008.1.2.4.201").unwrap();
        let dec_a = transcode_dcmnorm_object_owned(
            read_dicom_file(&out_a).unwrap(),
            uids::EXPLICIT_VR_LITTLE_ENDIAN,
        )
        .unwrap();
        assert_eq!(
            dec_a
                .element(tags::PIXEL_DATA)
                .unwrap()
                .to_bytes()
                .unwrap()
                .as_ref(),
            &[0x11, 0x22]
        );

        // Case B: single 1x3 8-bit frame [0x10, 0x20, 0x33] + trailing 0x00 pad -> two u16 LE words.
        let mut odd_single = build_be_ow_8bit(
            1,
            3,
            1,
            vec![
                u16::from_le_bytes([0x10, 0x20]),
                u16::from_le_bytes([0x33, 0x00]),
            ],
        );
        let in_b = temp_path("be-ow-1x3-in");
        let out_b = temp_path("be-ow-1x3-htj2k");
        write_dicom_file(&mut odd_single, &in_b).unwrap();
        assert!(
            !try_transcode_dicom_file_streaming(&in_b, &out_b, "1.2.840.10008.1.2.4.201").unwrap()
        );
        transcode_dicom_file(&in_b, &out_b, "1.2.840.10008.1.2.4.201").unwrap();
        let dec_b = transcode_dcmnorm_object_owned(
            read_dicom_file(&out_b).unwrap(),
            uids::EXPLICIT_VR_LITTLE_ENDIAN,
        )
        .unwrap();
        assert_eq!(
            &dec_b.element(tags::PIXEL_DATA).unwrap().to_bytes().unwrap()[..3],
            &[0x10, 0x20, 0x33]
        );

        for p in [in_a, out_a, in_b, out_b] {
            let _ = std::fs::remove_file(p);
        }
    }

    #[cfg(unix)]
    #[test]
    fn in_place_overwrite_preserves_unix_0600_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let mut obj = build_multiframe_u16_object(
            uids::EXPLICIT_VR_LITTLE_ENDIAN,
            VR::OB,
            16,
            16,
            2,
            10,
            false,
        );
        let path = temp_path("unix-mode-0600");
        write_dicom_file(&mut obj, &path).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();

        transcode_dicom_file(&path, &path, "1.2.840.10008.1.2.4.201").unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(
            mode, 0o600,
            "in-place overwrite must preserve 0600 permissions"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn passthrough_rebuilds_bot_when_encapsulated_fragments_have_odd_length() {
        let frags = vec![
            super::FragmentLocation {
                offset: 100,
                length: 11,
            }, // padded to 12 (+1)
            super::FragmentLocation {
                offset: 119,
                length: 14,
            }, // stays 14
            super::FragmentLocation {
                offset: 141,
                length: 9,
            }, // padded to 10 (+1)
        ];
        // Original BOT: [0, 8+11=19, 19+8+14=41]
        let rebuilt = super::rebuild_passthrough_offset_table(&[0, 19, 41], &frags).unwrap();
        // Padded BOT: [0, 8+12=20, 20+8+14=42]
        assert_eq!(rebuilt, vec![0, 20, 42]);
    }

    #[test]
    fn rejects_transcoding_encapsulated_nested_icon_to_different_transfer_syntax() {
        let mut obj = build_multiframe_u16_object(
            uids::EXPLICIT_VR_LITTLE_ENDIAN,
            VR::OB,
            16,
            16,
            1,
            10,
            false,
        );
        let mut icon = InMemDicomObject::new_empty();
        icon.put(DataElement::new(
            tags::ROWS,
            VR::US,
            PrimitiveValue::from(2u16),
        ));
        icon.put(DataElement::new(
            tags::COLUMNS,
            VR::US,
            PrimitiveValue::from(2u16),
        ));
        icon.put(DataElement::new(
            tags::PIXEL_DATA,
            VR::OB,
            Value::PixelSequence(dcmnorm_core::value::PixelFragmentSequence::new(
                Vec::new(),
                vec![vec![0xFF, 0xD8, 0xFF, 0xD9]],
            )),
        ));
        obj.put(DataElement::new(
            Tag(0x0088, 0x0200),
            VR::SQ,
            Value::Sequence(DataSetSequence::from(vec![icon])),
        ));
        let in_path = temp_path("encap-icon-in");
        let out_path = temp_path("encap-icon-out");
        write_dicom_file(&mut obj, &in_path).unwrap();
        let err =
            try_transcode_dicom_file_streaming(&in_path, &out_path, "1.2.840.10008.1.2.4.201")
                .unwrap_err();
        assert!(
            err.to_string().contains("encapsulated nested PixelData"),
            "unexpected error: {err}"
        );
        // The in-memory path must reject the same conversion consistently, while allowing same-TS.
        let mem_err =
            transcode_dcmnorm_object_owned(obj.clone(), "1.2.840.10008.1.2.4.201").unwrap_err();
        assert!(
            mem_err
                .to_string()
                .contains("encapsulated nested PixelData"),
            "unexpected in-memory error: {mem_err}"
        );
        transcode_dcmnorm_object_owned(obj, uids::EXPLICIT_VR_LITTLE_ENDIAN).unwrap();
        let _ = std::fs::remove_file(&in_path);
        let _ = std::fs::remove_file(&out_path);
    }

    #[cfg(unix)]
    #[test]
    fn streaming_refuses_to_overwrite_readonly_destination() {
        use std::os::unix::fs::PermissionsExt;

        let mut obj = build_multiframe_u16_object(
            uids::EXPLICIT_VR_LITTLE_ENDIAN,
            VR::OB,
            16,
            16,
            1,
            10,
            false,
        );
        let in_path = temp_path("readonly-in");
        let out_path = temp_path("readonly-out");
        write_dicom_file(&mut obj, &in_path).unwrap();
        std::fs::write(&out_path, b"original-readonly-bytes").unwrap();
        std::fs::set_permissions(&out_path, std::fs::Permissions::from_mode(0o444)).unwrap();

        let result = transcode_dicom_file(&in_path, &out_path, "1.2.840.10008.1.2.4.201");
        // Clean permissions back before asserting so temp cleanup always succeeds.
        let _ = std::fs::set_permissions(&out_path, std::fs::Permissions::from_mode(0o644));
        assert!(
            result.is_err(),
            "read-only destination must not be overwritten"
        );
        assert_eq!(
            std::fs::read(&out_path).unwrap(),
            b"original-readonly-bytes"
        );
        let _ = std::fs::remove_file(&in_path);
        let _ = std::fs::remove_file(&out_path);
    }

    #[test]
    fn next_batch_end_budgets_encoded_fragment_bytes_and_advances_single_oversized_frame() {
        // 4 frames whose declared decoded size is tiny (1 KiB), but whose encoded fragments are
        // 30 MiB each: only 2 frames fit within the 64 MiB budget (2 * ~30 MiB <= 64 MiB < 3 * 30 MiB).
        let frags: Vec<super::FragmentLocation> = (0..4)
            .map(|i| super::FragmentLocation {
                offset: i as u64 * 30 * 1024 * 1024,
                length: 30 * 1024 * 1024,
            })
            .collect();
        let mode = super::SourceMode::Encapsulated {
            fragments: frags,
            frame_ranges: vec![0..1, 1..2, 2..3, 3..4],
            decoder: super::EncapsulatedDecoder::JpegLs,
        };
        assert_eq!(super::next_batch_end(&mode, 0, 4, 8, 1024), 2);
        assert_eq!(super::next_batch_end(&mode, 2, 4, 8, 1024), 4);

        // Single oversized frame (> 64 MiB) still advances by 1 frame so streaming makes progress.
        let huge = super::SourceMode::Encapsulated {
            fragments: vec![
                super::FragmentLocation {
                    offset: 0,
                    length: 80 * 1024 * 1024,
                },
                super::FragmentLocation {
                    offset: 80 * 1024 * 1024,
                    length: 80 * 1024 * 1024,
                },
            ],
            frame_ranges: vec![0..1, 1..2],
            decoder: super::EncapsulatedDecoder::JpegLs,
        };
        assert_eq!(super::next_batch_end(&huge, 0, 2, 8, 1024), 1);
        assert_eq!(super::next_batch_end(&huge, 1, 2, 8, 1024), 2);
    }

    #[test]
    fn streaming_probes_jp2_codestream_past_64kib_leading_box() {
        // Build a 10-bit HTJ2K codestream, wrap it in a JP2 container with a >64 KiB leading box
        // before `jp2c`, and intentionally under-declare BitsStored=8 / HighBit=7 so streaming
        // must locate `jp2c` past 64 KiB to apply the same precision correction as in-memory.
        let base = build_multiframe_u16_object(
            uids::EXPLICIT_VR_LITTLE_ENDIAN,
            VR::OW,
            16,
            16,
            1,
            10,
            false,
        );
        let htj2k = transcode_dcmnorm_object_owned(base, "1.2.840.10008.1.2.4.201").unwrap();
        let raw_codestream = htj2k
            .element(tags::PIXEL_DATA)
            .unwrap()
            .fragments()
            .unwrap()[0]
            .clone();

        let dummy_payload = vec![0x42u8; 70 * 1024];
        let dummy_box_len = (8 + dummy_payload.len()) as u32;
        let jp2c_box_len = (8 + raw_codestream.len()) as u32;
        let mut jp2_fragment =
            Vec::with_capacity(12 + dummy_box_len as usize + jp2c_box_len as usize);
        jp2_fragment.extend_from_slice(&dcmnorm_jpeg2000::codestream::JP2_SIGNATURE);
        jp2_fragment.extend_from_slice(&dummy_box_len.to_be_bytes());
        jp2_fragment.extend_from_slice(b"free");
        jp2_fragment.extend_from_slice(&dummy_payload);
        jp2_fragment.extend_from_slice(&jp2c_box_len.to_be_bytes());
        jp2_fragment.extend_from_slice(b"jp2c");
        jp2_fragment.extend_from_slice(&raw_codestream);

        let mut obj = htj2k;
        obj.put(DataElement::new(
            tags::BITS_STORED,
            VR::US,
            PrimitiveValue::from(8u16),
        ));
        obj.put(DataElement::new(
            tags::HIGH_BIT,
            VR::US,
            PrimitiveValue::from(7u16),
        ));
        obj.put(DataElement::new(
            tags::PIXEL_DATA,
            VR::OB,
            Value::PixelSequence(dcmnorm_core::value::PixelFragmentSequence::new(
                Vec::new(),
                vec![jp2_fragment],
            )),
        ));

        assert_streaming_matches_in_memory(&obj, uids::EXPLICIT_VR_LITTLE_ENDIAN, "jp2_past_64k");
    }
}
