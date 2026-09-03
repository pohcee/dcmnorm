use std::borrow::Cow;
use std::sync::OnceLock;

use crate::perf;
use dcmnorm_core::value::Value;
use dcmnorm_core::{DataElement, PrimitiveValue, Tag, VR};
use dcmnorm_dictionary::{tags, uids};
use dcmnorm_encoding::transfer_syntax::{Codec, TransferSyntaxIndex};
use dcmnorm_object::DefaultDicomObject;
use dcmnorm_transcode::TransferSyntaxRegistry;
use image::codecs::jpeg::JpegEncoder;
use image::codecs::png::PngEncoder;
use image::{ColorType, GrayImage, ImageEncoder, RgbImage};
use lcms2::{Intent, PixelFormat, Profile, Transform};
use rayon::prelude::*;

use super::io::{
    apply_jpeg2000_component_correction, is_jpeg2000_transfer_syntax, jpeg2000_component_mismatch,
    jpeg2000_frame_uses_mct, kakadu_ffi_enabled, normalize_transfer_syntax_uid,
    transcode_dcmnorm_object, JPEG2000_DEBUG_ENV_FLAG,
};
use super::types::RenderError;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RenderOutputFormat {
    Raw,
    Png,
    Jpeg,
}

#[derive(Clone, Debug)]
pub enum BoxLength {
    Pixels(u32),
    Percent(f64),
}

impl Default for BoxLength {
    fn default() -> Self {
        Self::Pixels(0)
    }
}

/// A filled rectangle drawn on a rendered image for redaction purposes.
///
/// Coordinates are in output-image pixels (after any resizing).
#[derive(Clone, Debug, Default)]
pub struct BoundingBox {
    /// X offset for the left edge. Negative values are measured from the right edge.
    pub x: i32,
    /// Y offset for the top edge. Negative values are measured from the bottom edge.
    pub y: i32,
    /// Width of the box in pixels or as a percentage of image width.
    pub width: BoxLength,
    /// Height of the box in pixels or as a percentage of image height.
    pub height: BoxLength,
}

/// Summary of a DICOM overlay plane (group `60xx`) discovered on an instance, independent of
/// whether it was actually rendered - see `RenderPipelineOptions::show_overlays`/`overlay_index`.
#[derive(Clone, Debug, PartialEq)]
pub struct OverlaySummary {
    /// 0-based ordinal among the overlay groups present on this instance, ascending by group.
    /// This is the value `RenderPipelineOptions::overlay_index` selects by.
    pub index: usize,
    /// The raw DICOM overlay group, e.g. `0x6000`, `0x6002`, ... `0x601E`.
    pub group: u16,
    pub rows: u16,
    pub columns: u16,
    /// `OverlayType` (60xx,0040): `"G"` (graphics) or `"R"` (ROI), when present.
    pub overlay_type: Option<String>,
    /// `OverlayLabel` (60xx,1500), falling back to `OverlayDescription` (60xx,0022).
    pub label: Option<String>,
}

#[derive(Clone, Debug)]
pub struct RenderPipelineOptions {
    pub frame_index: usize,
    pub apply_modality_lut: bool,
    pub apply_voi_lut: bool,
    pub apply_icc_profile: bool,
    pub window_center: Option<f64>,
    pub window_width: Option<f64>,
    pub jpeg_quality: u8,
    /// Explicit output width in pixels. When combined with `output_height`, the image is scaled
    /// to the exact dimensions. When used alone, the height is computed from the aspect ratio.
    pub output_width: Option<u32>,
    /// Explicit output height in pixels. When combined with `output_width`, the image is scaled
    /// to the exact dimensions. When used alone, the width is computed from the aspect ratio.
    pub output_height: Option<u32>,
    /// Scale output while preserving aspect ratio so the longer side equals this value.
    pub scale_max_size: Option<u32>,
    /// Filled rectangles to draw over the output image for redaction.
    ///
    /// Coordinates are applied after any resizing, but before padding, referencing source image pixels.
    pub bounding_boxes: Vec<BoundingBox>,
    /// Fill color for bounding boxes as `[R, G, B]`, values 0-255. Defaults to `[0, 0, 0]` (black).
    pub bounding_box_color: [u8; 3],
    /// Pad the image to a square canvas when `scale_max_size` is set.
    pub pad: bool,
    /// Pad color for the square canvas as `[R, G, B]`. Defaults to `[0, 0, 0]` (black).
    pub pad_color: [u8; 3],
    /// Whether to composite an overlay plane onto the rendered image, when the instance has one.
    /// Defaults to `true` - the first available overlay renders by default.
    pub show_overlays: bool,
    /// Which overlay to render, by its `OverlaySummary::index` (0-based, ascending by DICOM
    /// group). `None` selects the first available overlay (index 0). Ignored when
    /// `show_overlays` is `false`.
    pub overlay_index: Option<usize>,
    /// Fill color for composited overlay pixels as `[R, G, B]`. Defaults to `[0, 255, 0]` (green).
    pub overlay_color: [u8; 3],
}

impl Default for RenderPipelineOptions {
    fn default() -> Self {
        Self {
            frame_index: 0,
            apply_modality_lut: true,
            apply_voi_lut: true,
            apply_icc_profile: true,
            window_center: None,
            window_width: None,
            jpeg_quality: 90,
            output_width: None,
            output_height: None,
            scale_max_size: None,
            bounding_boxes: Vec::new(),
            bounding_box_color: [0, 0, 0],
            pad: false,
            pad_color: [0, 0, 0],
            show_overlays: true,
            overlay_index: None,
            overlay_color: [0, 255, 0],
        }
    }
}

#[derive(Clone, Debug)]
pub struct RenderFrameOutput {
    pub width: u16,
    pub height: u16,
    pub samples_per_pixel: u16,
    pub bits_allocated: u16,
    pub format: RenderOutputFormat,
    pub bytes: Vec<u8>,
    /// All overlay planes present on the source instance, regardless of whether one was rendered.
    pub overlays: Vec<OverlaySummary>,
    /// Which overlay (by `OverlaySummary::index`) was actually composited into `bytes`, if any.
    pub selected_overlay_index: Option<usize>,
}

// pub(crate): shared with dicom_io::volume, which decodes/reformats a series' frames into a 3D
// volume via the same per-file metadata/pixel-decode primitives this module already uses for 2D
// rendering (see `decode_frame_grayscale_values` below), rather than duplicating them.
#[derive(Clone, Debug)]
pub(crate) struct RenderMetadata {
    pub(crate) rows: u16,
    pub(crate) cols: u16,
    pub(crate) samples_per_pixel: u16,
    pub(crate) bits_allocated: u16,
    pub(crate) bits_stored: u16,
    pub(crate) pixel_representation: u16,
    pub(crate) planar_configuration: u16,
    pub(crate) number_of_frames: usize,
    pub(crate) photometric_interpretation: String,
}

#[derive(Clone, Debug)]
pub(crate) struct RenderedFramePixels {
    pub(crate) width: u16,
    pub(crate) height: u16,
    pub(crate) samples_per_pixel: u16,
    pub(crate) bytes: Vec<u8>,
}

pub fn render_dicom_frame(
    object: &DefaultDicomObject,
    output_format: RenderOutputFormat,
    options: &RenderPipelineOptions,
) -> Result<RenderFrameOutput, RenderError> {
    let _scope = perf::scope("render.render_dicom_frame");
    let frames = render_dicom_frames(object, output_format, options)?;
    let frame = frames
        .into_iter()
        .next()
        .ok_or(RenderError::InvalidFrameIndex {
            requested: options.frame_index,
            number_of_frames: 0,
        })?;
    Ok(frame)
}

pub fn render_dicom_frames(
    object: &DefaultDicomObject,
    output_format: RenderOutputFormat,
    options: &RenderPipelineOptions,
) -> Result<Vec<RenderFrameOutput>, RenderError> {
    let _scope = perf::scope("render.render_dicom_frames");
    validate_user_window_overrides(options)?;

    if let Some(frame_object) = try_decode_single_frame_object(object, options.frame_index)? {
        let metadata = read_render_metadata(&frame_object)?;
        let mut frame_options = options.clone();
        frame_options.frame_index = 0;
        let overlays = discover_overlays(&frame_object);
        validate_overlay_index(options, &overlays)?;

        if output_format == RenderOutputFormat::Raw {
            let _raw_scope = perf::scope("render.raw_frame_extract");
            let bytes = get_frame_bytes(&frame_object, &metadata, 0)?;
            return Ok(vec![RenderFrameOutput {
                width: metadata.cols,
                height: metadata.rows,
                samples_per_pixel: metadata.samples_per_pixel,
                bits_allocated: metadata.bits_allocated,
                format: RenderOutputFormat::Raw,
                bytes,
                overlays,
                selected_overlay_index: None,
            }]);
        }

        let selected_overlay = resolve_selected_overlay(options, &overlays);
        let mut frame = render_single_frame(&frame_object, &metadata, &frame_options)?;
        if let Some(index) = selected_overlay {
            composite_overlay(
                &mut frame,
                &frame_object,
                &metadata,
                0,
                options.frame_index,
                &overlays[index],
                options.overlay_color,
            )?;
        }
        let mut frame = maybe_resize_frame(frame, &frame_options);
        draw_bounding_boxes(&mut frame, &frame_options);
        let frame = maybe_pad_frame(frame, &frame_options);
        let mut encoded = encode_rendered_frame(&frame, output_format, frame_options.jpeg_quality)?;
        encoded.overlays = overlays;
        encoded.selected_overlay_index = selected_overlay;
        return Ok(vec![encoded]);
    }

    let working = ensure_native_render_object(object)?;
    let metadata = read_render_metadata(working.as_ref())?;
    let overlays = discover_overlays(working.as_ref());
    validate_overlay_index(options, &overlays)?;

    if output_format == RenderOutputFormat::Raw {
        let _raw_scope = perf::scope("render.raw_frame_extract");
        let bytes = get_frame_bytes(working.as_ref(), &metadata, options.frame_index)?;
        return Ok(vec![RenderFrameOutput {
            width: metadata.cols,
            height: metadata.rows,
            samples_per_pixel: metadata.samples_per_pixel,
            bits_allocated: metadata.bits_allocated,
            format: RenderOutputFormat::Raw,
            bytes,
            overlays,
            selected_overlay_index: None,
        }]);
    }

    let selected_overlay = resolve_selected_overlay(options, &overlays);
    let mut frame = render_single_frame(working.as_ref(), &metadata, options)?;
    if let Some(index) = selected_overlay {
        composite_overlay(
            &mut frame,
            working.as_ref(),
            &metadata,
            options.frame_index,
            options.frame_index,
            &overlays[index],
            options.overlay_color,
        )?;
    }
    let mut frame = maybe_resize_frame(frame, options);
    draw_bounding_boxes(&mut frame, options);
    let frame = maybe_pad_frame(frame, options);
    let mut encoded = encode_rendered_frame(&frame, output_format, options.jpeg_quality)?;
    encoded.overlays = overlays;
    encoded.selected_overlay_index = selected_overlay;
    Ok(vec![encoded])
}

pub fn render_all_dicom_frames(
    object: &DefaultDicomObject,
    output_format: RenderOutputFormat,
    options: &RenderPipelineOptions,
) -> Result<Vec<RenderFrameOutput>, RenderError> {
    let _scope = perf::scope("render.render_all_dicom_frames");
    validate_user_window_overrides(options)?;
    let working = ensure_native_render_object(object)?;
    let metadata = read_render_metadata(working.as_ref())?;
    let overlays = discover_overlays(working.as_ref());
    validate_overlay_index(options, &overlays)?;
    let selected_overlay = resolve_selected_overlay(options, &overlays);

    if output_format == RenderOutputFormat::Raw {
        let mut frames = Vec::with_capacity(metadata.number_of_frames);
        for frame_index in 0..metadata.number_of_frames {
            let bytes = get_frame_bytes(working.as_ref(), &metadata, frame_index)?;
            frames.push(RenderFrameOutput {
                width: metadata.cols,
                height: metadata.rows,
                samples_per_pixel: metadata.samples_per_pixel,
                bits_allocated: metadata.bits_allocated,
                format: RenderOutputFormat::Raw,
                bytes,
                overlays: overlays.clone(),
                selected_overlay_index: None,
            });
        }
        return Ok(frames);
    }

    let rendered = render_all_frames(working.as_ref(), &metadata, options)?;
    let _post_scope = perf::scope("render.render_all_postprocess_parallel");
    let results = rendered
        .into_par_iter()
        .enumerate()
        .map(|(frame_index, mut frame)| {
            if let Some(index) = selected_overlay {
                composite_overlay(
                    &mut frame,
                    working.as_ref(),
                    &metadata,
                    frame_index,
                    frame_index,
                    &overlays[index],
                    options.overlay_color,
                )?;
            }
            let mut resized = maybe_resize_frame(frame, options);
            draw_bounding_boxes(&mut resized, options);
            let final_frame = maybe_pad_frame(resized, options);
            let mut encoded = encode_rendered_frame(&final_frame, output_format, options.jpeg_quality)?;
            encoded.overlays = overlays.clone();
            encoded.selected_overlay_index = selected_overlay;
            Ok(encoded)
        })
        .collect::<Vec<_>>();

    results.into_iter().collect()
}

pub fn render_all_dicom_video_frames(
    object: &DefaultDicomObject,
    options: &RenderPipelineOptions,
) -> Result<Vec<RenderFrameOutput>, RenderError> {
    let _scope = perf::scope("render.render_all_dicom_video_frames");
    validate_user_window_overrides(options)?;
    let working = ensure_native_render_object(object)?;
    let metadata = read_render_metadata(working.as_ref())?;
    let overlays = discover_overlays(working.as_ref());
    validate_overlay_index(options, &overlays)?;
    let selected_overlay = resolve_selected_overlay(options, &overlays);
    let rendered = render_all_frames(working.as_ref(), &metadata, options)?;
    let _post_scope = perf::scope("render.render_all_video_postprocess_parallel");
    let results = rendered
        .into_par_iter()
        .enumerate()
        .map(|(frame_index, mut frame)| {
            if let Some(index) = selected_overlay {
                composite_overlay(
                    &mut frame,
                    working.as_ref(),
                    &metadata,
                    frame_index,
                    frame_index,
                    &overlays[index],
                    options.overlay_color,
                )?;
            }
            let mut resized = maybe_resize_frame(frame, options);
            draw_bounding_boxes(&mut resized, options);
            let final_frame = maybe_pad_frame(resized, options);
            // These frames are piped straight to ffmpeg and discarded (see write_dicom_video) -
            // overlay metadata isn't consumed downstream, only the composited pixels matter here.
            Ok(RenderFrameOutput {
                width: final_frame.width,
                height: final_frame.height,
                samples_per_pixel: final_frame.samples_per_pixel,
                bits_allocated: 8,
                format: RenderOutputFormat::Raw,
                bytes: final_frame.bytes,
                overlays: Vec::new(),
                selected_overlay_index: None,
            })
        })
        .collect::<Vec<Result<RenderFrameOutput, RenderError>>>();

    results.into_iter().collect()
}

fn mpeg4_muxer_name(output_path: &std::path::Path) -> &'static str {
    match output_path
        .extension()
        .and_then(|value| value.to_str())
        .map(|value| value.to_ascii_lowercase())
        .as_deref()
    {
        Some("mov") => "mov",
        _ => "mp4",
    }
}

fn mpeg4_video_filter() -> &'static str {
    "pad=width=ceil(iw/2)*2:height=ceil(ih/2)*2"
}

fn mpeg4_input_pixel_format(samples_per_pixel: u16) -> Result<&'static str, RenderError> {
    match samples_per_pixel {
        1 => Ok("gray"),
        3 => Ok("rgb24"),
        other => Err(RenderError::Video(format!(
            "unsupported rendered movie samples-per-pixel value: {other}"
        ))),
    }
}

