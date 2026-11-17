<img src="https://raw.githubusercontent.com/pohcee/dcmnorm/main/docs/images/dcmnorm-icon.png" alt="dcmnorm" width="96">

# dcmnorm

Native DICOM command-line tools and an in-process Python binding, installed from PyPI:

- **`dcmnorm`** reads, inspects, edits, anonymizes, validates, transcodes and renders DICOM files:
  DICOM ⇄ JSON, `--set` edits, transfer syntax conversion (JPEG, JPEG-LS, JPEG 2000, HTJ2K, RLE,
  video), PNG/JPEG/raw frames, MP4 movies and multiplanar reformation.
- **`dcmtalk`** talks to DICOM network peers (PACS, modalities): C-ECHO, C-STORE, C-FIND, C-MOVE
  and a storage SCP.

```bash
uv tool install dcmnorm        # or: pipx install dcmnorm / pip install dcmnorm
dcmnorm --version
dcmnorm study/image.dcm               # print the DICOM as JSON
dcmnorm image.dcm frame.png           # render a frame
dcmtalk echoscu PACS_HOST:104         # check connectivity (C-ECHO)
```

The package holds the compiled Rust binaries for your platform (Linux x86_64/aarch64 with glibc
2.28+, macOS, Windows x86_64); installing puts `dcmnorm` and `dcmtalk` on your `PATH`. The CLIs
don't run in Python, so they install on any Python the installer runs on.

On CPython 3.9 and newer the same package also gives you `import dcmnorm`, the in-process binding
(the same Rust library, no subprocess):

```python
import dcmnorm
dataset = dcmnorm.read_json("study/image.dcm")      # DICOM JSON, as a string
dcmnorm.transcode("in.dcm", "out.dcm", "1.2.840.10008.1.2.4.80")   # to JPEG-LS lossless
```

The CLIs handle the video transfer syntaxes (MPEG-2, MPEG-4 AVC/H.264, HEVC) and `.mp4`/`.mov`
output with the `ffmpeg` command when it is installed (`apt install ffmpeg`, `brew install ffmpeg`,
`winget install ffmpeg`); the Python module has FFmpeg linked in for the transfer syntaxes.
Everything else needs nothing besides this package.

The binding's API: <https://github.com/pohcee/dcmnorm/tree/main/bindings/python>. Documentation,
other install options (release archives, `.deb`, Docker, Cargo) and the source:
<https://github.com/pohcee/dcmnorm>. Licensed under Apache-2.0.
