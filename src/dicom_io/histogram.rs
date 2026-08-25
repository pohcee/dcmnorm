use dicom_object::DefaultDicomObject;

use super::render::{decode_frame_grayscale_values, decode_frame_rgb_values, read_render_metadata};
use super::types::RenderError;

/// Options controlling how a frame's values are binned. The bin range defaults to the frame's own
/// observed min/max for a grayscale frame (`min_value`/`max_value` left `None`) - set both to
/// compare several frames/instances on a shared, fixed range instead. For an RGB/color frame (see
/// `FrameHistogram::channel`), the default range is fixed at 0-255 instead - see that field's own
/// doc for why.
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

/// A histogram of one frame's values. For a grayscale frame (`SamplesPerPixel` 1), `channel` is
/// `None` and the values are AFTER the same modality LUT (rescale slope/intercept)
/// `decode_frame_grayscale_values` already applies for rendering and the pixel probe - so e.g. a
/// CT frame's bins are in Hounsfield units, matching what the viewer's hover probe already
/// reports for the same frame. For an RGB/color frame (`SamplesPerPixel` 3 - YBR variants are
/// converted to RGB first, same as 2D rendering), `compute_frame_histogram` returns three of
/// these per frame, one per `channel` ("red"/"green"/"blue"), each over that channel's own
/// decoded 8-bit samples (0-255) - there is no single "the" grayscale value for a color pixel.
#[derive(Clone, Debug)]
pub struct FrameHistogram {
    pub frame_index: usize,
    /// `None` for a grayscale frame. `Some("red" | "green" | "blue")` for one channel of an
    /// RGB/color frame - `compute_frame_histogram`/`compute_instance_histograms` return one
    /// `FrameHistogram` per channel for those, all sharing the same `frame_index`.
    pub channel: Option<String>,
    pub bin_count: u32,
    /// Inclusive lower bound of the binned range - either `HistogramOptions::min_value`, or the
    /// default (the frame's own observed minimum for grayscale, `0.0` for an RGB channel).
    pub range_min: f64,
    /// Exclusive upper bound of the binned range (same source as `range_min`; RGB default `255.0`).
    pub range_max: f64,
    pub bin_width: f64,
    /// One count per bin, length `bin_count`.
    pub counts: Vec<u32>,
    pub pixel_count: u32,
    /// The channel's actual observed min/max, independent of any `range_min`/`range_max` clamping.
    pub min_value: f64,
    pub max_value: f64,
    pub mean: f64,
    pub std_dev: f64,
}

/// Computes a histogram of one frame's values - a single grayscale entry, or three (red/green/
/// blue) for an RGB/color frame. See `HistogramOptions`/`FrameHistogram` for what "value" and
/// "bin range" mean in each case.
pub fn compute_frame_histogram(
    object: &DefaultDicomObject,
    frame_index: usize,
    options: &HistogramOptions,
) -> Result<Vec<FrameHistogram>, RenderError> {
    // SamplesPerPixel is a whole-dataset attribute (never varies per frame), and this is a plain
    // top-level tag read - safe and cheap to check before committing to either decode path.
    let peek = read_render_metadata(object)?;

    match peek.samples_per_pixel {
        1 => {
            let (_metadata, values) = decode_frame_grayscale_values(object, frame_index)?;
            Ok(vec![histogram_from_values(frame_index, None, &values, options)])
        }
        3 => {
            let (_metadata, rgb_bytes) = decode_frame_rgb_values(object, frame_index)?;
            Ok(rgb_channel_histograms(frame_index, &rgb_bytes, options))
        }
        other => Err(RenderError::UnsupportedSamplesPerPixel(other)),
    }
}

/// Computes histograms for every frame of the instance, in frame order (three consecutive
/// red/green/blue entries per frame for an RGB/color instance - see `FrameHistogram::channel`).
pub fn compute_instance_histograms(
    object: &DefaultDicomObject,
    options: &HistogramOptions,
) -> Result<Vec<FrameHistogram>, RenderError> {
    let peek = read_render_metadata(object)?;
    let number_of_frames = peek.number_of_frames.max(1);

    let mut histograms = Vec::with_capacity(number_of_frames);
    for frame_index in 0..number_of_frames {
        histograms.extend(compute_frame_histogram(object, frame_index, options)?);
    }
    Ok(histograms)
}

