use dicom_object::DefaultDicomObject;

use super::render::decode_frame_grayscale_values;
use super::types::RenderError;

/// Options controlling how a frame's grayscale values are binned. The bin range defaults to the
/// frame's own observed min/max (`min_value`/`max_value` left `None`) - set both to compare
/// several frames/instances on a shared, fixed range instead.
#[derive(Clone, Debug)]
pub struct HistogramOptions {
    pub bin_count: u32,
    pub min_value: Option<f64>,
    pub max_value: Option<f64>,
}

impl Default for HistogramOptions {
    fn default() -> Self {
        Self {
            bin_count: 256,
            min_value: None,
            max_value: None,
        }
    }
}

/// A histogram of one frame's grayscale values, AFTER the same modality LUT (rescale
/// slope/intercept) `decode_frame_grayscale_values` already applies for rendering and the pixel
/// probe - so e.g. a CT frame's bins are in Hounsfield units, matching what the viewer's hover
/// probe already reports for the same frame.
#[derive(Clone, Debug)]
pub struct FrameHistogram {
    pub frame_index: usize,
    pub bin_count: u32,
    /// Inclusive lower bound of the binned range - either `HistogramOptions::min_value`, or the
    /// frame's own observed minimum when that was left unset.
    pub range_min: f64,
    /// Exclusive upper bound of the binned range (same source as `range_min`).
    pub range_max: f64,
    pub bin_width: f64,
    /// One count per bin, length `bin_count`.
    pub counts: Vec<u32>,
    pub pixel_count: u32,
    /// The frame's actual observed min/max, independent of any `range_min`/`range_max` clamping.
    pub min_value: f64,
    pub max_value: f64,
    pub mean: f64,
    pub std_dev: f64,
}

/// Computes a histogram of one frame's grayscale values. See `HistogramOptions`/`FrameHistogram`
/// for what "grayscale value" and "bin range" mean here.
pub fn compute_frame_histogram(
    object: &DefaultDicomObject,
    frame_index: usize,
    options: &HistogramOptions,
) -> Result<FrameHistogram, RenderError> {
    let (_metadata, values) = decode_frame_grayscale_values(object, frame_index)?;
    Ok(histogram_from_values(frame_index, &values, options))
}

/// Computes a histogram for every frame of the instance, in frame order. Mirrors
/// `render_all_dicom_frames`'s own per-frame-decode-in-a-loop shape.
pub fn compute_instance_histograms(
    object: &DefaultDicomObject,
    options: &HistogramOptions,
) -> Result<Vec<FrameHistogram>, RenderError> {
    let (first_metadata, first_values) = decode_frame_grayscale_values(object, 0)?;
    let number_of_frames = first_metadata.number_of_frames.max(1);

    let mut histograms = Vec::with_capacity(number_of_frames);
    histograms.push(histogram_from_values(0, &first_values, options));
    for frame_index in 1..number_of_frames {
        let (_metadata, values) = decode_frame_grayscale_values(object, frame_index)?;
        histograms.push(histogram_from_values(frame_index, &values, options));
    }
    Ok(histograms)
}

fn histogram_from_values(frame_index: usize, values: &[f64], options: &HistogramOptions) -> FrameHistogram {
    let bin_count = options.bin_count.max(1);
    let pixel_count = values.len();

    let (observed_min, observed_max, sum) = values.iter().fold(
        (f64::INFINITY, f64::NEG_INFINITY, 0.0),
        |(min, max, sum), &value| (min.min(value), max.max(value), sum + value),
    );
    let (observed_min, observed_max) = if pixel_count == 0 {
        (0.0, 0.0)
    } else {
        (observed_min, observed_max)
    };
    let mean = if pixel_count == 0 { 0.0 } else { sum / pixel_count as f64 };
    let variance = if pixel_count == 0 {
        0.0
    } else {
        values.iter().map(|value| (value - mean).powi(2)).sum::<f64>() / pixel_count as f64
    };

    let range_min = options.min_value.unwrap_or(observed_min);
    let mut range_max = options.max_value.unwrap_or(observed_max);
    if range_max <= range_min {
        // A blank/uniform frame, or a caller-supplied degenerate range - widen it so every value
        // still lands in bin 0 instead of dividing by a zero-width bin.
        range_max = range_min + 1.0;
    }
    let bin_width = (range_max - range_min) / bin_count as f64;

    let mut counts = vec![0u32; bin_count as usize];
    for &value in values {
        let bin_index = (((value - range_min) / bin_width) as i64).clamp(0, bin_count as i64 - 1) as usize;
        counts[bin_index] += 1;
    }

    FrameHistogram {
        frame_index,
        bin_count,
        range_min,
        range_max,
        bin_width,
        counts,
        pixel_count: pixel_count as u32,
        min_value: observed_min,
        max_value: observed_max,
        mean,
        std_dev: variance.sqrt(),
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::{compute_frame_histogram, compute_instance_histograms, HistogramOptions};
    use crate::dicom_io::read_dicom_file;

    fn fixture_path(name: &str) -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("test/files").join(name)
    }

    #[test]
    fn frame_histogram_bins_every_pixel_within_the_observed_range() {
        let object = read_dicom_file(fixture_path("ct.dcm")).unwrap();
        let options = HistogramOptions {
            bin_count: 16,
            ..Default::default()
        };

        let histogram = compute_frame_histogram(&object, 0, &options).unwrap();

        assert_eq!(histogram.bin_count, 16);
        assert_eq!(histogram.counts.len(), 16);
        let total: u64 = histogram.counts.iter().map(|&count| count as u64).sum();
        assert_eq!(total, histogram.pixel_count as u64);
        assert!(histogram.min_value >= histogram.range_min);
        assert!(histogram.max_value <= histogram.range_max);
        assert!(histogram.range_max > histogram.range_min);
    }

    #[test]
    fn explicit_range_clamps_out_of_range_values_into_the_edge_bins() {
        let object = read_dicom_file(fixture_path("ct.dcm")).unwrap();
        let options = HistogramOptions {
            bin_count: 4,
            min_value: Some(0.0),
            max_value: Some(100.0),
        };

        let histogram = compute_frame_histogram(&object, 0, &options).unwrap();

        assert_eq!(histogram.range_min, 0.0);
        assert_eq!(histogram.range_max, 100.0);
        let total: u64 = histogram.counts.iter().map(|&count| count as u64).sum();
        assert_eq!(total, histogram.pixel_count as u64);
    }

    #[test]
    fn instance_histograms_cover_every_frame_in_order() {
        let object = read_dicom_file(fixture_path("ct.dcm")).unwrap();
        let histograms = compute_instance_histograms(&object, &HistogramOptions::default()).unwrap();

        assert!(!histograms.is_empty());
        for (index, histogram) in histograms.iter().enumerate() {
            assert_eq!(histogram.frame_index, index);
        }
    }
}
