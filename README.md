<p align="center"><img src="docs/images/dcmnorm-icon.png" alt="dcmnorm icon: a scanner ring around a stack of image slices" width="128"></p>

# dcmnorm

Fast, open-source DICOM toolkit in Rust: parse, render, transcode, reformat, and network.
Ships as a CLI, a Rust library, and Node.js/Python/Java bindings.

[![License: Apache-2.0](https://img.shields.io/badge/license-Apache--2.0-blue.svg)](LICENSE)
[![Latest release](https://img.shields.io/github/v/release/pohcee/dcmnorm)](https://github.com/pohcee/dcmnorm/releases/latest)
[![PyPI](https://img.shields.io/pypi/v/dcmnorm?label=pypi)](https://pypi.org/project/dcmnorm/)
[![npm](https://img.shields.io/npm/v/dcmnorm?label=npm)](https://www.npmjs.com/package/dcmnorm)
[![Maven Central](https://img.shields.io/maven-central/v/com.pohcee/dcmnorm?label=maven%20central)](https://central.sonatype.com/artifact/com.pohcee/dcmnorm)
[![Live demo](https://img.shields.io/badge/live%20demo-try%20it-brightgreen)](https://dcmnorm-test-website-spzcbrdjlq-uc.a.run.app)

<!-- TODO: add a hero image, e.g. a rendered WSI tile next to an MPR reformat:
<p align="center"><img src="docs/images/hero.png" alt="dcmnorm renders: WSI tile and coronal MPR" width="800"></p>
-->

**🔗 Live demo:** try the Node bindings in your [browser](https://dcmnorm-test-website-spzcbrdjlq-uc.a.run.app) — upload a `.dcm` file (or pick a bundled
sample) and see its JSON metadata, a JPEG of every frame, and per-step timings.

## Install

| | |
|---|---|
| CLIs (`dcmnorm`, `dcmtalk`) | `uv tool install dcmnorm` or `pipx install dcmnorm`; `curl -sSL pohcee.com/dcmnorm \| sh` (Linux, macOS); [release archives and `.deb`](https://github.com/pohcee/dcmnorm/releases/latest); `cargo install --path exec/dcmnorm` from a checkout |
| Python | `pip install dcmnorm` (CPython 3.9+ for `import dcmnorm`; the wheel also carries the CLIs) |
| Node.js | `npm install dcmnorm` (Node 18+) |
| Java | `com.pohcee:dcmnorm` plus `com.pohcee:dcmnorm-natives` for your platform (Java 11+), below |
| Rust | the `dcmnorm` library crate from this repository (a git dependency) |

Java, Maven (`linux-x86_64` shown; also `linux-aarch_64`, `osx-aarch_64`, `osx-x86_64`,
`windows-x86_64`, or `${os.detected.classifier}` with the os-maven-plugin):

```xml
<dependency>
  <groupId>com.pohcee</groupId>
  <artifactId>dcmnorm</artifactId>
  <version>${dcmnorm.version}</version>
</dependency>
<dependency>
  <groupId>com.pohcee</groupId>
  <artifactId>dcmnorm-natives</artifactId>
  <version>${dcmnorm.version}</version>
  <classifier>linux-x86_64</classifier>
  <scope>runtime</scope>
</dependency>
```

Gradle (Kotlin DSL):

```kotlin
dependencies {
    implementation("com.pohcee:dcmnorm:$dcmnormVersion")
    runtimeOnly("com.pohcee:dcmnorm-natives:$dcmnormVersion:linux-x86_64")
}
```

Rust (`Cargo.toml`):

```toml
[dependencies]
dcmnorm = { git = "https://github.com/pohcee/dcmnorm", tag = "v0.15.0" }
```

More options (the agent plugin, Docker, building from source): [docs/install.md](docs/install.md).

## Quick start

CLI:

```bash
dcmnorm scan.dcm                                                   # DICOM → JSON
dcmnorm scan.dcm frame.png                                         # render a frame
dcmnorm scan.dcm out.dcm --transfer-syntax 1.2.840.10008.1.2.1     # transcode to Explicit VR LE
dcmnorm cine.dcm cine.mp4                                          # multi-frame → MP4
dcmnorm --mpr coronal series/*.dcm coronal.png                     # multiplanar reformat
dcmnorm register current/ prior/ --reg prior-to-current.dcm         # rigid registration → DICOM REG
dcmtalk echoscu somepacs.example.com:11112                         # verify a PACS connection
```

Python:

```python
import dcmnorm
from dcmnorm import Tag, TransferSyntax

ds = dcmnorm.read("scan.dcm", until_pixel_data=True)   # metadata only, PixelData not read
print(ds.get_string(Tag.StudyInstanceUID), ds.number_of_frames)
frame = dcmnorm.render_pixels("scan.dcm", 0)          # 8-bit pixels: frame.data, frame.shape
dcmnorm.transcode_to("scan.dcm", TransferSyntax.ExplicitVRLittleEndian, "out.dcm")
```

Node.js:

```js
const { read, renderPixels, transcodeTo, Tag, TransferSyntax } = require("dcmnorm");

const ds = await read("scan.dcm", { untilPixelData: true });
console.log(ds.getString(Tag.StudyInstanceUID), ds.numberOfFrames);
const { width, height, channels, pixels } = await renderPixels("scan.dcm", 0);
await transcodeTo("scan.dcm", TransferSyntax.ExplicitVRLittleEndian, "out.dcm");
```

Java:

```java
DicomDataset ds = Dcmnorm.read(Path.of("scan.dcm"), ReadOptions.untilPixelData());
System.out.println(ds.getString(Tag.StudyInstanceUID) + " " + ds.numberOfFrames());
BufferedImage image = Dcmnorm.renderImage(Path.of("scan.dcm"), 0, new RenderParams());
ImageIO.write(image, "png", new File("frame.png"));
Dcmnorm.transcode(Path.of("scan.dcm"), Path.of("out.dcm"), TransferSyntax.ExplicitVRLittleEndian.uid());
```

Rust:

```rust
use dcmnorm::dicom_io::{read_dataset_document, render_source_frame, transcode_source, write_dicom_file};
use dcmnorm::dicom_io::{DatasetReadOptions, DicomSource, RenderPipelineOptions, TreeValue};

let scan = std::path::Path::new("scan.dcm");
let options = DatasetReadOptions { until_pixel_data: true, ..Default::default() };
let ds = read_dataset_document(DicomSource::Path(scan), options)?;
if let Some(TreeValue::Strings(uid)) = ds.dataset.get(0x0020_000D).map(|e| &e.value) {
    println!("StudyInstanceUID {}", uid[0]);
}
let frame = render_source_frame(DicomSource::Path(scan), &RenderPipelineOptions::default())?;
let mut out = transcode_source(DicomSource::Path(scan), "1.2.840.10008.1.2.1")?;
write_dicom_file(&mut out, "out.dcm")?;
```

Each binding's README has its full API; for Java, [the guide](bindings/java/GUIDE.md) maps each
common workflow to its dcm4che idiom.

## Why dcmnorm

- **Fast:** faster than dcmtk and dcm4che in every parse, render, and transcode benchmark; DICOM → JSON takes ~3 ms whether the file is 90 KB or 29 MB ([benchmarks](docs/benchmarks.md#benchmarks))
- **Broad:** renders most DICOM imaging SOP classes, including WSI, with modality/VOI LUTs, windowing, and overlays; cine → MP4; MPR reformats (gantry tilt, Enhanced multi-frame); NIfTI/NRRD volume export; rigid CT/MR/PT registration and DICOM Spatial Registration objects
- **Networked:** `dcmtalk` covers C-ECHO/C-STORE/C-FIND/C-MOVE plus a storage SCP, with no dcmtk dependency
- **Embeddable:** in-process Node.js, Python, and Java bindings, plus a Rust library crate
- **Agent-ready:** ships with a skill for popular AI coding assistants
- **Powers [dcm2bq](https://github.com/GoogleCloudPlatform/dcm2bq):** an open-source DICOM-to-BigQuery project under GoogleCloudPlatform

## What's in this repository

- [`dcmnorm`](src/): a library crate with DICOM file, memory, JSON conversion, and DIMSE network helpers
- [`exec/dcmnorm`](exec/dcmnorm/): a CLI for converting between DICOM, transcoded DICOM, JSON, and rendered images/raw frames, and for writing a DICOMDIR
- [`exec/dcmtalk`](exec/dcmtalk/): a DIMSE network CLI (C-ECHO/C-STORE/C-FIND/C-MOVE SCU plus a storage SCP), covering the same ground as [dcmtk](https://dcmtk.org/)'s `echoscu`/`storescu`/`findscu`/`movescu`/`storescp`
- [`bindings/node`](bindings/node/): Node.js bindings (`npm i dcmnorm`) that call the library in-process via napi-rs — see that package's own README for its API
  - [`bindings/node/examples/test-website`](bindings/node/examples/test-website/): the source for the live demo above — also runnable locally, in Docker, or deployed to your own Cloud Run project
- [`bindings/python`](bindings/python/): Python bindings (`import dcmnorm`, in the `dcmnorm` PyPI package) that call the library in-process via PyO3 — see that package's own README for its API
- [`bindings/java`](bindings/java/): Java bindings (`com.pohcee:dcmnorm` + `com.pohcee:dcmnorm-natives` on Maven Central; `dcmnorm-java` / `dcmnorm-java-natives` up to 0.13.x, now relocated) that call the library in-process via the [jni](https://docs.rs/jni) crate — see that package's own README for its API

## Documentation

- [Installing](docs/install.md): PyPI, the agent plugin, FFmpeg for video, Cargo, release
  binaries and `.deb` packages, Docker
- [dcmnorm CLI](docs/cli.md): every option - JSON conversion, filtering, bulk data, validation,
  histograms, editing, rendering, texture export, MPR and volume export, overlays, batch mode,
  transcoding, video, JPEG 2000 codec selection, DICOMDIR, registration
- [dcmtalk CLI](docs/dcmtalk.md): C-ECHO, C-STORE, C-FIND, C-MOVE and a storage SCP
- Bindings: [Node.js](bindings/node/README.md), [Python](bindings/python/README.md),
  [Java](bindings/java/README.md) and the [Java guide for dcm4che users](bindings/java/GUIDE.md)
- [Benchmarks](docs/benchmarks.md): dcmnorm vs dcmtk and dcm4che
- [Building and testing](docs/building.md): workspace layout, prerequisites, Kakadu FFI
- [Releasing](docs/releasing.md) and [packaging](packaging/README.md)
- Design notes: [JPEG 2000 codec evaluation](docs/jpeg2000-codec-evaluation.md),
  [agent plugin distribution](docs/claude-plugin-distribution-plan.md)

## Contributing

Issues and pull requests are welcome! Bug reports with a sample file (de-identified, please) are
especially helpful. If dcmnorm is useful to you, a ⭐ helps others find it.

Before pushing, run `scripts/lint.sh` (rustfmt + clippy, as CI does; `--fix` applies the fixes).
`git config core.hooksPath .githooks` runs it for you on commit (rustfmt) and push (clippy).

## Thanks

This workspace is built on the [DICOM-rs](https://github.com/Enet4/dicom-rs) project's Rust
DICOM crates (`dicom-core`, `dicom-object`, `dicom-ul`, and others) for DICOM parsing, encoding,
and the Upper Layer/association protocol.

`exec/dcmtalk`'s subcommands (`echoscu`/`storescu`/`findscu`/`movescu`/`storescp`) follow the
naming and behavior established by [DCMTK](https://dcmtk.org/), the long-standing reference
DICOM toolkit.

Thanks to both projects and their maintainers.

## License

Licensed under the [Apache License, Version 2.0](LICENSE).

Bundled third-party code keeps its own license: OpenHTJ2K
(`crates/dcmnorm-jpeg2000/vendor/openhtj2k`) is BSD-3-Clause, see its
[LICENSE](crates/dcmnorm-jpeg2000/vendor/openhtj2k/LICENSE).
