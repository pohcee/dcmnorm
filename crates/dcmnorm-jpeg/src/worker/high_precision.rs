use alloc::vec::Vec;
use core::mem;

use crate::alloc::sync::Arc;
use crate::error::{Error, Result, UnsupportedFeature};
use crate::idct::dequantize_and_idct_block_high_precision;
use crate::parser::Component;

use super::{RowData, Worker};

/// Worker for JPEG Extended (Process 2 & 4) at sample precisions other than 8.
///
/// `dequantize_and_idct_block_8x8`/`_4x4`/`_2x2`/`_1x1` (used by `ImmediateWorker` and
/// `MpscWorker`) are 8-bit-only by construction, so this is a separate implementation rather
/// than a change to either - see `dequantize_and_idct_block_high_precision`'s own doc comment.
/// Single-threaded and scoped to exactly one grayscale component: this transfer syntax's real
/// DICOM usage (single-component 12-bit medical grayscale) doesn't need the multithreaded
/// dispatch `MpscWorker` exists for, and supporting >1 component would also mean generalizing
/// the color-conversion/chroma-upsampling pipeline (`compute_image`/`Upsampler`), which are
/// equally 8-bit-only - out of scope until there's a real multi-component >8-bit file to
/// justify it. `decoder.rs` gates component count to 1 before selecting this worker; `start`/
/// `append_row`/`get_result` all assert `index == 0` so a future caller that relaxes that gate
/// finds out immediately instead of silently mis-decoding component 1+.
pub struct HighPrecisionWorker {
    precision: u8,
    offset: usize,
    result: Vec<u16>,
    component: Option<Component>,
    quantization_table: Option<Arc<[u16; 64]>>,
}

impl HighPrecisionWorker {
    pub fn new(precision: u8) -> Self {
        HighPrecisionWorker {
            precision,
            offset: 0,
            result: Vec::new(),
            component: None,
            quantization_table: None,
        }
    }
}

impl Worker for HighPrecisionWorker {
    fn start(&mut self, data: RowData) -> Result<()> {
        assert_eq!(data.index, 0, "HighPrecisionWorker only supports a single grayscale component");

        if data.component.dct_scale != 8 {
            // Scaled-down (thumbnail-style) decode isn't implemented for this path - DICOM
            // rendering always wants the real image, so this should never actually be
            // requested, but fail cleanly rather than silently mis-decode if it ever is.
            return Err(Error::Unsupported(UnsupportedFeature::SamplePrecision(self.precision)));
        }

        self.offset = 0;
        self.result.clear();
        self.result.resize(
            data.component.block_size.width as usize
                * data.component.block_size.height as usize
                * data.component.dct_scale
                * data.component.dct_scale,
            0u16,
        );
        self.quantization_table = Some(data.quantization_table);
        self.component = Some(data.component);
        Ok(())
    }

    fn append_row(&mut self, (index, data): (usize, Vec<i16>)) -> Result<()> {
        assert_eq!(index, 0, "HighPrecisionWorker only supports a single grayscale component");

        let component = self.component.as_ref().expect("start() must be called before append_row()");
        let quantization_table = self
            .quantization_table
            .as_ref()
            .expect("start() must be called before append_row()");
        let block_count = component.block_size.width as usize * component.vertical_sampling_factor as usize;
        let line_stride = component.block_size.width as usize * component.dct_scale;

        debug_assert_eq!(data.len(), block_count * 64);

        for i in 0..block_count {
            let x = (i % component.block_size.width as usize) * component.dct_scale;
            let y = (i / component.block_size.width as usize) * component.dct_scale;

            let coefficients: &[i16; 64] = data[i * 64..(i + 1) * 64].try_into().unwrap();
            let output = &mut self.result[self.offset + y * line_stride + x..];

            dequantize_and_idct_block_high_precision(
                coefficients,
                quantization_table,
                self.precision,
                line_stride,
                output,
            );
        }

        self.offset += block_count * component.dct_scale * component.dct_scale;
        Ok(())
    }

    fn get_result(&mut self, index: usize) -> Result<Vec<u8>> {
        assert_eq!(index, 0, "HighPrecisionWorker only supports a single grayscale component");

        let component = self.component.as_ref().expect("start() must be called before get_result()");
        let line_stride = component.block_size.width as usize * component.dct_scale;
        let cropped_width = component.size.width as usize;
        let cropped_height = component.size.height as usize;

        let padded = mem::take(&mut self.result);
        let mut cropped = Vec::with_capacity(cropped_width * cropped_height);
        for row in 0..cropped_height {
            let start = row * line_stride;
            cropped.extend_from_slice(&padded[start..start + cropped_width]);
        }

        // Native-endian, matching compute_image_lossless's own established convention for
        // this crate's >8-bit output (see decoder/lossless.rs's convert_to_u8).
        Ok(cropped.iter().flat_map(|sample| sample.to_ne_bytes()).collect())
    }
}
