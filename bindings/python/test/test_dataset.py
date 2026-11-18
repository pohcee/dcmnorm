"""Tests of the typed dataset API (#6): read, accessors, sequences, PN, multi-value, bulk data
references, DICOM JSON options, typed errors, mutation round trips, rendering and the generated
constants. Plain asserts, run as a script like smoke.py (which also runs it)."""
import io
import json
import os
import tempfile

import dcmnorm
from dcmnorm import Tag, VR, TransferSyntax, SOPClassUID

FIXTURES = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "..", "..", "test", "files")


def fixture(name):
    return os.path.join(FIXTURES, name)


def expect_error(cls, call, check=lambda e: True):
    try:
        call()
    except cls as error:
        assert check(error), error
        return error
    raise AssertionError("expected %s" % cls.__name__)


class ChunkedReader(io.RawIOBase):
    """A forward-only binary stream that returns short reads and counts what was read."""

    def __init__(self, data):
        self._data, self.position = data, 0

    def readable(self):
        return True

    def read(self, n=-1):
        n = len(self._data) - self.position if n < 0 else min(n, 700)
        chunk = self._data[self.position : self.position + n]
        self.position += len(chunk)
        return chunk


def test_constants():
    assert Tag.StudyInstanceUID == 0x0020000D
    assert Tag.PixelData == 0x7FE00010
    assert VR.PN == "PN" and VR.US.kind == "int"
    jpeg = TransferSyntax.from_uid("1.2.840.10008.1.2.4.50\0")
    assert jpeg is TransferSyntax.JPEGBaseline8Bit and jpeg.lossy and jpeg.encapsulated and jpeg.compressed
    assert not TransferSyntax.ImplicitVRLittleEndian.explicit_vr
    assert TransferSyntax.ExplicitVRBigEndian.big_endian and TransferSyntax.ExplicitVRBigEndian.retired
    assert TransferSyntax.from_uid("1.2.3") is None
    assert SOPClassUID.CTImageStorage == "1.2.840.10008.5.1.4.1.1.2"
    assert SOPClassUID.CTImageStorage.description == "CT Image Storage"
    assert dcmnorm.keyword_of(Tag.PatientName) == "PatientName"
    assert dcmnorm.vr_of(Tag.PatientName) == "PN"
    assert dcmnorm.tag_for_keyword("StudyInstanceUID") == Tag.StudyInstanceUID
    assert dcmnorm.tag_for_keyword("NoSuchKeyword") is None


def test_accessors():
    ds = dcmnorm.read(fixture("mr_small.dcm"), until_pixel_data=True)
    assert ds.transfer_syntax_uid == "1.2.840.10008.1.2.1"
    assert ds.transfer_syntax is TransferSyntax.ExplicitVRLittleEndian
    assert ds.get_int(Tag.Rows) == 64
    assert ds.get_string(Tag.Columns) == "64"
    assert ds.vr(Tag.PatientName) == "PN"
    assert ds.get_string(Tag.PatientName) == "CompressedSamples^MR1"
    assert ds.get_string(0x00091001, "none") == "none"
    assert ds.get_strings(0x00091001) is None
    assert ds.get_strings(Tag.ImageType) == ["DERIVED", "SECONDARY", "OTHER"]
    assert ds.get_string(Tag.ImageType, index=1) == "SECONDARY"
    assert ds.get_floats(Tag.PixelSpacing) == [0.3125, 0.3125]
    assert ds.number_of_frames == 1 and ds.pixel_aspect_ratio == 1.0
    ref = ds.get_bulk_data_ref(Tag.PixelData)
    assert (ref.offset, ref.length, ref.vr) == (1500, 8192, "OW")
    assert ds.tags()[-1] == Tag.PixelData
    assert ds.file_meta.get_string(Tag.TransferSyntaxUID) == ds.transfer_syntax_uid
    assert Tag.Rows in ds and ds[Tag.Rows].type == "ints"
    loaded = dcmnorm.read(fixture("mr_small.dcm"), load_bulk_data=True)
    with open(fixture("mr_small.dcm"), "rb") as f:
        raw = f.read()
    assert loaded.get_bytes(Tag.PixelData) == raw[1500 : 1500 + 8192]

    rtdose = dcmnorm.read(fixture("rtdose.dcm"), until_pixel_data=True)
    assert rtdose.number_of_frames == 15
    assert rtdose.get_string(Tag.FrameIncrementPointer) == "3004000C"
    assert len(rtdose.get_floats(Tag.GridFrameOffsetVector)) == 15


