//! JPEG-LS decoder (ITU-T T.87).

use crate::bitstream::BitReader;
use crate::context::ContextModel;
use crate::error::CodecError;
use crate::predictor::{clamp, predict_med};
use crate::run_mode;

// ---------------------------------------------------------------------------
// JPEG-LS markers
// ---------------------------------------------------------------------------

const MARKER_SOI: u16 = 0xFFD8;
const MARKER_EOI: u16 = 0xFFD9;
const MARKER_SOS: u16 = 0xFFDA;
const MARKER_SOF55: u16 = 0xFFF7;
const MARKER_LSE: u16 = 0xFFF8;
const MARKER_DRI: u16 = 0xFFDD;

/// Default statistics reset threshold (T.87 A.2.1 RESET).
const DEFAULT_RESET: i32 = 64;

/// Hard cap on decoded image size (pixels), guarding the row-major
/// `vec![0u16; width*height]` against untrusted SOF55 dimensions. Matches the
/// `jpegli` / `jpegrle` decoders (`1 << 28` = 268 435 456 pixels ~= 512 MiB).
const MAX_PIXELS: usize = 1 << 28;

/// Minimum permitted LSE RESET value (T.87 C.2.4.1.1). A RESET below this
/// would suppress statistics halving and risk i32 overflow on long scans.
const MIN_RESET: i32 = 3;

// ---------------------------------------------------------------------------
// Frame / scan headers + LSE presets
// ---------------------------------------------------------------------------

struct FrameHeader {
    precision: u32,
    height: usize,
    width: usize,
    /// Number of components (Nf). 1 (grayscale) or 3 (e.g. RGB); other counts
    /// are rejected in [`Decoder::read_sof`].
    components: usize,
    component_ids: Vec<u8>,
}

struct ScanHeader {
    component_ids: Vec<u8>,
    near: i32,
    ilv: u8,
}

/// LSE ID=1 coding-parameter presets (T.87 C.2.4.1.1); `None` = spec default.
#[derive(Default)]
struct Presets {
    maxval: Option<i32>,
    t1: Option<i32>,
    t2: Option<i32>,
    t3: Option<i32>,
    reset: Option<i32>,
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Frame geometry and coding metadata parsed from a JPEG-LS codestream header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameInfo {
    /// Image width in pixels.
    pub width: u32,
    /// Image height in pixels.
    pub height: u32,
    /// Sample precision in bits (`2..=16`).
    pub bits_per_sample: u8,
    /// Component count (`1` or `3`).
    pub components: u8,
    /// Interleave mode (`0` = none/planar, `1` = line, `2` = sample).
    pub interleave_mode: u8,
    /// NEAR error bound (`0` = lossless).
    pub near: u8,
}

/// Parse JPEG-LS headers (SOI, SOF55, optional LSE/DRI, first SOS) without decoding pixels.
pub fn read_frame_info(data: &[u8]) -> Result<FrameInfo, CodecError> {
    let mut dec = Decoder::new(data);
    let (frame, presets, scan) = dec.read_headers()?;
    let _ = validate_params(&frame, &presets, scan.near)?;
    Ok(FrameInfo {
        width: frame.width as u32,
        height: frame.height as u32,
        bits_per_sample: frame.precision as u8,
        components: frame.components as u8,
        interleave_mode: scan.ilv,
        near: scan.near as u8,
    })
}

/// Decode a JPEG-LS compressed bitstream into a 16-bit grayscale pixel buffer.
///
/// `width` and `height` are the *expected* image dimensions, cross-checked
/// against the SOF55 marker.
pub fn decode(data: &[u8], width: u32, height: u32) -> Result<(Vec<u16>, u32, u32), CodecError> {
    let mut dec = Decoder::new(data);
    dec.decode(width, height)
}

/// A decoded JPEG-LS image with the frame metadata a DICOM caller needs.
///
/// Supports grayscale (`components == 1`) and 3-component frames across all
/// three interleave modes (`ILV = 0` planar, `ILV = 1` line, `ILV = 2` sample).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecodedImage {
    /// Samples in component-planar order — all of component 0, then component
    /// 1, … (`RRR…GGG…BBB…`). Length `width * height * components`.
    pub samples: Vec<u16>,
    /// Image width in pixels (from the codestream).
    pub width: u32,
    /// Image height in pixels (from the codestream).
    pub height: u32,
    /// Sample precision in bits (2..=16).
    pub bits_per_sample: u8,
    /// Component count (1 or 3).
    pub components: u8,
    /// Interleave mode from the scan headers (`0`, `1`, or `2`).
    pub interleave_mode: u8,
    /// NEAR error bound (0 = lossless).
    pub near: u8,
}

/// Decode a JPEG-LS bitstream, reading the geometry from the codestream.
///
/// The output sample buffer is limited to 64 MiB; use
/// [`decode_image_with_options`] to select a different budget.
pub fn decode_image(data: &[u8]) -> Result<DecodedImage, CodecError> {
    decode_image_with_options(data, &DecodeImageOptions::default())
}

/// Limits for decoding images whose geometry comes from the stream.
#[derive(Debug, Clone, Copy)]
pub struct DecodeImageOptions {
    /// Maximum decoded sample-buffer size in bytes. Default: 64 MiB.
    pub max_decoded_bytes: usize,
}

impl Default for DecodeImageOptions {
    fn default() -> Self {
        Self {
            max_decoded_bytes: 64 * 1024 * 1024,
        }
    }
}

/// Decode with a caller-supplied output allocation limit, checked before allocation.
pub fn decode_image_with_options(
    data: &[u8],
    opts: &DecodeImageOptions,
) -> Result<DecodedImage, CodecError> {
    Decoder::new(data).decode_image(opts)
}

/// Decode a JPEG-LS codestream directly into native little-endian DICOM pixel bytes.
///
/// - Samples with `bits_per_sample <= 8` are packed 1 byte per sample (`u8`).
/// - Samples with `bits_per_sample > 8` are packed 2 bytes per sample little-endian (`u16`).
/// - Multi-component layout matches DICOM/CharLS convention:
///   - `ILV = 0` (planar): component-planar (`RRR…GGG…BBB…`)
///   - `ILV = 1` or `ILV = 2` (interleaved): sample-interleaved (`RGBRGB…`)
pub fn decode_to_bytes(data: &[u8]) -> Result<(Vec<u8>, FrameInfo), CodecError> {
    decode_to_bytes_with_options(data, &DecodeImageOptions::default())
}

/// Like [`decode_to_bytes`], with a caller-supplied maximum decoded byte budget.
///
/// Decodes row-by-row directly into the output `Vec<u8>` without allocating an
/// intermediate full-frame `Vec<u16>`.
pub fn decode_to_bytes_with_options(
    data: &[u8],
    opts: &DecodeImageOptions,
) -> Result<(Vec<u8>, FrameInfo), CodecError> {
    let mut dec = Decoder::new(data);
    dec.decode_to_bytes(opts)
}

// ---------------------------------------------------------------------------
// Decoder state
// ---------------------------------------------------------------------------