/// Renders every frame of `object` and muxes them into an MPEG4 (or QuickTime, if `output_path`
/// ends in `.mov`) video file at `output_path`, via a piped `ffmpeg` subprocess (requires `ffmpeg`
/// on `PATH`) - mirrors the dcmnorm CLI's `--render-fps` output path. `options.frame_index` is
/// ignored; every frame is rendered.
pub fn write_dicom_video(
    object: &DefaultDicomObject,
    output_path: &std::path::Path,
    options: &RenderPipelineOptions,
    fps: f64,
) -> Result<(), RenderError> {
    use std::io::Write;
    use std::process::{Command, Stdio};

    let _scope = perf::scope("render.write_dicom_video");

    if !fps.is_finite() || fps <= 0.0 {
        return Err(RenderError::Video("fps must be greater than zero".to_owned()));
    }

    let frames = render_all_dicom_video_frames(object, options)?;
    let Some(first_frame) = frames.first() else {
        return Err(RenderError::Video("no frames rendered".to_owned()));
    };

    let first_frame_width = first_frame.width;
    let first_frame_height = first_frame.height;
    let first_frame_samples_per_pixel = first_frame.samples_per_pixel;
    let pixel_format = mpeg4_input_pixel_format(first_frame_samples_per_pixel)?;
    let video_size = format!("{first_frame_width}x{first_frame_height}");

    let mut command = Command::new("ffmpeg");
    command
        .arg("-y")
        .arg("-framerate")
        .arg(format!("{fps}"))
        .arg("-f")
        .arg("rawvideo")
        .arg("-pixel_format")
        .arg(pixel_format)
        .arg("-video_size")
        .arg(video_size)
        .arg("-i")
        .arg("-")
        .arg("-vf")
        .arg(mpeg4_video_filter())
        .arg("-c:v")
        .arg("libx264")
        .arg("-pix_fmt")
        .arg("yuv420p")
        .arg("-f")
        .arg(mpeg4_muxer_name(output_path))
        .arg(output_path)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null());

    let mut child = command.spawn().map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            RenderError::Video("ffmpeg executable not found in PATH (required for video output)".to_owned())
        } else {
            RenderError::Video(format!("failed to execute ffmpeg: {error}"))
        }
    })?;

    {
        let _pipe_scope = perf::scope("render.write_dicom_video.pipe_raw_frames");
        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| RenderError::Video("failed to open ffmpeg stdin for piped frame input".to_owned()))?;
        for frame in frames {
            if frame.width != first_frame_width
                || frame.height != first_frame_height
                || frame.samples_per_pixel != first_frame_samples_per_pixel
            {
                return Err(RenderError::Video(
                    "rendered movie frames must all have matching dimensions and pixel format".to_owned(),
                ));
            }
            stdin
                .write_all(&frame.bytes)
                .map_err(|error| RenderError::Video(format!("failed to write frame to ffmpeg stdin: {error}")))?;
        }
    }

    let status = child
        .wait()
        .map_err(|error| RenderError::Video(format!("failed while waiting for ffmpeg: {error}")))?;

    if !status.success() {
        return Err(RenderError::Video(format!("ffmpeg failed with exit status {status}")));
    }

    Ok(())
}

pub fn render_dicom_to_recompressed_object(
    object: &DefaultDicomObject,
    target_transfer_syntax_uid: &str,
    options: &RenderPipelineOptions,
) -> Result<DefaultDicomObject, RenderError> {
    let mut working = transcode_dcmnorm_object(object, uids::EXPLICIT_VR_LITTLE_ENDIAN)?;
    let metadata = read_render_metadata(&working)?;
    let rendered_frames = render_all_frames(&working, &metadata, options)?;

    let mut rendered_pixel_data = Vec::new();
    for frame in rendered_frames {
        rendered_pixel_data.extend_from_slice(&frame.bytes);
    }

    let output_samples_per_pixel = if metadata.samples_per_pixel == 1 {
        1u16
    } else {
        3u16
    };
    let output_photometric = if output_samples_per_pixel == 1 {
        "MONOCHROME2"
    } else {
        "RGB"
    };

    working.put(DataElement::new(
        tags::PIXEL_DATA,
        VR::OB,
        PrimitiveValue::from(rendered_pixel_data),
    ));
    working.put(DataElement::new(
        tags::BITS_ALLOCATED,
        VR::US,
        PrimitiveValue::from(8u16),
    ));
    working.put(DataElement::new(
        tags::BITS_STORED,
        VR::US,
        PrimitiveValue::from(8u16),
    ));
    working.put(DataElement::new(
        tags::HIGH_BIT,
        VR::US,
        PrimitiveValue::from(7u16),
    ));
    working.put(DataElement::new(
        tags::PIXEL_REPRESENTATION,
        VR::US,
        PrimitiveValue::from(0u16),
    ));
    working.put(DataElement::new(
        tags::SAMPLES_PER_PIXEL,
        VR::US,
        PrimitiveValue::from(output_samples_per_pixel),
    ));
    working.put(DataElement::new(
        tags::PHOTOMETRIC_INTERPRETATION,
        VR::CS,
        PrimitiveValue::from(output_photometric),
    ));

    if output_samples_per_pixel > 1 {
        working.put(DataElement::new(
            tags::PLANAR_CONFIGURATION,
            VR::US,
            PrimitiveValue::from(0u16),
        ));
    } else {
        working.remove_element(tags::PLANAR_CONFIGURATION);
    }

    let recompressed = transcode_dcmnorm_object(&working, target_transfer_syntax_uid)?;
    Ok(recompressed)
}

pub fn redact_dicom_pixels_to_transfer_syntax(
    object: &DefaultDicomObject,
    target_transfer_syntax_uid: &str,
    boxes: &[BoundingBox],
    color: [u8; 3],
) -> Result<DefaultDicomObject, RenderError> {
    let mut working = transcode_dcmnorm_object(object, uids::EXPLICIT_VR_LITTLE_ENDIAN)?;
    let metadata = read_render_metadata(&working)?;

    if metadata.bits_allocated != 1 && metadata.bits_allocated != 8 && metadata.bits_allocated != 16
    {
        return Err(RenderError::UnsupportedBitsAllocated(
            metadata.bits_allocated,
        ));
    }

    if metadata.samples_per_pixel != 1 && metadata.samples_per_pixel != 3 {
        return Err(RenderError::UnsupportedSamplesPerPixel(
            metadata.samples_per_pixel,
        ));
    }

    if metadata.samples_per_pixel == 3 && metadata.planar_configuration > 1 {
        return Err(RenderError::UnsupportedPlanarConfiguration(
            metadata.planar_configuration,
        ));
    }

    let frame_len = frame_length_bytes(&metadata)?;
    let mut redacted_pixel_data = Vec::with_capacity(frame_len * metadata.number_of_frames);
    for frame_index in 0..metadata.number_of_frames {
        let mut frame_bytes = get_frame_bytes(&working, &metadata, frame_index)?;
        apply_bounding_boxes_to_raw_frame(&mut frame_bytes, &metadata, boxes, color)?;
        redacted_pixel_data.extend_from_slice(&frame_bytes);
    }

    let pixel_vr = if metadata.bits_allocated > 8 {
        VR::OW
    } else {
        VR::OB
    };
    working.put(DataElement::new(
        tags::PIXEL_DATA,
        pixel_vr,
        PrimitiveValue::from(redacted_pixel_data),
    ));

    let recompressed = transcode_dcmnorm_object(&working, target_transfer_syntax_uid)?;
    Ok(recompressed)
}

fn render_all_frames(
    object: &DefaultDicomObject,
    metadata: &RenderMetadata,
    options: &RenderPipelineOptions,
) -> Result<Vec<RenderedFramePixels>, RenderError> {
    let _scope = perf::scope("render.render_all_frames");
    let results = (0..metadata.number_of_frames)
        .into_par_iter()
        .map(|frame_index| {
            let mut frame_options = options.clone();
            frame_options.frame_index = frame_index;
            render_single_frame(object, metadata, &frame_options)
        })
        .collect::<Vec<_>>();

    results.into_iter().collect()
}

fn frame_length_bytes(metadata: &RenderMetadata) -> Result<usize, RenderError> {
    let samples_per_frame = usize::from(metadata.rows)
        * usize::from(metadata.cols)
        * usize::from(metadata.samples_per_pixel);
    match metadata.bits_allocated {
        1 => Ok(samples_per_frame.div_ceil(8)),
        8 => Ok(samples_per_frame),
        16 => Ok(samples_per_frame * 2),
        other => Err(RenderError::UnsupportedBitsAllocated(other)),
    }
}

fn apply_bounding_boxes_to_raw_frame(
    frame_bytes: &mut [u8],
    metadata: &RenderMetadata,
    boxes: &[BoundingBox],
    color: [u8; 3],
) -> Result<(), RenderError> {
    if boxes.is_empty() {
        return Ok(());
    }

    let width = u32::from(metadata.cols);
    let height = u32::from(metadata.rows);
    let pixel_count = usize::from(metadata.rows) * usize::from(metadata.cols);

    match (metadata.samples_per_pixel, metadata.bits_allocated) {
        (1, 1) => {
            let threshold = rgb_to_luma(color) >= 128;
            for bbox in boxes {
                let (x_start, x_end, y_start, y_end) = clamped_box(bbox, width, height);
                for y in y_start..y_end {
                    for x in x_start..x_end {
                        let pixel_index = (y * width + x) as usize;
                        set_bit_sample(frame_bytes, pixel_index, threshold);
                    }
                }
            }
        }
        (1, 8) => {
            let value = scale_u8_to_bits_stored(rgb_to_luma(color), metadata.bits_stored) as u8;
            for bbox in boxes {
                let (x_start, x_end, y_start, y_end) = clamped_box(bbox, width, height);
                for y in y_start..y_end {
                    for x in x_start..x_end {
                        let pixel_index = (y * width + x) as usize;
                        frame_bytes[pixel_index] = value;
                    }
                }
            }
        }
        (1, 16) => {
            let value = scale_u8_to_bits_stored(rgb_to_luma(color), metadata.bits_stored);
            for bbox in boxes {
                let (x_start, x_end, y_start, y_end) = clamped_box(bbox, width, height);
                for y in y_start..y_end {
                    for x in x_start..x_end {
                        let pixel_index = (y * width + x) as usize;
                        let byte_index = pixel_index * 2;
                        let bytes = value.to_le_bytes();
                        frame_bytes[byte_index] = bytes[0];
                        frame_bytes[byte_index + 1] = bytes[1];
                    }
                }
            }
        }
        (3, 8) => {
            let r = scale_u8_to_bits_stored(color[0], metadata.bits_stored) as u8;
            let g = scale_u8_to_bits_stored(color[1], metadata.bits_stored) as u8;
            let b = scale_u8_to_bits_stored(color[2], metadata.bits_stored) as u8;
            for bbox in boxes {
                let (x_start, x_end, y_start, y_end) = clamped_box(bbox, width, height);
                for y in y_start..y_end {
                    for x in x_start..x_end {
                        let pixel_index = (y * width + x) as usize;
                        if metadata.planar_configuration == 0 {
                            let base = pixel_index * 3;
                            frame_bytes[base] = r;
                            frame_bytes[base + 1] = g;
                            frame_bytes[base + 2] = b;
                        } else {
                            frame_bytes[pixel_index] = r;
                            frame_bytes[pixel_index + pixel_count] = g;
                            frame_bytes[pixel_index + 2 * pixel_count] = b;
                        }
                    }
                }
            }
        }
        (3, 16) => {
            let r = scale_u8_to_bits_stored(color[0], metadata.bits_stored).to_le_bytes();
            let g = scale_u8_to_bits_stored(color[1], metadata.bits_stored).to_le_bytes();
            let b = scale_u8_to_bits_stored(color[2], metadata.bits_stored).to_le_bytes();
            for bbox in boxes {
                let (x_start, x_end, y_start, y_end) = clamped_box(bbox, width, height);
                for y in y_start..y_end {
                    for x in x_start..x_end {
                        let pixel_index = (y * width + x) as usize;
                        if metadata.planar_configuration == 0 {
                            let base = pixel_index * 6;
                            frame_bytes[base] = r[0];
                            frame_bytes[base + 1] = r[1];
                            frame_bytes[base + 2] = g[0];
                            frame_bytes[base + 3] = g[1];
                            frame_bytes[base + 4] = b[0];
                            frame_bytes[base + 5] = b[1];
                        } else {
                            let r_base = pixel_index * 2;
                            let g_base = (pixel_index + pixel_count) * 2;
                            let b_base = (pixel_index + 2 * pixel_count) * 2;
                            frame_bytes[r_base] = r[0];
                            frame_bytes[r_base + 1] = r[1];
                            frame_bytes[g_base] = g[0];
                            frame_bytes[g_base + 1] = g[1];
                            frame_bytes[b_base] = b[0];
                            frame_bytes[b_base + 1] = b[1];
                        }
                    }
                }
            }
        }
        (samples, _) => return Err(RenderError::UnsupportedSamplesPerPixel(samples)),
    }

    Ok(())
}

fn clamped_box(bbox: &BoundingBox, width: u32, height: u32) -> (u32, u32, u32, u32) {
    let x_start = resolve_axis_start(bbox.x, width);
    let box_width = resolve_axis_length(&bbox.width, width);
    let x_end = x_start.saturating_add(box_width).min(width);
    let y_start = resolve_axis_start(bbox.y, height);
    let box_height = resolve_axis_length(&bbox.height, height);
    let y_end = y_start.saturating_add(box_height).min(height);
    (x_start, x_end, y_start, y_end)
}

fn resolve_axis_start(offset: i32, extent: u32) -> u32 {
    if offset == i32::MIN {
        return extent;
    }

    if offset >= 0 {
        (offset as u32).min(extent)
    } else {
        extent.saturating_sub(offset.unsigned_abs().min(extent))
    }
}

fn resolve_axis_length(length: &BoxLength, extent: u32) -> u32 {
    match length {
        BoxLength::Pixels(value) => *value,
        BoxLength::Percent(percent) => {
            let clamped_percent = percent.clamp(0.0, 100.0);
            ((f64::from(extent) * clamped_percent) / 100.0).round() as u32
        }
    }
}

fn rgb_to_luma(color: [u8; 3]) -> u8 {
    (0.2126 * f64::from(color[0]) + 0.7152 * f64::from(color[1]) + 0.0722 * f64::from(color[2]))
        .round()
        .clamp(0.0, 255.0) as u8
}

fn scale_u8_to_bits_stored(value: u8, bits_stored: u16) -> u16 {
    let bits = bits_stored.clamp(1, 16);
    let max_value = (1u32 << u32::from(bits)) - 1;
    ((u32::from(value) * max_value + 127) / 255) as u16
}

fn set_bit_sample(bytes: &mut [u8], pixel_index: usize, value: bool) {
    let byte_index = pixel_index / 8;
    let bit = 7 - (pixel_index % 8);
    if value {
        bytes[byte_index] |= 1 << bit;
    } else {
        bytes[byte_index] &= !(1 << bit);
    }
}

fn render_single_frame(
    object: &DefaultDicomObject,
    metadata: &RenderMetadata,
    options: &RenderPipelineOptions,
) -> Result<RenderedFramePixels, RenderError> {
    let _scope = perf::scope("render.render_single_frame");
    if options.frame_index >= metadata.number_of_frames {
        return Err(RenderError::InvalidFrameIndex {
            requested: options.frame_index,
            number_of_frames: metadata.number_of_frames,
        });
    }

    let mut rendered = match metadata.samples_per_pixel {
        1 => render_grayscale_frame(object, metadata, options),
        3 => render_rgb_frame(object, metadata, options),
        other => Err(RenderError::UnsupportedSamplesPerPixel(other)),
    }?;

    if options.apply_icc_profile && rendered.samples_per_pixel == 3 {
        apply_embedded_icc_profile(object, &mut rendered.bytes);
    }

    Ok(rendered)
}

fn apply_embedded_icc_profile(object: &DefaultDicomObject, rgb_bytes: &mut [u8]) {
    if rgb_bytes.is_empty() || rgb_bytes.len() % 3 != 0 {
        return;
    }

    let Some(icc_profile_bytes) = read_icc_profile_bytes(object) else {
        return;
    };

    let Ok(input_profile) = Profile::new_icc(&icc_profile_bytes) else {
        return;
    };

    let output_profile = Profile::new_srgb();

    let Ok(transform) = Transform::new(
        &input_profile,
        PixelFormat::RGB_8,
        &output_profile,
        PixelFormat::RGB_8,
        Intent::Perceptual,
    ) else {
        return;
    };

    transform.transform_in_place(rgb_bytes);
}

fn read_icc_profile_bytes(object: &DefaultDicomObject) -> Option<Vec<u8>> {
    let element = object.get(tags::ICC_PROFILE)?;
    let bytes = element.to_bytes().ok()?;
    if bytes.is_empty() {
        return None;
    }
    Some(bytes.into_owned())
}

