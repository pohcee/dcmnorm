"""Regenerates the synthetic fixtures in this directory (no patient data).

Each fixture isolates one GitHub issue of pohcee/dcmnorm and is read by the tests in
src/dicom_io/tests.rs (`issue_*`). Run from anywhere with pydicom 2.4+, numpy and imagecodecs:

    python3 test/files/issues/make_fixtures.py

The reproductions in the issues used pydicom 3; this script sticks to the API both support.
"""

import struct
from pathlib import Path

import imagecodecs
import numpy as np
import pydicom
from pydicom.dataset import Dataset, FileDataset, FileMetaDataset
from pydicom.encaps import encapsulate
from pydicom.uid import (
    ExplicitVRLittleEndian,
    ImplicitVRLittleEndian,
    JPEG2000Lossless,
    JPEGBaseline8Bit,
    SecondaryCaptureImageStorage,
)

HERE = Path(__file__).resolve().parent


def sample(transfer_syntax=ExplicitVRLittleEndian):
    """The issues' common 2x2 16-bit MONOCHROME2 secondary capture."""
    meta = FileMetaDataset()
    meta.TransferSyntaxUID = transfer_syntax
    meta.MediaStorageSOPClassUID = SecondaryCaptureImageStorage
    meta.MediaStorageSOPInstanceUID = "2.25.91001"
    ds = FileDataset(None, {}, file_meta=meta, preamble=b"\0" * 128)
    ds.is_little_endian = True
    ds.is_implicit_VR = transfer_syntax == ImplicitVRLittleEndian
    ds.SOPClassUID = meta.MediaStorageSOPClassUID
    ds.SOPInstanceUID = meta.MediaStorageSOPInstanceUID
    ds.StudyInstanceUID = "2.25.91002"
    ds.SeriesInstanceUID = "2.25.91003"
    ds.Modality = "OT"
    ds.ConversionType = "WSD"
    ds.PatientName = "SYNTHETIC^REPRO"
    ds.PatientID = "SYNTHETIC"
    ds.Rows = ds.Columns = 2
    ds.SamplesPerPixel = 1
    ds.PhotometricInterpretation = "MONOCHROME2"
    ds.BitsAllocated = ds.BitsStored = 16
    ds.HighBit = 15
    ds.PixelRepresentation = 0
    ds.PixelData = struct.pack("<4H", 0, 1, 2, 3)
    return ds


def save(ds, name):
    ds.save_as(HERE / name, write_like_original=False)
    return HERE / name


def encapsulated(ds, frames):
    ds.PixelData = encapsulate(frames)
    ds["PixelData"].VR = "OB"
    ds["PixelData"].is_undefined_length = True


# 23: Frame Increment Pointer, AT values (0018,1063) and (0018,1065)
ds = sample()
ds.add_new(0x00280009, "AT", [0x00181063, 0x00181065])
save(ds, "at_values.dcm")

# 24: ISO 2022 code extensions, empty value 1
for name, charset, text in [
    ("charset_iso2022_ir87.dcm", ["", "ISO 2022 IR 87"], "試験^文字"),
    ("charset_iso2022_ir149.dcm", ["", "ISO 2022 IR 149"], "테스트^문자"),
]:
    ds = sample()
    ds.SpecificCharacterSet = charset
    ds.PatientName = text
    path = save(ds, name)
    assert str(pydicom.dcmread(path).PatientName) == text

# 25: private creator in an odd group matching the repeating (50xx,0010), Implicit VR
ds = sample(ImplicitVRLittleEndian)
ds.add_new(0x50F10010, "LO", "SYNTHETIC")
ds.add_new(0x50F11010, "CS", "TEST")
save(ds, "private_creator_50f1_implicit.dcm")

# 26: undefined-length private UN in Explicit VR whose item is Implicit VR (PS3.5 6.2.2)
ds = sample()
ds.add_new(0x00110010, "LO", "SYNTHETIC")
path = save(ds, "un_undefined_length_sequence.dcm")
inner = struct.pack("<HHI", 0x0008, 0x103E, 4) + b"TEST"
item = struct.pack("<HHI", 0xFFFE, 0xE000, len(inner)) + inner
value = item + struct.pack("<HHI", 0xFFFE, 0xE0DD, 0)
element = struct.pack("<HH2sHI", 0x0011, 0x1010, b"UN", 0, 0xFFFFFFFF) + value
raw = path.read_bytes()
offset = raw.index(b"\x20\x00\x0d\x00UI")  # before (0020,000D)
path.write_bytes(raw[:offset] + element + raw[offset:])
sequence = pydicom.dcmread(path)[0x00111010].value
assert len(sequence) == 1 and sequence[0].SeriesDescription == "TEST"

