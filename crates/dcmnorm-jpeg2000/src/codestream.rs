//! Structural reads of a JPEG 2000 fragment's main header, without decoding.
//!
//! A DICOM JPEG 2000 fragment is normally a raw codestream (ITU-T T.800 Annex A: `FF4F` SOC,
//! then the `FF51` SIZ marker segment), but some encoders store a whole JP2 file (T.800
//! Annex I: a signature box, `ftyp`, a `jp2h` header box - whose `colr` box can carry an ICC
//! profile - then the `jp2c` box holding the codestream). [`locate`] finds the codestream in
//! either form by walking the structure, never by searching for marker bytes: a byte search for
//! `FF51` can match inside an ICC profile (16-bit tone curves contain `FF xx` pairs) and read
//! nonsense as the component count or precision.

/// The JP2 signature box: length 12, type `jP  `, content `0D0A870A`.
pub const JP2_SIGNATURE: [u8; 12] = [
    0x00, 0x00, 0x00, 0x0C, 0x6A, 0x50, 0x20, 0x20, 0x0D, 0x0A, 0x87, 0x0A,
];
const SOC: [u8; 2] = [0xFF, 0x4F];
const SIZ: [u8; 2] = [0xFF, 0x51];
const COD: u8 = 0x52;
const SOT: u8 = 0x90;
const SOD: u8 = 0x93;

/// How the codestream is stored in the fragment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Container {
    /// A raw codestream (`FF4F FF51 ...`).
    Codestream,
    /// A JP2 file; the codestream is the payload of its `jp2c` box.
    Jp2,
}

/// The codestream inside a fragment, and how it was stored.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Located<'a> {
    pub container: Container,
    /// Starts with SOC. For a fragment that is only the first part of a frame, this is the
    /// part of the codestream that fragment holds.
    pub codestream: &'a [u8],
}

/// Finds the codestream in a fragment: the fragment itself when it starts with SOC, the `jp2c`
/// box's payload when it is a JP2 file. `None` when it is neither, or the JP2 boxes are cut off
/// before `jp2c` or its payload does not start with SOC.
pub fn locate(fragment: &[u8]) -> Option<Located<'_>> {
    if fragment.starts_with(&SOC) {
        return Some(Located {
            container: Container::Codestream,
            codestream: fragment,
        });
    }
    if !fragment.starts_with(&JP2_SIGNATURE) {
        return None;
    }
    let mut pos = 0usize;
    while pos + 8 <= fragment.len() {
        let lbox = u32::from_be_bytes(fragment[pos..pos + 4].try_into().ok()?) as u64;
        let kind = &fragment[pos + 4..pos + 8];
        let (header, length) = match lbox {
            // the box runs to the end of the file
            0 => (8u64, (fragment.len() - pos) as u64),
            // the 64-bit XLBox follows the type
            1 => {
                let xl = fragment.get(pos + 8..pos + 16)?;
                (16, u64::from_be_bytes(xl.try_into().ok()?))
            }
            2..=7 => return None,
            n => (8, n),
        };
        if length < header {
            return None;
        }
        let payload_start = pos + header as usize;
        if kind == b"jp2c" {
            // a fragment may hold only the start of the box
            let end = (pos as u64)
                .saturating_add(length)
                .min(fragment.len() as u64) as usize;
            let codestream = fragment.get(payload_start..end)?;
            return codestream.starts_with(&SOC).then_some(Located {
                container: Container::Jp2,
                codestream,
            });
        }
        pos = usize::try_from((pos as u64).checked_add(length)?).ok()?;
    }
    None
}

/// The image and component parameters of a codestream's SIZ marker segment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Siz {
    pub width: u32,
    pub height: u32,
    /// One entry per component (Csiz of them): its bit depth (Ssiz low 7 bits + 1) ...
    pub precisions: Vec<u8>,
    /// ... and whether its samples are signed (Ssiz bit 7).
    pub signed: Vec<bool>,
}

impl Siz {
    pub fn components(&self) -> u16 {
        self.precisions.len() as u16
    }

    /// The widest component's bit depth.
    pub fn max_precision(&self) -> u8 {
        self.precisions.iter().copied().max().unwrap_or(0)
    }
}