fn render_grayscale_frame(
    object: &DefaultDicomObject,
    metadata: &RenderMetadata,
    options: &RenderPipelineOptions,
) -> Result<RenderedFramePixels, RenderError> {
    if metadata.photometric_interpretation == "PALETTE COLOR" {
        return render_palette_color_frame(object, metadata, options);
    }

    if !matches!(
        metadata.photometric_interpretation.as_str(),
        "MONOCHROME1" | "MONOCHROME2"
    ) {
        return Err(RenderError::UnsupportedPhotometricInterpretation(
            metadata.photometric_interpretation.clone(),
        ));
    }

    let frame_bytes = get_frame_bytes(object, metadata, options.frame_index)?;
    let pixel_count = usize::from(metadata.rows) * usize::from(metadata.cols);

    let mut values = decode_grayscale_values(&frame_bytes, metadata)?;
    if values.len() != pixel_count {
        return Err(RenderError::InvalidPixelDataLength {
            expected: pixel_count,
            actual: values.len(),
        });
    }

    if options.apply_modality_lut {
        apply_modality_lut(object, options.frame_index, &mut values);
    }

    let mut rendered = if options.apply_voi_lut {
        // An explicit user override (--window-center/--window-width) wins over an embedded VOI
        // LUT Sequence, same as it already wins over the object's own WindowCenter/WindowWidth
        // in resolve_window - a caller who deliberately overrides display windowing means it for
        // whichever VOI mechanism the file happens to use.
        match (options.window_center, read_lut_from_sequence(object, tags::VOILUT_SEQUENCE)) {
            (None, Some(lut)) => apply_voi_lut(&values, &lut),
            _ => {
                let (center, width) = resolve_window(object, options)?;
                apply_voi_window(&values, center, width)
            }
        }
    } else {
        normalize_to_u8(&values)
    };

    if resolve_grayscale_invert(object, &metadata.photometric_interpretation) {
        for value in &mut rendered {
            *value = 255u8.saturating_sub(*value);
        }
    }

    Ok(RenderedFramePixels {
        width: metadata.cols,
        height: metadata.rows,
        samples_per_pixel: 1,
        bytes: rendered,
    })
}

fn render_palette_color_frame(
    object: &DefaultDicomObject,
    metadata: &RenderMetadata,
    options: &RenderPipelineOptions,
) -> Result<RenderedFramePixels, RenderError> {
    let frame_bytes = get_frame_bytes(object, metadata, options.frame_index)?;
    let mut values = decode_grayscale_values(&frame_bytes, metadata)?;

    if options.apply_modality_lut {
        apply_modality_lut(object, options.frame_index, &mut values);
    }

    let red = read_palette_channel(
        object,
        tags::RED_PALETTE_COLOR_LOOKUP_TABLE_DESCRIPTOR,
        tags::RED_PALETTE_COLOR_LOOKUP_TABLE_DATA,
    )?;
    let green = read_palette_channel(
        object,
        tags::GREEN_PALETTE_COLOR_LOOKUP_TABLE_DESCRIPTOR,
        tags::GREEN_PALETTE_COLOR_LOOKUP_TABLE_DATA,
    )?;
    let blue = read_palette_channel(
        object,
        tags::BLUE_PALETTE_COLOR_LOOKUP_TABLE_DESCRIPTOR,
        tags::BLUE_PALETTE_COLOR_LOOKUP_TABLE_DATA,
    )?;

    let mut rgb = Vec::with_capacity(values.len() * 3);
    for value in values {
        let index = palette_index_for_value(value, red.first_mapped, red.entries);
        rgb.push(red.values[index]);
        rgb.push(green.values[index]);
        rgb.push(blue.values[index]);
    }

    Ok(RenderedFramePixels {
        width: metadata.cols,
        height: metadata.rows,
        samples_per_pixel: 3,
        bytes: rgb,
    })
}

fn render_rgb_frame(
    object: &DefaultDicomObject,
    metadata: &RenderMetadata,
    options: &RenderPipelineOptions,
) -> Result<RenderedFramePixels, RenderError> {
    let is_ybr_full = matches!(
        metadata.photometric_interpretation.as_str(),
        "YBR_FULL" | "YBR_ICT"
    );
    let is_ybr_rct = metadata.photometric_interpretation == "YBR_RCT";
    let is_ybr_422 = metadata.photometric_interpretation == "YBR_FULL_422";
    let is_rgb = metadata.photometric_interpretation == "RGB";

    if !is_rgb && !is_ybr_full && !is_ybr_422 && !is_ybr_rct {
        return Err(RenderError::UnsupportedPhotometricInterpretation(
            metadata.photometric_interpretation.clone(),
        ));
    }

    let frame_bytes = get_frame_bytes(object, metadata, options.frame_index)?;
    let pixel_count = usize::from(metadata.rows) * usize::from(metadata.cols);

    let rendered = if metadata.bits_allocated == 8 {
        if metadata.planar_configuration > 1 {
            return Err(RenderError::UnsupportedPlanarConfiguration(
                metadata.planar_configuration,
            ));
        }

        if is_ybr_422 {
            let expected_bytes = pixel_count * 2;
            if frame_bytes.len() < expected_bytes {
                return Err(RenderError::InvalidPixelDataLength {
                    expected: expected_bytes,
                    actual: frame_bytes.len(),
                });
            }

            let mut rgb = Vec::with_capacity(pixel_count * 3);
            if metadata.planar_configuration == 0 {
                // Y0, Y1, Cb0, Cr0, Y2, Y3, Cb2, Cr2, ...
                for chunk in frame_bytes.chunks_exact(4).take(pixel_count / 2) {
                    let y0 = chunk[0];
                    let y1 = chunk[1];
                    let cb = chunk[2];
                    let cr = chunk[3];

                    let (r0, g0, b0) = ybr_to_rgb(y0, cb, cr);
                    let (r1, g1, b1) = ybr_to_rgb(y1, cb, cr);

                    rgb.push(r0);
                    rgb.push(g0);
                    rgb.push(b0);
                    rgb.push(r1);
                    rgb.push(g1);
                    rgb.push(b1);
                }
            } else {
                // Planar: Y plane (pixel_count), Cb plane (pixel_count/2), Cr plane (pixel_count/2)
                let y_plane = &frame_bytes[0..pixel_count];
                let cb_plane = &frame_bytes[pixel_count..pixel_count + pixel_count / 2];
                let cr_plane =
                    &frame_bytes[pixel_count + pixel_count / 2..pixel_count + pixel_count];

                for i in 0..pixel_count / 2 {
                    let y0 = y_plane[i * 2];
                    let y1 = y_plane[i * 2 + 1];
                    let cb = cb_plane[i];
                    let cr = cr_plane[i];

                    let (r0, g0, b0) = ybr_to_rgb(y0, cb, cr);
                    let (r1, g1, b1) = ybr_to_rgb(y1, cb, cr);

                    rgb.push(r0);
                    rgb.push(g0);
                    rgb.push(b0);
                    rgb.push(r1);
                    rgb.push(g1);
                    rgb.push(b1);
                }
            }
            rgb
        } else {
            let expected_components = pixel_count * 3;
            if frame_bytes.len() < expected_components {
                return Err(RenderError::InvalidPixelDataLength {
                    expected: expected_components,
                    actual: frame_bytes.len(),
                });
            }

            if metadata.planar_configuration == 0 {
                let mut bytes = frame_bytes[..expected_components].to_vec();
                maybe_repair_zeroed_lower_half_chroma(
                    &mut bytes,
                    usize::from(metadata.cols),
                    usize::from(metadata.rows),
                    is_ybr_full,
                    is_ybr_rct,
                );
                if is_ybr_full {
                    bytes
                        .chunks_exact(3)
                        .flat_map(|chunk| {
                            let (r, g, b) = ybr_to_rgb(chunk[0], chunk[1], chunk[2]);
                            [r, g, b]
                        })
                        .collect()
                } else if is_ybr_rct {
                    bytes
                        .chunks_exact(3)
                        .flat_map(|chunk| {
                            let (r, g, b) = ybr_rct_to_rgb(chunk[0], chunk[1], chunk[2]);
                            [r, g, b]
                        })
                        .collect()
                } else {
                    bytes
                }
            } else {
                let mut rgb = vec![0u8; expected_components];
                let plane_len = pixel_count;
                if frame_bytes.len() < plane_len * 3 {
                    return Err(RenderError::InvalidPixelDataLength {
                        expected: plane_len * 3,
                        actual: frame_bytes.len(),
                    });
                }
                for index in 0..pixel_count {
                    let c0 = frame_bytes[index];
                    let c1 = frame_bytes[plane_len + index];
                    let c2 = frame_bytes[2 * plane_len + index];

                    let (r, g, b) = if is_ybr_full {
                        ybr_to_rgb(c0, c1, c2)
                    } else if is_ybr_rct {
                        ybr_rct_to_rgb(c0, c1, c2)
                    } else {
                        (c0, c1, c2)
                    };

                    rgb[index * 3] = r;
                    rgb[index * 3 + 1] = g;
                    rgb[index * 3 + 2] = b;
                }
                rgb
            }
        }
    } else if metadata.bits_allocated == 16 {
        if metadata.planar_configuration > 1 {
            return Err(RenderError::UnsupportedPlanarConfiguration(
                metadata.planar_configuration,
            ));
        }

        if is_ybr_422 {
            return Err(RenderError::UnsupportedPhotometricInterpretation(
                metadata.photometric_interpretation.clone(),
            ));
        }

        let expected_components = pixel_count * 3;

        let expected_bytes = expected_components * 2;
        if frame_bytes.len() < expected_bytes {
            return Err(RenderError::InvalidPixelDataLength {
                expected: expected_bytes,
                actual: frame_bytes.len(),
            });
        }

        let max_value = ((1u32 << u32::from(metadata.bits_stored.min(16))) - 1).max(1) as f64;

        if metadata.planar_configuration == 0 {
            frame_bytes
                .chunks_exact(2)
                .take(expected_components)
                .collect::<Vec<_>>()
                .chunks_exact(3)
                .flat_map(|chunk| {
                    let c0 = {
                        let sample = u16::from_le_bytes([chunk[0][0], chunk[0][1]]);
                        ((f64::from(sample) / max_value) * 255.0).clamp(0.0, 255.0) as u8
                    };
                    let c1 = {
                        let sample = u16::from_le_bytes([chunk[1][0], chunk[1][1]]);
                        ((f64::from(sample) / max_value) * 255.0).clamp(0.0, 255.0) as u8
                    };
                    let c2 = {
                        let sample = u16::from_le_bytes([chunk[2][0], chunk[2][1]]);
                        ((f64::from(sample) / max_value) * 255.0).clamp(0.0, 255.0) as u8
                    };

                    let (r, g, b) = if is_ybr_full {
                        ybr_to_rgb(c0, c1, c2)
                    } else if is_ybr_rct {
                        ybr_rct_to_rgb(c0, c1, c2)
                    } else {
                        (c0, c1, c2)
                    };
                    [r, g, b]
                })
                .collect()
        } else {
            let plane_len = pixel_count;
            let mut planes = [
                vec![0u8; plane_len],
                vec![0u8; plane_len],
                vec![0u8; plane_len],
            ];
            for (channel, plane) in planes.iter_mut().enumerate() {
                for index in 0..plane_len {
                    let sample_index = channel * plane_len + index;
                    let byte_index = sample_index * 2;
                    let sample =
                        u16::from_le_bytes([frame_bytes[byte_index], frame_bytes[byte_index + 1]]);
                    plane[index] =
                        ((f64::from(sample) / max_value) * 255.0).clamp(0.0, 255.0) as u8;
                }
            }

            let mut interleaved = vec![0u8; expected_components];
            for index in 0..pixel_count {
                let (r, g, b) = if is_ybr_full {
                    ybr_to_rgb(planes[0][index], planes[1][index], planes[2][index])
                } else if is_ybr_rct {
                    ybr_rct_to_rgb(planes[0][index], planes[1][index], planes[2][index])
                } else {
                    (planes[0][index], planes[1][index], planes[2][index])
                };
                interleaved[index * 3] = r;
                interleaved[index * 3 + 1] = g;
                interleaved[index * 3 + 2] = b;
            }
            interleaved
        }
    } else {
        return Err(RenderError::UnsupportedBitsAllocated(
            metadata.bits_allocated,
        ));
    };

    Ok(RenderedFramePixels {
        width: metadata.cols,
        height: metadata.rows,
        samples_per_pixel: 3,
        bytes: rendered,
    })
}

fn ybr_to_rgb(y: u8, cb: u8, cr: u8) -> (u8, u8, u8) {
    let y = y as f32;
    let cb = cb as f32 - 128.0;
    let cr = cr as f32 - 128.0;

    let r = y + 1.4020 * cr;
    let g = y - 0.3441 * cb - 0.7141 * cr;
    let b = y + 1.7720 * cb;

    (
        r.clamp(0.0, 255.0) as u8,
        g.clamp(0.0, 255.0) as u8,
        b.clamp(0.0, 255.0) as u8,
    )
}

fn maybe_repair_zeroed_lower_half_chroma(
    interleaved: &mut [u8],
    width: usize,
    height: usize,
    is_ybr_full: bool,
    is_ybr_rct: bool,
) {
    if !(is_ybr_full || is_ybr_rct) {
        return;
    }
    if height < 2 || height % 2 != 0 || interleaved.len() < width * height * 3 {
        return;
    }

    let half = height / 2;
    let top_zero_ratio = zero_chroma_ratio(interleaved, width, 0, half);
    let bottom_zero_ratio = zero_chroma_ratio(interleaved, width, half, height);

    // Repair only when lower-half chroma is almost entirely zero while the top-half is not,
    // which indicates malformed decoder output seen in some JPEG2000 WSI frames.
    if bottom_zero_ratio < 0.98 || top_zero_ratio > 0.50 {
        return;
    }

    for y in half..height {
        let src_y = y - half;
        for x in 0..width {
            let index = (y * width + x) * 3;
            if interleaved[index + 1] == 0 && interleaved[index + 2] == 0 {
                let src_index = (src_y * width + x) * 3;
                interleaved[index + 1] = interleaved[src_index + 1];
                interleaved[index + 2] = interleaved[src_index + 2];
            }
        }
    }
}

fn zero_chroma_ratio(interleaved: &[u8], width: usize, y_start: usize, y_end: usize) -> f64 {
    let mut zero_pairs = 0usize;
    let mut total = 0usize;

    for y in y_start..y_end {
        for x in 0..width {
            let index = (y * width + x) * 3;
            if interleaved[index + 1] == 0 && interleaved[index + 2] == 0 {
                zero_pairs += 1;
            }
            total += 1;
        }
    }

    if total == 0 {
        0.0
    } else {
        zero_pairs as f64 / total as f64
    }
}

fn ybr_rct_to_rgb(y: u8, cb: u8, cr: u8) -> (u8, u8, u8) {
    let y = i32::from(y);
    let cb = i32::from(cb) - 128;
    let cr = i32::from(cr) - 128;

    let g = y - ((cb + cr) >> 2);
    let r = cr + g;
    let b = cb + g;

    (
        r.clamp(0, 255) as u8,
        g.clamp(0, 255) as u8,
        b.clamp(0, 255) as u8,
    )
}

// pub(crate): also used by dicom_io::histogram to cheaply peek SamplesPerPixel (a whole-dataset
// attribute, never per-frame) before deciding whether a frame needs the grayscale or RGB decode
// path - a plain top-level tag read, no pixel data touched, so calling it on the object BEFORE
// narrowing to a single frame/transcoding is always safe and cheap.
pub(crate) fn read_render_metadata(object: &DefaultDicomObject) -> Result<RenderMetadata, RenderError> {
    let rows = required_u16(object, tags::ROWS, "Rows")?;
    let cols = required_u16(object, tags::COLUMNS, "Columns")?;
    let samples_per_pixel = required_u16(object, tags::SAMPLES_PER_PIXEL, "SamplesPerPixel")?;
    let bits_allocated = required_u16(object, tags::BITS_ALLOCATED, "BitsAllocated")?;
    // BitsStored is Type 1 per PS3.3, but some non-conformant encoders (seen
    // from Hologic Cenova R2 CAD secondary captures) omit it. BitsAllocated
    // is the only sound default: it constrains the actual sample width, so
    // window/level and LUT math stay correct even when it overestimates the
    // stored bit depth. Mirrors the fallback in decode_jpeg2000_with_kakadu.
    let bits_stored = object
        .get(tags::BITS_STORED)
        .and_then(|element| element.uint16().ok())
        .unwrap_or(bits_allocated);
    let pixel_representation = object
        .get(tags::PIXEL_REPRESENTATION)
        .and_then(|element| element.uint16().ok())
        .unwrap_or(0);
    let planar_configuration = object
        .get(tags::PLANAR_CONFIGURATION)
        .and_then(|element| element.uint16().ok())
        .unwrap_or(0);
    let number_of_frames = object
        .get(tags::NUMBER_OF_FRAMES)
        .and_then(|element| element.to_str().ok())
        .and_then(|text| {
            text.split('\\')
                .next()
                .and_then(|value| value.trim().parse::<usize>().ok())
        })
        .unwrap_or(1);
    let photometric_interpretation = object
        .get(tags::PHOTOMETRIC_INTERPRETATION)
        .and_then(|element| element.to_str().ok())
        .map(|value| value.trim().to_owned())
        .unwrap_or_else(|| {
            if samples_per_pixel == 1 {
                "MONOCHROME2".to_owned()
            } else {
                "RGB".to_owned()
            }
        });

    if bits_allocated != 1 && bits_allocated != 8 && bits_allocated != 16 {
        return Err(RenderError::UnsupportedBitsAllocated(bits_allocated));
    }

    Ok(RenderMetadata {
        rows,
        cols,
        samples_per_pixel,
        bits_allocated,
        bits_stored,
        pixel_representation,
        planar_configuration,
        number_of_frames,
        photometric_interpretation,
    })
}