struct Decoder<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Decoder<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    // -- byte-level helpers ------------------------------------------------

    fn read_byte(&mut self) -> Result<u8, CodecError> {
        if self.pos >= self.data.len() {
            return Err(CodecError::InvalidData(
                "unexpected end of JPEG-LS data".into(),
            ));
        }
        let b = self.data[self.pos];
        self.pos += 1;
        Ok(b)
    }

    fn read_u16be(&mut self) -> Result<u16, CodecError> {
        let hi = self.read_byte()?;
        let lo = self.read_byte()?;
        Ok(u16::from(hi) << 8 | u16::from(lo))
    }

    fn skip(&mut self, n: usize) -> Result<(), CodecError> {
        if self.pos + n > self.data.len() {
            return Err(CodecError::InvalidData(
                "unexpected end of data during skip".into(),
            ));
        }
        self.pos += n;
        Ok(())
    }

    // -- marker parsing ----------------------------------------------------

    fn expect_marker(&mut self, expected: u16) -> Result<(), CodecError> {
        let b1 = self.read_byte()?;
        let b2 = self.read_byte()?;
        let marker = u16::from(b1) << 8 | u16::from(b2);
        if marker != expected {
            return Err(CodecError::InvalidData(format!(
                "expected marker 0x{expected:04X}, got 0x{marker:04X}"
            )));
        }
        Ok(())
    }

    fn read_marker(&mut self) -> Result<(u16, usize), CodecError> {
        let mut b = self.read_byte()?;
        if b != 0xFF {
            return Err(CodecError::InvalidData(format!(
                "expected 0xFF marker prefix, got 0x{b:02X}"
            )));
        }
        // A marker may be preceded by any number of 0xFF fill bytes
        // (T.81 B.1.1.2); skip them. The first non-0xFF byte is the code.
        while b == 0xFF {
            b = self.read_byte()?;
        }
        let marker = 0xFF00u16 | u16::from(b);

        let length = self.read_u16be()? as usize;
        // Length field includes its own 2 bytes.
        Ok((marker, length.saturating_sub(2)))
    }

    fn read_sof(&mut self, payload_len: usize) -> Result<FrameHeader, CodecError> {
        // P(1) + Y(2) + X(2) + Nf(1); checked before reading so a short
        // segment cannot pull its fields from the following marker.
        if payload_len < 6 {
            return Err(CodecError::InvalidData(format!(
                "SOF55 segment too short ({payload_len} payload bytes, need >= 6)"
            )));
        }
        let p = self.read_byte()?;
        if !(2..=16).contains(&p) {
            return Err(CodecError::InvalidParameter {
                name: "precision",
                value: i64::from(p),
                allowed: "2..=16",
            });
        }
        let height = self.read_u16be()? as usize;
        let width = self.read_u16be()? as usize;

        // Reject degenerate / oversized frames before any large allocation.
        // T.87 permits 1..=65535 per axis; also cap the pixel product so a
        // crafted 65535x65535 header cannot request an ~8 GiB buffer.
        if width == 0 || height == 0 {
            return Err(CodecError::InvalidData(
                "SOF55 dimensions must be non-zero".into(),
            ));
        }
        if width
            .checked_mul(height)
            .filter(|&n| n <= MAX_PIXELS)
            .is_none()
        {
            return Err(CodecError::InvalidParameter {
                name: "width*height",
                value: (width as i64).saturating_mul(height as i64),
                allowed: "product <= 268435456 pixels (1<<28)",
            });
        }

        let nf = self.read_byte()?;
        // Grayscale (1) and 3-component (e.g. RGB) frames are supported. The
        // single-scan `decode()` entry remains grayscale-only (it re-checks
        // below); `decode_image()` handles 3-component planar (ILV 0) streams.
        if nf != 1 && nf != 3 {
            return Err(CodecError::Unsupported(format!(
                "JPEG-LS component count {nf} not supported (only 1 or 3)"
            )));
        }

        // T.87 C.2.2: Lf = 8 + 3*Nf, i.e. the payload is exactly 6 + 3*Nf.
        let expected = 6 + 3 * usize::from(nf);
        if payload_len != expected {
            return Err(CodecError::InvalidData(format!(
                "SOF55 payload is {payload_len} bytes, expected {expected} for Nf = {nf}"
            )));
        }
        let mut component_ids = Vec::new();
        // Component specs: [Ci, sampling (Hi<<4 | Vi), Tqi] per component.
        // Require 1:1 sampling (no subsampling) and no per-component mapping
        // table (Tq 0); otherwise the scan would be mis-decoded. CharLS
        // enforces the same. Component IDs must be unique.
        for _ in 0..nf {
            let ci = self.read_byte()?;
            if component_ids.contains(&ci) {
                return Err(CodecError::InvalidData(
                    "duplicate SOF55 component ID".into(),
                ));
            }
            component_ids.push(ci);
            let sampling = self.read_byte()?;
            let tq = self.read_byte()?;
            if sampling != 0x11 {
                return Err(CodecError::Unsupported(format!(
                    "JPEG-LS component sampling {sampling:#04x} not supported (must be 1:1 = 0x11)"
                )));
            }
            if tq != 0 {
                return Err(CodecError::Unsupported(format!(
                    "JPEG-LS component mapping table {tq} not supported (must be 0)"
                )));
            }
        }
        Ok(FrameHeader {
            precision: u32::from(p),
            height,
            width,
            components: nf as usize,
            component_ids,
        })
    }

    /// Parse an LSE segment. Only ID=1 (coding parameters) is supported.
    fn read_lse(&mut self, payload_len: usize, presets: &mut Presets) -> Result<(), CodecError> {
        if payload_len < 1 {
            return Err(CodecError::InvalidData("empty LSE segment".into()));
        }
        let id = self.read_byte()?;
        match id {
            1 => {
                if payload_len < 11 {
                    return Err(CodecError::InvalidData("LSE ID=1 segment too short".into()));
                }
                let maxval = self.read_u16be()? as i32;
                let t1 = self.read_u16be()? as i32;
                let t2 = self.read_u16be()? as i32;
                let t3 = self.read_u16be()? as i32;
                let reset = self.read_u16be()? as i32;
                // A field of 0 means "use the default" for MAXVAL/T1/T2/T3.
                presets.maxval = (maxval > 0).then_some(maxval);
                presets.t1 = (t1 > 0).then_some(t1);
                presets.t2 = (t2 > 0).then_some(t2);
                presets.t3 = (t3 > 0).then_some(t3);
                // RESET must be an explicit, valid halving threshold
                // (T.87 C.2.4.1.1: RESET >= 3). A crafted RESET of 0/1/2 could
                // suppress statistics halving and overflow the A/B accumulators
                // on a long scan, so reject it outright rather than treating 0
                // as a "use default" sentinel.
                if reset < MIN_RESET {
                    return Err(CodecError::InvalidData(format!(
                        "LSE RESET={reset} invalid (must be >= {MIN_RESET})"
                    )));
                }
                presets.reset = Some(reset);
                // Skip any trailing bytes in an over-long segment.
                self.skip(payload_len - 11)?;
                Ok(())
            }
            2..=4 => Err(CodecError::Unsupported(format!(
                "LSE ID={id} (mapping tables) not supported"
            ))),
            other => Err(CodecError::InvalidData(format!("unknown LSE ID={other}"))),
        }
    }

    /// `nf` is the frame's component count when SOF55 has already been read
    /// (`None` if SOS precedes it; that stream fails later as missing SOF55).
    fn read_sos(
        &mut self,
        payload_len: usize,
        nf: Option<usize>,
    ) -> Result<ScanHeader, CodecError> {
        if payload_len < 6 {
            return Err(CodecError::InvalidData("SOS segment too short".into()));
        }
        let ns = self.read_byte()?;
        // T.87 C.2.3: 1 <= Ns <= 4, and a scan cannot cover more components
        // than the frame has. Such a header is malformed, not merely a scan
        // shape this decoder lacks, so it is InvalidData rather than
        // Unsupported.
        if ns == 0 || ns > 4 || nf.is_some_and(|nf| usize::from(ns) > nf) {
            let frame = nf.map_or(String::new(), |nf| format!(" for a frame with Nf = {nf}"));
            return Err(CodecError::InvalidData(format!(
                "SOS component count Ns = {ns} is invalid{frame}"
            )));
        }
        if ns != 1 && ns != 3 {
            return Err(CodecError::Unsupported(format!(
                "SOS component count Ns = {ns} not supported"
            )));
        }
        let expected_len = 4 + 2 * usize::from(ns);
        if payload_len != expected_len {
            return Err(CodecError::InvalidData(
                "SOS length does not match component count".into(),
            ));
        }
        let mut component_ids = Vec::with_capacity(usize::from(ns));
        for _ in 0..ns {
            let component_id = self.read_byte()?;
            let mapping = self.read_byte()?;
            if mapping != 0 {
                return Err(CodecError::Unsupported(format!(
                    "SOS mapping-table selector {mapping} not supported (must be 0)"
                )));
            }
            component_ids.push(component_id);
        }

        let near = self.read_byte()?;
        let ilv = self.read_byte()?;
        let al_ah = self.read_byte()?;

        if ns == 1 && ilv != 0 {
            return Err(CodecError::InvalidData(
                "single-component scan must use ILV = 0".into(),
            ));
        }
        if ns > 1 && ilv != 1 && ilv != 2 {
            return Err(CodecError::Unsupported(format!(
                "multi-component scan with ILV = {ilv} not supported"
            )));
        }
        if al_ah != 0 {
            return Err(CodecError::InvalidData(format!(
                "SOS point-transform byte {al_ah:#04x} must be 0"
            )));
        }

        Ok(ScanHeader {
            component_ids,
            near: i32::from(near),
            ilv,
        })
    }

    fn expect_eoi(&mut self) -> Result<(), CodecError> {
        let b1 = self.read_byte()?;
        let mut b2 = self.read_byte()?;
        // As in `read_marker`, 0xFF fill bytes may precede the EOI (T.81
        // B.1.1.2). DCMTK pads an odd-length stream to even this way by
        // default (`FF FF D9`); skip them. The first non-0xFF byte is the code.
        if b1 == 0xFF {
            while b2 == 0xFF {
                b2 = self.read_byte()?;
            }
        }
        let marker = u16::from(b1) << 8 | u16::from(b2);
        if marker != MARKER_EOI {
            return Err(CodecError::InvalidData(format!(
                "expected EOI (0xFFD9) after scan, got 0x{marker:04X}"
            )));
        }
        Ok(())
    }

    fn read_headers(&mut self) -> Result<(FrameHeader, Presets, ScanHeader), CodecError> {
        self.expect_marker(MARKER_SOI)?;

        let mut frame: Option<FrameHeader> = None;
        let mut presets = Presets::default();
        let scan: ScanHeader;

        loop {
            let (marker, length) = self.read_marker()?;
            match marker {
                MARKER_SOF55 => frame = Some(self.read_sof(length)?),
                MARKER_LSE => self.read_lse(length, &mut presets)?,
                MARKER_DRI => {
                    // Tolerate and skip DRI, as CharLS does: a zero restart
                    // interval is a no-op. A non-zero interval that actually
                    // emits RSTn markers in the scan is rejected by the entropy
                    // decoder (scan_error), so restarts are still unsupported —
                    // but a benign DRI segment no longer fails an otherwise
                    // decodable stream.
                    self.skip(length)?;
                }
                MARKER_SOS => {
                    scan = self.read_sos(length, frame.as_ref().map(|f| f.components))?;
                    break;
                }
                MARKER_EOI => {
                    return Err(CodecError::InvalidData("unexpected EOI before SOS".into()));
                }
                _ => self.skip(length)?,
            }
        }

        let frame = frame.ok_or_else(|| CodecError::InvalidData("missing SOF55 marker".into()))?;

        Ok((frame, presets, scan))
    }

    // -- main decode flow --------------------------------------------------

    fn decode(
        &mut self,
        exp_width: u32,
        exp_height: u32,
    ) -> Result<(Vec<u16>, u32, u32), CodecError> {
        let (frame, presets, scan) = self.read_headers()?;

        if frame.width != exp_width as usize || frame.height != exp_height as usize {
            return Err(CodecError::DimensionMismatch {
                expected: (exp_width as usize) * (exp_height as usize),
                actual: frame.width * frame.height,
            });
        }

        if frame.components != 1 {
            return Err(CodecError::Unsupported(format!(
                "decode() is grayscale-only; {}-component frames need decode_image()",
                frame.components
            )));
        }

        if scan.component_ids[0] != frame.component_ids[0] {
            return Err(CodecError::InvalidData(
                "unknown SOS component selector".into(),
            ));
        }
        let (max_val, reset) = validate_params(&frame, &presets, scan.near)?;

        let w = exp_width as usize;
        let h = exp_height as usize;
        let mut pixels = vec![0u16; w * h];

        let ctx = ContextModel::with_presets(
            max_val, scan.near, reset, presets.t1, presets.t2, presets.t3,
        );
        let scan_start = self.pos;
        let scan_data = &self.data[scan_start..];
        let consumed = decode_scan_t87(scan_data, &mut pixels, w, h, ctx, scan_start)?;
        self.pos = scan_start + consumed;
        self.expect_eoi()?;

        Ok((pixels, exp_width, exp_height))
    }

    /// Decode a full image directly into a single native little-endian `Vec<u8>`
    /// (1 byte/sample for `<= 8`-bit, 2 bytes/sample LE for `> 8`-bit; planar for
    /// `ILV = 0`, sample-interleaved for `ILV = 1` / `ILV = 2`).
    fn decode_to_bytes(
        &mut self,
        opts: &DecodeImageOptions,
    ) -> Result<(Vec<u8>, FrameInfo), CodecError> {
        let (frame, presets, first_scan) = self.read_headers()?;
        let near = first_scan.near;
        let (max_val, reset) = validate_params(&frame, &presets, near)?;

        let w = frame.width;
        let h = frame.height;
        let components = frame.components;
        let bps = if frame.precision <= 8 { 1usize } else { 2usize };
        let plane = w * h;
        let count = plane
            .checked_mul(components)
            .ok_or_else(|| CodecError::InvalidData("sample count overflow".into()))?;
        let bytes = count
            .checked_mul(bps)
            .ok_or_else(|| CodecError::InvalidData("decoded size overflow".into()))?;
        if bytes > opts.max_decoded_bytes {
            return Err(CodecError::InvalidData(format!(
                "decoded image needs {bytes} bytes, limit is {}",
                opts.max_decoded_bytes
            )));
        }

        let mut out = Vec::new();
        out.try_reserve_exact(bytes)
            .map_err(|_| CodecError::InvalidData("decoded image allocation failed".into()))?;
        out.resize(bytes, 0u8);

        let mut seen = [false; 3];
        let ilv = first_scan.ilv;

        if ilv == 0 {
            let plane_bytes = plane * bps;
            let mut scan = first_scan;
            for c in 0..components {
                if c > 0 {
                    let (marker, length) = self.read_marker()?;
                    if marker != MARKER_SOS {
                        return Err(CodecError::InvalidData(format!(
                            "expected SOS for component {c}, got marker 0x{marker:04X}"
                        )));
                    }
                    scan = self.read_sos(length, Some(components))?;
                    if scan.ilv != 0 {
                        return Err(CodecError::InvalidData(
                            "mixed ILV modes across scans".into(),
                        ));
                    }
                    if scan.near != near {
                        return Err(CodecError::InvalidData(format!(
                            "per-scan NEAR differs across components ({near} then {})",
                            scan.near
                        )));
                    }
                }
                let index = frame
                    .component_ids
                    .iter()
                    .position(|&id| id == scan.component_ids[0])
                    .ok_or_else(|| {
                        CodecError::InvalidData("unknown SOS component selector".into())
                    })?;
                if seen[index] {
                    return Err(CodecError::InvalidData(
                        "duplicate SOS component selector".into(),
                    ));
                }
                seen[index] = true;
                let plane_slot = &mut out[index * plane_bytes..(index + 1) * plane_bytes];
                let ctx = ContextModel::with_presets(
                    max_val, near, reset, presets.t1, presets.t2, presets.t3,
                );
                let scan_start = self.pos;
                let consumed = if bps == 1 {
                    decode_scan_t87_with(
                        &self.data[scan_start..],
                        w,
                        h,
                        ctx,
                        scan_start,
                        |y, row_cur| {
                            let dst = &mut plane_slot[y * w..(y + 1) * w];
                            for (d, &s) in dst.iter_mut().zip(row_cur.iter()) {
                                *d = s as u8;
                            }
                        },
                    )?
                } else {
                    decode_scan_t87_with(
                        &self.data[scan_start..],
                        w,
                        h,
                        ctx,
                        scan_start,
                        |y, row_cur| {
                            let dst = &mut plane_slot[y * w * 2..(y + 1) * w * 2];
                            for (d, &s) in dst.as_chunks_mut::<2>().0.iter_mut().zip(row_cur.iter())
                            {
                                *d = s.to_le_bytes();
                            }
                        },
                    )?
                };
                self.pos = scan_start + consumed;
            }
        } else {
            if first_scan.component_ids.len() != components || components != 3 {
                return Err(CodecError::InvalidData(
                    "interleaved scan must cover all 3 components".into(),
                ));
            }
            let mut comp_map = [0usize; 3];
            for (slot, &cid) in first_scan.component_ids.iter().enumerate() {
                let index = frame
                    .component_ids
                    .iter()
                    .position(|&id| id == cid)
                    .ok_or_else(|| {
                        CodecError::InvalidData("unknown SOS component selector".into())
                    })?;
                if seen[index] {
                    return Err(CodecError::InvalidData(
                        "duplicate SOS component selector".into(),
                    ));
                }
                seen[index] = true;
                comp_map[slot] = index;
            }
            let ctx = ContextModel::with_presets(
                max_val, near, reset, presets.t1, presets.t2, presets.t3,
            );
            let scan_start = self.pos;
            let consumed = if ilv == 1 {
                if bps == 1 {
                    decode_scan_ilv1_with(
                        &self.data[scan_start..],
                        w,
                        h,
                        ctx,
                        scan_start,
                        |y, c0, c1, c2| {
                            let row = &mut out[y * w * 3..(y + 1) * w * 3];
                            let dst = row.as_chunks_mut::<3>().0;
                            if comp_map == [0, 1, 2] {
                                for x in 0..w {
                                    dst[x] = [c0[x] as u8, c1[x] as u8, c2[x] as u8];
                                }
                            } else {
                                let (m0, m1, m2) = (comp_map[0], comp_map[1], comp_map[2]);
                                for x in 0..w {
                                    let mut px = [0u8; 3];
                                    px[m0] = c0[x] as u8;
                                    px[m1] = c1[x] as u8;
                                    px[m2] = c2[x] as u8;
                                    dst[x] = px;
                                }
                            }
                        },
                    )?
                } else {
                    decode_scan_ilv1_with(
                        &self.data[scan_start..],
                        w,
                        h,
                        ctx,
                        scan_start,
                        |y, c0, c1, c2| {
                            let row = &mut out[y * w * 6..(y + 1) * w * 6];
                            let dst = row.as_chunks_mut::<2>().0;
                            if comp_map == [0, 1, 2] {
                                for x in 0..w {
                                    dst[x * 3] = c0[x].to_le_bytes();
                                    dst[x * 3 + 1] = c1[x].to_le_bytes();
                                    dst[x * 3 + 2] = c2[x].to_le_bytes();
                                }
                            } else {
                                let (m0, m1, m2) = (comp_map[0], comp_map[1], comp_map[2]);
                                for x in 0..w {
                                    dst[x * 3 + m0] = c0[x].to_le_bytes();
                                    dst[x * 3 + m1] = c1[x].to_le_bytes();
                                    dst[x * 3 + m2] = c2[x].to_le_bytes();
                                }
                            }
                        },
                    )?
                }
            } else if bps == 1 {
                decode_scan_ilv2_with(
                    &self.data[scan_start..],
                    w,
                    h,
                    ctx,
                    scan_start,
                    |y, row_cur| {
                        let row = &mut out[y * w * 3..(y + 1) * w * 3];
                        let dst = row.as_chunks_mut::<3>().0;
                        if comp_map == [0, 1, 2] {
                            for (d, s) in dst.iter_mut().zip(row_cur.iter()) {
                                *d = [s[0] as u8, s[1] as u8, s[2] as u8];
                            }
                        } else {
                            let (m0, m1, m2) = (comp_map[0], comp_map[1], comp_map[2]);
                            for (d, s) in dst.iter_mut().zip(row_cur.iter()) {
                                let mut px = [0u8; 3];
                                px[m0] = s[0] as u8;
                                px[m1] = s[1] as u8;
                                px[m2] = s[2] as u8;
                                *d = px;
                            }
                        }
                    },
                )?
            } else {
                decode_scan_ilv2_with(
                    &self.data[scan_start..],
                    w,
                    h,
                    ctx,
                    scan_start,
                    |y, row_cur| {
                        let row = &mut out[y * w * 6..(y + 1) * w * 6];
                        let dst = row.as_chunks_mut::<2>().0;
                        if comp_map == [0, 1, 2] {
                            for (x, s) in row_cur.iter().enumerate() {
                                dst[x * 3] = s[0].to_le_bytes();
                                dst[x * 3 + 1] = s[1].to_le_bytes();
                                dst[x * 3 + 2] = s[2].to_le_bytes();
                            }
                        } else {
                            let (m0, m1, m2) = (comp_map[0], comp_map[1], comp_map[2]);
                            for (x, s) in row_cur.iter().enumerate() {
                                dst[x * 3 + m0] = s[0].to_le_bytes();
                                dst[x * 3 + m1] = s[1].to_le_bytes();
                                dst[x * 3 + m2] = s[2].to_le_bytes();
                            }
                        }
                    },
                )?
            };
            self.pos = scan_start + consumed;
        }
        self.expect_eoi()?;

        Ok((
            out,
            FrameInfo {
                width: w as u32,
                height: h as u32,
                bits_per_sample: frame.precision as u8,
                components: components as u8,
                interleave_mode: ilv,
                near: near as u8,
            },
        ))
    }

    /// Decode a full image (grayscale or 3-component planar), reading geometry
    /// from the codestream. T.87 path only.
    fn decode_image(&mut self, opts: &DecodeImageOptions) -> Result<DecodedImage, CodecError> {
        let (frame, presets, first_scan) = self.read_headers()?;
        let near = first_scan.near;
        let (max_val, reset) = validate_params(&frame, &presets, near)?;

        let w = frame.width;
        let h = frame.height;
        let components = frame.components;
        // w*h*components cannot overflow: read_sof caps w*h at MAX_PIXELS
        // (1<<28) and components <= 3.
        let plane = w * h;
        let count = plane
            .checked_mul(components)
            .ok_or_else(|| CodecError::InvalidData("sample count overflow".into()))?;
        let bytes = count
            .checked_mul(std::mem::size_of::<u16>())
            .ok_or_else(|| CodecError::InvalidData("decoded size overflow".into()))?;
        if bytes > opts.max_decoded_bytes {
            return Err(CodecError::InvalidData(format!(
                "decoded image needs {bytes} bytes, limit is {}",
                opts.max_decoded_bytes
            )));
        }
        let mut samples = Vec::new();
        samples
            .try_reserve_exact(count)
            .map_err(|_| CodecError::InvalidData("decoded image allocation failed".into()))?;
        samples.resize(count, 0u16);
        let mut seen = [false; 3];
        let ilv = first_scan.ilv;

        if ilv == 0 {
            let mut scan = first_scan;
            for c in 0..components {
                if c > 0 {
                    let (marker, length) = self.read_marker()?;
                    if marker != MARKER_SOS {
                        return Err(CodecError::InvalidData(format!(
                            "expected SOS for component {c}, got marker 0x{marker:04X}"
                        )));
                    }
                    scan = self.read_sos(length, Some(components))?;
                    if scan.ilv != 0 {
                        return Err(CodecError::InvalidData(
                            "mixed ILV modes across scans".into(),
                        ));
                    }
                    if scan.near != near {
                        return Err(CodecError::InvalidData(format!(
                            "per-scan NEAR differs across components ({near} then {})",
                            scan.near
                        )));
                    }
                }
                let index = frame
                    .component_ids
                    .iter()
                    .position(|&id| id == scan.component_ids[0])
                    .ok_or_else(|| {
                        CodecError::InvalidData("unknown SOS component selector".into())
                    })?;
                if seen[index] {
                    return Err(CodecError::InvalidData(
                        "duplicate SOS component selector".into(),
                    ));
                }
                seen[index] = true;
                let plane_slot = &mut samples[index * plane..(index + 1) * plane];
                let ctx = ContextModel::with_presets(
                    max_val, near, reset, presets.t1, presets.t2, presets.t3,
                );
                let scan_start = self.pos;
                let consumed =
                    decode_scan_t87(&self.data[scan_start..], plane_slot, w, h, ctx, scan_start)?;
                self.pos = scan_start + consumed;
            }
        } else {
            if first_scan.component_ids.len() != components || components != 3 {
                return Err(CodecError::InvalidData(
                    "interleaved scan must cover all 3 components".into(),
                ));
            }
            let mut comp_map = [0usize; 3];
            for (slot, &cid) in first_scan.component_ids.iter().enumerate() {
                let index = frame
                    .component_ids
                    .iter()
                    .position(|&id| id == cid)
                    .ok_or_else(|| {
                        CodecError::InvalidData("unknown SOS component selector".into())
                    })?;
                if seen[index] {
                    return Err(CodecError::InvalidData(
                        "duplicate SOS component selector".into(),
                    ));
                }
                seen[index] = true;
                comp_map[slot] = index;
            }
            let ctx = ContextModel::with_presets(
                max_val, near, reset, presets.t1, presets.t2, presets.t3,
            );
            let scan_start = self.pos;
            let consumed = if ilv == 1 {
                decode_scan_ilv1(
                    &self.data[scan_start..],
                    &mut samples,
                    comp_map,
                    w,
                    h,
                    ctx,
                    scan_start,
                )?
            } else {
                decode_scan_ilv2(
                    &self.data[scan_start..],
                    &mut samples,
                    comp_map,
                    w,
                    h,
                    ctx,
                    scan_start,
                )?
            };
            self.pos = scan_start + consumed;
        }
        self.expect_eoi()?;

        Ok(DecodedImage {
            samples,
            width: w as u32,
            height: h as u32,
            bits_per_sample: frame.precision as u8,
            components: components as u8,
            interleave_mode: ilv,
            near: near as u8,
        })
    }
}

