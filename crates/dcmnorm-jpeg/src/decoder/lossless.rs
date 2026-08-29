use crate::decoder::{Decoder, MAX_COMPONENTS};
use crate::error::{Error, Result};
use crate::huffman::HuffmanDecoder;
use crate::marker::Marker;
use crate::parser::Predictor;
use crate::parser::{Component, FrameInfo, ScanInfo};
use std::io::Read;

impl<R: Read> Decoder<R> {
    /// decode_scan_lossless
    pub fn decode_scan_lossless(
        &mut self,
        frame: &FrameInfo,
        scan: &ScanInfo,
    ) -> Result<(Option<Marker>, Vec<Vec<u16>>)> {
        let ncomp = scan.component_indices.len();
        let npixel = frame.image_size.height as usize * frame.image_size.width as usize;
        assert!(ncomp <= MAX_COMPONENTS);
        let mut results = vec![vec![0u16; npixel]; ncomp];

        let components: Vec<Component> = scan
            .component_indices
            .iter()
            .map(|&i| frame.components[i].clone())
            .collect();

        // Verify that all required huffman tables has been set.
        if scan
            .dc_table_indices
            .iter()
            .any(|&i| self.dc_huffman_tables[i].is_none())
        {
            return Err(Error::Format(
                "scan makes use of unset dc huffman table".to_owned(),
            ));
        }

        let mut huffman = HuffmanDecoder::new();
        let reader = &mut self.reader;
        let mut mcus_left_until_restart = self.restart_interval;
        let mut expected_rst_num = 0;
        let mut ra = [0u16; MAX_COMPONENTS];
        let mut rb = [0u16; MAX_COMPONENTS];
        let mut rc = [0u16; MAX_COMPONENTS];

        let width = frame.image_size.width as usize;
        let height = frame.image_size.height as usize;

        // Tracks, per pixel index, whether that sample is the first one decoded after a
        // restart marker (or the very first sample of the scan). Per T.81 Annex H.1.2.3,
        // prediction for such a sample must use the default constant regardless of its
        // position in the line, so this has to be recorded now while we still know where
        // the restart markers actually fell -- the reconstruction pass below happens as a
        // separate loop over `differences` and can no longer observe `mcus_left_until_restart`
        // transitioning per-pixel.
        let mut restart_starts = vec![false; npixel];
        let mut differences = vec![Vec::with_capacity(npixel); ncomp];
        for mcu_y in 0..height {
            for mcu_x in 0..width {
                if self.restart_interval > 0 {
                    if mcus_left_until_restart == 0 {
                        match huffman.take_marker(reader)? {
                            Some(Marker::RST(n)) => {
                                if n != expected_rst_num {
                                    return Err(Error::Format(format!(
                                        "found RST{} where RST{} was expected",
                                        n, expected_rst_num
                                    )));
                                }

                                huffman.reset();

                                expected_rst_num = (expected_rst_num + 1) % 8;
                                mcus_left_until_restart = self.restart_interval;
                                restart_starts[mcu_y * width + mcu_x] = true;
                            }
                            Some(marker) => {
                                return Err(Error::Format(format!(
                                    "found marker {:?} inside scan where RST{} was expected",
                                    marker, expected_rst_num
                                )))
                            }
                            None => {
                                return Err(Error::Format(format!(
                                    "no marker found where RST{} was expected",
                                    expected_rst_num
                                )))
                            }
                        }
                    }

                    mcus_left_until_restart -= 1;
                }

                for (i, _component) in components.iter().enumerate() {
                    let dc_table = self.dc_huffman_tables[scan.dc_table_indices[i]]
                        .as_ref()
                        .unwrap();
                    let value = huffman.decode(reader, dc_table)?;
                    let diff = match value {
                        0 => 0,
                        1..=15 => huffman.receive_extend(reader, value)? as i32,
                        16 => 32768,
                        _ => {
                            // Section F.1.2.1.1
                            // Table F.1
                            return Err(Error::Format(
                                "invalid DC difference magnitude category".to_owned(),
                            ));
                        }
                    };
                    differences[i].push(diff);
                }
            }
        }

        if scan.predictor_selection == Predictor::Ra {
            for (i, _component) in components.iter().enumerate() {
                for mcu_y in 0..height {
                    for mcu_x in 0..width {
                        let idx = mcu_y * width + mcu_x;
                        let diff = differences[i][idx];
                        let prediction = if (mcu_x == 0 && mcu_y == 0) || restart_starts[idx] {
                            // start of scan, or first sample after a restart marker: the
                            // encoder resets its predictor here too, so this must use the
                            // default constant even if mcu_x > 0 (restart intervals need
                            // not fall on line boundaries).
                            if frame.precision > 1 + scan.point_transform {
                                1 << (frame.precision - scan.point_transform - 1) as i32
                            } else {
                                0
                            }
                        } else if mcu_x == 0 {
                            // start of a line (not a restart): predict from the pixel above (Rb)
                            results[i][idx - width] as i32
                        } else {
                            // predict from the pixel to the left (Ra)
                            results[i][idx - 1] as i32
                        };
                        let result = ((prediction + diff) & 0xFFFF) as u16; // modulo 2^16
                        results[i][idx] = result << scan.point_transform;
                    }
                }
            }
        } else {
            for mcu_y in 0..height {
                for mcu_x in 0..width {
                    for (i, _component) in components.iter().enumerate() {
                        let diff = differences[i][mcu_y * width + mcu_x];

                        // The following lines could be further optimized, e.g. moving the checks
                        // and updates of the previous values into the prediction function or
                        // iterating such that diagonals with mcu_x + mcu_y = const are computed at
                        // the same time to exploit independent predictions in this case
                        if mcu_x > 0 {
                            ra[i] = results[i][mcu_y * frame.image_size.width as usize + mcu_x - 1];
                        }
                        if mcu_y > 0 {
                            rb[i] =
                                results[i][(mcu_y - 1) * frame.image_size.width as usize + mcu_x];
                            if mcu_x > 0 {
                                rc[i] = results[i]
                                    [(mcu_y - 1) * frame.image_size.width as usize + (mcu_x - 1)];
                            }
                        }
                        let prediction = predict(
                            ra[i] as i32,
                            rb[i] as i32,
                            rc[i] as i32,
                            scan.predictor_selection,
                            scan.point_transform,
                            frame.precision,
                            mcu_x,
                            mcu_y,
                            restart_starts[mcu_y * width + mcu_x],
                        );
                        let result = ((prediction + diff) & 0xFFFF) as u16; // modulo 2^16
                        results[i][mcu_y * width + mcu_x] = result << scan.point_transform;
                    }
                }
            }
        }

        let mut marker = huffman.take_marker(&mut self.reader)?;
        while let Some(Marker::RST(_)) = marker {
            marker = self.read_marker().ok();
        }
        Ok((marker, results))
    }
}