// pub(crate): shared with dicom_io::texture_export, whose GPU-texture pipeline needs the same
// invert decision as the classic grayscale render path below - see the module-level pipeline
// contract doc (docs/rendering-pipeline-contract.md at the repo root) for why this single
// resolver, not two independent checks, is the point.
//
// Per DICOM PS3.3 C.11.6.1, an explicit Presentation LUT Shape (2050,0020) - INVERSE or IDENTITY
// - REPLACES the Photometric Interpretation-derived default; it is never combined with it. In the
// absence of an explicit Shape, MONOCHROME1 behaves as INVERSE and MONOCHROME2 as IDENTITY.
pub(crate) fn resolve_grayscale_invert(object: &DefaultDicomObject, photometric_interpretation: &str) -> bool {
    let shape = object
        .get(tags::PRESENTATION_LUT_SHAPE)
        .and_then(|element| element.to_str().ok())
        .map(|value| value.trim().to_ascii_uppercase());
    match shape.as_deref() {
        Some("INVERSE") => true,
        Some("IDENTITY") => false,
        _ => photometric_interpretation == "MONOCHROME1",
    }
}

fn required_u16(
    object: &DefaultDicomObject,
    tag: dcmnorm_core::Tag,
    name: &'static str,
) -> Result<u16, RenderError> {
    object
        .get(tag)
        .and_then(|element| element.uint16().ok())
        .ok_or(RenderError::MissingImageAttribute(name))
}

fn get_frame_bytes(
    object: &DefaultDicomObject,
    metadata: &RenderMetadata,
    frame_index: usize,
) -> Result<Vec<u8>, RenderError> {
    let _scope = perf::scope("render.get_frame_bytes");
    let pixel_data = object
        .element(tags::PIXEL_DATA)
        .map_err(|_| RenderError::MissingImageAttribute("PixelData"))?;

    let samples_per_pixel = if metadata.photometric_interpretation == "YBR_FULL_422" {
        2
    } else {
        metadata.samples_per_pixel
    };

    let samples_per_frame = usize::from(metadata.rows)
        * usize::from(metadata.cols)
        * usize::from(samples_per_pixel);
    let frame_len = match metadata.bits_allocated {
        1 => samples_per_frame.div_ceil(8),
        8 => samples_per_frame,
        16 => samples_per_frame * 2,
        other => return Err(RenderError::UnsupportedBitsAllocated(other)),
    };
    let start = frame_index * frame_len;
    let expected = (frame_index + 1) * frame_len;

    match (metadata.bits_allocated, pixel_data.value()) {
        (1 | 8, Value::Primitive(PrimitiveValue::U8(values))) => {
            let bytes = values.as_ref();
            if bytes.len() < expected {
                return Err(RenderError::InvalidPixelDataLength {
                    expected,
                    actual: bytes.len(),
                });
            }
            return Ok(bytes[start..start + frame_len].to_vec());
        }
        (16, Value::Primitive(PrimitiveValue::U16(values))) => {
            let words = values.as_ref();
            let sample_start = frame_index * samples_per_frame;
            let sample_end = sample_start + samples_per_frame;
            let expected_words = (frame_index + 1) * samples_per_frame;

            if words.len() < expected_words {
                return Err(RenderError::InvalidPixelDataLength {
                    expected: expected_words * 2,
                    actual: words.len() * 2,
                });
            }

            let mut frame = Vec::with_capacity(frame_len);
            for sample in &words[sample_start..sample_end] {
                frame.extend_from_slice(&sample.to_le_bytes());
            }
            return Ok(frame);
        }
        _ => {}
    }

    let pixel_data = pixel_data
        .to_bytes()
        .map_err(|_| RenderError::MissingImageAttribute("PixelData"))?;

    if pixel_data.len() < expected {
        return Err(RenderError::InvalidPixelDataLength {
            expected,
            actual: pixel_data.len(),
        });
    }

    Ok(pixel_data[start..start + frame_len].to_vec())
}

fn decode_grayscale_values(
    frame_bytes: &[u8],
    metadata: &RenderMetadata,
) -> Result<Vec<f64>, RenderError> {
    let _scope = perf::scope("render.decode_grayscale_values");
    // render_dicom_frame's own samples_per_pixel dispatch (1 => here, 3 => render_rgb_frame)
    // already guarantees this for its two callers - this is a safety net for
    // decode_frame_grayscale_values (the texture-export path's own entry point below), which has
    // no such dispatch and previously called straight into here regardless of pixel layout. A
    // true multi-channel (RGB/YBR) frame's bytes are 3x this function's expected pixel_count and
    // interleaved besides - reading them as single-channel samples doesn't fail loudly, it just
    // silently produces a badly aliased/banded image, which is worse than an explicit error here.
    if metadata.samples_per_pixel > 1 {
        return Err(RenderError::UnsupportedSamplesPerPixel(metadata.samples_per_pixel));
    }
    let pixel_count = usize::from(metadata.rows) * usize::from(metadata.cols);

    match metadata.bits_allocated {
        1 => {
            let mut values = Vec::with_capacity(pixel_count);
            for pixel_index in 0..pixel_count {
                let byte = frame_bytes[pixel_index / 8];
                let bit = 7 - (pixel_index % 8);
                let value = (byte >> bit) & 1;
                values.push(f64::from(value));
            }
            Ok(values)
        }
        8 => {
            if frame_bytes.len() < pixel_count {
                return Err(RenderError::InvalidPixelDataLength {
                    expected: pixel_count,
                    actual: frame_bytes.len(),
                });
            }

            let mask = if metadata.bits_stored >= 8 {
                0xFFu16
            } else {
                ((1u16 << metadata.bits_stored) - 1).max(1)
            };

            let mut values = Vec::with_capacity(pixel_count);
            for byte in &frame_bytes[..pixel_count] {
                let raw = u16::from(*byte) & mask;
                values.push(sign_or_unsigned(
                    raw,
                    metadata.bits_stored,
                    metadata.pixel_representation,
                ));
            }
            Ok(values)
        }
        16 => {
            let expected = pixel_count * 2;
            if frame_bytes.len() < expected {
                return Err(RenderError::InvalidPixelDataLength {
                    expected,
                    actual: frame_bytes.len(),
                });
            }

            let mask = if metadata.bits_stored >= 16 {
                u16::MAX
            } else {
                ((1u16 << metadata.bits_stored) - 1).max(1)
            };

            let mut values = Vec::with_capacity(pixel_count);
            for chunk in frame_bytes[..expected].chunks_exact(2) {
                let raw = u16::from_le_bytes([chunk[0], chunk[1]]) & mask;
                values.push(sign_or_unsigned(
                    raw,
                    metadata.bits_stored,
                    metadata.pixel_representation,
                ));
            }
            Ok(values)
        }
        other => Err(RenderError::UnsupportedBitsAllocated(other)),
    }
}

fn sign_or_unsigned(raw: u16, bits_stored: u16, pixel_representation: u16) -> f64 {
    if pixel_representation == 0 {
        return f64::from(raw);
    }

    if bits_stored == 0 {
        return 0.0;
    }

    if bits_stored >= 16 {
        return f64::from(i16::from_le_bytes(raw.to_le_bytes()));
    }

    let shift = 16u16.saturating_sub(bits_stored);
    let value = ((raw << shift) as i16) >> shift;
    f64::from(value)
}

/// Decodes one frame to modality-LUT-applied grayscale values (e.g. Hounsfield units for CT with
/// a rescale slope/intercept) plus its `RenderMetadata` - the single reusable primitive
/// `dicom_io::volume` needs per source slice. Mirrors exactly what `render_dicom_frames`'s
/// `RenderOutputFormat::Raw` branch already does to reach raw pixel bytes (the single-frame fast
/// path when the transfer syntax allows per-frame codec decode, else a full native-object
/// transcode), plus `render_grayscale_frame`'s `decode_grayscale_values` + `apply_modality_lut`
/// steps - so compressed transfer syntaxes (JPEG/JPEG-LS/JPEG2000/RLE) are handled identically to
/// 2D rendering rather than needing a second decode path.
pub(crate) fn decode_frame_grayscale_values(
    object: &DefaultDicomObject,
    frame_index: usize,
) -> Result<(RenderMetadata, Vec<f64>), RenderError> {
    if let Some(frame_object) = try_decode_single_frame_object(object, frame_index)? {
        let metadata = read_render_metadata(&frame_object)?;
        let bytes = get_frame_bytes(&frame_object, &metadata, 0)?;
        let mut values = decode_grayscale_values(&bytes, &metadata)?;
        // frame_object's pixel data was narrowed to this one frame (see
        // try_decode_single_frame_object's own doc), but its functional group sequences are an
        // unmodified clone of `object`'s - still indexed by the ORIGINAL frame_index, not 0.
        apply_modality_lut(&frame_object, frame_index, &mut values);
        return Ok((metadata, values));
    }

    let working = ensure_native_render_object(object)?;
    let metadata = read_render_metadata(working.as_ref())?;
    let bytes = get_frame_bytes(working.as_ref(), &metadata, frame_index)?;
    let mut values = decode_grayscale_values(&bytes, &metadata)?;
    apply_modality_lut(working.as_ref(), frame_index, &mut values);
    Ok((metadata, values))
}

/// Decodes one frame's RGB pixel bytes - row-major, 3 interleaved 8-bit samples per pixel, using
/// the same YBR-to-RGB conversion and planar-configuration handling `render_rgb_frame` already
/// does for 2D rendering (16-bit sources are normalized down to 0-255 there too) - plus its
/// `RenderMetadata`. The RGB counterpart to `decode_frame_grayscale_values` above, for the same
/// reason: `dicom_io::histogram` needs a reusable, correctly-decoded pixel primitive rather than
/// duplicating the YBR/planar-config handling itself. Does NOT apply an embedded ICC profile
/// (unlike `render_single_frame`'s own RGB path) - that's a display color-management step with no
/// bearing on a value-distribution histogram of the decoded samples.
pub(crate) fn decode_frame_rgb_values(
    object: &DefaultDicomObject,
    frame_index: usize,
) -> Result<(RenderMetadata, Vec<u8>), RenderError> {
    if let Some(frame_object) = try_decode_single_frame_object(object, frame_index)? {
        let metadata = read_render_metadata(&frame_object)?;
        let options = RenderPipelineOptions { frame_index: 0, ..Default::default() };
        let rendered = render_rgb_frame(&frame_object, &metadata, &options)?;
        return Ok((metadata, rendered.bytes));
    }

    let working = ensure_native_render_object(object)?;
    let metadata = read_render_metadata(working.as_ref())?;
    let options = RenderPipelineOptions { frame_index, ..Default::default() };
    let rendered = render_rgb_frame(working.as_ref(), &metadata, &options)?;
    Ok((metadata, rendered.bytes))
}

/// Decodes one frame to interleaved RGB8 bytes for texture export (`texture_export::
/// pack_dicom_rgb_frame_texture`), NOT `decode_frame_rgb_values` above - that function is
/// purpose-built for histogram computation (calls `render_rgb_frame` directly, skips ICC
/// correction, and never handles PALETTE COLOR since that photometric interpretation has
/// SamplesPerPixel=1 and `render_rgb_frame` only accepts true RGB/YBR). This one instead calls
/// `render_single_frame` - the SAME dispatch the classic JPEG/PNG render path uses - so a color
/// texture matches the JPEG preview exactly: PALETTE COLOR is transparently handled via
/// `render_single_frame`'s own `render_grayscale_frame` → `render_palette_color_frame` redirect
/// (SamplesPerPixel=1 routes there, not through `render_rgb_frame`), and an embedded ICC profile
/// is applied identically to the JPEG path (`render_single_frame` does this internally whenever
/// `samples_per_pixel == 3`) - without both, "upgrading" a pane from its initial JPEG render to
/// this texture could visibly shift color, defeating the silent-upgrade contract every other GPU
/// tier already honors. No VOI-window stripping needed either way - color has no window/level
/// concept, unlike `decode_frame_grayscale_values`'s modality-LUT step.
pub(crate) fn decode_frame_texture_rgb_values(
    object: &DefaultDicomObject,
    frame_index: usize,
) -> Result<(RenderMetadata, Vec<u8>), RenderError> {
    let options = RenderPipelineOptions { frame_index, ..Default::default() };

    if let Some(frame_object) = try_decode_single_frame_object(object, frame_index)? {
        let metadata = read_render_metadata(&frame_object)?;
        assert_is_color(&metadata)?;
        let mut frame_options = options.clone();
        frame_options.frame_index = 0;
        let rendered = render_single_frame(&frame_object, &metadata, &frame_options)?;
        return Ok((metadata, rendered.bytes));
    }

    let working = ensure_native_render_object(object)?;
    let metadata = read_render_metadata(working.as_ref())?;
    assert_is_color(&metadata)?;
    let rendered = render_single_frame(working.as_ref(), &metadata, &options)?;
    Ok((metadata, rendered.bytes))
}

fn assert_is_color(metadata: &RenderMetadata) -> Result<(), RenderError> {
    let is_color = metadata.samples_per_pixel > 1
        || metadata.photometric_interpretation.eq_ignore_ascii_case("PALETTE COLOR");
    if is_color {
        Ok(())
    } else {
        Err(RenderError::UnsupportedSamplesPerPixel(metadata.samples_per_pixel))
    }
}

// Enhanced Multi-frame objects (Enhanced CT/MR/PET, etc.) don't carry RescaleSlope/Intercept or
// WindowCenter/Width at the top level at all - those live inside a functional group item instead:
// PerFrameFunctionalGroupsSequence[frame_index] if the value varies per frame (e.g. multi-energy
// CT), falling back to SharedFunctionalGroupsSequence's one item if it's the same for every frame
// (the common case). Looks up `value_tag` inside `sub_sequence_tag` under either group, per-frame
// first. Returns `None` if the object has no functional groups at all (a classic single-frame or
// legacy multiframe object - callers already check the top-level tag themselves first) or the
// tag genuinely isn't present in whichever functional group item was found.
fn functional_group_numeric_value(
    object: &DefaultDicomObject,
    frame_index: usize,
    sub_sequence_tag: Tag,
    value_tag: Tag,
) -> Option<f64> {
    let lookup = |group_tag: Tag, item_index: usize| -> Option<f64> {
        let group_item = object.get(group_tag)?.value().items()?.get(item_index)?;
        let sub_item = group_item.get(sub_sequence_tag)?.value().items()?.first()?;
        sub_item
            .get(value_tag)
            .and_then(|element| first_numeric_value(element.to_str().ok().as_deref()))
    };
    lookup(tags::PER_FRAME_FUNCTIONAL_GROUPS_SEQUENCE, frame_index)
        .or_else(|| lookup(tags::SHARED_FUNCTIONAL_GROUPS_SEQUENCE, 0))
}

/// A parsed Modality/VOI LUT Sequence item's descriptor + data (PS3.3 C.11.1.1.2 / C.11.2.1.2).
/// Only the sequence's first item is used - `dcmnorm` doesn't expose a way to pick among
/// multiple VOI LUT items the way `LUTExplanation`-driven UI pickers do, so the first one (the
/// object's own default) is what renders.
struct Lut {
    entries: usize,
    first_input_value: i32,
    bits_per_entry: u16,
    /// One raw output sample per entry. Per PS3.3, LUT Data for Modality/VOI LUT (unlike Palette
    /// Color LUT Data, which has a legacy 8-bit-packed form) is always 16-bit words (US/SS/OW),
    /// regardless of `bits_per_entry`.
    values: Vec<u16>,
}