/// Validate MAXVAL / NEAR / threshold ordering (T.87 C.2.4.1.1) for a scan and
/// return the effective `(max_val, reset)`. Shared by [`Decoder::decode`] and
/// [`Decoder::decode_image`]. `near` is the scan's NEAR; `presets` carries any
/// LSE ID=1 overrides (a `None` field means "use the spec default").
fn validate_params(
    frame: &FrameHeader,
    presets: &Presets,
    near: i32,
) -> Result<(i32, i32), CodecError> {
    let sof_max_val = (1i32 << frame.precision) - 1;
    let max_val = presets.maxval.unwrap_or(sof_max_val);

    // Effective MAXVAL must be positive and fit the SOF precision range; a
    // preset MAXVAL above (1<<precision)-1 would desync context arithmetic.
    if max_val <= 0 || max_val > sof_max_val {
        return Err(CodecError::InvalidData(format!(
            "LSE MAXVAL={max_val} outside 1..={sof_max_val} for precision {}",
            frame.precision
        )));
    }

    // NEAR validity depends on the effective MAXVAL.
    let near_max = (max_val / 2).min(255);
    if near < 0 || near > near_max {
        return Err(CodecError::InvalidParameter {
            name: "near",
            value: i64::from(near),
            allowed: "0..=min(255, MAXVAL/2)",
        });
    }

    // Thresholds must satisfy NEAR < T1 <= T2 <= T3 <= MAXVAL. Preset fields
    // left at their default fall back to the spec defaults (already valid).
    let (dt1, dt2, dt3) = ContextModel::default_thresholds(max_val, near);
    let t1 = presets.t1.unwrap_or(dt1);
    let t2 = presets.t2.unwrap_or(dt2);
    let t3 = presets.t3.unwrap_or(dt3);
    if !(near < t1 && t1 <= t2 && t2 <= t3 && t3 <= max_val) {
        return Err(CodecError::InvalidData(format!(
            "LSE thresholds violate NEAR<T1<=T2<=T3<=MAXVAL \
             (NEAR={near}, T1={t1}, T2={t2}, T3={t3}, MAXVAL={max_val})"
        )));
    }

    let reset = presets.reset.unwrap_or(DEFAULT_RESET);
    Ok((max_val, reset))
}