/// H.1.2.1
#[allow(clippy::too_many_arguments)]
fn predict(
    ra: i32,
    rb: i32,
    rc: i32,
    predictor: Predictor,
    point_transform: u8,
    input_precision: u8,
    ix: usize,
    iy: usize,
    restart: bool,
) -> i32 {
    if (ix == 0 && iy == 0) || restart {
        // start of first line or restart
        if input_precision > 1 + point_transform {
            1 << (input_precision - point_transform - 1)
        } else {
            0
        }
    } else if iy == 0 {
        // rest of first line
        ra
    } else if ix == 0 {
        // start of other line
        rb
    } else {
        // use predictor Table H.1
        match predictor {
            Predictor::NoPrediction => 0,
            Predictor::Ra => ra,
            Predictor::Rb => rb,
            Predictor::Rc => rc,
            Predictor::RaRbRc1 => ra + rb - rc,
            Predictor::RaRbRc2 => ra + ((rb - rc) >> 1),
            Predictor::RaRbRc3 => rb + ((ra - rc) >> 1),
            Predictor::RaRb => (ra + rb) / 2,
        }
    }
}

pub fn compute_image_lossless(frame: &FrameInfo, mut data: Vec<Vec<u16>>) -> Result<Vec<u8>> {
    if data.is_empty() || data.iter().any(Vec::is_empty) {
        return Err(Error::Format("not all components have data".to_owned()));
    }
    let output_size = frame.output_size;
    let components = &frame.components;
    let ncomp = components.len();

    if ncomp == 1 {
        let decoded = convert_to_u8(frame, data.remove(0));
        Ok(decoded)
    } else {
        let mut decoded: Vec<u16> =
            vec![0u16; ncomp * output_size.width as usize * output_size.height as usize];
        for (x, chunk) in decoded.chunks_mut(ncomp).enumerate() {
            for (i, (component_data, _)) in data.iter().zip(components.iter()).enumerate() {
                chunk[i] = component_data[x];
            }
        }
        let decoded = convert_to_u8(frame, decoded);
        Ok(decoded)
    }
}