impl Lut {
    /// Map one input sample through the LUT, per PS3.3 C.11.1.1.2 / C.11.2.1.2: values at or
    /// below the descriptor's first input value clamp to the first entry, values at or above the
    /// range clamp to the last entry.
    fn lookup(&self, input: f64) -> u16 {
        if self.entries == 0 {
            return 0;
        }
        let offset = input.round() as i64 - i64::from(self.first_input_value);
        let index = offset.clamp(0, self.entries as i64 - 1) as usize;
        self.values[index]
    }

    /// The largest value an entry can hold, per `bits_per_entry` - used to scale LUT output down
    /// to the 8-bit range `dcmnorm` renders to.
    fn max_output(&self) -> f64 {
        ((1u32 << u32::from(self.bits_per_entry.clamp(1, 16))) - 1).max(1) as f64
    }
}

/// Read the first item of a Modality/VOI LUT Sequence at `sequence_tag`, if present and
/// well-formed. `None` (not an error) covers every reason it isn't usable - absent, empty,
/// missing descriptor/data, or a malformed descriptor - so callers can fall straight through to
/// their existing Rescale Slope/Intercept or Window Center/Width handling exactly as if the
/// sequence had never been there.
fn read_lut_from_sequence(object: &DefaultDicomObject, sequence_tag: Tag) -> Option<Lut> {
    let element = object.get(sequence_tag)?;
    let Value::Sequence(sequence) = element.value() else { return None };
    let item = sequence.items().first()?;

    let descriptor = item.get(tags::LUT_DESCRIPTOR)?.value().to_multi_int::<i32>().ok()?;
    if descriptor.len() < 3 {
        return None;
    }
    let entries = if descriptor[0] == 0 { 65_536usize } else { descriptor[0].max(0) as usize };
    let first_input_value = descriptor[1];
    let bits_per_entry = descriptor[2].clamp(1, 16) as u16;
    if entries == 0 {
        return None;
    }

    let bytes = item.get(tags::LUT_DATA)?.value().to_bytes().ok()?;
    if bytes.len() < entries * 2 {
        return None;
    }
    let values: Vec<u16> = bytes[..entries * 2]
        .chunks_exact(2)
        .map(|chunk| u16::from_le_bytes([chunk[0], chunk[1]]))
        .collect();

    Some(Lut { entries, first_input_value, bits_per_entry, values })
}

fn apply_modality_lut(object: &DefaultDicomObject, frame_index: usize, values: &mut [f64]) {
    // Modality LUT Sequence takes precedence over Rescale Slope/Intercept when present - PS3.3
    // C.11.1 requires a conformant file to carry only one of the two, but a real-world file that
    // (non-conformantly) has both should still prefer the LUT, which is the more specific/
    // authoritative of the two mechanisms.
    if let Some(lut) = read_lut_from_sequence(object, tags::MODALITY_LUT_SEQUENCE) {
        for value in values.iter_mut() {
            *value = f64::from(lut.lookup(*value));
        }
        return;
    }

    let slope = object
        .get(tags::RESCALE_SLOPE)
        .and_then(|element| first_numeric_value(element.to_str().ok().as_deref()))
        .or_else(|| {
            functional_group_numeric_value(
                object,
                frame_index,
                tags::PIXEL_VALUE_TRANSFORMATION_SEQUENCE,
                tags::RESCALE_SLOPE,
            )
        })
        .unwrap_or(1.0);
    let intercept = object
        .get(tags::RESCALE_INTERCEPT)
        .and_then(|element| first_numeric_value(element.to_str().ok().as_deref()))
        .or_else(|| {
            functional_group_numeric_value(
                object,
                frame_index,
                tags::PIXEL_VALUE_TRANSFORMATION_SEQUENCE,
                tags::RESCALE_INTERCEPT,
            )
        })
        .unwrap_or(0.0);

    if (slope - 1.0).abs() < f64::EPSILON && intercept.abs() < f64::EPSILON {
        return;
    }

    for value in values {
        *value = (*value * slope) + intercept;
    }
}

fn validate_user_window_overrides(options: &RenderPipelineOptions) -> Result<(), RenderError> {
    if let Some(window_width) = options.window_width {
        if options.window_center.is_none() {
            return Err(RenderError::InvalidWindow(
                "window width is set but window center is missing".to_owned(),
            ));
        }

        if window_width <= 0.0 {
            return Err(RenderError::InvalidWindow(
                "window width must be greater than zero".to_owned(),
            ));
        }
    }

    Ok(())
}

fn resolve_window(
    object: &DefaultDicomObject,
    options: &RenderPipelineOptions,
) -> Result<(Option<f64>, Option<f64>), RenderError> {
    if let Some(window_width) = options.window_width {
        if options.window_center.is_none() {
            return Err(RenderError::InvalidWindow(
                "window width is set but window center is missing".to_owned(),
            ));
        }

        if window_width <= 0.0 {
            return Err(RenderError::InvalidWindow(
                "window width must be greater than zero".to_owned(),
            ));
        }

        return Ok((options.window_center, Some(window_width)));
    }

    if options.window_center.is_some() {
        return Ok((options.window_center, None));
    }

    Ok(resolve_default_window(object, options.frame_index))
}

/// The object's OWN default VOI window for `frame_index` - top-level WindowCenter/WindowWidth
/// first, falling back to the per-frame functional group the same way `apply_modality_lut` falls
/// back for RescaleSlope/Intercept (see that function's doc). `None`/`None` means "this object has
/// no usable VOI window anywhere" - callers fall back to their own min/max-derived normalization
/// rather than treating that as an error.
///
/// pub(crate): the one shared place both rendering pipelines resolve a default window from a
/// DICOM object - the classic JPEG/PNG pipeline via `resolve_window` above, and the GPU texture
/// pipeline via `texture_export::pack_dicom_frame_texture`/`pack_dicom_frame_stack_texture` -
/// so a file's own real VOI preset (wherever in the object it lives) applies identically to
/// both, instead of the texture pipeline falling straight through to a naive whole-image min/max
/// span whenever the tag isn't at the top level.
pub(crate) fn resolve_default_window(object: &DefaultDicomObject, frame_index: usize) -> (Option<f64>, Option<f64>) {
    let center = object
        .get(tags::WINDOW_CENTER)
        .and_then(|element| first_numeric_value(element.to_str().ok().as_deref()))
        .or_else(|| {
            functional_group_numeric_value(object, frame_index, tags::FRAME_VOILUT_SEQUENCE, tags::WINDOW_CENTER)
        });
    let width = object
        .get(tags::WINDOW_WIDTH)
        .and_then(|element| first_numeric_value(element.to_str().ok().as_deref()))
        .or_else(|| {
            functional_group_numeric_value(object, frame_index, tags::FRAME_VOILUT_SEQUENCE, tags::WINDOW_WIDTH)
        });

    // Real-world datasets sometimes carry malformed VOI values (for example,
    // width=0 or width without center). Ignore those tags and fall back to
    // robust min/max normalization instead of failing the whole render.
    if width.is_some() && center.is_none() {
        return (None, None);
    }

    if let Some(window_width) = width {
        if window_width <= 0.0 {
            return (None, None);
        }
    }

    (center, width)
}

/// Map already-modality-transformed values through a VOI LUT Sequence item, scaling its
/// (typically >8-bit) output down to the 8-bit range `dcmnorm` renders to.
fn apply_voi_lut(values: &[f64], lut: &Lut) -> Vec<u8> {
    let max_output = lut.max_output();
    values
        .iter()
        .map(|&value| ((f64::from(lut.lookup(value)) / max_output) * 255.0).clamp(0.0, 255.0) as u8)
        .collect()
}

// pub(crate): also used by dicom_io::volume to window a reformatted plane's interpolated values.
pub(crate) fn apply_voi_window(values: &[f64], center: Option<f64>, width: Option<f64>) -> Vec<u8> {
    let (Some(center), Some(width)) = (center, width) else {
        return normalize_to_u8(values);
    };

    if values.is_empty() {
        return Vec::new();
    }

    let min_value = values.iter().copied().fold(f64::INFINITY, f64::min);
    let max_value = values.iter().copied().fold(f64::NEG_INFINITY, f64::max);

    let mut rendered = Vec::with_capacity(values.len());
    let denominator = (width - 1.0).max(1.0);
    let lower = center - 0.5 - (width - 1.0) / 2.0;
    let upper = center - 0.5 + (width - 1.0) / 2.0;

    // Some instances carry window settings that barely intersect their pixel domain,
    // which tends to white-out the image. Fall back to robust min/max normalization.
    if upper <= min_value || lower >= max_value {
        return normalize_to_u8(values);
    }

    let data_span = (max_value - min_value).max(1.0);
    let overlap_low = lower.max(min_value);
    let overlap_high = upper.min(max_value);
    let overlap_span = (overlap_high - overlap_low).max(0.0);
    if overlap_span / data_span < 0.05 {
        return normalize_to_u8(values);
    }

    let inside_count = values
        .iter()
        .filter(|value| **value >= lower && **value <= upper)
        .count();
    if inside_count * 100 < values.len() {
        return normalize_to_u8(values);
    }

    for value in values {
        let mapped = if *value <= lower {
            0.0
        } else if *value > upper {
            255.0
        } else {
            ((*value - (center - 0.5)) / denominator + 0.5) * 255.0
        };
        rendered.push(mapped.clamp(0.0, 255.0) as u8);
    }

    rendered
}

fn normalize_to_u8(values: &[f64]) -> Vec<u8> {
    if values.is_empty() {
        return Vec::new();
    }

    let min = values.iter().copied().fold(f64::INFINITY, f64::min);
    let max = values.iter().copied().fold(f64::NEG_INFINITY, f64::max);

    if (max - min).abs() < f64::EPSILON {
        return vec![0u8; values.len()];
    }

    values
        .iter()
        .map(|value| (((*value - min) / (max - min)) * 255.0).clamp(0.0, 255.0) as u8)
        .collect()
}

fn encode_png(frame: &RenderedFramePixels) -> Result<Vec<u8>, RenderError> {
    let _scope = perf::scope("render.encode_png");
    let mut output = Vec::new();
    let encoder = PngEncoder::new(&mut output);
    encoder.write_image(
        &frame.bytes,
        u32::from(frame.width),
        u32::from(frame.height),
        color_type(frame.samples_per_pixel).into(),
    )?;
    Ok(output)
}

fn encode_jpeg(frame: &RenderedFramePixels, quality: u8) -> Result<Vec<u8>, RenderError> {
    let _scope = perf::scope("render.encode_jpeg");
    let mut output = Vec::new();
    let clamped_quality = quality.clamp(1, 100);
    let mut encoder = JpegEncoder::new_with_quality(&mut output, clamped_quality);
    encoder.encode(
        &frame.bytes,
        u32::from(frame.width),
        u32::from(frame.height),
        color_type(frame.samples_per_pixel).into(),
    )?;
    Ok(output)
}

fn color_type(samples_per_pixel: u16) -> ColorType {
    if samples_per_pixel == 1 {
        ColorType::L8
    } else {
        ColorType::Rgb8
    }
}

fn draw_bounding_boxes(frame: &mut RenderedFramePixels, options: &RenderPipelineOptions) {
    if options.bounding_boxes.is_empty() {
        return;
    }

    let width = u32::from(frame.width);
    let height = u32::from(frame.height);
    let [cr, cg, cb] = options.bounding_box_color;

    for bbox in &options.bounding_boxes {
        let x_start = resolve_axis_start(bbox.x, width);
        let box_width = resolve_axis_length(&bbox.width, width);
        let x_end = x_start.saturating_add(box_width).min(width);
        let y_start = resolve_axis_start(bbox.y, height);
        let box_height = resolve_axis_length(&bbox.height, height);
        let y_end = y_start.saturating_add(box_height).min(height);

        for y in y_start..y_end {
            for x in x_start..x_end {
                let pixel_index = (y * width + x) as usize;
                match frame.samples_per_pixel {
                    1 => {
                        let gray = rgb_to_luma([cr, cg, cb]);
                        frame.bytes[pixel_index] = gray;
                    }
                    3 => {
                        frame.bytes[pixel_index * 3] = cr;
                        frame.bytes[pixel_index * 3 + 1] = cg;
                        frame.bytes[pixel_index * 3 + 2] = cb;
                    }
                    _ => {}
                }
            }
        }
    }
}

/// Overlay tag offsets within a `60xx` group.
const OVERLAY_ROWS_OFFSET: u16 = 0x0010;
const OVERLAY_COLUMNS_OFFSET: u16 = 0x0011;
const OVERLAY_TYPE_OFFSET: u16 = 0x0040;
const OVERLAY_ORIGIN_OFFSET: u16 = 0x0050;
const IMAGE_FRAME_ORIGIN_OFFSET: u16 = 0x0051;
const OVERLAY_BITS_ALLOCATED_OFFSET: u16 = 0x0100;
const OVERLAY_BIT_POSITION_OFFSET: u16 = 0x0102;
const NUMBER_OF_FRAMES_IN_OVERLAY_OFFSET: u16 = 0x0015;
const OVERLAY_DESCRIPTION_OFFSET: u16 = 0x0022;
const OVERLAY_DATA_OFFSET: u16 = 0x3000;
const OVERLAY_LABEL_OFFSET: u16 = 0x1500;

/// Discovers every overlay plane present on `object`, in ascending group order (`0x6000` ..
/// `0x601E`, even groups only - up to 16 overlays per DICOM PS3.3 C.9.2). Cheap: tag lookups
/// only, no pixel decoding.
fn discover_overlays(object: &DefaultDicomObject) -> Vec<OverlaySummary> {
    let mut overlays = Vec::new();

    for group in (0x6000u16..=0x601Eu16).step_by(2) {
        let Some(rows) = object
            .get(Tag(group, OVERLAY_ROWS_OFFSET))
            .and_then(|element| element.uint16().ok())
        else {
            continue;
        };
        let Some(columns) = object
            .get(Tag(group, OVERLAY_COLUMNS_OFFSET))
            .and_then(|element| element.uint16().ok())
        else {
            continue;
        };

        let overlay_type = trimmed_str(object, Tag(group, OVERLAY_TYPE_OFFSET));
        let label = trimmed_str(object, Tag(group, OVERLAY_LABEL_OFFSET))
            .or_else(|| trimmed_str(object, Tag(group, OVERLAY_DESCRIPTION_OFFSET)));

        overlays.push(OverlaySummary {
            index: overlays.len(),
            group,
            rows,
            columns,
            overlay_type,
            label,
        });
    }

    overlays
}

fn trimmed_str(object: &DefaultDicomObject, tag: Tag) -> Option<String> {
    object
        .get(tag)
        .and_then(|element| element.to_str().ok())
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
}

/// Errors early (mirrors `validate_user_window_overrides`) if `options.overlay_index` names an
/// overlay that doesn't exist, regardless of whether `show_overlays` would actually use it.
fn validate_overlay_index(
    options: &RenderPipelineOptions,
    overlays: &[OverlaySummary],
) -> Result<(), RenderError> {
    if let Some(index) = options.overlay_index {
        if index >= overlays.len() {
            return Err(RenderError::InvalidOverlayIndex {
                requested: index,
                available: overlays.len(),
            });
        }
    }

    Ok(())
}

/// Resolves which overlay (by `OverlaySummary::index`) should be composited, or `None` if
/// overlays are disabled or absent. `options.overlay_index` must already have been validated via
/// `validate_overlay_index`.
fn resolve_selected_overlay(
    options: &RenderPipelineOptions,
    overlays: &[OverlaySummary],
) -> Option<usize> {
    if !options.show_overlays || overlays.is_empty() {
        return None;
    }

    let index = options.overlay_index.unwrap_or(0);
    (index < overlays.len()).then_some(index)
}

