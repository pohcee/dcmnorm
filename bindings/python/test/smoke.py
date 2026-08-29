import glob
import gzip
import json
import os
import re
import subprocess
import sys
import tempfile
import zipfile

import dcmnorm

FIXTURES = os.path.join(os.path.dirname(__file__), "..", "..", "..", "test", "files")
FIXTURE = os.path.join(FIXTURES, "us.dcm")
OVERLAY_FIXTURE = os.path.join(FIXTURES, "overlay.dcm")
OVERLAY_MULTI_FIXTURE = os.path.join(FIXTURES, "overlay_multi.dcm")
OVERLAY_EMBEDDED_FIXTURE = os.path.join(FIXTURES, "overlay_embedded.dcm")
CT_FIXTURE = os.path.join(FIXTURES, "ct.dcm")
WSI_FIXTURE = os.path.join(FIXTURES, "wsi.dcm")

# The wheel must run on the oldest glibc among its consumers' runtime images -
# python:3.12-slim-bookworm, GLIBC 2.36 - same baseline the Node bindings pin to (node:22-slim,
# also bookworm - see bindings/node/test/smoke.js's own copy of this check). Pinned to the
# "-bookworm" suffix specifically, not the bare "python:3.12-slim" floating tag, which tracks
# whatever Debian release happens to be current (it moved to trixie/glibc 2.41 after this was
# first written - see build-in-docker.sh's own comment on that). build-in-docker.sh builds inside
# that pinned image for exactly this reason, but nothing stops a plain host `maturin build` (e.g.
# during dev iteration) from silently producing a wheel linked against the *host's* (often newer)
# glibc instead - which then loads fine here but dlopen-fails only once actually deployed.
MAX_ALLOWED_GLIBC = "2.36"


def compare_versions(a, b):
    a_maj, a_min = (int(part) for part in a.split("."))
    b_maj, b_min = (int(part) for part in b.split("."))
    return (a_maj, a_min) > (b_maj, b_min)


def check_glibc_compatibility():
    # dist/*.whl is what build-in-docker.sh produces and what actually gets committed/shipped -
    # not whatever `maturin develop` last built into the active venv's site-packages, which may
    # be a host build. Nothing to check if no wheel has been built yet (e.g. fresh checkout
    # before the first `build-in-docker.sh` run).
    # Platform tag varies (manylinux_2_XX_x86_64 vs a plain linux_x86_64 fallback) depending on
    # which glibc symbol versions the compiled .so actually references - match any wheel here
    # rather than hardcoding one tag, since THAT tag is exactly what this check verifies.
    dist_dir = os.path.join(os.path.dirname(__file__), "..", "dist")
    wheels = glob.glob(os.path.join(dist_dir, "dcmnorm_python-*.whl"))
    if not wheels:
        return

    for wheel_path in wheels:
        with zipfile.ZipFile(wheel_path) as wheel:
            so_members = [name for name in wheel.namelist() if name.endswith(".so")]
            if not so_members:
                continue
            with tempfile.TemporaryDirectory() as tmp_dir:
                so_path = wheel.extract(so_members[0], tmp_dir)
                try:
                    output = subprocess.run(
                        ["objdump", "-T", so_path], capture_output=True, text=True, check=True
                    ).stdout
                except (OSError, subprocess.CalledProcessError) as error:
                    print(f"Skipping GLIBC compatibility check for {wheel_path}: objdump unavailable ({error})")
                    continue

                versions = re.findall(r"GLIBC_([0-9]+\.[0-9]+)", output)
                max_required = "0.0"
                for version in versions:
                    if compare_versions(version, max_required):
                        max_required = version
                if compare_versions(max_required, MAX_ALLOWED_GLIBC):
                    raise AssertionError(
                        f"{wheel_path} requires GLIBC_{max_required}, but the oldest consumer runtime "
                        f"(python:3.12-slim-bookworm) only has GLIBC_{MAX_ALLOWED_GLIBC} - it was likely built "
                        "directly on the host instead of via 'build-in-docker.sh', which links against "
                        "python:3.12-slim-bookworm's own glibc. Rebuild with 'build-in-docker.sh' before committing."
                    )


def check(condition, message="assertion failed"):
    if not condition:
        raise AssertionError(message)


