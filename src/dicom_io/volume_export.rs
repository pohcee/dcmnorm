//! Whole-volume export: writes a stack of raw (unwindowed) reformatted samples - see
//! `super::volume::reformat_plane_values` - out as a single NIfTI-1 (`.nii`/`.nii.gz`) or NRRD
//! (`.nrrd`) file. Both formats are simple enough (NIfTI-1: a fixed 348-byte header; NRRD: a
//! plain-text header + raw payload) to hand-write directly rather than depend on a crate - no
//! mature NRRD crate exists on crates.io, and the `nifti` crate's write support is thin/
//! undocumented.
//!
//! Also owns `generate_uid`, a small UUID-derived DICOM UID generator (PS3.5 Annex B,
//! `2.25.<uuid-as-decimal>`) shared with `secondary_capture`.

use std::error::Error;
use std::fmt;
use std::fs;
use std::io::{self, Write};
use std::path::Path;

use flate2::write::GzEncoder;
use flate2::Compression;

#[derive(Debug)]
pub enum VolumeExportError {
    Io(io::Error),
    /// `samples.len()` didn't match `dims.0 * dims.1 * dims.2`.
    SampleCountMismatch { expected: usize, found: usize },
    InvalidDimensions,
}

impl fmt::Display for VolumeExportError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "failed to write volume export file: {error}"),
            Self::SampleCountMismatch { expected, found } => write!(
                formatter,
                "volume export sample count ({found}) does not match its declared dimensions ({expected} expected)"
            ),
            Self::InvalidDimensions => write!(formatter, "volume export dimensions must all be non-zero"),
        }
    }
}

impl Error for VolumeExportError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            _ => None,
        }
    }
}

impl From<io::Error> for VolumeExportError {
    fn from(value: io::Error) -> Self {
        Self::Io(value)
    }
}

/// Generates a fresh DICOM UID via the UUID-derived scheme (PS3.5 Annex B): `2.25.<uuid-as-a-
/// 128-bit-decimal-number>`. Needs no registered organization root, unlike the traditional
/// incrementing-suffix scheme. `pub` (not `pub(crate)`) since the CLI also needs one - a shared
/// SeriesInstanceUID across a whole `--mpr` multi-file `.dcm` output stack.
pub fn generate_uid() -> String {
    format!("2.25.{}", uuid::Uuid::new_v4().as_u128())
}

/// The geometry of an exported volume: three orthonormal axis directions (LPS millimeters), each
/// implicitly scaled by its own step below, plus the world-space position of the CENTER of voxel
/// (col=0, row=0, slice=0) - the same "center of the first voxel" convention DICOM's own
/// `ImagePositionPatient` uses, so callers can derive `origin` directly from a `PlaneParams`
/// without an extra half-voxel correction beyond the usual half-width/half-height offset.
#[derive(Clone, Copy, Debug)]
pub struct VolumeGeometry {
    /// Unit vector: direction of travel as the COLUMN index increases.
    pub row_dir: [f64; 3],
    /// Unit vector: direction of travel as the ROW index increases.
    pub col_dir: [f64; 3],
    /// Unit vector: direction of travel as the SLICE/DEPTH index increases.
    pub normal_dir: [f64; 3],
    /// mm per column step (along `row_dir`).
    pub col_spacing_mm: f64,
    /// mm per row step (along `col_dir`).
    pub row_spacing_mm: f64,
    /// mm per slice step (along `normal_dir`).
    pub step_mm: f64,
    /// LPS mm, center of voxel (0,0,0).
    pub origin: [f64; 3],
}

const NIFTI_HEADER_SIZE: usize = 348;
const NIFTI_DATATYPE_FLOAT32: i16 = 16;
const NIFTI_BITPIX_FLOAT32: i16 = 32;

