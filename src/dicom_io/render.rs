use dicom_core::{DataElement, PrimitiveValue, VR};
use dicom_dictionary_std::{tags, uids};
use dicom_object::DefaultDicomObject;
use image::codecs::jpeg::JpegEncoder;
use image::codecs::png::PngEncoder;
use image::{ColorType, GrayImage, ImageEncoder, RgbImage};

use super::io::transcode_dicom_object;
use super::types::RenderError;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RenderOutputFormat {
    Raw,
    Png,
    Jpeg,
}

#[derive(Clone, Debug)]
pub struct RenderPipelineOptions {
    pub frame_index: usize,
    pub apply_modality_lut: bool,
    pub apply_voi_lut: bool,
    pub window_center: Option<f64>,
    pub window_width: Option<f64>,
    pub jpeg_quality: u8,
    /// Explicit output width in pixels. When combined with `output_height`, the image is scaled
    /// to the exact dimensions. When used alone, the height is computed from the aspect ratio.
    pub output_width: Option<u32>,
    /// Explicit output height in pixels. When combined with `output_width`, the image is scaled
    /// to the exact dimensions. When used alone, the width is computed from the aspect ratio.
    pub output_height: Option<u32>,
    /// Scale the output so that its width equals this value, preserving the aspect ratio.
    pub scale_x_size: Option<u32>,
    /// Scale the output so that its height equals this value, preserving the aspect ratio.
    pub scale_y_size: Option<u32>,
}