def test_sequences_and_names():
    sr = dcmnorm.read(fixture("sr.dcm"))
    content = sr.get_sequence(Tag.ContentSequence)
    assert content and isinstance(content[0], dcmnorm.DicomDataset)
    assert Tag.RelationshipType in content[0]
    assert sr.get_nested_dataset(Tag.ContentSequence) == content[0]
    name = dcmnorm.PersonName("Yamada^Tarou=山田^太郎=やまだ^たろう")
    assert name.family_name == "Yamada" and name.ideographic.given_name == "太郎"
    assert name.phonetic.family_name == "やまだ"
    sc = dcmnorm.read(fixture("sc_jpeg_baseline.dcm"), until_pixel_data=True)
    assert sc.get_person_name(Tag.PatientName).given_name == "G"


def test_sources():
    with open(fixture("us2.dcm"), "rb") as f:
        data = f.read()
    from_file = dcmnorm.read(fixture("us2.dcm"), until_pixel_data=True)
    from_bytes = dcmnorm.read(data, until_pixel_data=True)
    stream = ChunkedReader(data)
    from_stream = dcmnorm.read(stream, until_pixel_data=True)
    assert from_file == from_bytes == from_stream
    assert from_file.number_of_frames == 227
    offset = from_stream.get_bulk_data_ref(Tag.PixelData).offset
    assert stream.position <= offset + 64 * 1024, "untilPixelData reads no further than the PixelData header"
    assert dcmnorm.read(memoryview(data), until_pixel_data=True) == from_file

    rle = dcmnorm.read(fixture("mr_rle.dcm"), until_pixel_data=True)
    assert rle.get_bulk_data_ref(Tag.PixelData).length == -1
    loaded = dcmnorm.read(fixture("mr_rle.dcm"), load_bulk_data=True)
    assert loaded[Tag.PixelData].type == "fragments"


def test_json():
    path = fixture("mr_small.dcm")
    doc = json.loads(dcmnorm.to_dicom_json(path))
    assert doc["7FE00010"]["BulkDataURI"] == "?offset=1500&length=8192"
    assert doc["00100010"]["Value"][0]["Alphabetic"] == "CompressedSamples^MR1"
    assert not any(k.startswith("0002") for k in doc)
    filtered = json.loads(
        dcmnorm.to_dicom_json(
            path,
            include_tags=[Tag.PatientName, Tag.Rows, Tag.PixelData],
            exclude_tags=[Tag.Rows],
            include_file_meta=True,
            bulk_data_uri_builder=lambda ref: "https://pacs.example/bulk/%x/%d/%d/%s" % (ref.tag, ref.offset, ref.length, ref.vr),
        )
    )
    assert [k for k in filtered if not k.startswith("0002")] == ["00100010", "7FE00010"]
    assert "00020010" in filtered
    assert filtered["7FE00010"]["BulkDataURI"] == "https://pacs.example/bulk/7fe00010/1500/8192/OW"
    with open(path, "rb") as f:
        data = f.read()
    assert json.loads(dcmnorm.to_dicom_json(data)) == doc
    assert json.loads(dcmnorm.to_dicom_json(ChunkedReader(data))) == doc
    assert "InlineBinary" in json.loads(dcmnorm.to_dicom_json(path, bulk_data="inline"))["7FE00010"]
    edited = dcmnorm.read(path, until_pixel_data=True).to_builder().set(Tag.PatientName, VR.PN, "Doe^Jane").build()
    from_dataset = json.loads(dcmnorm.to_dicom_json(edited))
    assert from_dataset["00100010"]["Value"][0]["Alphabetic"] == "Doe^Jane"
    assert from_dataset["7FE00010"]["BulkDataURI"] == "?offset=1500&length=8192"

    def failing(ref):
        raise RuntimeError("boom")

    expect_error(RuntimeError, lambda: dcmnorm.to_dicom_json(path, bulk_data_uri_builder=failing))