/// Reads the SIZ marker segment, which T.800 A.5.1 places right after SOC. Layout: FF51, Lsiz
/// (2), Rsiz (2), Xsiz, Ysiz, XOsiz, YOsiz, XTsiz, YTsiz, XTOsiz, YTOsiz (4 each), Csiz (2), then
/// Ssiz/XRsiz/YRsiz (1 each) per component; Lsiz = 38 + 3 * Csiz.
pub fn read_siz(codestream: &[u8]) -> Option<Siz> {
    if !codestream.starts_with(&SOC) || codestream.get(2..4)? != SIZ {
        return None;
    }
    let segment = &codestream[4..];
    let lsiz = usize::from(u16::from_be_bytes(segment.get(0..2)?.try_into().ok()?));
    let u32_at = |offset: usize| -> Option<u32> {
        Some(u32::from_be_bytes(
            segment.get(offset..offset + 4)?.try_into().ok()?,
        ))
    };
    let (xsiz, ysiz, xosiz, yosiz) = (u32_at(4)?, u32_at(8)?, u32_at(12)?, u32_at(16)?);
    let csiz = usize::from(u16::from_be_bytes(segment.get(36..38)?.try_into().ok()?));
    if csiz == 0 || lsiz != 38 + 3 * csiz || xsiz <= xosiz || ysiz <= yosiz {
        return None;
    }
    let components = segment.get(38..38 + 3 * csiz)?;
    let ssiz = components.chunks_exact(3).map(|component| component[0]);
    Some(Siz {
        width: xsiz - xosiz,
        height: ysiz - yosiz,
        precisions: ssiz.clone().map(|s| (s & 0x7F) + 1).collect(),
        signed: ssiz.map(|s| s & 0x80 != 0).collect(),
    })
}