impl Default for RenderPipelineOptions {
    fn default() -> Self {
        Self {
            frame_index: 0,
            apply_modality_lut: true,
            apply_voi_lut: true,
            window_center: None,
            window_width: None,
            jpeg_quality: 90,
            output_width: None,
            output_height: None,
            scale_x_size: None,
            scale_y_size: None,
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
}

#[derive(Clone, Debug)]
struct RenderMetadata {
    rows: u16,
    cols: u16,
    samples_per_pixel: u16,
    bits_allocated: u16,
    bits_stored: u16,
    pixel_representation: u16,
    planar_configuration: u16,
    number_of_frames: usize,
    photometric_interpretation: String,
}

#[derive(Clone, Debug)]
struct RenderedFramePixels {
    width: u16,
    height: u16,
    samples_per_pixel: u16,
    bytes: Vec<u8>,
}

pub fn render_dicom_frame(
    object: &DefaultDicomObject,
    output_format: RenderOutputFormat,
    options: &RenderPipelineOptions,
) -> Result<RenderFrameOutput, RenderError> {
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
    let working = transcode_dicom_object(object, uids::EXPLICIT_VR_LITTLE_ENDIAN)?;
    let metadata = read_render_metadata(&working)?;

    let frame = render_single_frame(&working, &metadata, options)?;
    let frame = maybe_resize_frame(frame, options);
    let encoded = encode_rendered_frame(&frame, output_format, options.jpeg_quality)?;
    Ok(vec![encoded])
}

pub fn render_all_dicom_frames(
    object: &DefaultDicomObject,
    output_format: RenderOutputFormat,
    options: &RenderPipelineOptions,
) -> Result<Vec<RenderFrameOutput>, RenderError> {
    let working = transcode_dicom_object(object, uids::EXPLICIT_VR_LITTLE_ENDIAN)?;
    let metadata = read_render_metadata(&working)?;
    let rendered = render_all_frames(&working, &metadata, options)?;
    rendered
        .iter()
        .map(|frame| {
            let resized = maybe_resize_frame(frame.clone(), options);
            encode_rendered_frame(&resized, output_format, options.jpeg_quality)
        })
        .collect()
}

pub fn render_dicom_to_recompressed_object(
    object: &DefaultDicomObject,
    target_transfer_syntax_uid: &str,
    options: &RenderPipelineOptions,
) -> Result<DefaultDicomObject, RenderError> {
    let mut working = transcode_dicom_object(object, uids::EXPLICIT_VR_LITTLE_ENDIAN)?;
    let metadata = read_render_metadata(&working)?;
    let rendered_frames = render_all_frames(&working, &metadata, options)?;

    let mut rendered_pixel_data = Vec::new();
    for frame in rendered_frames {
        rendered_pixel_data.extend_from_slice(&frame.bytes);
    }

    let output_samples_per_pixel = if metadata.samples_per_pixel == 1 { 1u16 } else { 3u16 };
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

    let recompressed = transcode_dicom_object(&working, target_transfer_syntax_uid)?;
    Ok(recompressed)
}

fn render_all_frames(
    object: &DefaultDicomObject,
    metadata: &RenderMetadata,
    options: &RenderPipelineOptions,
) -> Result<Vec<RenderedFramePixels>, RenderError> {
    let mut rendered = Vec::with_capacity(metadata.number_of_frames);
    for frame_index in 0..metadata.number_of_frames {
        let mut frame_options = options.clone();
        frame_options.frame_index = frame_index;
        rendered.push(render_single_frame(object, metadata, &frame_options)?);
    }
    Ok(rendered)
}

fn render_single_frame(
    object: &DefaultDicomObject,
    metadata: &RenderMetadata,
    options: &RenderPipelineOptions,
) -> Result<RenderedFramePixels, RenderError> {
    if options.frame_index >= metadata.number_of_frames {
        return Err(RenderError::InvalidFrameIndex {
            requested: options.frame_index,
            number_of_frames: metadata.number_of_frames,
        });
    }

    match metadata.samples_per_pixel {
        1 => render_grayscale_frame(object, metadata, options),
        3 => render_rgb_frame(object, metadata, options),
        other => Err(RenderError::UnsupportedSamplesPerPixel(other)),
    }
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
        apply_modality_lut(object, &mut values);
    }

    let mut rendered = if options.apply_voi_lut {
        let (center, width) = resolve_window(object, options)?;
        apply_voi_window(&values, center, width)
    } else {
        normalize_to_u8(&values)
    };

    if metadata.photometric_interpretation == "MONOCHROME1" {
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
        apply_modality_lut(object, &mut values);
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
    if !matches!(
        metadata.photometric_interpretation.as_str(),
        "RGB" | "YBR_FULL" | "YBR_FULL_422"
    ) {
        return Err(RenderError::UnsupportedPhotometricInterpretation(
            metadata.photometric_interpretation.clone(),
        ));
    }

    let frame_bytes = get_frame_bytes(object, metadata, options.frame_index)?;
    let pixel_count = usize::from(metadata.rows) * usize::from(metadata.cols);
    let expected_components = pixel_count * 3;

    let rendered = if metadata.bits_allocated == 8 {
        if metadata.planar_configuration > 1 {
            return Err(RenderError::UnsupportedPlanarConfiguration(
                metadata.planar_configuration,
            ));
        }

        if frame_bytes.len() < expected_components {
            return Err(RenderError::InvalidPixelDataLength {
                expected: expected_components,
                actual: frame_bytes.len(),
            });
        }

        if metadata.planar_configuration == 0 {
            frame_bytes[..expected_components].to_vec()
        } else {
            let mut interleaved = vec![0u8; expected_components];
            let plane_len = pixel_count;
            if frame_bytes.len() < plane_len * 3 {
                return Err(RenderError::InvalidPixelDataLength {
                    expected: plane_len * 3,
                    actual: frame_bytes.len(),
                });
            }
            for index in 0..pixel_count {
                interleaved[index * 3] = frame_bytes[index];
                interleaved[index * 3 + 1] = frame_bytes[plane_len + index];
                interleaved[index * 3 + 2] = frame_bytes[2 * plane_len + index];
            }
            interleaved
        }
    } else if metadata.bits_allocated == 16 {
        if metadata.planar_configuration > 1 {
            return Err(RenderError::UnsupportedPlanarConfiguration(
                metadata.planar_configuration,
            ));
        }

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
                .map(|chunk| {
                    let sample = u16::from_le_bytes([chunk[0], chunk[1]]);
                    ((f64::from(sample) / max_value) * 255.0).clamp(0.0, 255.0) as u8
                })
                .collect()
        } else {
            let plane_len = pixel_count;
            let mut planes = [vec![0u8; plane_len], vec![0u8; plane_len], vec![0u8; plane_len]];
            for (channel, plane) in planes.iter_mut().enumerate() {
                for index in 0..plane_len {
                    let sample_index = channel * plane_len + index;
                    let byte_index = sample_index * 2;
                    let sample = u16::from_le_bytes([frame_bytes[byte_index], frame_bytes[byte_index + 1]]);
                    plane[index] = ((f64::from(sample) / max_value) * 255.0).clamp(0.0, 255.0) as u8;
                }
            }

            let mut interleaved = vec![0u8; expected_components];
            for index in 0..pixel_count {
                interleaved[index * 3] = planes[0][index];
                interleaved[index * 3 + 1] = planes[1][index];
                interleaved[index * 3 + 2] = planes[2][index];
            }
            interleaved
        }
    } else {
        return Err(RenderError::UnsupportedBitsAllocated(metadata.bits_allocated));
    };

    Ok(RenderedFramePixels {
        width: metadata.cols,
        height: metadata.rows,
        samples_per_pixel: 3,
        bytes: rendered,
    })
}

fn read_render_metadata(object: &DefaultDicomObject) -> Result<RenderMetadata, RenderError> {
    let rows = required_u16(object, tags::ROWS, "Rows")?;
    let cols = required_u16(object, tags::COLUMNS, "Columns")?;
    let samples_per_pixel = required_u16(object, tags::SAMPLES_PER_PIXEL, "SamplesPerPixel")?;
    let bits_allocated = required_u16(object, tags::BITS_ALLOCATED, "BitsAllocated")?;
    let bits_stored = required_u16(object, tags::BITS_STORED, "BitsStored")?;
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

fn required_u16(
    object: &DefaultDicomObject,
    tag: dicom_core::Tag,
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
    let pixel_data = object
        .element(tags::PIXEL_DATA)
        .map_err(|_| RenderError::MissingImageAttribute("PixelData"))?
        .to_bytes()
        .map_err(|_| RenderError::MissingImageAttribute("PixelData"))?;

    let samples_per_frame = usize::from(metadata.rows)
        * usize::from(metadata.cols)
        * usize::from(metadata.samples_per_pixel);
    let frame_len = match metadata.bits_allocated {
        1 => samples_per_frame.div_ceil(8),
        8 => samples_per_frame,
        16 => samples_per_frame * 2,
        other => return Err(RenderError::UnsupportedBitsAllocated(other)),
    };
    let start = frame_index * frame_len;
    let expected = (frame_index + 1) * frame_len;

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
                values.push(sign_or_unsigned(raw, metadata.bits_stored, metadata.pixel_representation));
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
                values.push(sign_or_unsigned(raw, metadata.bits_stored, metadata.pixel_representation));
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

fn apply_modality_lut(object: &DefaultDicomObject, values: &mut [f64]) {
    let slope = object
        .get(tags::RESCALE_SLOPE)
        .and_then(|element| first_numeric_value(element.to_str().ok().as_deref()))
        .unwrap_or(1.0);
    let intercept = object
        .get(tags::RESCALE_INTERCEPT)
        .and_then(|element| first_numeric_value(element.to_str().ok().as_deref()))
        .unwrap_or(0.0);

    if (slope - 1.0).abs() < f64::EPSILON && intercept.abs() < f64::EPSILON {
        return;
    }

    for value in values {
        *value = (*value * slope) + intercept;
    }
}

fn resolve_window(
    object: &DefaultDicomObject,
    options: &RenderPipelineOptions,
) -> Result<(Option<f64>, Option<f64>), RenderError> {
    let center = options.window_center.or_else(|| {
        object
            .get(tags::WINDOW_CENTER)
            .and_then(|element| first_numeric_value(element.to_str().ok().as_deref()))
    });
    let width = options.window_width.or_else(|| {
        object
            .get(tags::WINDOW_WIDTH)
            .and_then(|element| first_numeric_value(element.to_str().ok().as_deref()))
    });

    if width.is_some() && center.is_none() {
        return Err(RenderError::InvalidWindow(
            "window width is set but window center is missing".to_owned(),
        ));
    }

    if let Some(window_width) = width {
        if window_width <= 0.0 {
            return Err(RenderError::InvalidWindow(
                "window width must be greater than zero".to_owned(),
            ));
        }
    }

    Ok((center, width))
}

fn apply_voi_window(values: &[f64], center: Option<f64>, width: Option<f64>) -> Vec<u8> {
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

fn maybe_resize_frame(frame: RenderedFramePixels, options: &RenderPipelineOptions) -> RenderedFramePixels {
    let Some((new_width, new_height)) = compute_output_dimensions(frame.width, frame.height, options) else {
        return frame;
    };

    if new_width == u32::from(frame.width) && new_height == u32::from(frame.height) {
        return frame;
    }

    use image::imageops::{self, FilterType};
    let resized_bytes = if frame.samples_per_pixel == 1 {
        let img = GrayImage::from_raw(u32::from(frame.width), u32::from(frame.height), frame.bytes)
            .expect("grayscale frame buffer size mismatch");
        imageops::resize(&img, new_width, new_height, FilterType::Lanczos3).into_raw()
    } else {
        let img = RgbImage::from_raw(u32::from(frame.width), u32::from(frame.height), frame.bytes)
            .expect("RGB frame buffer size mismatch");
        imageops::resize(&img, new_width, new_height, FilterType::Lanczos3).into_raw()
    };

    RenderedFramePixels {
        width: new_width as u16,
        height: new_height as u16,
        samples_per_pixel: frame.samples_per_pixel,
        bytes: resized_bytes,
    }
}

fn compute_output_dimensions(
    original_width: u16,
    original_height: u16,
    options: &RenderPipelineOptions,
) -> Option<(u32, u32)> {
    let ow = u32::from(original_width);
    let oh = u32::from(original_height);
    match (options.output_width, options.output_height, options.scale_x_size, options.scale_y_size) {
        (Some(w), Some(h), None, None) => Some((w, h)),
        (Some(w), None, None, None) => Some((w, scale_by_ratio(oh, ow, w))),
        (None, Some(h), None, None) => Some((scale_by_ratio(ow, oh, h), h)),
        (None, None, Some(w), None) => Some((w, scale_by_ratio(oh, ow, w))),
        (None, None, None, Some(h)) => Some((scale_by_ratio(ow, oh, h), h)),
        (None, None, None, None) => None,
        _ => None,
    }
}

fn scale_by_ratio(to_scale: u32, reference: u32, new_reference: u32) -> u32 {
    if reference == 0 {
        return to_scale;
    }
    let scaled = f64::from(new_reference) / f64::from(reference) * f64::from(to_scale);
    (scaled.round() as u32).max(1)
}

fn encode_rendered_frame(
    frame: &RenderedFramePixels,
    output_format: RenderOutputFormat,
    jpeg_quality: u8,
) -> Result<RenderFrameOutput, RenderError> {
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
    descriptor_tag: dicom_core::Tag,
    data_tag: dicom_core::Tag,
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