// ---------------------------------------------------------------------------
// T.87 conformant scan decode (regular + run mode)
// ---------------------------------------------------------------------------

/// Classify a bit-reader error hit mid-scan (before all samples are decoded).
#[cold]
#[inline(never)]
fn scan_error(e: CodecError, offset: usize) -> CodecError {
    match e {
        CodecError::Marker(m) if (0xFFD0..=0xFFD7).contains(&m) => {
            CodecError::Unsupported("JPEG-LS restart markers (RSTn) in scan not supported".into())
        }
        CodecError::Marker(_) | CodecError::InvalidData(_) => CodecError::Truncated {
            offset,
            context: "entropy-coded scan",
        },
        other => other,
    }
}

/// Decode one regular sample given pre-computed signed context id `qs` and neighbors `(ra, rb, rc)`.
#[inline(always)]
#[allow(clippy::too_many_arguments)]
fn decode_regular_qs<const LOSSLESS: bool, const POW2: bool>(
    br: &mut BitReader<'_>,
    ctx: &mut ContextModel,
    qs: i32,
    ra: i32,
    rb: i32,
    rc: i32,
    base_offset: usize,
) -> Result<i32, CodecError> {
    let near = if LOSSLESS { 0 } else { ctx.near };
    let max_val = ctx.max_val;
    let range = ctx.range;
    let limit = ctx.limit;
    let qbpp = ctx.qbpp;
    let reset = ctx.reset;
    let (q, sign) = ContextModel::split_qs(qs);
    let e = &mut ctx.ctx[q];
    let k = ContextModel::compute_k_from(e.n, e.a);
    let mut px = predict_med(ra, rb, rc) + sign * e.c;
    px = clamp(px, 0, max_val);

    let (mut err_q, from_lut) = match br.read_regular_error(k, limit, qbpp) {
        Ok(v) => v,
        Err(err) => return Err(scan_error(err, base_offset + br.pos())),
    };
    if LOSSLESS && k == 0 && 2 * e.b <= -e.n {
        err_q ^= -1;
    }
    if !from_lut || range < 128 {
        if let Err(err) = run_mode::check_errval(err_q, range) {
            return Err(scan_error(err, base_offset + br.pos()));
        }
    }
    if LOSSLESS {
        e.update_lossless(err_q, reset);
        let mut v = px + sign * err_q;
        if POW2 {
            v &= max_val;
        } else if v < 0 {
            v += range;
        } else if v > max_val {
            v -= range;
        }
        Ok(v)
    } else {
        e.update_lossy(err_q, 2 * near + 1, reset);
        Ok(run_mode::fix_reconstructed(
            px + sign * err_q * (2 * near + 1),
            near,
            range,
            max_val,
        ))
    }
}