/// The Multiple Component Transformation flag of the main header's COD marker segment (T.800
/// A.6.1: FF52, Lcod (2), Scod (1), progression order (1), layers (2), MCT (1)). Walks the main
/// header's marker segments from SIZ to the first tile-part (SOT); `None` when there is no COD
/// there or the header is malformed.
pub fn uses_mct(codestream: &[u8]) -> Option<bool> {
    if !codestream.starts_with(&SOC) {
        return None;
    }
    let mut pos = 2usize;
    loop {
        let marker = codestream.get(pos..pos + 2)?;
        if marker[0] != 0xFF || marker[1] == SOT || marker[1] == SOD {
            return None;
        }
        let length = usize::from(u16::from_be_bytes(
            codestream.get(pos + 2..pos + 4)?.try_into().ok()?,
        ));
        if length < 2 {
            return None;
        }
        if marker[1] == COD {
            return codestream.get(pos + 8).map(|&mct| mct != 0);
        }
        pos += 2 + length;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// SOC + SIZ (64x32, the given Ssiz per component) + COD (MCT as given) + SOT.
    fn codestream(ssiz: &[u8], mct: bool) -> Vec<u8> {
        let mut out = vec![0xFF, 0x4F, 0xFF, 0x51];
        out.extend_from_slice(&((38 + 3 * ssiz.len()) as u16).to_be_bytes());
        out.extend_from_slice(&[0, 0]); // Rsiz
        for value in [64u32, 32, 0, 0, 64, 32, 0, 0] {
            out.extend_from_slice(&value.to_be_bytes());
        }
        out.extend_from_slice(&(ssiz.len() as u16).to_be_bytes());
        for &s in ssiz {
            out.extend_from_slice(&[s, 1, 1]);
        }
        out.extend_from_slice(&[
            0xFF,
            0x52,
            0x00,
            0x0C,
            0x00,
            0x00,
            0x00,
            0x01,
            u8::from(mct),
            5,
            4,
            4,
            0,
            1,
        ]);
        out.extend_from_slice(&[0xFF, 0x90, 0x00, 0x0A, 0, 0, 0, 0, 0, 0, 0, 1]);
        out
    }

    fn jp2_box(kind: &[u8; 4], payload: &[u8]) -> Vec<u8> {
        let mut out = ((8 + payload.len()) as u32).to_be_bytes().to_vec();
        out.extend_from_slice(kind);
        out.extend_from_slice(payload);
        out
    }

    /// A JP2 file around `codestream` whose colr box carries `icc` as its ICC profile.
    fn jp2(codestream: &[u8], icc: &[u8]) -> Vec<u8> {
        let mut out = JP2_SIGNATURE.to_vec();
        out.extend(jp2_box(b"ftyp", b"jp2 \0\0\0\0jp2 "));
        let mut header = jp2_box(b"ihdr", &[0, 0, 0, 32, 0, 0, 0, 64, 0, 3, 7, 7, 0, 0]);
        let mut colr = vec![2, 0, 0];
        colr.extend_from_slice(icc);
        header.extend(jp2_box(b"colr", &colr));
        out.extend(jp2_box(b"jp2h", &header));
        out.extend(jp2_box(b"jp2c", codestream));
        out
    }

    /// Bytes that look like the start of a SIZ segment to a byte search: FF51, and 40 bytes
    /// on 0x97, which a naive reader takes for a 24-bit Ssiz.
    fn decoy() -> Vec<u8> {
        let mut icc = vec![0x11u8; 64];
        icc.extend_from_slice(&[0xFF, 0x51]);
        icc.extend(std::iter::repeat(0x22).take(38));
        icc.push(0x97);
        icc.extend(std::iter::repeat(0x33).take(64));
        // and a decoy COD with MCT 0
        icc.extend_from_slice(&[0xFF, 0x52, 0, 12, 0, 0, 0, 1, 0]);
        icc
    }

    #[test]
    fn reads_a_raw_codestream() {
        let cs = codestream(&[7, 7, 7], true);
        let located = locate(&cs).unwrap();
        assert_eq!(located.container, Container::Codestream);
        let siz = read_siz(located.codestream).unwrap();
        assert_eq!(
            (siz.width, siz.height, siz.components(), siz.max_precision()),
            (64, 32, 3, 8)
        );
        assert_eq!(siz.signed, vec![false; 3]);
        assert_eq!(uses_mct(located.codestream), Some(true));
    }

    #[test]
    fn finds_the_codestream_of_a_jp2_past_an_icc_profile_with_marker_like_bytes() {
        let cs = codestream(&[7, 7, 7], true);
        let file = jp2(&cs, &decoy());
        let located = locate(&file).unwrap();
        assert_eq!(located.container, Container::Jp2);
        assert_eq!(located.codestream, &cs[..]);
        let siz = read_siz(located.codestream).unwrap();
        assert_eq!((siz.components(), siz.max_precision()), (3, 8));
        assert_eq!(uses_mct(located.codestream), Some(true));
    }

    #[test]
    fn per_component_precision_and_sign() {
        let siz = read_siz(&codestream(&[0x8B, 7, 15], false)).unwrap();
        assert_eq!(siz.precisions, vec![12, 8, 16]);
        assert_eq!(siz.signed, vec![true, false, false]);
        assert_eq!(siz.max_precision(), 16);
        assert_eq!(uses_mct(&codestream(&[7], false)), Some(false));
    }

    #[test]
    fn handles_xlbox_and_to_end_boxes() {
        let cs = codestream(&[7], false);
        let mut file = JP2_SIGNATURE.to_vec();
        // ftyp with a 64-bit length
        file.extend_from_slice(&1u32.to_be_bytes());
        file.extend_from_slice(b"ftyp");
        file.extend_from_slice(&(16u64 + 12).to_be_bytes());
        file.extend_from_slice(b"jp2 \0\0\0\0jp2 ");
        // jp2c running to the end of the file
        file.extend_from_slice(&0u32.to_be_bytes());
        file.extend_from_slice(b"jp2c");
        file.extend_from_slice(&cs);
        assert_eq!(locate(&file).unwrap().codestream, &cs[..]);
    }

    #[test]
    fn rejects_what_is_not_a_codestream() {
        assert_eq!(locate(&[0, 1, 2, 3]), None);
        // a JP2 cut off before its jp2c box
        let file = jp2(&codestream(&[7], false), &decoy());
        assert_eq!(locate(&file[..100]), None);
        // a jp2c box that does not hold a codestream
        let mut bad = JP2_SIGNATURE.to_vec();
        bad.extend(jp2_box(b"jp2c", &[0xFF, 0x51, 0, 0]));
        assert_eq!(locate(&bad), None);
        // SIZ must follow SOC: marker-like bytes later are not read
        let mut cs = vec![0xFF, 0x4F, 0xFF, 0x52, 0, 4, 0, 0];
        cs.extend(decoy());
        assert_eq!(read_siz(&cs), None);
        // an Lsiz that disagrees with Csiz
        let mut cs = codestream(&[7, 7, 7], false);
        cs[5] = 41;
        assert_eq!(read_siz(&cs), None);
    }
}
