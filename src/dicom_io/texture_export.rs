//! Packs a `Volume` (see `super::volume`) or a single decoded 2D frame into a lossless,
//! GPU-upload-ready texture payload: a small metadata object (dimensions, physical geometry, a
//! rescale slope/intercept pair for reconstructing physical values from quantized samples) plus a
//! raw row-major `int16`/`uint16` byte buffer, optionally gzip-compressed for the wire.
//!
//! Deliberately NOT the same thing as `super::volume::reformat_plane`/`super::volume_export`:
//! those resample an arbitrary oblique plane or stack (windowed 8-bit render, or a full-precision
//! NIfTI/NRRD export respectively). This module ships the volume's own NATIVE voxel lattice (or a
//! single frame's native pixel grid) once, unwindowed, so a client can do arbitrary oblique
//! reslicing/window-level itself in a GPU shader instead of round-tripping to the server per
//! interaction.

use std::error::Error;
use std::fmt;
use std::io::{self, Write};

use flate2::write::GzEncoder;
use flate2::Compression;
use serde_json::json;

use super::render::decode_frame_grayscale_values;
use super::types::RenderError;
use super::volume::{resample_volume, Interpolation, Volume};

#[derive(Debug)]
pub enum TextureExportError {
    Io(io::Error),
    Render(RenderError),
    /// `values.len()` didn't match the declared `width * height` (or `width * height * depth`).
    SampleCountMismatch { expected: usize, found: usize },
    InvalidDimensions,
}

impl fmt::Display for TextureExportError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "failed to pack texture export: {error}"),
            Self::Render(error) => write!(formatter, "failed to decode frame for texture export: {error}"),
            Self::SampleCountMismatch { expected, found } => write!(
                formatter,
                "texture export sample count ({found}) does not match its declared dimensions ({expected} expected)"
            ),
            Self::InvalidDimensions => write!(formatter, "texture export dimensions must all be non-zero"),
        }
    }
}

impl Error for TextureExportError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::Render(error) => Some(error),
            _ => None,
        }
    }
}

impl From<io::Error> for TextureExportError {
    fn from(value: io::Error) -> Self {
        Self::Io(value)
    }
}