#[inline(always)]
fn decode_line_t87<const LOSSLESS: bool, const POW2: bool>(
    br: &mut BitReader<'_>,
    ctx: &mut ContextModel,
    cur: &mut [u16],
    prev: &mut [u16],
    width: usize,
    base_offset: usize,
) -> Result<(), CodecError> {
    let cur = &mut cur[..width + 2];
    let prev = &mut prev[..width + 2];
    cur[0] = prev[1];
    prev[width + 1] = prev[width];

    let mut x = 0usize;
    let mut ra = i32::from(cur[0]);
    let mut rc = i32::from(prev[0]);
    let mut rb = i32::from(prev[1]);
    while x < width {
        let rd = i32::from(prev[x + 2]);
        let qs = ctx.compute_qs(rd - rb, rb - rc, rc - ra);

        if qs == 0 {
            x = match run_mode::decode_run(br, ctx, cur, prev, x, width) {
                Ok(v) => v,
                Err(e) => return Err(scan_error(e, base_offset + br.pos())),
            };
            if x < width {
                ra = i32::from(cur[x]);
                rc = i32::from(prev[x]);
                rb = i32::from(prev[x + 1]);
            }
            continue;
        }

        let recon = decode_regular_qs::<LOSSLESS, POW2>(br, ctx, qs, ra, rb, rc, base_offset)?;
        cur[x + 1] = recon as u16;
        ra = recon;
        rc = rb;
        rb = rd;
        x += 1;
    }
    Ok(())
}

/// Decode a single-component scan, delivering each decoded row `&cur[1..=width]` to `emit_row`.
fn decode_scan_t87_with<F: FnMut(usize, &[u16])>(
    scan_data: &[u8],
    width: usize,
    height: usize,
    mut ctx: ContextModel,
    base_offset: usize,
    mut emit_row: F,
) -> Result<usize, CodecError> {
    let mut br = BitReader::new(scan_data);
    let mut prev = vec![0u16; width + 2];
    let mut cur = vec![0u16; width + 2];

    if ctx.is_pow2_range {
        for y in 0..height {
            decode_line_t87::<true, true>(
                &mut br,
                &mut ctx,
                &mut cur,
                &mut prev,
                width,
                base_offset,
            )?;
            emit_row(y, &cur[1..width + 1]);
            std::mem::swap(&mut prev, &mut cur);
        }
    } else if ctx.near == 0 {
        for y in 0..height {
            decode_line_t87::<true, false>(
                &mut br,
                &mut ctx,
                &mut cur,
                &mut prev,
                width,
                base_offset,
            )?;
            emit_row(y, &cur[1..width + 1]);
            std::mem::swap(&mut prev, &mut cur);
        }
    } else {
        for y in 0..height {
            decode_line_t87::<false, false>(
                &mut br,
                &mut ctx,
                &mut cur,
                &mut prev,
                width,
                base_offset,
            )?;
            emit_row(y, &cur[1..width + 1]);
            std::mem::swap(&mut prev, &mut cur);
        }
    }

    Ok(br.marker_pos())
}

/// Decode the entropy-coded scan (T.87 A.4-A.7); returns bytes consumed.
pub(crate) fn decode_scan_t87(
    scan_data: &[u8],
    pixels: &mut [u16],
    width: usize,
    height: usize,
    ctx: ContextModel,
    base_offset: usize,
) -> Result<usize, CodecError> {
    decode_scan_t87_with(scan_data, width, height, ctx, base_offset, |y, row_cur| {
        pixels[y * width..(y + 1) * width].copy_from_slice(row_cur);
    })
}

/// Decode a 3-component line-interleaved (`ILV = 1`) scan, delivering each decoded row's
/// 3 component slices to `emit_row`.
fn decode_scan_ilv1_with<F: FnMut(usize, &[u16], &[u16], &[u16])>(
    scan_data: &[u8],
    width: usize,
    height: usize,
    mut ctx: ContextModel,
    base_offset: usize,
    mut emit_row: F,
) -> Result<usize, CodecError> {
    let mut br = BitReader::new(scan_data);
    let mut prev = [
        vec![0u16; width + 2],
        vec![0u16; width + 2],
        vec![0u16; width + 2],
    ];
    let mut cur = [
        vec![0u16; width + 2],
        vec![0u16; width + 2],
        vec![0u16; width + 2],
    ];

    let mut run_indices = [0usize; 3];
    if ctx.is_pow2_range {
        for y in 0..height {
            for c in 0..3 {
                ctx.run_index = run_indices[c];
                decode_line_t87::<true, true>(
                    &mut br,
                    &mut ctx,
                    &mut cur[c],
                    &mut prev[c],
                    width,
                    base_offset,
                )?;
                run_indices[c] = ctx.run_index;
            }
            emit_row(
                y,
                &cur[0][1..width + 1],
                &cur[1][1..width + 1],
                &cur[2][1..width + 1],
            );
            for c in 0..3 {
                std::mem::swap(&mut prev[c], &mut cur[c]);
            }
        }
    } else if ctx.near == 0 {
        for y in 0..height {
            for c in 0..3 {
                ctx.run_index = run_indices[c];
                decode_line_t87::<true, false>(
                    &mut br,
                    &mut ctx,
                    &mut cur[c],
                    &mut prev[c],
                    width,
                    base_offset,
                )?;
                run_indices[c] = ctx.run_index;
            }
            emit_row(
                y,
                &cur[0][1..width + 1],
                &cur[1][1..width + 1],
                &cur[2][1..width + 1],
            );
            for c in 0..3 {
                std::mem::swap(&mut prev[c], &mut cur[c]);
            }
        }
    } else {
        for y in 0..height {
            for c in 0..3 {
                ctx.run_index = run_indices[c];
                decode_line_t87::<false, false>(
                    &mut br,
                    &mut ctx,
                    &mut cur[c],
                    &mut prev[c],
                    width,
                    base_offset,
                )?;
                run_indices[c] = ctx.run_index;
            }
            emit_row(
                y,
                &cur[0][1..width + 1],
                &cur[1][1..width + 1],
                &cur[2][1..width + 1],
            );
            for c in 0..3 {
                std::mem::swap(&mut prev[c], &mut cur[c]);
            }
        }
    }
    Ok(br.marker_pos())
}

/// Decode a 3-component line-interleaved (`ILV = 1`) scan into component-planar `samples`.
fn decode_scan_ilv1(
    scan_data: &[u8],
    samples: &mut [u16],
    comp_map: [usize; 3],
    width: usize,
    height: usize,
    ctx: ContextModel,
    base_offset: usize,
) -> Result<usize, CodecError> {
    let plane = width * height;
    decode_scan_ilv1_with(
        scan_data,
        width,
        height,
        ctx,
        base_offset,
        |y, c0, c1, c2| {
            let row_base = y * width;
            samples[comp_map[0] * plane + row_base..][..width].copy_from_slice(c0);
            samples[comp_map[1] * plane + row_base..][..width].copy_from_slice(c1);
            samples[comp_map[2] * plane + row_base..][..width].copy_from_slice(c2);
        },
    )
}

/// Decode a 3-component sample-interleaved (`ILV = 2`) scan, delivering each decoded row's
/// triplet slice `&cur[1..=width]` to `emit_row`.
fn decode_scan_ilv2_with<F: FnMut(usize, &[[u16; 3]])>(
    scan_data: &[u8],
    width: usize,
    height: usize,
    ctx: ContextModel,
    base_offset: usize,
    emit_row: F,
) -> Result<usize, CodecError> {
    if ctx.is_pow2_range {
        decode_scan_ilv2_inner::<true, true, _>(
            scan_data,
            width,
            height,
            ctx,
            base_offset,
            emit_row,
        )
    } else if ctx.near == 0 {
        decode_scan_ilv2_inner::<true, false, _>(
            scan_data,
            width,
            height,
            ctx,
            base_offset,
            emit_row,
        )
    } else {
        decode_scan_ilv2_inner::<false, false, _>(
            scan_data,
            width,
            height,
            ctx,
            base_offset,
            emit_row,
        )
    }
}

/// Decode a 3-component sample-interleaved (`ILV = 2`) scan into component-planar `samples`.
fn decode_scan_ilv2(
    scan_data: &[u8],
    samples: &mut [u16],
    comp_map: [usize; 3],
    width: usize,
    height: usize,
    ctx: ContextModel,
    base_offset: usize,
) -> Result<usize, CodecError> {
    let plane = width * height;
    decode_scan_ilv2_with(scan_data, width, height, ctx, base_offset, |y, row_cur| {
        let row_base = y * width;
        let p0 = comp_map[0] * plane + row_base;
        let p1 = comp_map[1] * plane + row_base;
        let p2 = comp_map[2] * plane + row_base;
        for (x, s) in row_cur.iter().enumerate() {
            samples[p0 + x] = s[0];
        }
        for (x, s) in row_cur.iter().enumerate() {
            samples[p1 + x] = s[1];
        }
        for (x, s) in row_cur.iter().enumerate() {
            samples[p2 + x] = s[2];
        }
    })
}