def test_mutation_round_trip():
    path = fixture("mr_small.dcm")
    ds = dcmnorm.read(path)
    raw = dcmnorm.read(path, load_bulk_data=True).get_bytes(Tag.PixelData)
    changed = (
        ds.to_builder()
        .set(Tag.PatientName, VR.PN, "Doe^Jane")
        .set(Tag.PatientID, ["ID-42"])
        .set_int(Tag.SeriesNumber, VR.IS, 7)
        .set_float(Tag.SliceThickness, VR.DS, 2.5)
        .set(Tag.ImageType, VR.CS, "ORIGINAL", "PRIMARY")
        .remove(Tag.StudyDescription)
        .set_sequence(
            Tag.ReferencedStudySequence,
            [dcmnorm.DicomDataset.empty().to_builder().set(Tag.ReferencedSOPInstanceUID, VR.UI, "1.2.3.4").build()],
        )
        .edit_sequence_item(Tag.ReferencedStudySequence, 0, lambda item: item.set(Tag.ReferencedSOPInstanceUID, VR.UI, "1.2.3.5"))
        .build()
    )
    assert ds.get_string(Tag.PatientName) == "CompressedSamples^MR1", "the source is unchanged"
    with tempfile.TemporaryDirectory() as tmp:
        out = os.path.join(tmp, "out.dcm")
        dcmnorm.write(changed, out)
        back = dcmnorm.read(out, load_bulk_data=True)
    assert back.get_string(Tag.PatientName) == "Doe^Jane"
    assert back.vr(Tag.PatientID) == "LO"
    assert back.get_int(Tag.SeriesNumber) == 7
    assert back.get_float(Tag.SliceThickness) == 2.5
    assert back.get_strings(Tag.ImageType) == ["ORIGINAL", "PRIMARY"]
    assert Tag.StudyDescription not in back
    assert back.get_nested_dataset(Tag.ReferencedStudySequence).get_string(Tag.ReferencedSOPInstanceUID) == "1.2.3.5"
    assert back.get_bytes(Tag.PixelData) == raw, "pixel data passed through from the source"
    assert back.file_meta.get_string(Tag.MediaStorageSOPInstanceUID) == ds.get_string(Tag.SOPInstanceUID)

    implicit = dcmnorm.read(dcmnorm.write(changed, transfer_syntax=TransferSyntax.ImplicitVRLittleEndian), load_bulk_data=True)
    assert implicit.transfer_syntax_uid == "1.2.840.10008.1.2" and implicit.get_bytes(Tag.PixelData) == raw
    buffer = io.BytesIO()
    dcmnorm.write(changed, buffer)
    assert dcmnorm.read(buffer.getvalue()).get_string(Tag.PatientName) == "Doe^Jane"

    with open(path, "rb") as f:
        streamed = dcmnorm.read(f, until_pixel_data=True)
    error = expect_error(dcmnorm.DcmnormError, lambda: dcmnorm.write(streamed))
    assert error.kind == "InvalidArgument"
    assert dcmnorm.read(dcmnorm.write(streamed, bulk_data_source=path), load_bulk_data=True).get_bytes(Tag.PixelData) == raw
    expect_error(
        dcmnorm.DcmnormError,
        lambda: dcmnorm.write(ds.to_builder().set_int(Tag.Rows, VR.US, 70000).build()),
        lambda e: e.kind == "InvalidArgument",
    )
    expect_error(ValueError, lambda: ds.to_builder().set(Tag.TransferSyntaxUID, VR.UI, "1.2"))