fn convert_to_u8(frame: &FrameInfo, data: Vec<u16>) -> Vec<u8> {
    if frame.precision == 8 {
        data.iter().map(|x| *x as u8).collect()
    } else {
        // we output native endian, which is the standard for image-rs
        let ne_bytes: Vec<_> = data.iter().map(|x| x.to_ne_bytes()).collect();
        ne_bytes.concat()
    }
}

#[cfg(test)]
mod restart_marker_regression_test {
    use crate::Decoder;

    /// Regression test for a real production bug (periodic banding on lossless JPEG using
    /// restart markers, common in mammography/tomosynthesis DICOM - see this crate's README):
    /// the `Predictor::Ra` (Selection Value 1) reconstruction pass must reset to the frame's
    /// default predictor constant for the first sample after *every* restart marker, not just
    /// the very first sample of the scan, and regardless of where that sample falls relative to
    /// a line boundary (PS3.5/T.81 Annex H.1.2.3). This builds a minimal, hand-encoded 4x1
    /// single-component lossless JPEG (predictor Ra, 8-bit precision, restart_interval=2, one
    /// RST0 marker after the first two samples) using a small custom canonical Huffman table,
    /// and checks the decoded pixel values against values computed by hand from the spec rule.
    #[test]
    fn restart_resets_predictor_to_default_constant_not_left_neighbor() {
        // Custom 3-symbol canonical DC Huffman table (table class 0, id 0):
        // BITS = 1 code of length 1, 1 of length 2, 1 of length 3.
        // HUFFVAL = [0, 1, 2] (DC difference categories 0, 1, 2).
        // Canonical codes (ITU T.81 Annex C): category 0 -> "0", category 1 -> "10",
        // category 2 -> "110".
        let dht: &[u8] = &[
            0xFF, 0xC4, 0x00, 0x16, // DHT, length 22
            0x00, // Tc=0 (DC), Th=0
            0x01, 0x01, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, // BITS[1..16]
            0x00, 0x01, 0x02, // HUFFVAL
        ];
        // SOF3 (lossless sequential), 8-bit precision, height=1, width=4, 1 component.
        let sof3: &[u8] = &[
            0xFF, 0xC3, 0x00, 0x0B, // SOF3, length 11
            0x08, // precision
            0x00, 0x01, // height = 1
            0x00, 0x04, // width = 4
            0x01, // Nf = 1 component
            0x01, 0x11, 0x00, // component id=1, sampling 1x1, quant table id 0 (unused)
        ];
        // DRI: restart_interval = 2 MCUs (one restart marker after samples 0 and 1).
        let dri: &[u8] = &[0xFF, 0xDD, 0x00, 0x04, 0x00, 0x02];
        // SOS: 1 component, selector=1 using DC table 0, predictor selection Ss=1 (Ra), Se=0,
        // Ah/Al=0.
        let sos: &[u8] = &[
            0xFF, 0xDA, 0x00, 0x08, 0x01, 0x01, 0x00, 0x01, 0x00, 0x00,
        ];
        // Entropy-coded data, MSB-first bit packing:
        //   sample 0 (scan start, default predictor=128): category 2 "110" + extra "10"
        //     (additional=2 => diff=+2) => "11010101" (byte-aligned) = 0xD5, decoded value 130
        //   sample 1 (left predictor=130): category 1 "10" + extra "1" (additional=1 => diff=+1)
        //     => folded into the byte above, decoded value 131
        //   [restart marker RST0]
        //   sample 2 (restart start, default predictor=128 - NOT left-neighbor 131): category 2
        //     "110" + extra "00" (additional=0 => diff=-3) => decoded value 125
        //   sample 3 (left predictor=125): category 1 "10" + extra "0" (additional=0 => diff=-1)
        //     => folded into the byte above = "11000100" = 0xC4, decoded value 124
        let entropy: &[u8] = &[0xD5, 0xFF, 0xD0, 0xC4];
        let eoi: &[u8] = &[0xFF, 0xD9];

        let mut bytes = vec![0xFF, 0xD8]; // SOI
        bytes.extend_from_slice(dht);
        bytes.extend_from_slice(sof3);
        bytes.extend_from_slice(dri);
        bytes.extend_from_slice(sos);
        bytes.extend_from_slice(entropy);
        bytes.extend_from_slice(eoi);

        let mut decoder = Decoder::new(&bytes[..]);
        let pixels = decoder.decode().expect("synthetic restart-marker JPEG should decode");

        // If the restart-reset fix were absent, sample 2 would incorrectly predict from the
        // left neighbor (131) instead of resetting to the default constant (128), giving 128
        // instead of 125 - and sample 3 would cascade from that wrong value too.
        assert_eq!(pixels, vec![130, 131, 125, 124]);
    }
}