impl From<RenderError> for TextureExportError {
    fn from(value: RenderError) -> Self {
        Self::Render(value)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SampleFormat {
    Uint16,
    Int16,
}

impl SampleFormat {
    fn as_str(self) -> &'static str {
        match self {
            Self::Uint16 => "uint16",
            Self::Int16 => "int16",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ContentKind {
    Volume,
    Image2D,
}

impl ContentKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Volume => "volume",
            Self::Image2D => "image2d",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TextureCompression {
    None,
    Gzip,
}

impl TextureCompression {
    fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Gzip => "gzip",
        }
    }
}

/// Mirrors the cross-repo `TextureMeta` JSON contract (see the dcmnorm GPU-texture plan) -
/// `to_json` is the single source of truth for field names/casing consumed by the CLI sidecar,
/// the Node bindings, and eventually the render-server's WS response.
#[derive(Clone, Debug)]
pub struct TextureMeta {
    pub content_kind: ContentKind,
    pub sample_format: SampleFormat,
    pub compression: TextureCompression,
    pub lossless: bool,
    /// Columns - fastest-varying axis in the payload.
    pub width: u32,
    /// Rows.
    pub height: u32,
    /// Slices - always 1 for `ContentKind::Image2D`.
    pub depth: u32,
    /// `texel * rescale_slope + rescale_intercept` recovers the physical value (e.g. HU).
    pub rescale_slope: f64,
    pub rescale_intercept: f64,
    /// mm per row step (along `col_dir`) - matches `Volume::row_spacing_mm`'s convention exactly.
    pub row_spacing_mm: f64,
    /// mm per column step (along `row_dir`).
    pub col_spacing_mm: f64,
    /// mm per slice step (along `normal_dir`) - `0.0` for `ContentKind::Image2D`.
    pub slice_spacing_mm: f64,
    /// LPS mm, center of voxel (0,0,0).
    pub origin: [f64; 3],
    pub row_dir: [f64; 3],
    pub col_dir: [f64; 3],
    pub normal_dir: [f64; 3],
    pub default_window_center: Option<f64>,
    pub default_window_width: Option<f64>,
    /// `(width, height, depth)` before any capability/progressive-driven downsampling.
    pub native_dims: (u32, u32, u32),
    pub downsampled: bool,
    pub payload_bytes_raw: u64,
    pub payload_bytes_stored: u64,
}

impl TextureMeta {
    pub fn to_json(&self) -> serde_json::Value {
        json!({
            "formatVersion": 1,
            "contentKind": self.content_kind.as_str(),
            "sampleFormat": self.sample_format.as_str(),
            "compression": self.compression.as_str(),
            "lossless": self.lossless,
            "width": self.width,
            "height": self.height,
            "depth": self.depth,
            "rescaleSlope": self.rescale_slope,
            "rescaleIntercept": self.rescale_intercept,
            "rowSpacingMm": self.row_spacing_mm,
            "colSpacingMm": self.col_spacing_mm,
            "sliceSpacingMm": self.slice_spacing_mm,
            "origin": self.origin,
            "rowDir": self.row_dir,
            "colDir": self.col_dir,
            "normalDir": self.normal_dir,
            "defaultWindowCenter": self.default_window_center,
            "defaultWindowWidth": self.default_window_width,
            "nativeDims": [self.native_dims.0, self.native_dims.1, self.native_dims.2],
            "downsampled": self.downsampled,
            "payloadBytesRaw": self.payload_bytes_raw,
            "payloadBytesStored": self.payload_bytes_stored,
        })
    }
}

pub struct PackedTexture {
    pub meta: TextureMeta,
    pub payload: Vec<u8>,
}

impl fmt::Debug for PackedTexture {
    // Manual impl (mirroring `Volume`'s) so a large `payload` (up to hundreds of MB) never floods
    // `{:?}`/`assert!` failure output.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PackedTexture")
            .field("meta", &self.meta)
            .field("payload_len", &self.payload.len())
            .finish()
    }
}

fn gzip_bytes(bytes: &[u8]) -> Result<Vec<u8>, TextureExportError> {
    let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(bytes)?;
    Ok(encoder.finish()?)
}

/// Quantizes `raw` physical-value samples into a lossless little-endian byte buffer, picking
/// whichever of two encodings applies:
///
/// - **Exact** (the common case: CT/MR samples are always derived from an integer stored value):
///   if every sample round-trips through an `i16` lattice with zero error, store `int16` with an
///   identity slope/intercept (`1.0`/`0.0`) - genuinely lossless, no requantization at all.
/// - **Bounded-error fallback** (fractional-valued modalities, or a range that doesn't fit
///   `i16`): quantize to `uint16` via a slope/intercept spanning `[min, max]`; error is bounded by
///   `±0.5 * slope` and `lossless` is reported as `false` - callers must not claim losslessness
///   this path took.
///
/// Returns `(bytes, format, rescale_slope, rescale_intercept, lossless)`.
pub fn quantize_samples(raw: &[f32]) -> (Vec<u8>, SampleFormat, f64, f64, bool) {
    if raw.is_empty() {
        return (Vec::new(), SampleFormat::Int16, 1.0, 0.0, true);
    }

    let mut min = f64::INFINITY;
    let mut max = f64::NEG_INFINITY;
    let mut all_integral = true;
    for &sample in raw {
        let value = sample as f64;
        min = min.min(value);
        max = max.max(value);
        if all_integral && (value - value.round()).abs() > 1e-4 {
            all_integral = false;
        }
    }

    if all_integral && min >= i16::MIN as f64 && max <= i16::MAX as f64 {
        let mut bytes = Vec::with_capacity(raw.len() * 2);
        for &sample in raw {
            bytes.extend_from_slice(&(sample.round() as i16).to_le_bytes());
        }
        return (bytes, SampleFormat::Int16, 1.0, 0.0, true);
    }

    let range = (max - min).max(1e-6);
    let slope = range / 65535.0;
    let intercept = min;
    let mut bytes = Vec::with_capacity(raw.len() * 2);
    for &sample in raw {
        let scaled = ((sample as f64 - intercept) / slope).round().clamp(0.0, 65535.0);
        bytes.extend_from_slice(&(scaled as u16).to_le_bytes());
    }
    (bytes, SampleFormat::Uint16, slope, intercept, false)
}

fn apply_compression(quantized: Vec<u8>, compression: TextureCompression) -> Result<(Vec<u8>, u64), TextureExportError> {
    let payload_bytes_raw = quantized.len() as u64;
    let payload = match compression {
        TextureCompression::None => quantized,
        TextureCompression::Gzip => gzip_bytes(&quantized)?,
    };
    Ok((payload, payload_bytes_raw))
}

/// Packs `volume`'s own native voxel lattice (NOT a resampled oblique plane - see the module doc)
/// into a lossless texture payload. If `target_max_dim` is set and any of the volume's own
/// `cols`/`rows`/`num_slices` exceeds it, the whole grid is first proportionally downsampled (via
/// `super::volume::resample_volume`) to fit, preserving physical extent.
pub fn pack_volume_texture(
    volume: &Volume,
    target_max_dim: Option<u32>,
    default_window: Option<(f64, f64)>,
    compression: TextureCompression,
) -> Result<PackedTexture, TextureExportError> {
    let native_dims = (volume.cols, volume.rows, volume.num_slices);
    if native_dims.0 == 0 || native_dims.1 == 0 || native_dims.2 == 0 {
        return Err(TextureExportError::InvalidDimensions);
    }

    let native_max = native_dims.0.max(native_dims.1).max(native_dims.2);
    let (raw, dims, downsampled): (Vec<f32>, (u32, u32, u32), bool) = match target_max_dim {
        Some(max_dim) if native_max > max_dim.max(1) => {
            let scale = max_dim.max(1) as f64 / native_max as f64;
            let target_cols = ((native_dims.0 as f64 * scale).round() as u32).max(1);
            let target_rows = ((native_dims.1 as f64 * scale).round() as u32).max(1);
            let target_slices = ((native_dims.2 as f64 * scale).round() as u32).max(1);
            let resampled = resample_volume(volume, target_cols, target_rows, target_slices, Interpolation::Trilinear);
            (resampled, (target_cols, target_rows, target_slices), true)
        }
        _ => (volume.samples.clone(), native_dims, false),
    };

    let expected = dims.0 as usize * dims.1 as usize * dims.2 as usize;
    if raw.len() != expected {
        return Err(TextureExportError::SampleCountMismatch { expected, found: raw.len() });
    }

    let (quantized, sample_format, rescale_slope, rescale_intercept, lossless) = quantize_samples(&raw);
    let (payload, payload_bytes_raw) = apply_compression(quantized, compression)?;
    let payload_bytes_stored = payload.len() as u64;

    // Downsampling shrinks voxel COUNT while covering the same physical extent, so per-axis
    // spacing grows by the inverse of that axis's shrink ratio.
    let col_scale = native_dims.0 as f64 / dims.0 as f64;
    let row_scale = native_dims.1 as f64 / dims.1 as f64;
    let slice_nominal_spacing = if volume.slice_zs.len() > 1 {
        ((volume.slice_zs[volume.slice_zs.len() - 1] - volume.slice_zs[0]) / (volume.slice_zs.len() - 1) as f64).abs()
    } else {
        0.0
    };
    let slice_scale = native_dims.2 as f64 / dims.2 as f64;

    let meta = TextureMeta {
        content_kind: ContentKind::Volume,
        sample_format,
        compression,
        lossless,
        width: dims.0,
        height: dims.1,
        depth: dims.2,
        rescale_slope,
        rescale_intercept,
        row_spacing_mm: volume.row_spacing_mm * row_scale,
        col_spacing_mm: volume.col_spacing_mm * col_scale,
        slice_spacing_mm: slice_nominal_spacing * slice_scale,
        origin: volume.origin,
        row_dir: volume.row_vector,
        col_dir: volume.col_vector,
        normal_dir: volume.slice_normal,
        default_window_center: default_window.map(|(center, _)| center),
        default_window_width: default_window.map(|(_, width)| width),
        native_dims,
        downsampled,
        payload_bytes_raw,
        payload_bytes_stored,
    };

    Ok(PackedTexture { meta, payload })
}

/// 2D bilinear box-downsample of a single frame's raw values to fit within `target_max_dim` on
/// its longest axis, preserving aspect ratio. Deliberately independent of `super::render`'s
/// existing (lossy, 8-bit-targeted) resize path used for `--scale-max-size` - this operates on
/// raw physical values, before any windowing/quantization. Returns `(values, width, height)`;
/// a no-op (clone) if the frame already fits.
pub fn resample_frame_2d(values: &[f32], width: u32, height: u32, target_max_dim: u32) -> (Vec<f32>, u32, u32) {
    let native_max = width.max(height);
    if native_max == 0 || native_max <= target_max_dim.max(1) {
        return (values.to_vec(), width, height);
    }

    let scale = target_max_dim.max(1) as f64 / native_max as f64;
    let target_width = ((width as f64 * scale).round() as u32).max(1);
    let target_height = ((height as f64 * scale).round() as u32).max(1);

    let col_scale = if target_width > 1 { (width as f64 - 1.0) / (target_width as f64 - 1.0) } else { 0.0 };
    let row_scale = if target_height > 1 { (height as f64 - 1.0) / (target_height as f64 - 1.0) } else { 0.0 };

    let sample_at = |x_f: f64, y_f: f64| -> f32 {
        let x0 = x_f.floor().clamp(0.0, (width - 1) as f64) as usize;
        let y0 = y_f.floor().clamp(0.0, (height - 1) as f64) as usize;
        let x1 = (x0 + 1).min(width as usize - 1);
        let y1 = (y0 + 1).min(height as usize - 1);
        let tx = x_f - x0 as f64;
        let ty = y_f - y0 as f64;
        let get = |x: usize, y: usize| values[y * width as usize + x] as f64;
        let top = get(x0, y0) * (1.0 - tx) + get(x1, y0) * tx;
        let bottom = get(x0, y1) * (1.0 - tx) + get(x1, y1) * tx;
        (top * (1.0 - ty) + bottom * ty) as f32
    };

    let mut out = vec![0f32; target_width as usize * target_height as usize];
    for target_row in 0..target_height {
        let y_f = if target_height > 1 {
            target_row as f64 * row_scale
        } else {
            (height as f64 - 1.0) / 2.0
        };
        for target_col in 0..target_width {
            let x_f = if target_width > 1 {
                target_col as f64 * col_scale
            } else {
                (width as f64 - 1.0) / 2.0
            };
            out[target_row as usize * target_width as usize + target_col as usize] = sample_at(x_f, y_f);
        }
    }
    (out, target_width, target_height)
}

/// Packs a single 2D frame's already-decoded raw physical values (e.g. from
/// `super::render::decode_frame_grayscale_values`) as a depth-1 "1-slice volume" texture -
/// letting a large diagnostic 2D image reuse the exact same client GPU texture/shader machinery
/// as an MPR volume instead of a separate implementation.
pub fn pack_frame_texture(
    values: &[f32],
    width: u32,
    height: u32,
    default_window: Option<(f64, f64)>,
    target_max_dim: Option<u32>,
    compression: TextureCompression,
) -> Result<PackedTexture, TextureExportError> {
    if width == 0 || height == 0 {
        return Err(TextureExportError::InvalidDimensions);
    }
    let expected = width as usize * height as usize;
    if values.len() != expected {
        return Err(TextureExportError::SampleCountMismatch { expected, found: values.len() });
    }
    let native_dims = (width, height, 1);

    let (raw, out_width, out_height, downsampled) = match target_max_dim {
        Some(max_dim) if width.max(height) > max_dim.max(1) => {
            let (resampled, w, h) = resample_frame_2d(values, width, height, max_dim);
            (resampled, w, h, true)
        }
        _ => (values.to_vec(), width, height, false),
    };

    let (quantized, sample_format, rescale_slope, rescale_intercept, lossless) = quantize_samples(&raw);
    let (payload, payload_bytes_raw) = apply_compression(quantized, compression)?;
    let payload_bytes_stored = payload.len() as u64;

    let meta = TextureMeta {
        content_kind: ContentKind::Image2D,
        sample_format,
        compression,
        lossless,
        width: out_width,
        height: out_height,
        depth: 1,
        rescale_slope,
        rescale_intercept,
        // A single decoded frame carries no volume-level geometry here - callers with real
        // PixelSpacing available (the CLI/Node bindings, from the source DICOM object) can layer
        // physical spacing on top later; pan/zoom/W-L don't require it.
        row_spacing_mm: 1.0,
        col_spacing_mm: 1.0,
        slice_spacing_mm: 0.0,
        origin: [0.0, 0.0, 0.0],
        row_dir: [1.0, 0.0, 0.0],
        col_dir: [0.0, 1.0, 0.0],
        normal_dir: [0.0, 0.0, 1.0],
        default_window_center: default_window.map(|(center, _)| center),
        default_window_width: default_window.map(|(_, width)| width),
        native_dims,
        downsampled,
        payload_bytes_raw,
        payload_bytes_stored,
    };

    Ok(PackedTexture { meta, payload })
}

/// Convenience wrapper: decodes frame `frame_index` of `object` (modality-LUT-applied, raw
/// physical values - the same primitive `super::volume::build_volume` uses per-slice) and packs
/// it, so callers (the CLI, and eventually the Node bindings) don't need to touch
/// `decode_frame_grayscale_values` directly.
pub fn pack_dicom_frame_texture(
    object: &dicom_object::DefaultDicomObject,
    frame_index: usize,
    target_max_dim: Option<u32>,
    default_window: Option<(f64, f64)>,
    compression: TextureCompression,
) -> Result<PackedTexture, TextureExportError> {
    let (metadata, values) = decode_frame_grayscale_values(object, frame_index)?;
    let raw: Vec<f32> = values.into_iter().map(|value| value as f32).collect();
    pack_frame_texture(&raw, metadata.cols as u32, metadata.rows as u32, default_window, target_max_dim, compression)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dicom_io::volume::Volume;

    fn axis_aligned_test_volume(cols: u32, rows: u32, num_slices: u32) -> Volume {
        let plane = (rows * cols) as usize;
        let mut samples = vec![0f32; plane * num_slices as usize];
        for (index, sample) in samples.iter_mut().enumerate() {
            *sample = index as f32 - 1024.0; // exercises negative HU-like values too
        }
        Volume {
            samples,
            rows,
            cols,
            num_slices,
            origin: [10.0, -20.0, 30.0],
            row_vector: [1.0, 0.0, 0.0],
            col_vector: [0.0, 1.0, 0.0],
            slice_normal: [0.0, 0.0, 1.0],
            col_spacing_mm: 0.5,
            row_spacing_mm: 0.6,
            slice_zs: (0..num_slices).map(|index| index as f64 * 2.0).collect(),
        }
    }

    #[test]
    fn quantize_samples_of_integer_values_is_exact_and_lossless() {
        let raw = vec![-1024.0f32, 0.0, 1500.0, 3071.0];
        let (bytes, format, slope, intercept, lossless) = quantize_samples(&raw);
        assert_eq!(format, SampleFormat::Int16);
        assert_eq!(slope, 1.0);
        assert_eq!(intercept, 0.0);
        assert!(lossless);
        assert_eq!(bytes.len(), raw.len() * 2);
        for (index, &expected) in raw.iter().enumerate() {
            let value = i16::from_le_bytes(bytes[index * 2..index * 2 + 2].try_into().unwrap());
            assert_eq!(value as f32, expected);
        }
    }

    #[test]
    fn quantize_samples_of_fractional_values_falls_back_to_bounded_error_uint16() {
        let raw = vec![0.0f32, 0.25, 0.5, 1.0];
        let (bytes, format, slope, intercept, lossless) = quantize_samples(&raw);
        assert_eq!(format, SampleFormat::Uint16);
        assert!(!lossless);
        assert_eq!(bytes.len(), raw.len() * 2);
        for (index, &expected) in raw.iter().enumerate() {
            let quantized = u16::from_le_bytes(bytes[index * 2..index * 2 + 2].try_into().unwrap());
            let reconstructed = quantized as f64 * slope + intercept;
            assert!((reconstructed - expected as f64).abs() <= 0.5 * slope + 1e-9);
        }
    }

    #[test]
    fn quantize_samples_of_out_of_i16_range_values_falls_back_to_bounded_error_uint16() {
        let raw = vec![-40000.0f32, 0.0, 40000.0];
        let (_, format, _, _, lossless) = quantize_samples(&raw);
        assert_eq!(format, SampleFormat::Uint16);
        assert!(!lossless);
    }

    #[test]
    fn pack_volume_texture_without_downsampling_round_trips_exactly() {
        let volume = axis_aligned_test_volume(4, 4, 4);
        let packed = pack_volume_texture(&volume, None, None, TextureCompression::None).unwrap();
        assert!(packed.meta.lossless);
        assert!(!packed.meta.downsampled);
        assert_eq!((packed.meta.width, packed.meta.height, packed.meta.depth), (4, 4, 4));
        assert_eq!(packed.meta.native_dims, (4, 4, 4));
        assert_eq!(packed.payload.len(), volume.samples.len() * 2);

        for (index, &expected) in volume.samples.iter().enumerate() {
            let value = i16::from_le_bytes(packed.payload[index * 2..index * 2 + 2].try_into().unwrap());
            assert_eq!(value as f32, expected);
        }
    }

    #[test]
    fn pack_volume_texture_downsamples_to_fit_target_max_dim() {
        let volume = axis_aligned_test_volume(8, 8, 8);
        let packed = pack_volume_texture(&volume, Some(4), None, TextureCompression::None).unwrap();
        assert!(packed.meta.downsampled);
        assert!(packed.meta.width <= 4 && packed.meta.height <= 4 && packed.meta.depth <= 4);
        assert_eq!(packed.meta.native_dims, (8, 8, 8));
        // Spacing must widen proportionally so physical extent is preserved.
        assert!(packed.meta.col_spacing_mm > volume.col_spacing_mm);
        assert!(packed.meta.row_spacing_mm > volume.row_spacing_mm);
    }

    #[test]
    fn pack_volume_texture_below_target_max_dim_is_a_no_op() {
        let volume = axis_aligned_test_volume(4, 4, 4);
        let packed = pack_volume_texture(&volume, Some(64), None, TextureCompression::None).unwrap();
        assert!(!packed.meta.downsampled);
        assert_eq!((packed.meta.width, packed.meta.height, packed.meta.depth), (4, 4, 4));
    }

    #[test]
    fn pack_volume_texture_gzip_round_trips_to_the_uncompressed_payload() {
        let volume = axis_aligned_test_volume(4, 4, 4);
        let plain = pack_volume_texture(&volume, None, None, TextureCompression::None).unwrap();
        let gzipped = pack_volume_texture(&volume, None, None, TextureCompression::Gzip).unwrap();

        assert_eq!(gzipped.meta.payload_bytes_raw, plain.payload.len() as u64);
        assert_eq!(gzipped.meta.payload_bytes_stored, gzipped.payload.len() as u64);

        let mut decoder = flate2::read::GzDecoder::new(&gzipped.payload[..]);
        let mut decompressed = Vec::new();
        std::io::Read::read_to_end(&mut decoder, &mut decompressed).unwrap();
        assert_eq!(decompressed, plain.payload);
    }

    #[test]
    fn pack_volume_texture_carries_default_window_through_to_meta() {
        let volume = axis_aligned_test_volume(2, 2, 2);
        let packed = pack_volume_texture(&volume, None, Some((40.0, 400.0)), TextureCompression::None).unwrap();
        assert_eq!(packed.meta.default_window_center, Some(40.0));
        assert_eq!(packed.meta.default_window_width, Some(400.0));
    }

    #[test]
    fn resample_frame_2d_is_a_no_op_when_already_within_target() {
        let values: Vec<f32> = (0..16).map(|value| value as f32).collect();
        let (out, w, h) = resample_frame_2d(&values, 4, 4, 8);
        assert_eq!((w, h), (4, 4));
        assert_eq!(out, values);
    }

    #[test]
    fn resample_frame_2d_downsamples_a_linear_ramp_correctly() {
        // 8x8, values = col only (a ramp 0..7 repeated in every row) - halving to 4x4 should
        // sample columns 0,2.333,4.666,7 (bilinear on an exact linear ramp reproduces the ramp
        // value at any fractional position).
        let width = 8u32;
        let height = 8u32;
        let mut values = vec![0f32; (width * height) as usize];
        for row in 0..height {
            for col in 0..width {
                values[(row * width + col) as usize] = col as f32;
            }
        }
        let (out, out_w, out_h) = resample_frame_2d(&values, width, height, 4);
        assert_eq!((out_w, out_h), (4, 4));
        let expected_cols = [0.0f32, 7.0 / 3.0, 14.0 / 3.0, 7.0];
        for row in 0..out_h {
            for col in 0..out_w {
                let value = out[(row * out_w + col) as usize];
                assert!(
                    (value - expected_cols[col as usize]).abs() < 1e-4,
                    "row={row} col={col} value={value} expected={}",
                    expected_cols[col as usize]
                );
            }
        }
    }

    #[test]
    fn pack_frame_texture_rejects_a_sample_count_mismatch() {
        let error = pack_frame_texture(&[0.0; 3], 2, 2, None, None, TextureCompression::None).unwrap_err();
        assert!(matches!(
            error,
            TextureExportError::SampleCountMismatch { expected: 4, found: 3 }
        ));
    }

    #[test]
    fn pack_frame_texture_downsamples_and_round_trips_within_tolerance() {
        let width = 8u32;
        let height = 8u32;
        let values: Vec<f32> = (0..width * height).map(|index| index as f32).collect();
        let packed = pack_frame_texture(&values, width, height, None, Some(4), TextureCompression::None).unwrap();
        assert!(packed.meta.downsampled);
        assert_eq!(packed.meta.depth, 1);
        assert_eq!(packed.meta.native_dims, (8, 8, 1));
        assert!(packed.meta.width <= 4 && packed.meta.height <= 4);
    }

    #[test]
    fn texture_meta_to_json_round_trips_expected_fields() {
        let volume = axis_aligned_test_volume(2, 2, 2);
        let packed = pack_volume_texture(&volume, None, Some((40.0, 400.0)), TextureCompression::Gzip).unwrap();
        let json = packed.meta.to_json();
        assert_eq!(json["formatVersion"], 1);
        assert_eq!(json["contentKind"], "volume");
        assert_eq!(json["compression"], "gzip");
        assert_eq!(json["width"], 2);
        assert_eq!(json["defaultWindowCenter"], 40.0);
        assert_eq!(json["defaultWindowWidth"], 400.0);
        assert_eq!(json["nativeDims"], serde_json::json!([2, 2, 2]));
    }
}