def test_rendering():
    gray = dcmnorm.render_pixels(fixture("mr_small.dcm"))
    assert (gray.width, gray.height, gray.channels, len(gray.data)) == (64, 64, 1, 64 * 64)
    assert gray.shape == (64, 64) and gray.memoryview().shape == (64, 64)
    rgb = dcmnorm.render_pixels(fixture("sc_jpeg_baseline.dcm"), 0, output_width=50)
    assert (rgb.width, rgb.height, rgb.channels, len(rgb.data)) == (50, 50, 3, 50 * 50 * 3)
    assert rgb.memoryview()[10, 20, 1] == rgb.data[(10 * 50 + 20) * 3 + 1]
    windowed = dcmnorm.render_pixels(fixture("mr_small.dcm"), window_center=100, window_width=50)
    assert windowed.data != gray.data
    overlay = dcmnorm.render_pixels(fixture("overlay.dcm"))
    assert overlay.overlays and overlay.selected_overlay_index == 0
    with open(fixture("us2.dcm"), "rb") as f:
        last = dcmnorm.render_pixels(f, 226, output_width=90)
    assert last.width == 90
    try:
        import numpy  # optional
    except ImportError:
        numpy = None
    if numpy is not None:
        array = rgb.to_numpy()
        assert array.shape == (50, 50, 3) and array.dtype == numpy.uint8


def test_transfer_syntaxes_and_transcode():
    support = {ts["uid"]: ts for ts in dcmnorm.transfer_syntax_support()}
    assert support[TransferSyntax.ExplicitVRLittleEndian]["can_transcode_to"]
    assert support[TransferSyntax.RLELossless]["can_decode"]
    data = dcmnorm.transcode_to(fixture("mr_rle.dcm"), TransferSyntax.ExplicitVRLittleEndian)
    assert dcmnorm.read(data).transfer_syntax_uid == TransferSyntax.ExplicitVRLittleEndian


def test_errors():
    error = expect_error(dcmnorm.InvalidDicomError, lambda: dcmnorm.read(fixture("notdicom.txt")))
    assert error.kind == "InvalidDicom" and isinstance(error, dcmnorm.DcmnormError)
    assert expect_error(dcmnorm.DcmnormError, lambda: dcmnorm.read(fixture("missing.dcm"))).kind == "Io"
    error = expect_error(
        dcmnorm.UnsupportedTransferSyntaxError, lambda: dcmnorm.transcode_to(fixture("mr_small.dcm"), "1.2.3.4.5")
    )
    assert error.target_transfer_syntax_uid == "1.2.3.4.5"
    assert error.source_transfer_syntax_uid == "1.2.840.10008.1.2.1"
    error = expect_error(dcmnorm.RenderError, lambda: dcmnorm.render_pixels(fixture("mr_small.dcm"), 3))
    assert error.frame_index == 3
    expect_error(TypeError, lambda: dcmnorm.read(12345))


def test_readme_sample():
    """The root README's Python sample, as written, on a copy of a fixture named scan.dcm."""
    import shutil

    cwd = os.getcwd()
    with tempfile.TemporaryDirectory() as tmp:
        shutil.copy(fixture("mr_small.dcm"), os.path.join(tmp, "scan.dcm"))
        os.chdir(tmp)
        try:
            ds = dcmnorm.read("scan.dcm", until_pixel_data=True)
            print(ds.get_string(Tag.StudyInstanceUID), ds.number_of_frames)
            frame = dcmnorm.render_pixels("scan.dcm", 0)
            dcmnorm.transcode_to("scan.dcm", TransferSyntax.ExplicitVRLittleEndian, "out.dcm")
            assert frame.shape == (64, 64) and os.path.getsize("out.dcm") > 8192
        finally:
            os.chdir(cwd)


def main():
    for name, test in sorted(globals().items()):
        if name.startswith("test_") and callable(test):
            test()
    print("dataset tests passed")


if __name__ == "__main__":
    main()