/// Composites `overlay` onto `frame` (still at native/pre-resize resolution) in `color`.
///
/// `local_frame_index` is the frame index within `object`'s own pixel data (used for the
/// embedded-in-pixel-data style, via the same raw-byte extraction `get_frame_bytes` already
/// provides for display rendering). `original_frame_index` is the frame index within the source
/// instance's original frame numbering (used to resolve which sub-frame of a multi-frame
/// `OverlayData` blob applies, per `NumberOfFramesInOverlay`/`ImageFrameOrigin`) - the two differ
/// when `object` is a single already-decoded frame extracted from a larger multi-frame instance.
fn composite_overlay(
    frame: &mut RenderedFramePixels,
    object: &DefaultDicomObject,
    metadata: &RenderMetadata,
    local_frame_index: usize,
    original_frame_index: usize,
    overlay: &OverlaySummary,
    color: [u8; 3],
) -> Result<(), RenderError> {
    let rows = usize::from(overlay.rows);
    let columns = usize::from(overlay.columns);
    if rows == 0 || columns == 0 {
        return Ok(());
    }

    let overlay_bits_allocated = object
        .get(Tag(overlay.group, OVERLAY_BITS_ALLOCATED_OFFSET))
        .and_then(|element| element.uint16().ok())
        .unwrap_or(1);

    let bitmap = if overlay_bits_allocated > 1 && overlay_bits_allocated == metadata.bits_allocated
    {
        let bit_position = object
            .get(Tag(overlay.group, OVERLAY_BIT_POSITION_OFFSET))
            .and_then(|element| element.uint16().ok())
            .unwrap_or(0);
        decode_embedded_overlay_bits(object, metadata, local_frame_index, rows, columns, bit_position)?
    } else {
        match decode_overlay_data_bits(object, overlay, rows, columns, original_frame_index)? {
            Some(bitmap) => bitmap,
            None => return Ok(()),
        }
    };

    let origin = object
        .get(Tag(overlay.group, OVERLAY_ORIGIN_OFFSET))
        .and_then(|element| element.to_multi_int::<i32>().ok())
        .filter(|values| values.len() == 2)
        .map(|values| (values[0], values[1]))
        .unwrap_or((1, 1));

    paint_overlay_bitmap(frame, &bitmap, rows, columns, origin, color);
    Ok(())
}

fn paint_overlay_bitmap(
    frame: &mut RenderedFramePixels,
    bitmap: &[bool],
    rows: usize,
    columns: usize,
    origin: (i32, i32),
    color: [u8; 3],
) {
    let (origin_row, origin_col) = origin;
    let image_width = i64::from(frame.width);
    let image_height = i64::from(frame.height);
    let luma = rgb_to_luma(color);

    for overlay_row in 0..rows {
        let image_row = i64::from(origin_row) - 1 + overlay_row as i64;
        if image_row < 0 || image_row >= image_height {
            continue;
        }

        for overlay_col in 0..columns {
            let bit_index = overlay_row * columns + overlay_col;
            if !bitmap.get(bit_index).copied().unwrap_or(false) {
                continue;
            }

            let image_col = i64::from(origin_col) - 1 + overlay_col as i64;
            if image_col < 0 || image_col >= image_width {
                continue;
            }

            let pixel_index = image_row as usize * frame.width as usize + image_col as usize;
            match frame.samples_per_pixel {
                1 => frame.bytes[pixel_index] = luma,
                3 => {
                    let base = pixel_index * 3;
                    frame.bytes[base] = color[0];
                    frame.bytes[base + 1] = color[1];
                    frame.bytes[base + 2] = color[2];
                }
                _ => {}
            }
        }
    }
}

/// Legacy overlay encoding: the overlay bit lives at `bit_position` of each raw pixel-data
/// sample. Uses `get_frame_bytes`, which returns raw *unmasked* words - `decode_grayscale_values`
/// masks these same bits away via `bits_stored` before display, so this must read the raw bytes
/// directly rather than reuse the display-decode path.
fn decode_embedded_overlay_bits(
    object: &DefaultDicomObject,
    metadata: &RenderMetadata,
    frame_index: usize,
    rows: usize,
    columns: usize,
    bit_position: u16,
) -> Result<Vec<bool>, RenderError> {
    let frame_bytes = get_frame_bytes(object, metadata, frame_index)?;
    let pixel_count = rows * columns;
    let expected = pixel_count * 2;
    if frame_bytes.len() < expected {
        return Err(RenderError::InvalidPixelDataLength {
            expected,
            actual: frame_bytes.len(),
        });
    }

    let mut bits = Vec::with_capacity(pixel_count);
    for chunk in frame_bytes[..expected].chunks_exact(2) {
        let raw = u16::from_le_bytes([chunk[0], chunk[1]]);
        bits.push(((raw >> bit_position) & 1) != 0);
    }
    Ok(bits)
}

/// Current-standard overlay encoding: a distinct `OverlayData` element, 1 bit per pixel, packed
/// **LSB-first** (pixel `p` is byte `p/8`, bit `p%8`) - confirmed empirically against
/// `overlay.dcm`'s bundled dose-report text overlay (LSB-first decodes to legible text; the
/// MSB-first order this crate's *pixel data* 1-bpp path uses elsewhere produces garbled output).
///
/// Returns `Ok(None)` when there's no `OverlayData` element, or when a multi-frame overlay
/// (`NumberOfFramesInOverlay` > 1) doesn't cover `original_frame_index`.
fn decode_overlay_data_bits(
    object: &DefaultDicomObject,
    overlay: &OverlaySummary,
    rows: usize,
    columns: usize,
    original_frame_index: usize,
) -> Result<Option<Vec<bool>>, RenderError> {
    let Ok(element) = object.element(Tag(overlay.group, OVERLAY_DATA_OFFSET)) else {
        return Ok(None);
    };
    let Ok(data) = element.to_bytes() else {
        return Ok(None);
    };

    let pixel_count = rows * columns;
    let plane_len = pixel_count.div_ceil(8);

    let number_of_frames_in_overlay = object
        .get(Tag(overlay.group, NUMBER_OF_FRAMES_IN_OVERLAY_OFFSET))
        .and_then(|element| element.to_str().ok())
        .and_then(|text| {
            text.split('\\')
                .next()
                .and_then(|value| value.trim().parse::<usize>().ok())
        })
        .unwrap_or(1);

    let sub_frame = if number_of_frames_in_overlay > 1 {
        let image_frame_origin = object
            .get(Tag(overlay.group, IMAGE_FRAME_ORIGIN_OFFSET))
            .and_then(|element| element.uint16().ok())
            .unwrap_or(1);
        let start = usize::from(image_frame_origin).saturating_sub(1);
        if original_frame_index < start || original_frame_index >= start + number_of_frames_in_overlay
        {
            return Ok(None);
        }
        original_frame_index - start
    } else {
        0
    };

    let offset = sub_frame * plane_len;
    if data.len() < offset + plane_len {
        return Err(RenderError::InvalidPixelDataLength {
            expected: offset + plane_len,
            actual: data.len(),
        });
    }

    let plane_bytes = &data[offset..offset + plane_len];
    let mut bits = Vec::with_capacity(pixel_count);
    for pixel_index in 0..pixel_count {
        let byte = plane_bytes[pixel_index / 8];
        let bit = pixel_index % 8;
        bits.push(((byte >> bit) & 1) != 0);
    }
    Ok(Some(bits))
}

#[cfg(test)]
mod tests {
    use super::{
        apply_modality_lut, apply_voi_lut, clamped_box, mpeg4_input_pixel_format,
        mpeg4_muxer_name, mpeg4_video_filter, normalize_decoded_render_attributes,
        read_lut_from_sequence, render_dicom_frame, try_extract_passthrough_jpeg_frame,
        ybr_rct_to_rgb, ybr_to_rgb, BoundingBox, BoxLength, Lut, RenderFrameOutput,
        RenderOutputFormat, RenderPipelineOptions,
    };
    use crate::dicom_io::read_dicom_file;
    use dcmnorm_core::{DataElement, PrimitiveValue, Tag, VR};
    use dcmnorm_dictionary::tags;
    use std::path::PathBuf;

    #[test]
    fn resolves_negative_offsets_from_right_and_bottom() {
        let bbox = BoundingBox {
            x: -20,
            y: -10,
            width: BoxLength::Pixels(5),
            height: BoxLength::Pixels(4),
        };

        assert_eq!(clamped_box(&bbox, 100, 50), (80, 85, 40, 44));
    }

    #[test]
    fn clamps_negative_offsets_beyond_image_edge() {
        let bbox = BoundingBox {
            x: -500,
            y: -500,
            width: BoxLength::Pixels(10),
            height: BoxLength::Pixels(10),
        };

        assert_eq!(clamped_box(&bbox, 64, 32), (0, 10, 0, 10));
    }

    #[test]
    fn resolves_percentage_width_and_height() {
        let bbox = BoundingBox {
            x: -20,
            y: -10,
            width: BoxLength::Percent(25.0),
            height: BoxLength::Percent(50.0),
        };

        assert_eq!(clamped_box(&bbox, 200, 100), (180, 200, 90, 100));
    }

    #[test]
    fn treats_negative_zero_as_edge_anchor() {
        let bbox = BoundingBox {
            x: i32::MIN,
            y: 0,
            width: BoxLength::Percent(50.0),
            height: BoxLength::Percent(50.0),
        };

        assert_eq!(clamped_box(&bbox, 100, 80), (100, 100, 0, 40));
    }

    #[test]
    fn maybe_pad_frame_pads_to_square() {
        use super::{maybe_pad_frame, RenderPipelineOptions, RenderedFramePixels};

        let input = RenderedFramePixels {
            width: 2,
            height: 1,
            samples_per_pixel: 1,
            bytes: vec![255, 255], // white
        };
        let options = RenderPipelineOptions {
            scale_max_size: Some(4),
            pad: true,
            pad_color: [100, 100, 100],
            ..RenderPipelineOptions::default()
        };

        let result = maybe_pad_frame(input, &options);
        assert_eq!(result.width, 4);
        assert_eq!(result.height, 4);
        assert_eq!(result.samples_per_pixel, 1);

        // Dimensions 4x4=16 bytes.
        assert_eq!(result.bytes.len(), 16);

        // X offset = (4 - 2) / 2 = 1. Y offset = (4 - 1) / 2 = 1.
        // Source image goes into Row 1 (0-indexed), Columns 1 and 2.
        // The rest should be the padding color (gray).
        // Note: Luma math (0.2126*100 + 0.7152*100 + 0.0722*100) = 100.

        // Row 0 (padded)
        assert_eq!(result.bytes[0], 100);
        // Row 1, pixel 0 (padded), pixel 1 & 2 (original data), pixel 3 (padded)
        assert_eq!(result.bytes[4], 100);
        assert_eq!(result.bytes[5], 255);
        assert_eq!(result.bytes[6], 255);
        assert_eq!(result.bytes[7], 100);
    }

    #[test]
    fn ybr_ict_neutral_chroma_maps_to_gray() {
        assert_eq!(ybr_to_rgb(243, 128, 128), (243, 243, 243));
    }

    #[test]
    fn ybr_rct_round_trip_example() {
        // RGB (200,100,50) -> YBR_RCT (112,78,228) for 8-bit data.
        assert_eq!(ybr_rct_to_rgb(112, 78, 228), (200, 100, 50));
    }

    #[test]
    fn normalize_decoded_attributes_keeps_ybr_ict_without_mct() {
        let fixture = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("test")
            .join("files")
            .join("dx.dcm");
        let mut object = read_dicom_file(&fixture).expect("fixture should be readable");
        object.put(DataElement::new(
            tags::SAMPLES_PER_PIXEL,
            VR::US,
            PrimitiveValue::from(3u16),
        ));
        object.put(DataElement::new(
            tags::PHOTOMETRIC_INTERPRETATION,
            VR::CS,
            PrimitiveValue::from("YBR_ICT"),
        ));

        // A codestream that did not apply MCT still needs the manual
        // YBR_ICT->RGB conversion downstream, so the label must be kept.
        normalize_decoded_render_attributes(&mut object, "1.2.840.10008.1.2.4.91", Some(false));

        let photometric = object
            .get(tags::PHOTOMETRIC_INTERPRETATION)
            .and_then(|element| element.to_str().ok())
            .unwrap_or_default();
        assert_eq!(photometric, "YBR_ICT");

        let planar = object
            .get(tags::PLANAR_CONFIGURATION)
            .and_then(|element| element.uint16().ok())
            .unwrap_or(u16::MAX);
        assert_eq!(planar, 0);
    }

    #[test]
    fn normalize_decoded_attributes_relabels_ybr_rct_to_rgb_when_mct_used() {
        // Regression test: a conformant JPEG 2000 encoder signals MCT in the
        // codestream, and the decoder reverses it internally, so the decoded
        // bytes are already RGB. Keeping the YBR_RCT label in that case made
        // render_rgb_frame apply a second, incorrect color conversion -
        // corrupting colors (e.g. a solid green cast) for ordinary
        // JPEG2000-compressed secondary captures/ultrasound images.
        let fixture = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("test")
            .join("files")
            .join("dx.dcm");
        let mut object = read_dicom_file(&fixture).expect("fixture should be readable");
        object.put(DataElement::new(
            tags::SAMPLES_PER_PIXEL,
            VR::US,
            PrimitiveValue::from(3u16),
        ));
        object.put(DataElement::new(
            tags::PHOTOMETRIC_INTERPRETATION,
            VR::CS,
            PrimitiveValue::from("YBR_RCT"),
        ));

        normalize_decoded_render_attributes(&mut object, "1.2.840.10008.1.2.4.90", Some(true));

        let photometric = object
            .get(tags::PHOTOMETRIC_INTERPRETATION)
            .and_then(|element| element.to_str().ok())
            .unwrap_or_default();
        assert_eq!(photometric, "RGB");
    }

    #[test]
    fn chooses_mov_muxer_for_mov_extension() {
        assert_eq!(mpeg4_muxer_name(&PathBuf::from("out.mov")), "mov");
    }

    #[test]
    fn chooses_mp4_muxer_for_mpeg4_extension() {
        assert_eq!(mpeg4_muxer_name(&PathBuf::from("out.mpeg4")), "mp4");
    }

    #[test]
    fn uses_even_dimension_padding_filter_for_movie_output() {
        assert_eq!(mpeg4_video_filter(), "pad=width=ceil(iw/2)*2:height=ceil(ih/2)*2");
    }

    #[test]
    fn maps_movie_input_pixel_formats() {
        assert_eq!(mpeg4_input_pixel_format(1).unwrap(), "gray");
        assert_eq!(mpeg4_input_pixel_format(3).unwrap(), "rgb24");

        let error = mpeg4_input_pixel_format(2).unwrap_err().to_string();
        assert!(error.contains("unsupported rendered movie samples-per-pixel value"));
    }

