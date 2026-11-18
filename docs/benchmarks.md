# Benchmarks

`benchmarks/` compares `dcmnorm` against [dcmtk](https://dcmtk.org/) 3.6.7 and
[dcm4che](https://www.dcm4che.org/) 5.35.1 on parsing (DICOM → JSON), rendering
(pixel data → PNG), and transcoding (→ Explicit VR Little Endian, decompressing
JPEG/JPEG2000 sources along the way).

### Methodology

All three tools run inside the same Docker container
(`benchmarks/Dockerfile`: `debian:bookworm-slim`, dcmtk from apt, dcm4che's
official binary distribution, `dcmnorm` built from this source tree) so the
comparison isn't skewed by different host installs, library versions, or
filesystems. The container is capped to 4 CPUs (`docker run --cpus=4`) for a
consistent, resource-isolated run. Each (operation, fixture, tool) combination
is timed with [hyperfine](https://github.com/sharkdp/hyperfine) (2 warmup runs
+ at least 8 measured runs, reporting mean/stddev/median/min/max wall time).
Reproduce with:

```bash
docker build -f benchmarks/Dockerfile -t dcmnorm-bench .
docker run --rm --cpus=4 \
  -v "$(pwd)/test/files":/fixtures:ro \
  -v "$(pwd)/benchmarks/results":/results \
  dcmnorm-bench bash /repo/benchmarks/run.sh
```

### Fixtures

| File | Transfer syntax | Dimensions | Size |
|---|---|---|---|
| `mr.dcm` | Explicit VR LE (uncompressed) | 512×512, 1 frame | 526 KB |
| `us2.dcm` | Explicit VR LE (uncompressed) | 360×360, 227 frames | 29.4 MB |
| `wsi.dcm` | JPEG Baseline | 240×240, 96 frames | 1.5 MB |
| `ct.dcm` | JPEG 2000 | 512×512, 1 frame | 90 KB |
| `dx2.dcm` | JPEG 2000 (Lossless-only) | 1736×2022, 1 frame | 3.6 MB |

### Results (mean ± stddev, milliseconds; lower is better)

Run 2026-10-04 (dcmnorm 0.3.3 + deferred bulk data reads for JSON, dcmtk 3.6.7, dcm4che 5.35.1).

**Parse** (`dcm2json` / `dcm2json` / `dcmnorm <file>`)

| Fixture | dcmtk | dcm4che | dcmnorm |
|---|---|---|---|
| mr.dcm | 17.5 ± 1.5 | 215.0 ± 3.7 | **2.9 ± 0.6** |
| us2.dcm | 290.2 ± 6.4¹ | 225.5 ± 12.2 | **2.7 ± 0.5** |
| wsi.dcm | n/a¹ | 257.4 ± 13.7 | **3.2 ± 0.6** |
| ct.dcm | n/a¹ | 260.3 ± 24.4 | **3.2 ± 0.6** |
| dx2.dcm | n/a¹ | 222.7 ± 4.7 | **3.0 ± 0.6** |

**Render one frame** (`dcmj2pnm +F N --write-png` / `dcm2jpg --frame N -F png` / `dcmnorm <file> <out.png> --render-frame N-1`)

Every tool renders exactly one frame, the same one: the middle frame of the multi-frame fixtures
(`us2.dcm` frame 114 of 227, `wsi.dcm` frame 49 of 96), else the only frame. For `us2.dcm`,
dcmtk's and dcmnorm's decoded pixels for that frame were checked to be byte-identical.

| Fixture | dcmtk | dcm4che | dcmnorm |
|---|---|---|---|
| mr.dcm | 29.4 ± 2.6 | 327.2 ± 22.1 | **6.2 ± 0.9** |
| us2.dcm | 18.5 ± 2.0 | 324.0 ± 12.5 | **4.2 ± 0.9** |
| wsi.dcm | 20.1 ± 3.0 | 334.6 ± 6.6 | **6.1 ± 0.8** |
| ct.dcm | n/a² | 393.1 ± 18.2 | **19.7 ± 2.3** |
| dx2.dcm | n/a² | 510.3 ± 19.6 | **451.7 ± 9.8** |

**Transcode → Explicit VR LE** (`dcmconv +te` / `dcmdjpeg`³ / `dcm2dcm -t ...` / `dcmnorm <in> <out> --transfer-syntax ...`)

| Fixture | dcmtk | dcm4che | dcmnorm |
|---|---|---|---|
| mr.dcm | 12.5 ± 1.6 | 257.8 ± 7.5 | **4.8 ± 1.0** |
| us2.dcm | 37.1 ± 3.4 | 323.2 ± 17.2 | **26.6 ± 2.8** |
| wsi.dcm | 61.6 ± 2.9 | 455.6 ± 25.9 | **40.6 ± 4.0** |
| ct.dcm | n/a² | 332.6 ± 12.4 | **18.0 ± 1.5** |
| dx2.dcm | n/a² | 499.7 ± 42.9 | **431.9 ± 8.7** |

¹ dcmtk's `dcm2json` (this build) has no bulk-data-by-reference/exclude option
— unlike dcm4che's `-B`/`--no-bulkdata` or dcmnorm's default `bulkData: uri`
mode, it always inlines `PixelData` as base64, and fails outright
("JSON InlineBinary encoding not supported for compressed pixel data") on any
compressed source. Confirmed by running it directly outside the benchmark
harness, not a harness bug. Its `us2.dcm` parse is therefore not like-for-like:
it's the only tool base64-encoding all 29MB of pixel data inline, where dcmnorm
and dcm4che both emit a reference instead.

² dcmtk's apt-packaged build (`dcmdjp2k` is not installed alongside `dcmtk`,
and `dcmj2pnm`/`dcmconv` have no JPEG2000 codec registered) cannot decode or
transcode JPEG2000 at all — confirmed via `dcmconv +te` on `ct.dcm`:
`E: Pixel representation cannot be changed`.

³ `wsi.dcm` (JPEG Baseline) uses dcmtk's dedicated `dcmdjpeg` decompressor
rather than `dcmconv +te`, matching how dcmtk itself expects JPEG sources to
be decompressed; `dcm2dcm` and `dcmnorm --transfer-syntax` handle both the
plain VR/endian conversion and JPEG/JPEG2000 decompression through the same
one invocation.

`run.sh` discards any timing whose command didn't produce its output file, so a
tool can't "win" by failing fast (e.g. a dcmtk built without libpng rejects
`--write-png` immediately, which would otherwise time as a ~4ms render).