# 27: signed pixels rescaled to 0..65535, VOI LUT Descriptor explicitly US [4, 40000, 16]
ds = sample()
ds.PixelRepresentation = 1
ds.RescaleSlope = "1"
ds.RescaleIntercept = "32768"
ds.RescaleType = "US"
item = Dataset()
item.add_new(0x00283002, "US", [4, 40000, 16])
item.add_new(0x00283006, "OW", struct.pack("<4H", 0, 1, 2, 3))
ds.VOILUTSequence = [item]
path = save(ds, "voi_lut_unsigned_after_rescale.dcm")
assert pydicom.dcmread(path).VOILUTSequence[0][0x00283002].VR == "US"

# 28: zero-length SQ and an empty component of a multi-valued LO
ds = sample()
ds.add_new(0x00081111, "SQ", [])
ds.add_new(0x00110010, "LO", "SYNTHETIC")
ds.add_new(0x00111010, "LO", ["one", "", "three"])
save(ds, "empty_values.dcm")

# 29: JPEG 2000 Lossless with a 10-bit codestream under BitsStored 8 / HighBit 7
samples = np.array([[0, 1], [256, 644]], dtype=np.uint16)
frame = imagecodecs.jpeg2k_encode(samples, bitspersample=10, reversible=True, codecformat="j2k", resolutions=1)
assert np.array_equal(imagecodecs.jpeg2k_decode(frame), samples)
ds = sample(JPEG2000Lossless)
ds.BitsStored = 8
ds.HighBit = 7
encapsulated(ds, [frame])
save(ds, "j2k_codestream_precision_above_bits_stored.dcm")


# 30, 31: native 8-bit RGB, 1 row x 2 columns: planar single frame, interleaved two frames
def rgb(frames, planar):
    expected = np.stack(
        [np.array([1, 2, 255, 4, 5, 254], dtype="u1").reshape(1, 2, 3) - i for i in range(frames)]
    )
    ds = sample()
    ds.Rows, ds.Columns, ds.NumberOfFrames = 1, 2, str(frames)
    ds.SamplesPerPixel, ds.PhotometricInterpretation, ds.PlanarConfiguration = 3, "RGB", planar
    ds.BitsAllocated, ds.BitsStored, ds.HighBit, ds.PixelRepresentation = 8, 8, 7, 0
    ds.PixelData = (expected.transpose(0, 3, 1, 2) if planar else expected).tobytes()
    ds["PixelData"].VR = "OB"
    return ds


save(rgb(1, 1), "rgb_planar_1x2.dcm")
save(rgb(2, 0), "rgb_interleaved_1x2_2frames.dcm")


# 32: JPEG Baseline RGB, constant [200, 50, 10], component identifiers 0,1,2, with and without
# an Adobe APP14 transform-0 marker; the RGB coefficients are the same in both
def numeric_rgb(stream, keep_adobe):
    result = bytearray(stream[:2])
    offset = 2
    while offset < len(stream):
        assert stream[offset] == 0xFF
        marker = stream[offset + 1]
        length = int.from_bytes(stream[offset + 2 : offset + 4], "big")
        segment = bytearray(stream[offset : offset + 2 + length])
        if marker == 0xC0:  # SOF0
            assert segment[9] == 3
            for channel in range(3):
                segment[10 + channel * 3] = channel
        if marker == 0xDA:  # SOS
            assert segment[4] == 3
            for channel in range(3):
                segment[5 + channel * 2] = channel
            result.extend(segment)
            result.extend(stream[offset + 2 + length :])
            return bytes(result)
        if not 0xE0 <= marker <= 0xEF or (keep_adobe and marker == 0xEE):
            result.extend(segment)
        offset += 2 + length
    raise AssertionError("missing JPEG SOS")


