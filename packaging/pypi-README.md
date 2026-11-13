<img src="https://raw.githubusercontent.com/pohcee/dcmnorm/main/plugin/.claude-plugin/icon.png" alt="dcmnorm" width="96">

# dcmnorm (command-line tools)

Native DICOM command-line tools, installed from PyPI:

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
2.28+, macOS, Windows x86_64). Nothing runs in Python; installing just puts `dcmnorm` and
`dcmtalk` on your `PATH`.

The video transfer syntaxes (MPEG-2, MPEG-4 AVC/H.264, HEVC) and `.mp4`/`.mov` output use the
`ffmpeg` command when it is installed (`apt install ffmpeg`, `brew install ffmpeg`,
`winget install ffmpeg`); everything else needs nothing besides this package.

This package is the CLIs only. For in-process use from Python, see the Python binding in the
repository. Documentation, other install options (release archives, `.deb`, Docker, Cargo) and
the source: <https://github.com/pohcee/dcmnorm>. Licensed under Apache-2.0.