def main():
    check_glibc_compatibility()

    check(dcmnorm.check_dicom(FIXTURE) is True, "check_dicom should be True for a real DICOM file")
    check(dcmnorm.check_dicom(__file__) is False, "check_dicom should be False for a non-DICOM file")

    tags = json.loads(dcmnorm.read_tags(FIXTURE, ["StudyInstanceUID", "SOPInstanceUID"]))
    check("0020000D" in tags, "expected StudyInstanceUID (0020000D) in filtered read_tags output")
    check("00080018" in tags, "expected SOPInstanceUID (00080018) in filtered read_tags output")
    check("00080060" not in tags, "Modality (00080060) was not requested and should be filtered out")

    full = json.loads(dcmnorm.read_json(FIXTURE))
    check(len(full.keys()) > len(tags.keys()), "full read_json should have more keys than the filtered read_tags")
    full_json_str = dcmnorm.read_json(FIXTURE)
    check(
        isinstance(full["PixelData"].get("BulkDataURI"), str) and "InlineBinary" not in full["PixelData"],
        "read_json's default bulk_data mode should be 'uri' (matching the CLI), not inline-embed PixelData",
    )
    check(
        len(full_json_str) < 5000,
        f"read_json output for a filtered non-bulk fixture should stay small (was {len(full_json_str)} bytes)",
    )

    inline = json.loads(dcmnorm.read_json(FIXTURE, bulk_data="inline"))
    check(
        isinstance(inline["PixelData"].get("InlineBinary"), str) and len(inline["PixelData"]["InlineBinary"]) > 1000,
        "read_json with bulk_data='inline' should base64-embed PixelData",
    )

    with tempfile.TemporaryDirectory(prefix="dcmnorm-python-smoke-") as tmp_dir:
        edited = os.path.join(tmp_dir, "edited.dcm")
        dcmnorm.edit_tags(FIXTURE, output_path=edited, set={"PatientName": "SMOKE^TEST"}, remove_private_tags=True)
        edited_tags = json.loads(dcmnorm.read_tags(edited, ["PatientName"]))
        check(edited_tags["00100010"] == "SMOKE^TEST", "edit_tags should have set PatientName")

        transcoded = os.path.join(tmp_dir, "transcoded.dcm")
        dcmnorm.transcode(FIXTURE, transcoded, "1.2.840.10008.1.2.1")
        check(dcmnorm.check_dicom(transcoded) is True, "transcoded output should still be valid DICOM")

        try:
            dcmnorm.read_tags(FIXTURE, ["NotARealKeyword"])
            raise AssertionError("read_tags with an invalid tag keyword should raise")
        except dcmnorm.DcmnormError as error:
            check("invalid --filter key" in str(error), f"unexpected error message: {error}")

        with_overlay = dcmnorm.render_frame(OVERLAY_FIXTURE, format="png")
        check(len(with_overlay.overlays) == 1, "overlay.dcm should report exactly one overlay plane")
        check(with_overlay.selected_overlay_index == 0, "the first overlay should render by default")

        without_overlay = dcmnorm.render_frame(OVERLAY_FIXTURE, format="png", show_overlays=False)
        check(without_overlay.selected_overlay_index is None, "show_overlays=False should render no overlay")
        check(with_overlay.data != without_overlay.data, "rendering with and without the overlay should differ")

        red_overlay = dcmnorm.render_frame(OVERLAY_FIXTURE, format="png", overlay_color="255,0,0")
        check(with_overlay.data != red_overlay.data, "a different overlay_color should change the rendered bytes")

        try:
            dcmnorm.render_frame(OVERLAY_FIXTURE, format="png", overlay_index=5)
            raise AssertionError("an out-of-range overlay_index should raise")
        except dcmnorm.DcmnormError as error:
            check("overlay index" in str(error), f"unexpected error message: {error}")

        multi_first = dcmnorm.render_frame(OVERLAY_MULTI_FIXTURE, format="png")
        check(len(multi_first.overlays) == 2, "overlay_multi.dcm should report two overlay planes")
        check(multi_first.selected_overlay_index == 0, "first overlay should be selected")
        multi_second = dcmnorm.render_frame(OVERLAY_MULTI_FIXTURE, format="png", overlay_index=1)
        check(multi_second.selected_overlay_index == 1, "second overlay should be selected")
        check(multi_first.data != multi_second.data, "selecting a different overlay index should change the bytes")

        embedded = dcmnorm.render_frame(OVERLAY_EMBEDDED_FIXTURE, format="png")
        check(len(embedded.overlays) == 1, "overlay_embedded.dcm should report one overlay plane")
        check(embedded.selected_overlay_index == 0, "embedded overlay should be selected")

        # wsi.dcm is JPEG Baseline (transfer syntax 1.2.840.10008.1.2.4.50) - the one fixture in
        # this suite that exercises the in-house dcmnorm-jpeg decoder (crates/dcmnorm-jpeg)
        # rather than an uncompressed or JPEG2000/openjpeg-sys path. Every other render_frame
        # call above uses us.dcm/overlay*.dcm (uncompressed) or ct.dcm (JPEG2000), so without
        # this the python binding's own test suite would never prove the JPEG decode path works
        # when called through this binding, not just through the Rust test suite directly.
        wsi_frame = dcmnorm.render_frame(WSI_FIXTURE, format="png")
        check(wsi_frame.width == 240, "wsi.dcm (JPEG Baseline) should decode to 240x240")
        check(wsi_frame.height == 240, "wsi.dcm (JPEG Baseline) should decode to 240x240")
        check(len(wsi_frame.data) > 0, "JPEG-decoded frame should produce non-empty PNG bytes")

        # --- MPR volume + GPU texture export -------------------------------------------------
        slice_count = 4
        slice_paths = []
        for index in range(slice_count):
            slice_path = os.path.join(tmp_dir, f"ct-slice-{index}.dcm")
            dcmnorm.edit_tags(
                CT_FIXTURE,
                output_path=slice_path,
                set={"ImagePositionPatient": f"-151.493508\\-36.6564417\\{1115.0 + index}"},
            )
            slice_paths.append(slice_path)

        volume = dcmnorm.build_volume(slice_paths)
        check(volume.rows == 512, "ct.dcm fixture should be 512 rows")
        check(volume.cols == 512, "ct.dcm fixture should be 512 cols")
        check(volume.num_slices == slice_count)

        volume_texture = volume.export_texture(compression="gzip", window_center=40, window_width=400)
        check(volume_texture.content_kind == "volume")
        check(volume_texture.sample_format == "int16")
        check(volume_texture.compression == "gzip")
        check(volume_texture.lossless is True)
        check(volume_texture.width == 512)
        check(volume_texture.height == 512)
        check(volume_texture.depth == slice_count)
        check(
            (volume_texture.native_width, volume_texture.native_height, volume_texture.native_depth) == (512, 512, slice_count)
        )
        check(volume_texture.downsampled is False)
        check(volume_texture.default_window_center == 40)
        check(volume_texture.default_window_width == 400)
        check(volume_texture.payload_bytes_raw == 512 * 512 * slice_count * 2)
        check(volume_texture.payload_bytes_stored == len(volume_texture.data))

        decompressed = gzip.decompress(volume_texture.data)
        check(len(decompressed) == volume_texture.payload_bytes_raw)

        bytes_per_slice = 512 * 512 * 2
        first_slice = decompressed[0:bytes_per_slice]
        for index in range(1, slice_count):
            sl = decompressed[index * bytes_per_slice : (index + 1) * bytes_per_slice]
            check(sl == first_slice, f"slice {index} should be byte-identical to slice 0")

        frame_texture = dcmnorm.export_frame_texture(CT_FIXTURE, compression="none")
        check(frame_texture.content_kind == "image2d")
        check(frame_texture.depth == 1)
        check(frame_texture.width == 512)
        check(frame_texture.height == 512)
        check(
            frame_texture.data == first_slice,
            "export_frame_texture on the unedited base file should byte-match the volume texture's first slice",
        )

        downsampled = volume.export_texture(target_max_dim=2, compression="none")
        check(downsampled.downsampled is True)
        check(downsampled.width <= 2 and downsampled.height <= 2 and downsampled.depth <= 2)
        check(
            (downsampled.native_width, downsampled.native_height, downsampled.native_depth) == (512, 512, slice_count)
        )

        try:
            volume.export_texture(compression="bogus")
            raise AssertionError("an invalid compression value should raise")
        except dcmnorm.DcmnormError as error:
            check("compression" in str(error), f"unexpected error message: {error}")

        # --- MPR reformat ----------------------------------------------------------------------
        row_dir, col_dir = volume.native_basis[0:3], volume.native_basis[3:6]
        reformatted = volume.reformat(
            origin=volume.center,
            row_dir=row_dir,
            col_dir=col_dir,
            output_width=64,
            output_height=64,
            spacing_mm=volume.min_spacing_mm,
            format="png",
        )
        check(reformatted.mime_type == "image/png")
        check(reformatted.width == 64 and reformatted.height == 64)
        check(len(reformatted.data) > 0)

        # --- Frame-stack texture -----------------------------------------------------------
        stack = dcmnorm.export_frame_stack_texture(
            [dcmnorm.FrameStackSource(file_path=CT_FIXTURE, frame_indices=[0])],
            compression="none",
        )
        check(stack.content_kind == "framestack")
        check(stack.depth == 1)

    print("smoke test passed")


if __name__ == "__main__":
    try:
        main()
    except Exception as error:  # noqa: BLE001
        print(f"FAILED: {error}", file=sys.stderr)
        raise