### Takeaways

- **dcmnorm is fastest in every row** — including `us2.dcm`, a 227-frame,
  29MB native ultrasound cine, where the previous run (2026-08-29) had it
  losing to dcmtk on render (52.6 vs 18.1ms) and transcode (104.4 vs 61.9ms).
  That gap was never the DICOM work itself (decode + window + PNG for one
  frame is ~1ms); it was overhead that grew with file size, fixed since:
  - every invocation SHA-256'd the whole (statically linked, ~30MB) binary
    to build its `--version` string — now only done for `--version`;
  - PixelData was copied 3-4 times between `fs::read`, the parser's scratch
    buffer, a same-transfer-syntax "transcode" clone, and the writer
    cloning each value to serialize it — now read once and written by
    reference;
  - single-frame render read all 227 frames to render one — it now reads
    just the requested frame, as dcmtk's `dcmj2pnm` does.
- **Parse (DICOM → JSON) no longer depends on file size** — ~3ms for every
  fixture, from a 90KB CT to the 29MB cine. JSON references bulk values
  (PixelData etc.) by `BulkDataURI` offset/length rather than embedding
  them, so the parser now seeks past them and records where they were,
  instead of reading the whole file, parsing a copy of each value, and then
  re-scanning the raw bytes to find (and byte-compare) its offset. It reads
  ~2KB of `us2.dcm` instead of 29MB — which matters even more for a file
  that isn't already in the page cache.
- **dcm4che's numbers are dominated by JVM cold-start** (~200-300ms of every
  single-invocation timing here is the JVM spinning up, not DICOM work) — this
  benchmark reflects a CLI invoked once per file, not a long-running server
  reusing a warm JVM, which would look very different. Not a fair "dcm4che the
  library is slow" conclusion; it's specifically a CLI-cold-start cost.
- **`dx2.dcm` (1736×2022 JPEG 2000) is decode-bound** (~430ms in OpenJPEG for
  both render and transcode); it's the one fixture where dcmnorm's lead over
  dcm4che is narrow, since the codec dominates both.
- **dcmtk's apt-packaged build has real capability gaps**: no
  bulk-data-reference JSON mode, no JPEG2000 support at all. Both are almost
  certainly build-configuration choices (dcmtk itself supports JPEG2000 when
  compiled with the right codec module) rather than fundamental limitations
  of the toolkit — but they're what ships via `apt`, which is what most
  deployments actually run.