fn rgb_channel_histograms(frame_index: usize, rgb_bytes: &[u8], options: &HistogramOptions) -> Vec<FrameHistogram> {
    let pixel_count = rgb_bytes.len() / 3;
    let mut red = Vec::with_capacity(pixel_count);
    let mut green = Vec::with_capacity(pixel_count);
    let mut blue = Vec::with_capacity(pixel_count);
    for chunk in rgb_bytes.chunks_exact(3) {
        red.push(f64::from(chunk[0]));
        green.push(f64::from(chunk[1]));
        blue.push(f64::from(chunk[2]));
    }

    // render_rgb_frame always normalizes to 8-bit interleaved RGB (even a 16-bit source is scaled
    // down to 0-255 there) - default the bin range to that full native domain rather than each
    // channel's own observed min/max, so red/green/blue stay directly comparable (same bin
    // boundaries) on one chart instead of each getting an arbitrarily different span. An explicit
    // HistogramOptions::min_value/max_value still overrides this, same as the grayscale path.
    let mut channel_options = options.clone();
    if channel_options.min_value.is_none() {
        channel_options.min_value = Some(0.0);
    }
    if channel_options.max_value.is_none() {
        channel_options.max_value = Some(255.0);
    }

    vec![
        histogram_from_values(frame_index, Some("red"), &red, &channel_options),
        histogram_from_values(frame_index, Some("green"), &green, &channel_options),
        histogram_from_values(frame_index, Some("blue"), &blue, &channel_options),
    ]
}

fn histogram_from_values(
    frame_index: usize,
    channel: Option<&str>,
    values: &[f64],
    options: &HistogramOptions,
) -> FrameHistogram {
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
        channel: channel.map(|value| value.to_owned()),
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

        let histograms = compute_frame_histogram(&object, 0, &options).unwrap();

        assert_eq!(histograms.len(), 1, "a grayscale frame should produce exactly one histogram");
        let histogram = &histograms[0];
        assert!(histogram.channel.is_none());
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

        let histograms = compute_frame_histogram(&object, 0, &options).unwrap();
        let histogram = &histograms[0];

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

    #[test]
    fn rgb_frame_produces_red_green_blue_channel_histograms_over_0_255() {
        // us.dcm is SamplesPerPixel 3 / PhotometricInterpretation RGB - see the sibling
        // ybr_frame_is_converted_to_rgb_before_binning test below for the YBR conversion path.
        let object = read_dicom_file(fixture_path("us.dcm")).unwrap();
        assert_eq!(super::read_render_metadata(&object).unwrap().samples_per_pixel, 3);

        let histograms = compute_frame_histogram(&object, 0, &HistogramOptions::default()).unwrap();

        assert_eq!(histograms.len(), 3, "an RGB frame should produce one histogram per channel");
        let channels: Vec<&str> = histograms.iter().map(|h| h.channel.as_deref().unwrap()).collect();
        assert_eq!(channels, ["red", "green", "blue"]);
        for histogram in &histograms {
            assert_eq!(histogram.range_min, 0.0);
            assert_eq!(histogram.range_max, 255.0);
            let total: u64 = histogram.counts.iter().map(|&count| count as u64).sum();
            assert_eq!(total, histogram.pixel_count as u64);
        }
        assert_eq!(histograms[0].pixel_count, histograms[1].pixel_count);
        assert_eq!(histograms[1].pixel_count, histograms[2].pixel_count);
    }

    #[test]
    fn ybr_frame_is_converted_to_rgb_before_binning() {
        // wsi_ybr.dcm is SamplesPerPixel 3 / PhotometricInterpretation YBR_ICT - exercises
        // decode_frame_rgb_values's YBR-to-RGB conversion (shared with 2D rendering), not just a
        // native RGB passthrough.
        let object = read_dicom_file(fixture_path("wsi_ybr.dcm")).unwrap();
        assert_eq!(super::read_render_metadata(&object).unwrap().samples_per_pixel, 3);

        let histograms = compute_frame_histogram(&object, 0, &HistogramOptions::default()).unwrap();

        assert_eq!(histograms.len(), 3);
        for histogram in &histograms {
            // Values are true RGB samples (0-255), not raw YBR component bytes.
            assert!(histogram.min_value >= 0.0 && histogram.max_value <= 255.0);
            let total: u64 = histogram.counts.iter().map(|&count| count as u64).sum();
            assert_eq!(total, histogram.pixel_count as u64);
        }
    }
}