    fn fixture(name: &str) -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("test").join("files").join(name)
    }

    fn render_raw(name: &str) -> RenderFrameOutput {
        let object = read_dicom_file(fixture(name)).expect("fixture should be readable");
        render_dicom_frame(&object, RenderOutputFormat::Raw, &RenderPipelineOptions::default())
            .expect("fixture should render")
    }

    /// A deliberately non-linear, non-identity LUT (unlike both real fixtures below, whose
    /// embedded LUTs happen to be exact linear identity transforms - see
    /// `voi_lut_sequence_png_output_matches_the_source_bytes_exactly`'s doc comment) so this
    /// test can't coincidentally pass against a broken lookup/scaling implementation.
    fn synthetic_lut() -> Lut {
        Lut { entries: 4, first_input_value: 10, bits_per_entry: 8, values: vec![5, 200, 100, 250] }
    }

    #[test]
    fn lut_lookup_indexes_by_first_input_value_and_clamps_out_of_range_inputs() {
        let lut = synthetic_lut();
        assert_eq!(lut.lookup(10.0), 5);
        assert_eq!(lut.lookup(11.0), 200);
        assert_eq!(lut.lookup(12.0), 100);
        assert_eq!(lut.lookup(13.0), 250);
        // below the descriptor's range: clamps to the first entry
        assert_eq!(lut.lookup(9.0), 5);
        assert_eq!(lut.lookup(-1000.0), 5);
        // above the descriptor's range: clamps to the last entry
        assert_eq!(lut.lookup(14.0), 250);
        assert_eq!(lut.lookup(1000.0), 250);
    }

    #[test]
    fn apply_voi_lut_scales_output_by_bits_per_entry() {
        let lut = synthetic_lut(); // bits_per_entry: 8, so max_output is exactly 255 - a no-op scale
        assert_eq!(apply_voi_lut(&[10.0, 11.0, 9.0, 14.0], &lut), vec![5, 200, 5, 250]);
    }

    #[test]
    fn apply_voi_lut_scales_a_wider_output_range_down_to_eight_bits() {
        let lut = Lut { entries: 2, first_input_value: 0, bits_per_entry: 16, values: vec![0, 65535] };
        // bits_per_entry: 16, so a full-scale 16-bit output must come back scaled to 0/255.
        assert_eq!(apply_voi_lut(&[0.0, 1.0], &lut), vec![0, 255]);
    }

    /// Real end-to-end proof the VOI LUT Sequence path actually runs against a real file (not
    /// just the synthetic `Lut` math above): this fixture's embedded LUT happens to be an exact
    /// linear identity - `LUT[i] = i * 257`, and `257 * 255 == 65535` exactly, so scaling that
    /// back down to 8 bits recovers the original sample unchanged. The source data is already
    /// 8-bit MONOCHROME2, so a correct VOI LUT application reproduces the PNG's decoded pixel
    /// bytes exactly equal to the object's own raw stored bytes.
    #[test]
    fn voi_lut_sequence_png_output_matches_the_source_bytes_exactly() {
        let object =
            read_dicom_file(fixture("voi_lut_sequence.dcm")).expect("fixture should be readable");
        let options = RenderPipelineOptions::default();
        let raw = render_dicom_frame(&object, RenderOutputFormat::Raw, &options)
            .expect("fixture should render raw");
        let png = render_dicom_frame(&object, RenderOutputFormat::Png, &options)
            .expect("fixture should render png");
        let decoded = image::load_from_memory(&png.bytes).expect("valid PNG").to_luma8();
        assert_eq!(decoded.as_raw(), &raw.bytes);
    }

    /// Same idea as the VOI LUT test above, but calls `apply_modality_lut` directly with
    /// synthetic input values against the real embedded LUT, which avoids the ambiguity of going
    /// through PNG rendering's own downstream auto-windowing (a second linear rescale on top of
    /// a near-linear LUT can coincidentally produce the same final bytes regardless of whether
    /// the first one even ran - seen firsthand with this exact fixture during development).
    /// Expected values are this fixture's own real, observed `LUTData` entries (not a formula -
    /// the LUT is close to but not exactly `i * 16`; it clips at the top of the 16-bit range),
    /// read directly from its JSON dump rather than assumed.
    #[test]
    fn modality_lut_sequence_maps_real_embedded_lut_values_precisely() {
        let object = read_dicom_file(fixture("modality_lut_sequence.dcm"))
            .expect("fixture should be readable");
        let lut = read_lut_from_sequence(&object, tags::MODALITY_LUT_SEQUENCE)
            .expect("fixture should have a Modality LUT Sequence");
        assert_eq!((lut.entries, lut.first_input_value, lut.bits_per_entry), (4096, -2048, 16));
        assert_eq!(lut.values[0], 0);
        assert_eq!(lut.values[2048], 32776);
        assert_eq!(lut.values[4095], 65535);

        let mut values = vec![-2048.0, 0.0, 2047.0, -3000.0, 3000.0];
        apply_modality_lut(&object, 0, &mut values);
        assert_eq!(values, vec![0.0, 32776.0, 65535.0, 0.0, 65535.0]);
    }

    #[test]
    fn renders_jpeg2000_lossless_frame() {
        let out = render_raw("mr_jpeg2000_lossless.dcm");
        assert_eq!((out.width, out.height, out.samples_per_pixel), (64, 64, 1));
    }

    /// Real 12-bit JPEG Extended (Process 2 & 4) file - `dcmnorm-jpeg`'s fast/SIMD 8-bit IDCT
    /// path (used by JPEG Baseline etc.) can't handle any precision but 8, so this exercises the
    /// dedicated `HighPrecisionWorker` path added for exactly this case. No uncompressed sibling
    /// of this particular file is available to compare byte-for-byte (unlike the JPEG-LS
    /// lossless test above), so this instead pins the decoded value range: real, non-garbage
    /// 12-bit nuclear medicine pixel data (this file's own actual Modality), not saturated at
    /// the clamp boundary and not all-zero - either of those would indicate a level-shift or
    /// clamping bug in the precision generalization instead of a correct decode.
    #[test]
    fn renders_real_12_bit_jpeg_extended_frame_with_plausible_values() {
        let out = render_raw("jpeg_extended_12bit.dcm");
        assert_eq!((out.width, out.height, out.samples_per_pixel), (256, 1024, 1));

        let values: Vec<u16> =
            out.bytes.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
        let max_value = *values.iter().max().unwrap();
        assert!(max_value > 0, "decoded frame should not be all-zero");
        assert!(
            max_value < 4095,
            "decoded max {max_value} is saturated at the 12-bit clamp boundary - suggests a \
             level-shift/clamping bug rather than real image content"
        );
    }

    /// JPEG-LS is lossless here, so a correct decode reproduces the same MR_small reference
    /// image's pixels exactly - compares against `mr_small.dcm` (the uncompressed encoding of
    /// the same underlying image) rather than hardcoded pixel values, which also transitively
    /// exercises `jpeg_ls.rs`'s real charls FFI decode path end to end.
    #[test]
    fn renders_jpegls_lossless_frame_byte_identical_to_the_uncompressed_reference() {
        let reference = render_raw("mr_small.dcm");
        let out = render_raw("mr_jpegls_lossless.dcm");
        assert_eq!((out.width, out.height, out.samples_per_pixel), (64, 64, 1));
        assert_eq!(out.bytes, reference.bytes);
    }

    #[test]
    fn renders_rle_lossless_frame() {
        let out = render_raw("mr_rle.dcm");
        assert_eq!((out.width, out.height, out.samples_per_pixel), (64, 64, 1));
    }

    /// Pins dcmnorm's own in-house JPEG decoder against the exact pixel values upstream
    /// `dicom-rs` verified its own JPEG adapter against (see `dicom-transfer-syntax-registry`'s
    /// excluded `tests/jpeg.rs`) - same reference fixture, same four sample coordinates, no
    /// tolerance needed since JPEG Lossless is, as the name says, lossless.
    #[test]
    fn renders_jpeg_lossless_with_known_pixel_values() {
        let out = render_raw("sc_jpeg_lossless.dcm");
        assert_eq!((out.width, out.height, out.samples_per_pixel), (100, 100, 3));

        let px = |x: usize, y: usize| {
            let i = (y * out.width as usize + x) * 3;
            (out.bytes[i], out.bytes[i + 1], out.bytes[i + 2])
        };
        assert_eq!(px(0, 0), (255, 0, 0));
        assert_eq!(px(50, 50), (128, 128, 255));
        assert_eq!(px(75, 75), (64, 64, 64));
        assert_eq!(px(16, 49), (0, 0, 255));
    }

    /// Same reference fixture/coordinates as `renders_jpeg_lossless_with_known_pixel_values`,
    /// but JPEG Baseline is lossy - matches upstream's own error margin for this exact case.
    #[test]
    fn renders_jpeg_baseline_with_known_pixel_values() {
        let out = render_raw("sc_jpeg_baseline.dcm");
        assert_eq!((out.width, out.height, out.samples_per_pixel), (100, 100, 3));

        let margin = 7u8;
        let px = |x: usize, y: usize| {
            let i = (y * out.width as usize + x) * 3;
            (out.bytes[i], out.bytes[i + 1], out.bytes[i + 2])
        };
        let close = |got: (u8, u8, u8), want: (u8, u8, u8)| {
            got.0.abs_diff(want.0) <= margin
                && got.1.abs_diff(want.1) <= margin
                && got.2.abs_diff(want.2) <= margin
        };
        assert!(close(px(0, 0), (254, 0, 0)));
        assert!(close(px(50, 50), (124, 124, 255)));
        assert!(close(px(75, 75), (64, 64, 64)));
        assert!(close(px(16, 49), (4, 4, 226)));
    }

    /// A truncated/malformed JPEG2000 codestream must fail cleanly, not panic or hang - the same
    /// robustness bar this codebase already holds its in-house JPEG decoder to (see the restart-
    /// marker regression test in dcmnorm-jpeg). The truncation here cuts into the encapsulated
    /// PixelData sequence's own item structure, not just the codestream bytes within an already-
    /// parsed fragment, so the clean error can surface either while reading the data set or
    /// while decoding the frame - either is an acceptable place for it, as long as it's an error
    /// and not a panic.
    #[test]
    fn truncated_jpeg2000_codestream_is_a_clean_error_not_a_panic() {
        let result: Result<(), String> = (|| {
            let object =
                read_dicom_file(fixture("jpeg2000_truncated.dcm")).map_err(|e| e.to_string())?;
            render_dicom_frame(&object, RenderOutputFormat::Raw, &RenderPipelineOptions::default())
                .map_err(|e| e.to_string())?;
            Ok(())
        })();
        assert!(result.is_err(), "expected a clean error, got {result:?}");
    }

    /// Six real files from DCMTK's own JPEG encoder, covering distinct YBR chroma-subsampling
    /// and color-range variants (`+cr`, `+cy+n1/n2/np/s2/s4`) - exactly the kind of "different
    /// real-world encoder wrote a slightly different bitstream" case that catches decoder bugs a
    /// synthetic fixture wouldn't.
    #[test]
    fn renders_dcmtk_jpeg_ybr_chroma_subsampling_variants() {
        for name in [
            "sc_jpeg_dcmtk_cr.dcm",
            "sc_jpeg_dcmtk_ybr_n1.dcm",
            "sc_jpeg_dcmtk_ybr_n2.dcm",
            "sc_jpeg_dcmtk_ybr_np.dcm",
            "sc_jpeg_dcmtk_ybr_s2.dcm",
            "sc_jpeg_dcmtk_ybr_s4.dcm",
        ] {
            let out = render_raw(name);
            assert_eq!(
                (out.width, out.height, out.samples_per_pixel),
                (100, 100, 3),
                "{name} decoded to unexpected dimensions"
            );
        }
    }

    /// `PlanarConfiguration` (0 = interleaved, 1 = planar) must not change the rendered image -
    /// these two fixtures are the same real instance (same SOPInstanceUID/SeriesDescription),
    /// re-encoded with each layout. Note this specifically exercises PNG output: `Raw` output
    /// deliberately preserves the source's native storage layout (see `get_frame_bytes`), so
    /// only the actually-normalizing encode paths (PNG/JPEG) can be expected to agree here.
    #[test]
    fn planar_and_interleaved_rgb_render_to_identical_png_bytes() {
        let planar = read_dicom_file(fixture("rgb_planar.dcm")).expect("fixture should be readable");
        let interleaved =
            read_dicom_file(fixture("rgb_interleaved.dcm")).expect("fixture should be readable");
        let options = RenderPipelineOptions::default();
        let out_planar = render_dicom_frame(&planar, RenderOutputFormat::Png, &options)
            .expect("planar fixture should render");
        let out_interleaved = render_dicom_frame(&interleaved, RenderOutputFormat::Png, &options)
            .expect("interleaved fixture should render");
        assert_eq!(out_planar.bytes, out_interleaved.bytes);
    }

    #[test]
    fn passthrough_jpeg_frame_returns_original_bytes_for_color_jpeg_baseline() {
        let object = read_dicom_file(fixture("sc_jpeg_baseline.dcm")).expect("fixture should be readable");
        let expected_bytes = object
            .element(tags::PIXEL_DATA)
            .expect("PixelData should be present")
            .fragments()
            .expect("JPEG Baseline PixelData should be encapsulated")
            .first()
            .expect("should have at least one fragment")
            .clone();

        let result = try_extract_passthrough_jpeg_frame(&object, 0)
            .expect("passthrough attempt should not error")
            .expect("color JPEG Baseline source with no overlay should qualify for passthrough");

        assert_eq!(result.format, RenderOutputFormat::Jpeg);
        assert_eq!(result.bytes, expected_bytes);
        assert!(result.overlays.is_empty());
    }

    #[test]
    fn passthrough_jpeg_frame_declines_grayscale_source() {
        // Synthetic minimal grayscale (MONOCHROME2, SamplesPerPixel=1) JPEG Baseline object - no
        // real fixture in test/files is both grayscale AND JPEG Baseline ("sc_jpeg_dcmtk_cr.dcm"
        // is DCMTK's "+cr" color-range flag, not "Computed Radiography" - it's YBR color, like its
        // siblings). Only metadata tags are needed: try_extract_passthrough_jpeg_frame must
        // return Ok(None) for a grayscale source based on SamplesPerPixel alone, before ever
        // touching PixelData - passthrough must never apply to grayscale, since it would make
        // server-side window/level impossible afterward.
        use dcmnorm_object::{FileMetaTableBuilder, InMemDicomObject};

        let elements = vec![
            DataElement::new(tags::SOP_CLASS_UID, VR::UI, PrimitiveValue::from("1.2.840.10008.5.1.4.1.1.7")),
            DataElement::new(tags::SOP_INSTANCE_UID, VR::UI, PrimitiveValue::from("1.2.3.4.5.6")),
            DataElement::new(tags::ROWS, VR::US, PrimitiveValue::from(2u16)),
            DataElement::new(tags::COLUMNS, VR::US, PrimitiveValue::from(2u16)),
            DataElement::new(tags::SAMPLES_PER_PIXEL, VR::US, PrimitiveValue::from(1u16)),
            DataElement::new(tags::BITS_ALLOCATED, VR::US, PrimitiveValue::from(8u16)),
            DataElement::new(tags::BITS_STORED, VR::US, PrimitiveValue::from(8u16)),
            DataElement::new(tags::PIXEL_REPRESENTATION, VR::US, PrimitiveValue::from(0u16)),
            DataElement::new(tags::PHOTOMETRIC_INTERPRETATION, VR::CS, PrimitiveValue::from("MONOCHROME2")),
        ];
        let object = InMemDicomObject::from_element_iter(elements)
            .with_meta(
                FileMetaTableBuilder::new()
                    .media_storage_sop_class_uid("1.2.840.10008.5.1.4.1.1.7")
                    .media_storage_sop_instance_uid("1.2.3.4.5.6")
                    .transfer_syntax(dcmnorm_dictionary::uids::JPEG_BASELINE8_BIT),
            )
            .unwrap();

        let result = try_extract_passthrough_jpeg_frame(&object, 0).expect("should not error");
        assert!(result.is_none(), "grayscale source must never be passthrough-eligible");
    }

    #[test]
    fn passthrough_jpeg_frame_declines_when_overlay_present() {
        let mut object =
            read_dicom_file(fixture("sc_jpeg_baseline.dcm")).expect("fixture should be readable");
        object.put(DataElement::new(
            Tag(0x6000, 0x0010),
            VR::US,
            PrimitiveValue::from(8u16),
        ));
        object.put(DataElement::new(
            Tag(0x6000, 0x0011),
            VR::US,
            PrimitiveValue::from(8u16),
        ));

        let result = try_extract_passthrough_jpeg_frame(&object, 0).expect("should not error");
        assert!(result.is_none(), "an overlay-bearing instance must fall through to the decode path");
    }

    #[test]
    fn passthrough_jpeg_frame_declines_non_jpeg_baseline_source() {
        let object = read_dicom_file(fixture("rgb_interleaved.dcm")).expect("fixture should be readable");
        let result = try_extract_passthrough_jpeg_frame(&object, 0).expect("should not error");
        assert!(result.is_none(), "an uncompressed source is never passthrough-eligible");
    }
}

fn maybe_resize_frame(
    frame: RenderedFramePixels,
    options: &RenderPipelineOptions,
) -> RenderedFramePixels {
    let Some((new_width, new_height)) =
        compute_output_dimensions(frame.width, frame.height, options)
    else {
        return frame;
    };

    if new_width == u32::from(frame.width) && new_height == u32::from(frame.height) {
        return frame;
    }

    use image::imageops;
    let _scope = perf::scope("render.resize_frame");
    let filter = resize_filter(
        u32::from(frame.width),
        u32::from(frame.height),
        new_width,
        new_height,
    );
    let resized_bytes = if frame.samples_per_pixel == 1 {
        let img = GrayImage::from_raw(u32::from(frame.width), u32::from(frame.height), frame.bytes)
            .expect("grayscale frame buffer size mismatch");
        imageops::resize(&img, new_width, new_height, filter).into_raw()
    } else {
        let img = RgbImage::from_raw(u32::from(frame.width), u32::from(frame.height), frame.bytes)
            .expect("RGB frame buffer size mismatch");
        imageops::resize(&img, new_width, new_height, filter).into_raw()
    };

    RenderedFramePixels {
        width: new_width as u16,
        height: new_height as u16,
        samples_per_pixel: frame.samples_per_pixel,
        bytes: resized_bytes,
    }
}