fn put_i16(buf: &mut [u8], offset: usize, value: i16) {
    buf[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
}

fn put_i32(buf: &mut [u8], offset: usize, value: i32) {
    buf[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}

fn put_f32(buf: &mut [u8], offset: usize, value: f32) {
    buf[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}

fn put_bytes(buf: &mut [u8], offset: usize, value: &[u8]) {
    let end = (offset + value.len()).min(buf.len());
    buf[offset..end].copy_from_slice(&value[..end - offset]);
}

/// Writes `samples` (row-major within each slice, slice-major overall - the same layout
/// `Volume::samples`/`reformat_plane_values` use) as a NIfTI-1 volume. `.nii.gz` (by extension,
/// checked by the caller) gzips the whole file.
///
/// NIfTI's sform is RAS+ (Right/Anterior/Superior), while `dcmnorm`'s geometry (like DICOM
/// itself) is LPS+ (Left/Posterior/Superior) - every direction vector and the origin get their
/// X/Y components negated when building `srow_x/y/z`, the same convention `dcm2niix`/`nibabel`
/// use for DICOM-derived NIfTI files.
pub fn write_nifti(
    samples: &[f32],
    dims: (u32, u32, u32),
    geometry: &VolumeGeometry,
    path: &Path,
    gzip: bool,
) -> Result<(), VolumeExportError> {
    let (cols, rows, slices) = dims;
    if cols == 0 || rows == 0 || slices == 0 {
        return Err(VolumeExportError::InvalidDimensions);
    }
    let expected = cols as usize * rows as usize * slices as usize;
    if samples.len() != expected {
        return Err(VolumeExportError::SampleCountMismatch { expected, found: samples.len() });
    }

    let mut header = [0u8; NIFTI_HEADER_SIZE];
    put_i32(&mut header, 0, NIFTI_HEADER_SIZE as i32); // sizeof_hdr
    header[38] = b'r'; // regular

    // dim[0..8]: dim[0] = number of dimensions in use, dim[1..4] = cols/rows/slices, remaining
    // trailing entries conventionally 1.
    put_i16(&mut header, 40, 3);
    put_i16(&mut header, 42, cols as i16);
    put_i16(&mut header, 44, rows as i16);
    put_i16(&mut header, 46, slices as i16);
    for extra_dim_offset in [48, 50, 52, 54] {
        put_i16(&mut header, extra_dim_offset, 1);
    }

    put_i16(&mut header, 70, NIFTI_DATATYPE_FLOAT32);
    put_i16(&mut header, 72, NIFTI_BITPIX_FLOAT32);

    // pixdim[0] (qfac) = 1, pixdim[1..4] = the actual voxel spacings.
    put_f32(&mut header, 76, 1.0);
    put_f32(&mut header, 80, geometry.col_spacing_mm as f32);
    put_f32(&mut header, 84, geometry.row_spacing_mm as f32);
    put_f32(&mut header, 88, geometry.step_mm as f32);

    put_f32(&mut header, 108, (NIFTI_HEADER_SIZE + 4) as f32); // vox_offset

    put_bytes(&mut header, 148, b"dcmnorm MPR volume export");

    // sform only (qform_code = 0); sform_code = 1 (NIFTI_XFORM_SCANNER_ANAT).
    put_i16(&mut header, 254, 1);

    let row_scaled = scale3(geometry.row_dir, geometry.col_spacing_mm);
    let col_scaled = scale3(geometry.col_dir, geometry.row_spacing_mm);
    let normal_scaled = scale3(geometry.normal_dir, geometry.step_mm);
    let lps_to_ras = |v: [f64; 3]| [-v[0], -v[1], v[2]];
    let row_ras = lps_to_ras(row_scaled);
    let col_ras = lps_to_ras(col_scaled);
    let normal_ras = lps_to_ras(normal_scaled);
    let origin_ras = lps_to_ras(geometry.origin);

    put_f32(&mut header, 280, row_ras[0] as f32);
    put_f32(&mut header, 284, col_ras[0] as f32);
    put_f32(&mut header, 288, normal_ras[0] as f32);
    put_f32(&mut header, 292, origin_ras[0] as f32);

    put_f32(&mut header, 296, row_ras[1] as f32);
    put_f32(&mut header, 300, col_ras[1] as f32);
    put_f32(&mut header, 304, normal_ras[1] as f32);
    put_f32(&mut header, 308, origin_ras[1] as f32);

    put_f32(&mut header, 312, row_ras[2] as f32);
    put_f32(&mut header, 316, col_ras[2] as f32);
    put_f32(&mut header, 320, normal_ras[2] as f32);
    put_f32(&mut header, 324, origin_ras[2] as f32);

    put_bytes(&mut header, 344, b"n+1\0"); // magic

    let mut payload = Vec::with_capacity(NIFTI_HEADER_SIZE + 4 + samples.len() * 4);
    payload.extend_from_slice(&header);
    payload.extend_from_slice(&[0u8; 4]); // extension flag: no extensions
    for value in samples {
        payload.extend_from_slice(&value.to_le_bytes());
    }

    if gzip {
        let file = fs::File::create(path)?;
        let mut encoder = GzEncoder::new(file, Compression::default());
        encoder.write_all(&payload)?;
        encoder.finish()?;
    } else {
        fs::write(path, payload)?;
    }
    Ok(())
}

/// Writes `samples` as an NRRD ("nearly raw raster data") volume: a plain-text header followed by
/// a blank line and the raw little-endian `f32` payload. No LPS/RAS conversion needed - NRRD
/// names its coordinate space directly (`space: left-posterior-superior`), so origins/directions
/// are written exactly as given.
pub fn write_nrrd(samples: &[f32], dims: (u32, u32, u32), geometry: &VolumeGeometry, path: &Path) -> Result<(), VolumeExportError> {
    let (cols, rows, slices) = dims;
    if cols == 0 || rows == 0 || slices == 0 {
        return Err(VolumeExportError::InvalidDimensions);
    }
    let expected = cols as usize * rows as usize * slices as usize;
    if samples.len() != expected {
        return Err(VolumeExportError::SampleCountMismatch { expected, found: samples.len() });
    }

    let row_scaled = scale3(geometry.row_dir, geometry.col_spacing_mm);
    let col_scaled = scale3(geometry.col_dir, geometry.row_spacing_mm);
    let normal_scaled = scale3(geometry.normal_dir, geometry.step_mm);

    let mut header = String::new();
    header.push_str("NRRD0004\n");
    header.push_str("# Complete NRRD file format specification at:\n");
    header.push_str("# http://teem.sourceforge.net/nrrd/format.html\n");
    header.push_str("type: float\n");
    header.push_str("dimension: 3\n");
    header.push_str("space: left-posterior-superior\n");
    header.push_str(&format!("sizes: {cols} {rows} {slices}\n"));
    header.push_str(&format!(
        "space directions: ({:.9},{:.9},{:.9}) ({:.9},{:.9},{:.9}) ({:.9},{:.9},{:.9})\n",
        row_scaled[0], row_scaled[1], row_scaled[2],
        col_scaled[0], col_scaled[1], col_scaled[2],
        normal_scaled[0], normal_scaled[1], normal_scaled[2],
    ));
    header.push_str(&format!(
        "space origin: ({:.9},{:.9},{:.9})\n",
        geometry.origin[0], geometry.origin[1], geometry.origin[2]
    ));
    header.push_str("endian: little\n");
    header.push_str("encoding: raw\n");
    header.push('\n');

    let mut file = fs::File::create(path)?;
    file.write_all(header.as_bytes())?;
    for value in samples {
        file.write_all(&value.to_le_bytes())?;
    }
    Ok(())
}

fn scale3(v: [f64; 3], s: f64) -> [f64; 3] {
    [v[0] * s, v[1] * s, v[2] * s]
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    fn axis_aligned_geometry() -> VolumeGeometry {
        VolumeGeometry {
            row_dir: [1.0, 0.0, 0.0],
            col_dir: [0.0, 1.0, 0.0],
            normal_dir: [0.0, 0.0, 1.0],
            col_spacing_mm: 0.5,
            row_spacing_mm: 0.6,
            step_mm: 2.0,
            origin: [10.0, -20.0, 30.0],
        }
    }

    fn temp_path(name: &str) -> std::path::PathBuf {
        let mut path = std::env::temp_dir();
        path.push(format!("dcmnorm-volume-export-test-{name}-{}", std::process::id()));
        path
    }

    #[test]
    fn generate_uid_produces_distinct_uuid_derived_uids() {
        let a = generate_uid();
        let b = generate_uid();
        assert!(a.starts_with("2.25."));
        assert!(b.starts_with("2.25."));
        assert_ne!(a, b);
    }

    #[test]
    fn write_nifti_rejects_a_sample_count_mismatch() {
        let path = temp_path("mismatch.nii");
        let error = write_nifti(&[0.0; 3], (2, 2, 1), &axis_aligned_geometry(), &path, false).unwrap_err();
        assert!(matches!(error, VolumeExportError::SampleCountMismatch { expected: 4, found: 3 }));
    }

    #[test]
    fn write_nifti_writes_a_well_formed_header_with_the_lps_to_ras_flip_applied() {
        let path = temp_path("axis-aligned.nii");
        let samples = vec![1.0f32; 2 * 3 * 4];
        write_nifti(&samples, (2, 3, 4), &axis_aligned_geometry(), &path, false).unwrap();

        let bytes = fs::read(&path).unwrap();
        fs::remove_file(&path).ok();
        assert_eq!(bytes.len(), NIFTI_HEADER_SIZE + 4 + samples.len() * 4);

        let read_i32 = |offset: usize| i32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap());
        let read_i16 = |offset: usize| i16::from_le_bytes(bytes[offset..offset + 2].try_into().unwrap());
        let read_f32 = |offset: usize| f32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap());

        assert_eq!(read_i32(0), NIFTI_HEADER_SIZE as i32);
        assert_eq!(read_i16(40), 3);
        assert_eq!(read_i16(42), 2); // cols
        assert_eq!(read_i16(44), 3); // rows
        assert_eq!(read_i16(46), 4); // slices
        assert_eq!(read_i16(70), NIFTI_DATATYPE_FLOAT32);
        assert_eq!(read_i16(72), NIFTI_BITPIX_FLOAT32);
        assert_eq!(read_f32(108), (NIFTI_HEADER_SIZE + 4) as f32);
        assert_eq!(&bytes[344..348], b"n+1\0");

        // Axis-aligned geometry: row_dir=[1,0,0]*0.5, col_dir=[0,1,0]*0.6, normal=[0,0,1]*2.0,
        // origin=[10,-20,30]. LPS->RAS negates X/Y only.
        assert_eq!(read_f32(280), -0.5); // srow_x: row axis
        assert_eq!(read_f32(284), 0.0); // srow_x: col axis
        assert_eq!(read_f32(288), 0.0); // srow_x: normal axis
        assert_eq!(read_f32(292), -10.0); // srow_x: origin (RAS X = -LPS X)

        assert_eq!(read_f32(296), 0.0); // srow_y: row axis
        assert_eq!(read_f32(300), -0.6); // srow_y: col axis
        assert_eq!(read_f32(304), 0.0); // srow_y: normal axis
        assert_eq!(read_f32(308), 20.0); // srow_y: origin (RAS Y = -LPS Y)

        assert_eq!(read_f32(312), 0.0); // srow_z: row axis
        assert_eq!(read_f32(316), 0.0); // srow_z: col axis
        assert_eq!(read_f32(320), 2.0); // srow_z: normal axis
        assert_eq!(read_f32(324), 30.0); // srow_z: origin (RAS Z = LPS Z, unchanged)
    }

    #[test]
    fn write_nifti_gz_produces_a_valid_gzip_stream_decompressing_back_to_the_same_bytes() {
        let plain_path = temp_path("plain.nii");
        let gz_path = temp_path("gz.nii.gz");
        let samples = vec![2.5f32; 2 * 2 * 2];
        write_nifti(&samples, (2, 2, 2), &axis_aligned_geometry(), &plain_path, false).unwrap();
        write_nifti(&samples, (2, 2, 2), &axis_aligned_geometry(), &gz_path, true).unwrap();

        let plain_bytes = fs::read(&plain_path).unwrap();
        let gz_bytes = fs::read(&gz_path).unwrap();
        fs::remove_file(&plain_path).ok();
        fs::remove_file(&gz_path).ok();

        let mut decoder = flate2::read::GzDecoder::new(&gz_bytes[..]);
        let mut decompressed = Vec::new();
        decoder.read_to_end(&mut decompressed).unwrap();
        assert_eq!(decompressed, plain_bytes);
    }

    #[test]
    fn write_nrrd_writes_a_parseable_text_header_with_no_axis_flip() {
        let path = temp_path("axis-aligned.nrrd");
        let samples = vec![7.0f32; 2 * 3 * 4];
        write_nrrd(&samples, (2, 3, 4), &axis_aligned_geometry(), &path).unwrap();

        let bytes = fs::read(&path).unwrap();
        fs::remove_file(&path).ok();

        let header_end = bytes.windows(2).position(|w| w == b"\n\n").map(|i| i + 2).unwrap();
        let header_text = std::str::from_utf8(&bytes[..header_end]).unwrap();
        assert!(header_text.starts_with("NRRD0004\n"));
        assert!(header_text.contains("type: float\n"));
        assert!(header_text.contains("sizes: 2 3 4\n"));
        assert!(header_text.contains("space: left-posterior-superior\n"));
        assert!(header_text.contains("space directions: (0.500000000,0.000000000,0.000000000) (0.000000000,0.600000000,0.000000000) (0.000000000,0.000000000,2.000000000)\n"));
        assert!(header_text.contains("space origin: (10.000000000,-20.000000000,30.000000000)\n"));
        assert!(header_text.contains("encoding: raw\n"));

        let payload = &bytes[header_end..];
        assert_eq!(payload.len(), samples.len() * 4);
        let first_value = f32::from_le_bytes(payload[0..4].try_into().unwrap());
        assert_eq!(first_value, 7.0);
    }

    #[test]
    fn write_nrrd_rejects_zero_dimensions() {
        let path = temp_path("zero-dim.nrrd");
        let error = write_nrrd(&[], (0, 3, 4), &axis_aligned_geometry(), &path).unwrap_err();
        assert!(matches!(error, VolumeExportError::InvalidDimensions));
    }
}