fn decode_scan_ilv2_inner<const LOSSLESS: bool, const POW2: bool, F: FnMut(usize, &[[u16; 3]])>(
    scan_data: &[u8],
    width: usize,
    height: usize,
    mut ctx: ContextModel,
    base_offset: usize,
    mut emit_row: F,
) -> Result<usize, CodecError> {
    let mut br = BitReader::new(scan_data);
    let mut prev_vec = vec![[0u16; 3]; width + 2];
    let mut cur_vec = vec![[0u16; 3]; width + 2];

    for y in 0..height {
        let prev = &mut prev_vec[..width + 2];
        let cur = &mut cur_vec[..width + 2];
        cur[0] = prev[1];
        prev[width + 1] = prev[width];
        let mut x = 0usize;
        let mut ra = [
            i32::from(cur[0][0]),
            i32::from(cur[0][1]),
            i32::from(cur[0][2]),
        ];
        let mut rc = [
            i32::from(prev[0][0]),
            i32::from(prev[0][1]),
            i32::from(prev[0][2]),
        ];
        let mut rb = [
            i32::from(prev[1][0]),
            i32::from(prev[1][1]),
            i32::from(prev[1][2]),
        ];
        while x < width {
            let rd_u16 = prev[x + 2];
            let rd = [
                i32::from(rd_u16[0]),
                i32::from(rd_u16[1]),
                i32::from(rd_u16[2]),
            ];
            let qs0 = ctx.compute_qs(rd[0] - rb[0], rb[0] - rc[0], rc[0] - ra[0]);
            let qs1 = ctx.compute_qs(rd[1] - rb[1], rb[1] - rc[1], rc[1] - ra[1]);
            let qs2 = ctx.compute_qs(rd[2] - rb[2], rb[2] - rc[2], rc[2] - ra[2]);

            if (qs0 | qs1 | qs2) == 0 {
                let run_len = match run_mode::decode_run_pixels(&mut br, &mut ctx, width - x) {
                    Ok(v) => v,
                    Err(e) => return Err(scan_error(e, base_offset + br.pos())),
                };
                let ra_u16 = [ra[0] as u16, ra[1] as u16, ra[2] as u16];
                cur[x + 1..x + 1 + run_len].fill(ra_u16);
                x += run_len;
                if x == width {
                    break;
                }
                let rb_i = prev[x + 1];
                let mut recon = [0i32; 3];
                for c in 0..3 {
                    recon[c] = match run_mode::decode_run_interruption_ilv2(
                        &mut br,
                        &mut ctx,
                        ra[c],
                        i32::from(rb_i[c]),
                    ) {
                        Ok(v) => v,
                        Err(e) => return Err(scan_error(e, base_offset + br.pos())),
                    };
                }
                cur[x + 1] = [recon[0] as u16, recon[1] as u16, recon[2] as u16];
                ctx.decrement_run_index();
                x += 1;
                if x < width {
                    ra = recon;
                    let p_x = prev[x];
                    let p_x1 = prev[x + 1];
                    rc = [i32::from(p_x[0]), i32::from(p_x[1]), i32::from(p_x[2])];
                    rb = [i32::from(p_x1[0]), i32::from(p_x1[1]), i32::from(p_x1[2])];
                }
                continue;
            }

            let r0 = decode_regular_qs::<LOSSLESS, POW2>(
                &mut br,
                &mut ctx,
                qs0,
                ra[0],
                rb[0],
                rc[0],
                base_offset,
            )?;
            let r1 = decode_regular_qs::<LOSSLESS, POW2>(
                &mut br,
                &mut ctx,
                qs1,
                ra[1],
                rb[1],
                rc[1],
                base_offset,
            )?;
            let r2 = decode_regular_qs::<LOSSLESS, POW2>(
                &mut br,
                &mut ctx,
                qs2,
                ra[2],
                rb[2],
                rc[2],
                base_offset,
            )?;
            cur[x + 1] = [r0 as u16, r1 as u16, r2 as u16];
            ra = [r0, r1, r2];
            rc = rb;
            rb = rd;
            x += 1;
        }
        emit_row(y, &cur_vec[1..width + 1]);
        std::mem::swap(&mut prev_vec, &mut cur_vec);
    }

    Ok(br.marker_pos())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::encode::{encode, encode_bytes, encode_image, encode_scan_t87, encode_with_options};
    use crate::EncodeOptions;

    fn roundtrip(pixels: &[u16], w: u32, h: u32) -> Vec<u16> {
        let mut buf = Vec::new();
        encode_with_options(pixels, w, h, &EncodeOptions::default(), &mut buf).expect("encode");
        let (decoded, dw, dh) = decode(&buf, w, h).expect("decode");
        assert_eq!((dw, dh), (w, h));
        decoded
    }

    #[test]
    fn roundtrip_small_gradient() {
        let (w, h) = (7u32, 5u32);
        let pixels: Vec<u16> = (0..w * h).map(|i| (i * 3 % 256) as u16).collect();
        assert_eq!(roundtrip(&pixels, w, h), pixels);
    }

    #[test]
    fn roundtrip_constant_forces_run() {
        let pixels = vec![777u16; 24 * 8];
        assert_eq!(roundtrip(&pixels, 24, 8), pixels);
    }

    #[test]
    fn roundtrip_1x1() {
        assert_eq!(roundtrip(&[42u16], 1, 1), vec![42u16]);
    }

    /// Hand-build a stream carrying a non-default LSE ID=1 preset wrapping our
    /// own scan bytes, and verify it decodes exactly.
    #[test]
    fn lse_id1_presets_honored() {
        let (w, h) = (16usize, 12usize);
        let pixels: Vec<u16> = (0..(w * h) as u32)
            .map(|i| ((i * 5) % 200) as u16)
            .collect();
        let (maxval, t1, t2, t3, reset) = (255i32, 5i32, 12i32, 40i32, 32i32);

        let mut out: Vec<u8> = Vec::new();
        out.extend_from_slice(&[0xFF, 0xD8]); // SOI
                                              // SOF55
        out.extend_from_slice(&[0xFF, 0xF7]);
        out.extend_from_slice(&11u16.to_be_bytes());
        out.push(8); // precision
        out.extend_from_slice(&(h as u16).to_be_bytes());
        out.extend_from_slice(&(w as u16).to_be_bytes());
        out.extend_from_slice(&[1, 1, 0x11, 0x00]); // Nf + component spec
                                                    // LSE ID=1
        out.extend_from_slice(&[0xFF, 0xF8]);
        out.extend_from_slice(&13u16.to_be_bytes());
        out.push(1);
        for v in [maxval, t1, t2, t3, reset] {
            out.extend_from_slice(&(v as u16).to_be_bytes());
        }
        // SOS
        out.extend_from_slice(&[0xFF, 0xDA]);
        out.extend_from_slice(&8u16.to_be_bytes());
        out.extend_from_slice(&[1, 1, 0x00, 0, 0, 0]);
        // scan encoded with the same presets
        let ctx = ContextModel::with_presets(maxval, 0, reset, Some(t1), Some(t2), Some(t3));
        encode_scan_t87(&mut out, &pixels, w, h, ctx).unwrap();
        out.extend_from_slice(&[0xFF, 0xD9]); // EOI

        let (decoded, _, _) = decode(&out, w as u32, h as u32).expect("LSE decode");
        assert_eq!(decoded, pixels);
    }

    /// Build SOI + SOF55 + LSE(ID=1) + SOS carrying the given preset fields.
    /// Validation fires before scan decode, so no scan/EOI bytes are needed.
    #[allow(clippy::too_many_arguments)]
    fn lse_preset_stream(
        precision: u8,
        w: u16,
        h: u16,
        maxval: u16,
        t1: u16,
        t2: u16,
        t3: u16,
        reset: u16,
        near: u8,
    ) -> Vec<u8> {
        let mut out: Vec<u8> = vec![0xFF, 0xD8]; // SOI
        out.extend_from_slice(&[0xFF, 0xF7]); // SOF55
        out.extend_from_slice(&11u16.to_be_bytes());
        out.push(precision);
        out.extend_from_slice(&h.to_be_bytes());
        out.extend_from_slice(&w.to_be_bytes());
        out.extend_from_slice(&[1, 1, 0x11, 0x00]);
        out.extend_from_slice(&[0xFF, 0xF8]); // LSE ID=1
        out.extend_from_slice(&13u16.to_be_bytes());
        out.push(1);
        for v in [maxval, t1, t2, t3, reset] {
            out.extend_from_slice(&v.to_be_bytes());
        }
        out.extend_from_slice(&[0xFF, 0xDA]); // SOS
        out.extend_from_slice(&8u16.to_be_bytes());
        out.extend_from_slice(&[1, 1, 0x00, near, 0, 0]);
        out
    }

    #[test]
    fn sof_zero_dimension_rejected() {
        // height = 0, width = 4 -> reject before allocation.
        let mut out: Vec<u8> = vec![0xFF, 0xD8, 0xFF, 0xF7];
        out.extend_from_slice(&11u16.to_be_bytes());
        out.push(8);
        out.extend_from_slice(&0u16.to_be_bytes()); // height
        out.extend_from_slice(&4u16.to_be_bytes()); // width
        out.extend_from_slice(&[1, 1, 0x11, 0x00]);
        assert!(matches!(
            decode(&out, 4, 0),
            Err(CodecError::InvalidData(_))
        ));
    }

    #[test]
    fn sof_length_must_match_component_count() {
        // Lf must be exactly 8 + 3*Nf (11 for Nf = 1). A short segment must not
        // read its fields from the next marker; a long one is malformed too.
        let mut buf = Vec::new();
        encode_with_options(&[0u16; 16], 4, 4, &EncodeOptions::default(), &mut buf).unwrap();
        let sof = buf.windows(2).position(|p| p == [0xFF, 0xF7]).unwrap();
        assert_eq!(&buf[sof + 2..sof + 4], &[0x00, 11]);
        for lf in [2u8, 5, 8, 10, 12] {
            let mut bad = buf.clone();
            bad[sof + 3] = lf;
            assert!(
                matches!(decode(&bad, 4, 4), Err(CodecError::InvalidData(_))),
                "Lf = {lf}"
            );
        }
    }

    #[test]
    fn sof_huge_dimensions_rejected_not_aborted() {
        // 65535 x 65535 would demand ~8 GiB; the pixel cap must reject it as a
        // clean Err rather than abort or OOM.
        let mut out: Vec<u8> = vec![0xFF, 0xD8, 0xFF, 0xF7];
        out.extend_from_slice(&11u16.to_be_bytes());
        out.push(8);
        out.extend_from_slice(&65535u16.to_be_bytes()); // height
        out.extend_from_slice(&65535u16.to_be_bytes()); // width
        out.extend_from_slice(&[1, 1, 0x11, 0x00]);
        assert!(matches!(
            decode(&out, 65535, 65535),
            Err(CodecError::InvalidParameter {
                name: "width*height",
                ..
            })
        ));
    }

    #[test]
    fn lse_maxval_exceeds_precision_rejected() {
        // precision 8 -> MAXVAL must be <= 255; a preset MAXVAL of 65535 fails.
        let out = lse_preset_stream(8, 4, 4, 65535, 0, 0, 0, 64, 0);
        assert!(matches!(
            decode(&out, 4, 4),
            Err(CodecError::InvalidData(_))
        ));
    }

    #[test]
    fn lse_threshold_ordering_rejected() {
        // T1 > T2 violates NEAR < T1 <= T2 <= T3 <= MAXVAL.
        let out = lse_preset_stream(8, 4, 4, 0, 100, 50, 200, 64, 0);
        assert!(matches!(
            decode(&out, 4, 4),
            Err(CodecError::InvalidData(_))
        ));
    }

    #[test]
    fn lse_t3_exceeds_maxval_rejected() {
        // Explicit MAXVAL 200, but T3 = 250 > MAXVAL.
        let out = lse_preset_stream(8, 4, 4, 200, 5, 12, 250, 64, 0);
        assert!(matches!(
            decode(&out, 4, 4),
            Err(CodecError::InvalidData(_))
        ));
    }

    #[test]
    fn lse_reset_zero_rejected() {
        // RESET below the T.87 minimum (3) is rejected in read_lse.
        let out = lse_preset_stream(8, 4, 4, 0, 0, 0, 0, 0, 0);
        assert!(matches!(
            decode(&out, 4, 4),
            Err(CodecError::InvalidData(_))
        ));
    }

    #[test]
    fn lse_id2_unsupported() {
        // SOI, SOF55, LSE ID=2 -> Unsupported.
        let mut out: Vec<u8> = vec![0xFF, 0xD8, 0xFF, 0xF7];
        out.extend_from_slice(&11u16.to_be_bytes());
        out.push(8);
        out.extend_from_slice(&4u16.to_be_bytes());
        out.extend_from_slice(&4u16.to_be_bytes());
        out.extend_from_slice(&[1, 1, 0x11, 0x00]);
        out.extend_from_slice(&[0xFF, 0xF8]);
        out.extend_from_slice(&4u16.to_be_bytes());
        out.push(2); // ID=2
        out.push(0);
        assert!(matches!(
            decode(&out, 4, 4),
            Err(CodecError::Unsupported(_))
        ));
    }

    #[test]
    fn dri_zero_interval_is_tolerated() {
        // A benign DRI segment (restart interval 0) before the scan must not
        // fail an otherwise decodable stream — CharLS accepts it. (A non-zero
        // interval that actually emits RSTn markers is still rejected by the
        // scan decoder.)
        let (w, h) = (4u32, 4u32);
        let pixels: Vec<u16> = (0..16).map(|i| i * 4).collect();
        let mut buf = Vec::new();
        encode_with_options(&pixels, w, h, &EncodeOptions::default(), &mut buf).unwrap();
        let sos = buf
            .windows(2)
            .position(|p| p == [0xFF, 0xDA])
            .expect("SOS present");
        let mut with_dri = buf[..sos].to_vec();
        with_dri.extend_from_slice(&[0xFF, 0xDD, 0x00, 0x04, 0x00, 0x00]); // DRI, interval 0
        with_dri.extend_from_slice(&buf[sos..]);
        let (decoded, _, _) = decode(&with_dri, w, h).expect("benign DRI should decode");
        assert_eq!(decoded, pixels);
    }

    #[test]
    fn fill_bytes_before_marker_tolerated() {
        // 0xFF fill bytes may precede any marker (T.81 B.1.1.2); they must be
        // skipped, not mistaken for a marker.
        let (w, h) = (4u32, 4u32);
        let pixels: Vec<u16> = (0..16).map(|i| i * 4).collect();
        let mut buf = Vec::new();
        encode_with_options(&pixels, w, h, &EncodeOptions::default(), &mut buf).unwrap();
        // Two 0xFF fill bytes right after SOI, before SOF55.
        let mut filled = buf[..2].to_vec();
        filled.extend_from_slice(&[0xFF, 0xFF]);
        filled.extend_from_slice(&buf[2..]);
        let (decoded, _, _) = decode(&filled, w, h).expect("fill bytes should be skipped");
        assert_eq!(decoded, pixels);
    }

    #[test]
    fn fill_bytes_before_eoi_tolerated() {
        // The EOI after the last scan may be preceded by 0xFF fill bytes too:
        // DCMTK's default padding of an odd-length stream is `FF FF D9`.
        let (w, h) = (4u32, 4u32);
        let pixels: Vec<u16> = (0..16).map(|i| i * 4).collect();
        let mut buf = Vec::new();
        encode_with_options(&pixels, w, h, &EncodeOptions::default(), &mut buf).unwrap();
        let (scan, eoi) = buf.split_at(buf.len() - 2);
        assert_eq!(eoi, [0xFF, 0xD9]);
        for fill in 1..=3 {
            let mut filled = scan.to_vec();
            filled.extend_from_slice(&vec![0xFF; fill]);
            filled.extend_from_slice(eoi);
            let (decoded, _, _) = decode(&filled, w, h).expect("fill bytes should be skipped");
            assert_eq!(decoded, pixels, "{fill} fill byte(s), decode");
            let image = decode_image(&filled).expect("fill bytes should be skipped");
            assert_eq!(image.samples, pixels, "{fill} fill byte(s), decode_image");
        }
    }

    #[test]
    fn fill_bytes_without_eoi_rejected() {
        // A run of 0xFF fill bytes that ends the data, or that ends in a
        // marker other than EOI, is still not an EOI.
        let (w, h) = (4u32, 4u32);
        let pixels: Vec<u16> = (0..16).map(|i| i * 4).collect();
        let mut buf = Vec::new();
        encode_with_options(&pixels, w, h, &EncodeOptions::default(), &mut buf).unwrap();
        let scan = &buf[..buf.len() - 2];
        for tail in [&[0xFF][..], &[0xFF, 0xFF], &[0xFF, 0xFF, 0xD8]] {
            let mut bad = scan.to_vec();
            bad.extend_from_slice(tail);
            assert!(decode(&bad, w, h).is_err(), "tail {tail:02X?}");
            assert!(decode_image(&bad).is_err(), "tail {tail:02X?}");
        }
    }

    fn encode_mono_4x4() -> (Vec<u8>, Vec<u16>) {
        let pixels: Vec<u16> = (0..16).map(|i| i * 4).collect();
        let mut buf = Vec::new();
        encode_with_options(&pixels, 4, 4, &EncodeOptions::default(), &mut buf).unwrap();
        (buf, pixels)
    }

    #[test]
    fn sos_mapping_table_and_point_transform_rejected() {
        // SOS: FF DA | Ls(2) | Ns | Csj | mapping | NEAR | ILV | Ah/Al.
        let (buf, _) = encode_mono_4x4();
        let sos = buf.windows(2).position(|p| p == [0xFF, 0xDA]).unwrap();
        let mut mapping = buf.clone();
        mapping[sos + 6] = 1; // non-zero mapping-table selector
        assert!(matches!(
            decode(&mapping, 4, 4),
            Err(CodecError::Unsupported(_))
        ));
        let mut point = buf.clone();
        point[sos + 9] = 3; // non-zero point transform
        assert!(matches!(
            decode(&point, 4, 4),
            Err(CodecError::InvalidData(_))
        ));
    }

    #[test]
    fn sos_component_count_is_validated_against_the_frame() {
        // An impossible Ns (0, > 4, or more than the frame's Nf) is malformed;
        // a legal multi-component scan (ILV 1/2) is merely unsupported.
        let (mono, _) = encode_mono_4x4();
        let sos = mono.windows(2).position(|p| p == [0xFF, 0xDA]).unwrap();
        for ns in [0u8, 2, 5, 7] {
            let mut bad = mono.clone();
            bad[sos + 4] = ns;
            match decode(&bad, 4, 4) {
                Err(CodecError::InvalidData(m)) => assert!(m.contains("Ns = "), "{m}"),
                other => panic!("mono Ns = {ns}: {other:?}"),
            }
        }

        let planes: Vec<Vec<u16>> = (0..3).map(|c| vec![c * 40; 16]).collect();
        let rgb = build_planar_rgb(&planes, 4, 4, 8);
        let sos = rgb.windows(2).position(|p| p == [0xFF, 0xDA]).unwrap();
        let mut unsupported_ns = rgb.clone();
        unsupported_ns[sos + 4] = 2;
        assert!(matches!(
            decode_image(&unsupported_ns),
            Err(CodecError::Unsupported(_))
        ));
        let mut bad_sos_len = rgb.clone();
        bad_sos_len[sos + 4] = 3;
        assert!(matches!(
            decode_image(&bad_sos_len),
            Err(CodecError::InvalidData(_))
        ));
        let mut too_many = rgb.clone();
        too_many[sos + 4] = 4;
        assert!(matches!(
            decode_image(&too_many),
            Err(CodecError::InvalidData(_))
        ));
    }

    #[test]
    fn sof_subsampling_and_component_mapping_rejected() {
        // SOF55: FF F7 | Ls(2) | P | H(2) | W(2) | Nf | [Ci, sampling, Tq]...
        let (buf, _) = encode_mono_4x4();
        let sof = buf.windows(2).position(|p| p == [0xFF, 0xF7]).unwrap();
        let mut sub = buf.clone();
        sub[sof + 11] = 0x22; // 2:2 subsampling, not 1:1
        assert!(matches!(
            decode(&sub, 4, 4),
            Err(CodecError::Unsupported(_))
        ));
        let mut tq = buf.clone();
        tq[sof + 12] = 1; // per-component mapping table
        assert!(matches!(decode(&tq, 4, 4), Err(CodecError::Unsupported(_))));
    }

    /// Assemble a 3-component, planar (ILV 0) JPEG-LS stream: one SOS + scan
    /// per plane, sharing one SOF55 (Nf = 3). Lossless, default presets.
    fn build_planar_rgb(planes: &[Vec<u16>], w: usize, h: usize, precision: u8) -> Vec<u8> {
        let nf = planes.len();
        let max_val = (1i32 << precision) - 1;
        let mut out: Vec<u8> = vec![0xFF, 0xD8]; // SOI
                                                 // SOF55: Ls = 8 + 3*Nf.
        out.extend_from_slice(&[0xFF, 0xF7]);
        out.extend_from_slice(&((8 + 3 * nf) as u16).to_be_bytes());
        out.push(precision);
        out.extend_from_slice(&(h as u16).to_be_bytes());
        out.extend_from_slice(&(w as u16).to_be_bytes());
        out.push(nf as u8);
        for c in 0..nf {
            out.extend_from_slice(&[(c as u8) + 1, 0x11, 0x00]); // id, sampling 1:1, Tq 0
        }
        // One SOS + entropy scan per component (ILV 0).
        for (c, plane) in planes.iter().enumerate() {
            out.extend_from_slice(&[0xFF, 0xDA]);
            out.extend_from_slice(&8u16.to_be_bytes()); // Ls = 8 for Ns = 1
            out.push(1); // Ns = 1
            out.extend_from_slice(&[(c as u8) + 1, 0x00]); // Csj, mapping table 0
            out.extend_from_slice(&[0x00, 0x00, 0x00]); // NEAR = 0, ILV = 0, point transform 0
            let ctx = ContextModel::new(max_val, 0, DEFAULT_RESET);
            encode_scan_t87(&mut out, plane, w, h, ctx).unwrap();
        }
        out.extend_from_slice(&[0xFF, 0xD9]); // EOI
        out
    }

    #[test]
    fn decode_is_grayscale_only() {
        // A well-formed 3-component stream is rejected by the grayscale-only
        // decode() entry; decode_image() handles it (next test).
        let plane: Vec<u16> = (0..16).collect();
        let s = build_planar_rgb(&[plane.clone(), plane.clone(), plane], 4, 4, 8);
        assert!(matches!(decode(&s, 4, 4), Err(CodecError::Unsupported(_))));
    }

    #[test]
    fn decode_image_rgb_planar_roundtrip() {
        let (w, h) = (5usize, 3usize);
        let n = (w * h) as u16;
        let r: Vec<u16> = (0..n).map(|i| i * 3 % 256).collect();
        let g: Vec<u16> = (0..n).map(|i| (i * 7 + 1) % 256).collect();
        let b: Vec<u16> = (0..n).map(|i| 200u16.wrapping_sub(i * 5) % 256).collect();
        let s = build_planar_rgb(&[r.clone(), g.clone(), b.clone()], w, h, 8);
        let img = decode_image(&s).expect("decode_image");
        assert_eq!((img.width, img.height), (w as u32, h as u32));
        assert_eq!(img.components, 3);
        assert_eq!(img.interleave_mode, 0);
        assert_eq!((img.near, img.bits_per_sample), (0, 8));
        // Component-planar layout: R plane, then G, then B.
        let mut expected = r;
        expected.extend_from_slice(&g);
        expected.extend_from_slice(&b);
        assert_eq!(img.samples, expected);
    }

    #[test]
    fn decode_image_mono_matches_source() {
        let (w, h) = (7u32, 5u32);
        let pixels: Vec<u16> = (0..w * h).map(|i| (i * 3 % 256) as u16).collect();
        let mut buf = Vec::new();
        encode_with_options(&pixels, w, h, &EncodeOptions::default(), &mut buf).unwrap();
        let img = decode_image(&buf).expect("decode_image mono");
        assert_eq!(img.components, 1);
        assert_eq!((img.width, img.height), (w, h));
        assert_eq!(img.samples, pixels);
    }

    #[test]
    fn encode_image_roundtrips_through_decode_image() {
        // Mono.
        let mono: Vec<u16> = (0..35u16).map(|i| i * 3 % 256).collect();
        let mut buf = Vec::new();
        encode_image(&mono, 7, 5, 1, &EncodeOptions::default(), &mut buf).unwrap();
        let img = decode_image(&buf).unwrap();
        assert_eq!((img.components, img.width, img.height), (1, 7, 5));
        assert_eq!(img.samples, mono);

        // 3-component planar (ILV 0).
        let (w, h) = (5u32, 3u32);
        let n = (w * h) as u16;
        let mut planar: Vec<u16> = Vec::new();
        planar.extend((0..n).map(|i| i * 3 % 256));
        planar.extend((0..n).map(|i| (i * 7 + 1) % 256));
        planar.extend((0..n).map(|i| 200u16.wrapping_sub(i * 5) % 256));
        let mut buf = Vec::new();
        encode_image(&planar, w, h, 3, &EncodeOptions::default(), &mut buf).unwrap();
        let img = decode_image(&buf).unwrap();
        assert_eq!((img.components, img.interleave_mode), (3, 0));
        assert_eq!(img.samples, planar);
    }

    #[test]
    fn precision_out_of_range_rejected() {
        for bad in [0u8, 1, 17] {
            let mut out: Vec<u8> = vec![0xFF, 0xD8, 0xFF, 0xF7];
            out.extend_from_slice(&11u16.to_be_bytes());
            out.push(bad);
            out.extend_from_slice(&4u16.to_be_bytes());
            out.extend_from_slice(&4u16.to_be_bytes());
            out.extend_from_slice(&[1, 1, 0x11, 0x00]);
            assert!(
                matches!(
                    decode(&out, 4, 4),
                    Err(CodecError::InvalidParameter {
                        name: "precision",
                        ..
                    })
                ),
                "precision {bad} should be rejected"
            );
        }
    }

    #[test]
    fn truncated_scan_is_error_not_panic() {
        let pixels: Vec<u16> = (0..64).map(|i| (i * 4) as u16).collect();
        let mut buf = Vec::new();
        encode_with_options(&pixels, 8, 8, &EncodeOptions::default(), &mut buf).unwrap();
        // Chop off the tail (scan + EOI) mid-stream.
        let truncated = &buf[..buf.len() - 3];
        let err = decode(truncated, 8, 8).unwrap_err();
        assert!(matches!(
            err,
            CodecError::Truncated { .. } | CodecError::InvalidData(_)
        ));
    }

    #[test]
    fn missing_eoi_rejected() {
        let pixels: Vec<u16> = (0..64).map(|i| (i * 4) as u16).collect();
        let mut buf = Vec::new();
        encode_with_options(&pixels, 8, 8, &EncodeOptions::default(), &mut buf).unwrap();
        // Replace the trailing EOI bytes with garbage.
        let n = buf.len();
        buf[n - 2] = 0x12;
        buf[n - 1] = 0x34;
        assert!(decode(&buf, 8, 8).is_err());
    }

    #[test]
    fn decode_rejects_bad_soi() {
        assert!(decode(&[0x00, 0x00, 0x00, 0x00], 1, 1).is_err());
    }

    #[test]
    fn ilv1_ilv2_and_bytes_roundtrip() {
        let (w, h) = (32usize, 24usize);
        let n = w * h;
        let mut rgb = Vec::with_capacity(n * 3);
        for y in 0..h {
            for x in 0..w {
                if y < 6 {
                    // Uniform band exercises multi-component run mode + interruption.
                    rgb.extend_from_slice(&[10u8, 20, 30]);
                } else {
                    rgb.push(((x * 7 + y * 3) & 0xFF) as u8);
                    rgb.push(((x * 3 + y * 11) & 0xFF) as u8);
                    rgb.push(((x * 5 + y * 13) & 0xFF) as u8);
                }
            }
        }
        for ilv in [1u8, 2u8] {
            for near in [0u8, 3u8] {
                let encoded = encode_bytes(&rgb, w as u32, h as u32, 3, 8, near, ilv).unwrap();
                let info = read_frame_info(&encoded).unwrap();
                assert_eq!(info.width, w as u32);
                assert_eq!(info.height, h as u32);
                assert_eq!(info.components, 3);
                assert_eq!(info.bits_per_sample, 8);
                assert_eq!(info.interleave_mode, ilv);
                assert_eq!(info.near, near);
                let (decoded, dec_info) = decode_to_bytes(&encoded).unwrap();
                assert_eq!(dec_info, info);
                assert_eq!(decoded.len(), rgb.len());
                if near == 0 {
                    assert_eq!(decoded, rgb, "ILV={ilv} lossless mismatch");
                } else {
                    assert!(
                        decoded
                            .iter()
                            .zip(rgb.iter())
                            .all(|(&a, &b)| a.abs_diff(b) <= near),
                        "ILV={ilv} near={near} error exceeded bound"
                    );
                }
            }
        }
    }

    #[test]
    fn eoi_preceded_by_ff_fill_byte_decodes() {
        let pixels: Vec<u16> = (0..64).map(|i| (i * 513) as u16).collect();
        let mut buf = Vec::new();
        encode(&pixels, 8, 8, &mut buf).unwrap();
        // Insert an extra 0xFF fill byte right before EOI (`FF FF D9`), as DCMTK
        // and legacy encoders emit to pad odd-length streams to even length.
        let eoi = buf.len() - 2;
        assert_eq!(&buf[eoi..], &[0xFF, 0xD9]);
        buf.insert(eoi, 0xFF);
        let decoded = decode_image(&buf).expect("FF FF D9 fill byte must be accepted");
        assert_eq!(decoded.samples, pixels);
    }
}
