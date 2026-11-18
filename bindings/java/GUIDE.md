# dcmnorm for Java: a guide for dcm4che users

This guide walks through the common Java DICOM workflows with the `dcmnorm` call next to the
dcm4che idiom it replaces (#6): ingest metadata, DICOMweb metadata JSON, frame rendering,
transcoding, metadata mutation and file rewrite, and DIMSE retrieve. The README has
installation and the full API list; the javadoc has the details of every method.

dcmnorm borrows dcm4che's names where they help - `Tag.*` constants, a `VR` enum, `get*()`
accessors with defaults - but not its API shape: there are no streams to manage, readers to
configure or handles to close. Each operation is one static call on `Dcmnorm` that runs in
native code and returns plain Java objects.

Requirements: Java 11 or later; `com.pohcee:dcmnorm-java` plus the natives jar for your platform
(README, "Install").

- [Concepts](#concepts)
- [1. Ingest metadata](#1-ingest-metadata)
- [2. DICOMweb metadata (DICOM JSON)](#2-dicomweb-metadata-dicom-json)
- [3. Frame rendering](#3-frame-rendering)
- [4. Transfer syntax and transcoding](#4-transfer-syntax-and-transcoding)
- [5. Metadata mutation and file rewrite](#5-metadata-mutation-and-file-rewrite)
- [6. DIMSE retrieve](#6-dimse-retrieve)
- [Errors](#errors)
- [Threads, memory and copies](#threads-memory-and-copies)
- [Not yet covered](#not-yet-covered)

## Concepts

**Sources.** Every reading call takes the DICOM object as a file (`Path` or `String`), the whole
object as a `byte[]`, or an `InputStream`. A stream is read front to back once, in 64 KiB
chunks, and is never closed by dcmnorm - it can come straight from object storage (an S3
`GetObject` body, a servlet request).

**`DicomDataset`.** A read-only data set built in one native call as plain Java objects: no
native handle, nothing to close, safe to share between threads. Values are already typed:

| VR | held as | read with |
|---|---|---|
| text VRs (AE, CS, DA, DS, IS, LO, PN, SH, UI, UT, ...) | `String`, one per value, padding removed | `getString`, `getStrings`; `getInt`/`getDouble` parse IS/DS |
| SS, US, SL, UL, SV, UV | `long` | `getInt`, `getInts`, `getLong`, `getLongs` |
| FL, FD | `double` | `getDouble`, `getDoubles`, `getFloat` |
| AT | `int` tags | `getTags`; `getString` gives `"00100010"` |
| OB, OW, OF, OD, OL, OV, UN | `byte[]` (little-endian), or a `BulkDataRef` | `getBytes`, `getBulkDataRef` |
| SQ | `List<DicomDataset>` | `getSequence`, `getNestedDataset` |
| PN | text, split on demand | `getPersonName` |

**Bulk data references.** PixelData and other large binary values (longer than 32 bytes) are,
by default, not read: the dataset holds a `BulkDataRef` (offset and length in the source)
instead. A file or `byte[]` is read with a seek past them; a stream reads and discards them. The
DICOM JSON writer turns them into `BulkDataURI`s and `Dcmnorm.write` reads them back from the
source, so pixel data never has to enter the JVM heap. `ReadOptions.full().loadBulkData(true)`
reads them instead.

**Constants.** `Tag`, `VR`, `TransferSyntax` and `SOPClassUID` are generated from the same data
dictionary the native library uses (`crates/dcmnorm-codegen`), with the standard keywords as
names: `Tag.StudyInstanceUID`, `TransferSyntax.JPEG2000Lossless`, `SOPClassUID.CTImageStorage`.
`ElementDictionary.keywordOf(tag)` / `vrOf(tag)` / `tagForKeyword(keyword)` look them up at run
time.

## 1. Ingest metadata

dcm4che:

```java
try (DicomInputStream in = new DicomInputStream(input)) {
    Attributes attrs = in.readDatasetUntilPixelData();
    String studyUid = attrs.getString(Tag.StudyInstanceUID);
    String seriesUid = attrs.getString(Tag.SeriesInstanceUID);
    int frames = attrs.getInt(Tag.NumberOfFrames, 1);
    String tsuid = in.getTransferSyntax();
}
```

dcmnorm:

```java
DicomDataset ds = Dcmnorm.read(input, ReadOptions.untilPixelData());   // Path, byte[] or InputStream
String studyUid  = ds.getString(Tag.StudyInstanceUID);
String seriesUid = ds.getString(Tag.SeriesInstanceUID);
int frames       = ds.getInt(Tag.NumberOfFrames, 1);                   // or ds.numberOfFrames()
String tsuid     = ds.transferSyntaxUid();                             // ds.transferSyntax() -> enum
DicomDataset fmi = ds.fileMeta();                                      // group 0002 as read
```

`untilPixelData()` stops at the PixelData header, exactly like `readDatasetUntilPixelData()`:
nothing past it is read, and for an `InputStream` nothing past it is consumed - an object-storage
download can be abandoned there. PixelData itself is in the dataset as a `BulkDataRef` to where
its value starts (`getBulkDataRef(Tag.PixelData)`; for compressed pixel data its length is `-1`,
since the read didn't walk the fragments).

More accessors, all with dcm4che's semantics (absent or empty: the default, or `null`):

```java
String[] imageType = ds.getStrings(Tag.ImageType);            // multi-value, null if absent
String second      = ds.getString(Tag.ImageType, 1, null);    // value 1
double[] spacing   = ds.getDoubles(Tag.PixelSpacing);         // DS parsed
VR vr              = ds.vr(Tag.PatientName);                  // VR.PN

PersonName name = ds.getPersonName(Tag.PatientName);         // "Yamada^Tarou=山田^太郎=やまだ^たろう"
name.getFamilyName();                                         // "Yamada"
name.getIdeographic().getGivenName();                         // "太郎"

for (DicomDataset item : ds.getSequence(Tag.ReferencedSeriesSequence)) {
    String uid = item.getString(Tag.SeriesInstanceUID);
}
DicomDataset first = ds.getNestedDataset(Tag.ReferencedSeriesSequence);

for (DicomElement e : ds) { /* every element, ascending tag order */ }
```

Multi-frame and geometry metadata comes from the dataset too: `numberOfFrames()`,
`frameTimeMillis()` (FrameTime), `frameTimeVector()` (FrameTimeVector), `framesPerSecond()`
(CineRate, else RecommendedDisplayFrameRate, else 1000 / FrameTime) and `pixelAspectRatio()`
(PixelAspectRatio, else the PixelSpacing ratio, else 1).

## 2. DICOMweb metadata (DICOM JSON)

dcm4che:

```java
try (DicomInputStream in = new DicomInputStream(dicomInput)) {
    in.setIncludeBulkData(DicomInputStream.IncludeBulkData.URI);
    in.setBulkDataCreator(dis -> { dis.skipFully(dis.length()); return new BulkData(null, "", dis.bigEndian()); });
    Attributes attrs = in.readDatasetUntilPixelData();
    jsonWriter.write(attrs);
}
```

dcmnorm:

```java
String json = Dcmnorm.toDicomJson(dicomInput, new JsonOptions()
    .bulkDataUriBuilder(ref -> instanceUrl + "/bulkdata/" + Tag.toString(ref.getTag())
        + "?offset=" + ref.getOffset() + "&length=" + ref.getLength()));
```

The output is PS3.18 Annex F DICOM JSON: tag keys, `vr`, `Value` arrays (numbers as JSON
numbers, PN as `Alphabetic`/`Ideographic`/`Phonetic` objects, AT as `"00100010"`, empty values of
a multi-valued attribute as `null`, an empty sequence without `Value`), with sequences at any
depth and the VR of every element preserved. Bulk values are not read from the source: values
longer than 32 bytes (and PixelData, native or compressed) become `BulkDataURI`s, shorter ones
`InlineBinary`. Without a builder the URI is `?offset=N&length=M` relative to the source, or
`<base>?offset=N&length=M` with `bulkDataUriBase(base)`.

`JsonOptions`:

| option | default | |
|---|---|---|
| `includeTags(int...)` | all | only these top-level attributes |
| `excludeTags(int...)` | none | leave these out (e.g. `Tag.PixelData`) |
| `bulkData(URI \| INLINE)` | `URI` | `INLINE` base64-embeds every value (reads them) |
| `bulkDataUriBase(String)` | none | prefix for the default URI form |
| `bulkDataUriBuilder(Function<BulkDataRef, String>)` | none | build each URI yourself; gets tag, VR, offset, length |
| `includeFileMeta(boolean)` | `false` | include group 0002 (not part of a DICOMweb data set) |
| `keywordAndVM(boolean)` | `false` | add dcmnorm's `Keyword`/`VM` members (not Annex F) |

The builder runs on the calling thread, once per value written as a URI. If it throws (or
returns `null`) the call fails with that message.

To change values before writing JSON (the "rewrite BulkDataURI values here" step in the dcm4che
code), read a `DicomDataset`, edit it (section 5) and pass the dataset:
`Dcmnorm.toDicomJson(dataset, options)`. Its `BulkDataRef`s are written as URIs the same way.

For series metadata, call `toDicomJson` per instance and join the objects into a JSON array.

## 3. Frame rendering

dcm4che:

```java
ImageReader reader = ImageIO.getImageReadersByFormatName("DICOM").next();
reader.setInput(ImageIO.createImageInputStream(file));
DicomImageReadParam param = (DicomImageReadParam) reader.getDefaultReadParam();
param.setWindowCenter(40); param.setWindowWidth(400);
BufferedImage image = reader.read(frameIndex, param);
```

dcmnorm:

```java
BufferedImage image = Dcmnorm.renderImage(file, frameIndex, new RenderParams().window(40, 400));
ImageIO.write(image, "jpeg", response.getOutputStream());
```

`renderImage` returns a `BufferedImage` built directly over the rendered 8-bit pixels - there is
no PNG/JPEG encode and decode in between: `TYPE_BYTE_GRAY` for grayscale, `TYPE_3BYTE_BGR` for
color, standard image types that `Graphics2D`, `ImageIO` writers (JPEG/PNG/GIF) and image
libraries take as they are. `renderPixels` returns the same pixels as a `RenderedPixels`
(`getWidth()`, `getHeight()`, `getChannels()` 1 or 3, `getPixels()` R,G,B interleaved) plus the
overlay information, and `toBufferedImage()` on it.

Frames are 0-based; the count is `ds.numberOfFrames()`. A file source reads only the requested
frame where the pixel data is native (uncompressed); compressed pixel data decodes the frame's
fragments.

**Rendering behaviour** (`RenderParams`). Grayscale: stored values, then the Modality LUT
(Rescale Slope/Intercept or a Modality LUT Sequence; `modalityLut(false)` skips it), then the
VOI transform - the `window(center, width)` given here, else the data set's VOI LUT Sequence,
else its first Window Center/Width, else the frame's min-max range (`voiLut(false)` skips VOI
and scales the min-max range instead) - then the presentation LUT shape: inverted when
Presentation LUT Shape is INVERSE or, without one, for MONOCHROME1 (taken from the data set;
there is no override). Color: palette color, YBR and RGB become RGB, and an embedded ICC profile
is applied (`iccProfile(false)` skips it). Then the first overlay plane (group 60xx) is
composited, if there is one (`overlays(false)`, `overlayIndex(i)`, `overlayColor("#RRGGBB")`),
and the image scaled (`outputSize(w, h)`, `outputWidth(w)`, `outputHeight(h)`). Output is always
8 bits per sample.

Pixel aspect ratio is not corrected: for non-square pixels (`ds.pixelAspectRatio() != 1`),
scale the output height by `1 / ratio` (`outputSize`).

**Buffer ownership and copies.** The native renderer produces the pixels in native memory; they
are copied once into a new Java `byte[]`. `RenderedPixels.getPixels()` returns that array itself
(not a copy), and the `BufferedImage` wraps the same array as its `DataBufferByte`: the image and
the `RenderedPixels` share it, and nothing else holds a reference. For color images,
`renderImage` has the native side store the pixels as B,G,R, so the array backs a
`TYPE_3BYTE_BGR` image without any per-pixel work in Java; `renderPixels` returns R,G,B.

## 4. Transfer syntax and transcoding

dcm4che:

```java
String tsuid = in.getTransferSyntax();
transcoder.setDestinationTransferSyntax(UID.JPEG2000Lossless);
```

dcmnorm:

```java
TransferSyntax ts = ds.transferSyntax();          // e.g. TransferSyntax.JPEGBaseline8Bit
ts.isEncapsulated(); ts.isCompressed(); ts.isLossy(); ts.isExplicitVR(); ts.isBigEndian();

TransferSyntaxSupport target = Dcmnorm.transferSyntaxSupport(TransferSyntax.JPEG2000Lossless.uid());
if (target.canTranscodeTo()) {
    Dcmnorm.transcode(inputPath, outputPath, target.getUid());          // String paths, as before
    byte[] out = Dcmnorm.transcode(bytes, target.getUid());            // byte[] -> byte[]
    Dcmnorm.transcode(inputStream, outputStream, target.getUid());     // stream -> stream
}
```

`Dcmnorm.transferSyntaxSupport()` lists, for every transfer syntax this build knows, whether it
can read and write data sets in it and decode and encode its pixel data - what's compiled in
differs by build, so check it rather than assume. Transcoding failures are typed (see
[Errors](#errors)): an unknown or unsupported target or source is an
`UnsupportedTransferSyntaxException`, a pixel data codec failure a `DecodeException` /
`EncodeException`, each naming the source and target UIDs. Multi-frame objects are transcoded
frame by frame; JPEG, JPEG-LS, JPEG 2000 (and HTJ2K), RLE, JPEG XL, deflate and the MPEG/HEVC
video syntaxes go through the same call.

A write can transcode too: `Dcmnorm.write(ds, out, WriteOptions.transferSyntax(uid))`.

## 5. Metadata mutation and file rewrite

dcm4che:

```java
attrs.setString(Tag.PatientName, VR.PN, value);
out.writeDataset(attrs.createFileMetaInformation(tsuid), attrs);
```

dcmnorm:

```java
DicomDataset ds = Dcmnorm.read(input);                 // bulk data stays in the source
DicomDataset fixed = ds.toBuilder()
    .set(Tag.PatientName, VR.PN, value)
    .set(Tag.PatientID, "NEW-ID")                      // VR from the dictionary
    .setInt(Tag.SeriesNumber, VR.IS, 3)
    .remove(Tag.OtherPatientIDsSequence)
    .editSequenceItem(Tag.ReferencedStudySequence, 0,
        item -> item.set(Tag.ReferencedSOPInstanceUID, VR.UI, newStudyUid))
    .build();
Dcmnorm.write(fixed, outputPath, WriteOptions.transferSyntax(tsuid));   // or WriteOptions.defaults()
```

`toBuilder()` copies; the original dataset doesn't change. `set` takes text for any VR (`set(Tag.Rows,
VR.US, "512")`), or use `setInt`/`setLong`/`setDouble`/`setTags`/`setBytes`/`setSequence`/
`addSequenceItem`/`removeSequenceItem`/`setNull`/`removePrivateTags`. Values are checked when
written: a US value out of range or text that isn't a number for a binary VR fails with a
`DcmnormException` of kind `INVALID_ARGUMENT`.

`write` creates the File Meta Information the way `createFileMetaInformation(tsuid)` does:
Media Storage SOP Class/Instance UID from the data set's SOP Class/Instance UID (so a new
SOPInstanceUID is reflected), the transfer syntax written, dcmnorm as the implementation (the
source's Source AE Title is kept). Group 0002 can't be set on the builder.

**Pixel data is streamed through.** The dataset's `BulkDataRef`s - PixelData included - are read
by the native writer straight from the source the dataset was read from, so a correction or
de-identification pass over a large multi-frame object never copies its pixel data into the JVM
heap. A dataset read from a `Path` or `byte[]` knows its source. One read from an `InputStream`
doesn't (the bytes are gone): pass `WriteOptions.defaults().bulkDataSource(path)` (or a `byte[]`)
naming where the same object is, or read it with `loadBulkData(true)`. Outputs: a `Path`/`String`,
an `OutputStream` (64 KiB writes, not closed), or `Dcmnorm.toBytes(dataset, options)`.

Study split/merge: build each output dataset with the new Study/Series/SOP Instance UIDs set and
`write` it; the pixel data comes from each instance's own source.

The older path-based `Dcmnorm.editTags(path, new EditTagsOptions().set(...).remove(...))` still
works for in-place edits by keyword.

## 6. DIMSE retrieve

dcm4che's `MoveSCU`/`GetSCU` and a `StoreSCP` become:

```java
MoveScuResult result = Dcmnorm.moveScu("pacs.example:11112", "MY_STORE_AE", studyInstanceUid,
    new MoveScuOptions().callingAeTitle("MY_AE").calledAeTitle("PACS").queryRetrieveLevel("SERIES")
        .seriesInstanceUid(seriesUid));
// result.getStatus(), getCompleted(), getFailed(), getWarning(), getRemaining()

List<String> matches = Dcmnorm.findScu("pacs.example:11112",
    Map.of("QueryRetrieveLevel", "STUDY", "PatientID", "12345", "StudyInstanceUID", ""));
List<StoreScuResult> sent = Dcmnorm.storeScu("pacs.example:11112", List.of(path1, path2));
int status = Dcmnorm.echoScu("pacs.example:11112");
```

C-MOVE asks the peer to send to an AE title it knows (`MY_STORE_AE`); this binding has no
storage SCP of its own - run `dcmtalk storescp` (shipped with dcmnorm) or your existing SCP as
the destination, and read what arrives with `Dcmnorm.read`. A failed association or operation is
a `DimseException`; a peer's non-success status is data in the result, not an exception. C-FIND
results are flat DICOM JSON strings today (one per match).

## Errors

Every failure is a checked `DcmnormException`; the native library classifies it and raises the
matching subclass:

| exception | `getKind()` | when |
|---|---|---|
| `InvalidDicomException` | `INVALID_DICOM` | not DICOM, malformed or truncated |
| `UnsupportedTransferSyntaxException` | `UNSUPPORTED_TRANSFER_SYNTAX` | a transfer syntax this build can't read, decode or encode, or an unknown UID |
| `DecodeException` | `DECODE` | pixel data couldn't be decoded (source UID, frame index) |
| `EncodeException` | `ENCODE` | pixel data couldn't be encoded, or the output not written (target UID) |
| `RenderException` | `RENDER` | frame index out of range, unsupported photometric interpretation or bit depth, bad window |
| `DimseException` | `DIMSE` | association or DIMSE operation failure |
| `DcmnormException` | `IO`, `INVALID_ARGUMENT`, `INTERNAL`, `OTHER` | file/stream I/O, bad arguments, a caught native panic |

`getSourceTransferSyntaxUid()`, `getTargetTransferSyntaxUid()` and `getFrameIndex()` (or `null` /
`-1`) carry the context.

```java
try {
    Dcmnorm.write(ds, out, WriteOptions.transferSyntax(TransferSyntax.JPEGLSLossless));
} catch (UnsupportedTransferSyntaxException e) {
    // fall back: e.getTargetTransferSyntaxUid()
} catch (DecodeException | EncodeException e) {
    log.warn("{} -> {} failed", e.getSourceTransferSyntaxUid(), e.getTargetTransferSyntaxUid());
}
```

## Threads, memory and copies

- Every call blocks the calling thread; run calls on your own executor for concurrency. Calls are
  independent and thread-safe; a `DicomDataset` is immutable.
- `read`: the native side builds the whole dataset and hands it over as one `byte[]`, which is
  decoded into Java objects (strings, arrays, lists) and dropped. Bulk values stay in the source
  unless `loadBulkData(true)`.
- A `byte[]` source is read in place where the JVM allows (pinned, or a copy the JVM makes; never
  written to). A dataset read from a `byte[]` keeps a reference to it for `write`: don't modify it.
- An `InputStream` is read through one 64 KiB buffer, on the calling thread, from inside the
  native call; an `IOException` from it fails the call (kind `IO`).
- Accessors return copies of arrays (`getBytes`, `getStrings`, ...); rendered pixels are the
  exception, shared by design (section 3).

## Not yet covered

- **An ImageIO `ImageReaderSpi`**, so `ImageIO.getImageReadersByFormatName("DICOM")` finds a
  dcmnorm reader, is a possible later step. It would not replace dcm4che's
  `DicomImageReadParam`/`DicomMetaData` either; `renderImage` is the integration point for now.
- C-GET and a storage SCP in this binding (the Python binding has `start_dicom_server`).
- C-FIND results as `DicomDataset`s.