fn maybe_pad_frame(
    frame: RenderedFramePixels,
    options: &RenderPipelineOptions,
) -> RenderedFramePixels {
    let Some(max_dim) = options.scale_max_size else {
        return frame;
    };

    if !options.pad {
        return frame;
    }

    let current_w = u32::from(frame.width);
    let current_h = u32::from(frame.height);

    // If we're somehow larger than the square max_dim target, clamping/skipping logic
    // but normally the max_dim IS current_w.max(current_h).
    let target_size = max_dim.max(current_w).max(current_h);

    if target_size == current_w && target_size == current_h {
        return frame; // already square and correctly sized
    }

    let x_offset = (target_size - current_w) / 2;
    let y_offset = (target_size - current_h) / 2;

    use image::{GenericImage, GrayImage, Luma, Rgb, RgbImage};

    let padded_bytes = if frame.samples_per_pixel == 1 {
        let luma = rgb_to_luma(options.pad_color);
        let mut base = GrayImage::from_pixel(target_size, target_size, Luma([luma]));
        let img = GrayImage::from_raw(current_w, current_h, frame.bytes)
            .expect("grayscale frame buffer size mismatch");
        base.copy_from(&img, x_offset, y_offset)
            .expect("failed to overlay grayscale image during padding");
        base.into_raw()
    } else if frame.samples_per_pixel == 3 {
        let [r, g, b] = options.pad_color;
        let mut base = RgbImage::from_pixel(target_size, target_size, Rgb([r, g, b]));
        let img = RgbImage::from_raw(current_w, current_h, frame.bytes)
            .expect("RGB frame buffer size mismatch");
        base.copy_from(&img, x_offset, y_offset)
            .expect("failed to overlay RGB image during padding");
        base.into_raw()
    } else {
        // Handle other formats (e.g. unknown channels) by bypassing padding
        // or panicking explicitly to protect memory consistency.
        return frame;
    };

    RenderedFramePixels {
        width: target_size as u16,
        height: target_size as u16,
        samples_per_pixel: frame.samples_per_pixel,
        bytes: padded_bytes,
    }
}

fn compute_output_dimensions(
    original_width: u16,
    original_height: u16,
    options: &RenderPipelineOptions,
) -> Option<(u32, u32)> {
    let ow = u32::from(original_width);
    let oh = u32::from(original_height);
    match (
        options.output_width,
        options.output_height,
        options.scale_max_size,
    ) {
        (Some(w), Some(h), None) => Some((w, h)),
        (Some(w), None, None) => Some((w, scale_by_ratio(oh, ow, w))),
        (None, Some(h), None) => Some((scale_by_ratio(ow, oh, h), h)),
        (None, None, Some(max_dim)) => Some(scale_to_max_dimension(ow, oh, max_dim)),
        (None, None, None) => None,
        _ => None,
    }
}

fn scale_to_max_dimension(width: u32, height: u32, max_dimension: u32) -> (u32, u32) {
    if width == 0 || height == 0 {
        return (width.max(1), height.max(1));
    }

    let current_max = width.max(height);
    if current_max == 0 {
        return (width, height);
    }

    let scaled_width = scale_by_ratio(width, current_max, max_dimension);
    let scaled_height = scale_by_ratio(height, current_max, max_dimension);
    (scaled_width.max(1), scaled_height.max(1))
}

fn scale_by_ratio(to_scale: u32, reference: u32, new_reference: u32) -> u32 {
    if reference == 0 {
        return to_scale;
    }
    let scaled = f64::from(new_reference) / f64::from(reference) * f64::from(to_scale);
    (scaled.round() as u32).max(1)
}

fn resize_filter(
    source_width: u32,
    source_height: u32,
    target_width: u32,
    target_height: u32,
) -> image::imageops::FilterType {
    use image::imageops::FilterType;

    // Large downscales are substantially faster with Triangle and still look good for
    // diagnostic render output. Use CatmullRom elsewhere for balanced quality/speed.
    let downscale_x = target_width.saturating_mul(2) < source_width;
    let downscale_y = target_height.saturating_mul(2) < source_height;
    if downscale_x || downscale_y {
        FilterType::Triangle
    } else {
        FilterType::CatmullRom
    }
}

// pub(crate): also used by dicom_io::volume to encode a reformatted plane's grayscale pixels.
pub(crate) fn encode_rendered_frame(
    frame: &RenderedFramePixels,
    output_format: RenderOutputFormat,
    jpeg_quality: u8,
) -> Result<RenderFrameOutput, RenderError> {
    let _scope = perf::scope("render.encode_rendered_frame");
    let bytes = match output_format {
        RenderOutputFormat::Raw => frame.bytes.clone(),
        RenderOutputFormat::Png => encode_png(frame)?,
        RenderOutputFormat::Jpeg => encode_jpeg(frame, jpeg_quality)?,
    };

    Ok(RenderFrameOutput {
        width: frame.width,
        height: frame.height,
        samples_per_pixel: frame.samples_per_pixel,
        bits_allocated: 8,
        format: output_format,
        bytes,
        // Callers attach the real overlay discovery/selection results after calling this.
        overlays: Vec::new(),
        selected_overlay_index: None,
    })
}

#[derive(Clone, Debug)]
struct PaletteChannel {
    entries: usize,
    first_mapped: i32,
    values: Vec<u8>,
}

fn read_palette_channel(
    object: &DefaultDicomObject,
    descriptor_tag: dcmnorm_core::Tag,
    data_tag: dcmnorm_core::Tag,
) -> Result<PaletteChannel, RenderError> {
    let descriptor_element = object
        .element(descriptor_tag)
        .map_err(|_| RenderError::MissingImageAttribute("Palette LUT descriptor"))?;
    let descriptor_values = descriptor_element
        .value()
        .to_multi_int::<i32>()
        .map_err(|_| RenderError::MissingImageAttribute("Palette LUT descriptor"))?;

    if descriptor_values.len() < 3 {
        return Err(RenderError::MissingImageAttribute("Palette LUT descriptor"));
    }

    let entries = if descriptor_values[0] == 0 {
        65_536usize
    } else {
        descriptor_values[0].max(0) as usize
    };
    let first_mapped = descriptor_values[1];
    let bits_per_entry = descriptor_values[2].max(1) as u16;

    let bytes = object
        .element(data_tag)
        .map_err(|_| RenderError::MissingImageAttribute("Palette LUT data"))?
        .to_bytes()
        .map_err(|_| RenderError::MissingImageAttribute("Palette LUT data"))?;

    let expected = if bits_per_entry <= 8 {
        entries
    } else {
        entries * 2
    };

    if bytes.len() < expected {
        return Err(RenderError::InvalidPixelDataLength {
            expected,
            actual: bytes.len(),
        });
    }

    let values = if bits_per_entry <= 8 {
        bytes[..entries].to_vec()
    } else {
        let max_sample = ((1u32 << u32::from(bits_per_entry.min(16))) - 1).max(1) as f64;
        bytes[..entries * 2]
            .chunks_exact(2)
            .map(|chunk| {
                let sample = u16::from_le_bytes([chunk[0], chunk[1]]);
                ((f64::from(sample) / max_sample) * 255.0).clamp(0.0, 255.0) as u8
            })
            .collect()
    };

    Ok(PaletteChannel {
        entries,
        first_mapped,
        values,
    })
}

fn palette_index_for_value(value: f64, first_mapped: i32, entries: usize) -> usize {
    if entries == 0 {
        return 0;
    }

    let index = value.round() as i32 - first_mapped;
    index.clamp(0, (entries.saturating_sub(1)) as i32) as usize
}

fn first_numeric_value(text: Option<&str>) -> Option<f64> {
    let source = text?;
    source
        .split('\\')
        .next()
        .and_then(|value| value.trim().parse::<f64>().ok())
}

/// Attempts to serve `frame_index` of `object` directly from its own encapsulated JPEG bytes,
/// with NO decode/recompress step - only when ALL of:
///  - transfer syntax is JPEG Baseline (Process 1) (`uids::JPEG_BASELINE8_BIT`)
///  - the frame is COLOR (SamplesPerPixel > 1, or PALETTE COLOR - checked for completeness even
///    though PALETTE COLOR is never JPEG Baseline in practice)
///  - the instance has no DICOM overlay plane (group 60xx) - compositing needs decoded pixels
///  - PixelData has exactly one fragment per frame (the common case; a frame split across
///    multiple fragments falls through rather than guessing how to reassemble it)
///
/// Returns `Ok(None)` (not an error) whenever any of the above doesn't hold - the caller falls
/// through to the ordinary decode+encode path. Grayscale is INTENTIONALLY never eligible here:
/// passthrough ships undecoded bytes, which makes server-side window/level impossible afterward -
/// grayscale must stay on the decode+re-encode path to keep interactive W/L (see the color-image
/// plan's Piece 2 doc for the full reasoning).
pub fn try_extract_passthrough_jpeg_frame(
    object: &DefaultDicomObject,
    frame_index: usize,
) -> Result<Option<RenderFrameOutput>, RenderError> {
    let source_uid = normalize_transfer_syntax_uid(object.meta().transfer_syntax());
    if source_uid != uids::JPEG_BASELINE8_BIT {
        return Ok(None);
    }

    let metadata = read_render_metadata(object)?;
    let is_color = metadata.samples_per_pixel > 1
        || metadata.photometric_interpretation.eq_ignore_ascii_case("PALETTE COLOR");
    if !is_color {
        return Ok(None);
    }

    if !discover_overlays(object).is_empty() {
        return Ok(None);
    }

    let Ok(element) = object.element(tags::PIXEL_DATA) else {
        return Ok(None);
    };
    // `None` here means native (non-encapsulated) PixelData - shouldn't happen for a JPEG
    // Baseline source, but nothing to pass through if it did.
    let Some(fragments) = element.fragments() else {
        return Ok(None);
    };
    if fragments.len() != metadata.number_of_frames {
        // Not exactly one fragment per frame - falls through rather than guessing how to
        // reassemble a frame split across multiple fragments.
        return Ok(None);
    }
    let Some(bytes) = fragments.get(frame_index) else {
        return Ok(None);
    };

    Ok(Some(RenderFrameOutput {
        width: metadata.cols,
        height: metadata.rows,
        samples_per_pixel: metadata.samples_per_pixel,
        bits_allocated: metadata.bits_allocated,
        format: RenderOutputFormat::Jpeg,
        bytes: bytes.clone(),
        overlays: Vec::new(),
        selected_overlay_index: None,
    }))
}

fn ensure_native_render_object<'a>(
    object: &'a DefaultDicomObject,
) -> Result<Cow<'a, DefaultDicomObject>, RenderError> {
    let source_uid = normalize_transfer_syntax_uid(object.meta().transfer_syntax());
    let has_native_pixel_data = object
        .get(tags::PIXEL_DATA)
        .map(|element| matches!(element.value(), Value::Primitive(_)))
        .unwrap_or(false);

    if source_uid == uids::EXPLICIT_VR_LITTLE_ENDIAN && has_native_pixel_data {
        return Ok(Cow::Borrowed(object));
    }

    let _scope = perf::scope("render.transcode_to_explicit_vr_le");
    let transcoded = transcode_dcmnorm_object(object, uids::EXPLICIT_VR_LITTLE_ENDIAN)?;
    Ok(Cow::Owned(transcoded))
}

fn try_decode_single_frame_object(
    object: &DefaultDicomObject,
    frame_index: usize,
) -> Result<Option<DefaultDicomObject>, RenderError> {
    let source_uid = normalize_transfer_syntax_uid(object.meta().transfer_syntax());
    if source_uid == uids::EXPLICIT_VR_LITTLE_ENDIAN {
        return Ok(None);
    }

    let source_ts = TransferSyntaxRegistry
        .get(source_uid)
        .ok_or_else(|| RenderError::Transcode(super::types::TranscodeError::UnknownTransferSyntax(source_uid.to_owned())))?;

    if is_jpeg2000_transfer_syntax(source_uid) && kakadu_ffi_enabled() {
        // Route JPEG2000 through full transcode to leverage the Kakadu decode path.
        jpeg2000_debug_log("render single-frame path defers JPEG2000 to transcode path for Kakadu decode");
        return Ok(None);
    }

    let Codec::EncapsulatedPixelData(Some(reader), _) = source_ts.codec() else {
        return Ok(None);
    };

    // Some non-conformant JPEG 2000 encoders declare a SamplesPerPixel that
    // disagrees with the codestream's real component count (e.g. an
    // ultrasound machine-UI screen capture saved as SamplesPerPixel=3/RGB
    // when the codestream is genuinely single-component grayscale). Decoding
    // against the declared count then leaves unfilled channels zeroed,
    // rendering as solid red - correct the attributes to match the
    // codestream before decoding.
    let corrected_object;
    let decode_object: &DefaultDicomObject = if is_jpeg2000_transfer_syntax(source_uid) {
        match jpeg2000_component_mismatch(object, frame_index) {
            Some(actual_components) => {
                let mut cloned = object.clone();
                apply_jpeg2000_component_correction(&mut cloned, actual_components);
                corrected_object = cloned;
                &corrected_object
            }
            None => object,
        }
    } else {
        object
    };

    let mut decoded = Vec::new();
    let _scope = perf::scope("render.decode_single_frame_only");
    if is_jpeg2000_transfer_syntax(source_uid) {
        jpeg2000_debug_log(format!(
            "render single-frame codec decode start: uid={} frame={}",
            source_ts.uid(),
            frame_index
        ));
    }
    reader
        .decode_frame(decode_object, frame_index as u32, &mut decoded)
        .map_err(|error| {
            if is_jpeg2000_transfer_syntax(source_uid) {
                jpeg2000_debug_log(format!(
                    "render single-frame codec decode failed: {}",
                    error
                ));
            }
            RenderError::Transcode(super::types::TranscodeError::DecodePixelData {
                uid: source_ts.uid().to_owned(),
                name: source_ts.name().to_owned(),
                message: error.to_string(),
            })
        })?;

    if is_jpeg2000_transfer_syntax(source_uid) {
        jpeg2000_debug_log(format!(
            "render single-frame codec decode succeeded ({} decoded bytes)",
            decoded.len()
        ));
    }

    let jpeg2000_uses_mct = is_jpeg2000_transfer_syntax(source_uid)
        .then(|| jpeg2000_frame_uses_mct(decode_object, frame_index))
        .flatten();

    let mut working = decode_object.clone();
    replace_with_native_frame_pixel_data(&mut working, decoded)?;
    normalize_decoded_render_attributes(&mut working, source_uid, jpeg2000_uses_mct);
    working.remove_element(tags::NUMBER_OF_FRAMES);
    working.meta_mut().set_transfer_syntax(
        TransferSyntaxRegistry
            .get(uids::EXPLICIT_VR_LITTLE_ENDIAN)
            .expect("explicit VR little endian transfer syntax must exist"),
    );

    Ok(Some(working))
}

fn replace_with_native_frame_pixel_data(
    object: &mut DefaultDicomObject,
    decoded: Vec<u8>,
) -> Result<(), RenderError> {
    let bits_allocated = object
        .get(tags::BITS_ALLOCATED)
        .and_then(|element| element.uint16().ok())
        .ok_or(RenderError::MissingImageAttribute("BitsAllocated"))?;

    let value = match bits_allocated {
        1..=8 => PrimitiveValue::from(decoded),
        9..=16 => {
            if decoded.len() % 2 != 0 {
                return Err(RenderError::InvalidPixelDataLength {
                    expected: decoded.len() + 1,
                    actual: decoded.len(),
                });
            }
            let words: Vec<u16> = decoded
                .chunks_exact(2)
                .map(|chunk| u16::from_le_bytes([chunk[0], chunk[1]]))
                .collect();
            PrimitiveValue::U16(words.into())
        }
        other => return Err(RenderError::UnsupportedBitsAllocated(other)),
    };

    let vr = if bits_allocated <= 8 { VR::OB } else { VR::OW };
    object.remove_element(Tag(0x7FE0, 0x0001));
    object.remove_element(Tag(0x7FE0, 0x0002));
    object.remove_element(Tag(0x7FE0, 0x0003));
    object.put(DataElement::new(tags::PIXEL_DATA, vr, value));
    Ok(())
}

fn normalize_decoded_render_attributes(
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
        // Only RLE Lossless, and JPEG 2000 codestreams that did not apply the
        // Multiple Component Transformation, hand back raw un-converted YBR
        // component samples. JPEG (baseline/extended) and MCT-using JPEG 2000
        // both perform the YCbCr/RCT/ICT->RGB color transform internally as
        // part of standard decompression, so keeping a YBR_* label after
        // decoding those would cause render_rgb_frame to apply a second,
        // incorrect color conversion on top of already-RGB bytes. See
        // jpeg2000_frame_uses_mct (io.rs) for why JPEG 2000 needs a per-file
        // check rather than a blanket assumption, and mirrors the equivalent
        // check in normalize_decoded_pixel_data_attributes (io.rs).
        let preserves_ybr_on_decode = normalize_transfer_syntax_uid(source_ts_uid) == uids::RLE_LOSSLESS
            || (is_jpeg2000_transfer_syntax(source_ts_uid) && jpeg2000_uses_mct == Some(false));
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
    }
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