pixels = np.full((8, 8, 3), [200, 50, 10], dtype=np.uint8)
jpeg = imagecodecs.jpeg_encode(pixels, level=100, colorspace="RGB", outcolorspace="RGB")
for name, keep_adobe in [("jpeg_rgb_adobe_transform0.dcm", True), ("jpeg_rgb_no_color_marker.dcm", False)]:
    stream = numeric_rgb(jpeg, keep_adobe)
    assert (b"\xff\xee" in stream) == keep_adobe
    np.testing.assert_array_equal(
        imagecodecs.jpeg_decode(stream, colorspace="RGB", outcolorspace="RGB"), pixels
    )
    ds = sample(JPEGBaseline8Bit)
    ds.Rows = ds.Columns = 8
    ds.SamplesPerPixel, ds.PhotometricInterpretation, ds.PlanarConfiguration = 3, "RGB", 0
    ds.BitsAllocated, ds.BitsStored, ds.HighBit, ds.PixelRepresentation = 8, 8, 7, 0
    ds.LossyImageCompression = "01"
    encapsulated(ds, [stream])
    save(ds, name)

# 3: JPEG XL Lossless encoding: 12-bit MONOCHROME2 in 16 allocated bits, 3 rows x 4 columns,
# 2 frames, with the extreme values; and 8-bit planar RGB, 2x2
ds = sample()
ds.Rows, ds.Columns, ds.NumberOfFrames = 3, 4, "2"
ds.BitsAllocated, ds.BitsStored, ds.HighBit = 16, 12, 11
mono = np.array([[0, 1, 2048, 4095], [7, 300, 1000, 4094], [4095, 0, 123, 2222]], dtype="<u2")
ds.PixelData = np.stack([mono, 4095 - mono]).tobytes()
ds["PixelData"].VR = "OW"
save(ds, "mono12_3x4_2frames.dcm")

ds = rgb(1, 1)
ds.Rows, ds.Columns = 2, 2
pixels = np.array([[[255, 0, 0], [0, 255, 0]], [[0, 0, 255], [17, 128, 250]]], dtype="u1")
ds.PixelData = pixels.transpose(2, 0, 1).tobytes()
save(ds, "rgb_planar_2x2.dcm")


# 58: JPEG 2000 Lossless stored as a JP2 file (not a raw codestream) whose colr box carries an
# ICC profile with marker-like bytes: an FF51 pair with 0x97 40 bytes on (read by a search for
# the first FF51 as a SIZ with 24-bit samples), and an FF52 pair with a 0 "MCT" byte. The
# codestream is 8-bit RGB with the reversible color transform (YBR_RCT), as in vendor-made
# fused RGB secondary captures.
def jp2_box(kind, payload):
    return struct.pack(">I", 8 + len(payload)) + kind + payload


pixels = np.array([[[255, 0, 0], [0, 255, 0]], [[0, 0, 255], [17, 128, 250]]], dtype="u1")
codestream = imagecodecs.jpeg2k_encode(pixels, level=0, reversible=True, mct=True, codecformat="j2k", resolutions=1)
assert codestream[:4] == b"\xff\x4f\xff\x51"
assert np.array_equal(imagecodecs.jpeg2k_decode(codestream), pixels)
icc = b"\x11" * 128 + b"\xff\x51" + b"\x22" * 38 + b"\x97" + b"\xff\x52\x00\x0c\x00\x00\x00\x01\x00" + b"\x33" * 64
jp2 = (
    jp2_box(b"jP  ", b"\r\n\x87\n")
    + jp2_box(b"ftyp", b"jp2 \0\0\0\0jp2 ")
    + jp2_box(b"jp2h", jp2_box(b"ihdr", struct.pack(">IIHBBBB", 2, 2, 3, 7, 7, 0, 0)) + jp2_box(b"colr", b"\x02\0\0" + icc))
    + jp2_box(b"jp2c", codestream)
)
first = jp2.find(b"\xff\x51")
assert first < jp2.find(b"jp2c") and (jp2[first + 40] & 0x7F) + 1 == 24
ds = sample(JPEG2000Lossless)
ds.SamplesPerPixel, ds.PhotometricInterpretation, ds.PlanarConfiguration = 3, "YBR_RCT", 0
ds.BitsAllocated, ds.BitsStored, ds.HighBit, ds.PixelRepresentation = 8, 8, 7, 0
encapsulated(ds, [jp2])
save(ds, "jp2_ybr_rct_icc_marker_bytes.dcm")
