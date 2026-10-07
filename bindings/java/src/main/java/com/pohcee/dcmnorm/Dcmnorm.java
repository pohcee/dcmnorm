package com.pohcee.dcmnorm;

import com.pohcee.dcmnorm.internal.Json;
import com.pohcee.dcmnorm.internal.NativeLoader;
import java.util.ArrayList;
import java.util.List;
import java.util.Map;

/**
 * Native Java bindings for dcmnorm, built with the <a href="https://docs.rs/jni">jni</a> crate.
 * Calls straight into the {@code dcmnorm} lib crate in-process (no CLI subprocess, no stdio/JSON
 * round trip). This is the Java counterpart of {@code bindings/node} (napi-rs) and {@code
 * bindings/python} (PyO3) - same underlying crate, same feature set.
 *
 * <p>Every method here blocks the calling thread until it completes - there is no JS event loop
 * or Python GIL to avoid blocking here, so (unlike the Node bindings) there is no separate
 * async/Promise-returning API; run a call on your own background thread/{@code ExecutorService}
 * if you need one. Every method that can fail declares {@code throws DcmnormException}; a panic
 * inside the native library (e.g. a severely malformed DICOM file) is also surfaced this way
 * rather than crashing the JVM - see {@link DcmnormException}.
 *
 * <p>Optional/structured arguments are passed as small {@code Options} objects (e.g. {@link
 * ReadJsonOptions}), each a tiny fluent builder - see this module's own README for why these
 * cross the JNI boundary as JSON text rather than individual JNI fields.
 */
public final class Dcmnorm {
    static {
        NativeLoader.ensureLoaded();
    }

    private Dcmnorm() {
    }

    // -----------------------------------------------------------------------------------------
    // Core: readTags / readJson / writeJson / editTags / transcode / checkDicom
    // -----------------------------------------------------------------------------------------

    /**
     * Reads only the requested tags from a DICOM file, stopping as soon as the highest one has
     * been parsed. Mirrors {@code dcmnorm --filter ... --format flat --keys hex}. {@code tags}
     * accepts DICOM keywords ({@code "StudyInstanceUID"}) or tag expressions ({@code
     * "(0020,000D)"}); the returned JSON is keyed by bare hex tag ({@code "0020000D"}).
     *
     * @return a JSON string - parse it with your own JSON library
     */
    public static String readTags(String filePath, List<String> tags) throws DcmnormException {
        return nativeReadTags(filePath, Json.encodeStringArray(tags));
    }

    /** Reads the full DICOM dataset as JSON with default options - see {@link
     * #readJson(String, ReadJsonOptions)}. */
    public static String readJson(String filePath) throws DcmnormException {
        return readJson(filePath, new ReadJsonOptions());
    }

    /**
     * Reads the full DICOM dataset as JSON. Mirrors plain {@code dcmnorm file.dcm} (flat/name
     * keys, bulk data as a URI reference, by default). {@link ReadJsonOptions#bulkData}
     * defaults to {@code "uri"} (matching the CLI), not the Rust library's own default of
     * inline-embedding bulk data - getting this wrong makes a huge difference: {@code "inline"}
     * base64-embeds elements like {@code PixelData} directly, ~1000x larger output for a typical
     * image, instead of a small {@code "?offset=..&length=.."} reference.
     *
     * @return a JSON string - parse it with your own JSON library
     */
    public static String readJson(String filePath, ReadJsonOptions options) throws DcmnormException {
        return nativeReadJson(filePath, options.toJson());
    }

    /** Writes a DICOM file from JSON with default options - see {@link #writeJson(String,
     * String, WriteJsonOptions)}. */
    public static void writeJson(String json, String outputPath) throws DcmnormException {
        writeJson(json, outputPath, new WriteJsonOptions());
    }

    /**
     * Writes a DICOM file from JSON (flat or standard format, auto never guessed - pass the
     * same format used to read it). Mirrors {@code dcmnorm dataset.json out.dcm}.
     */
    public static void writeJson(String json, String outputPath, WriteJsonOptions options) throws DcmnormException {
        nativeWriteJson(json, outputPath, options.toJson());
    }

    /** Edits a DICOM file in place with no changes requested - mostly useful as a round-trip
     * check; see {@link #editTags(String, EditTagsOptions)} for the real use. */
    public static void editTags(String filePath) throws DcmnormException {
        editTags(filePath, new EditTagsOptions());
    }

    /**
     * Sets/removes DICOM attributes, optionally stripping private tags. Mirrors {@code dcmnorm
     * --set KEY=VALUE --remove KEY --remove-private-tags}. Writes back to {@code filePath} in
     * place unless {@link EditTagsOptions#outputPath} is given.
     */
    public static void editTags(String filePath, EditTagsOptions options) throws DcmnormException {
        nativeEditTags(filePath, options.toJson());
    }

    /** Transcodes a DICOM file to the given transfer syntax UID. Mirrors {@code dcmnorm
     * --transfer-syntax UID in.dcm out.dcm}. */
    public static void transcode(String filePath, String outputPath, String transferSyntaxUid) throws DcmnormException {
        nativeTranscode(filePath, outputPath, transferSyntaxUid);
    }

    /** Reports whether a file looks like valid DICOM. Mirrors {@code dcmnorm --check-dicom}.
     * Never raises - an unreadable/malformed/non-DICOM file is simply {@code false}. */
    public static boolean checkDicom(String filePath) {
        return nativeCheckDicom(filePath);
    }

    // -----------------------------------------------------------------------------------------
    // DIMSE: echoScu / storeScu / findScu / moveScu - every call blocks the calling thread until
    // it completes. Unlike the Python bindings' handle-based start_*_scu variants, there is no
    // early abort()/release() here - see this module's README for the scope decision.
    // -----------------------------------------------------------------------------------------

    public static int echoScu(String destination) throws DcmnormException {
        return echoScu(destination, new EchoScuOptions());
    }

    /** Performs a C-ECHO (DICOM Verification) against {@code destination} ("host:port").
     * Returns the response Status code (0 = success); raises {@link DcmnormException} only if
     * the association itself could not be established. */
    public static int echoScu(String destination, EchoScuOptions options) throws DcmnormException {
        return nativeEchoScu(destination, options.toJson(), options.getOnLog());
    }

    public static List<StoreScuResult> storeScu(String destination, List<String> files) throws DcmnormException {
        return storeScu(destination, files, new StoreScuOptions());
    }

    /**
     * Sends each of {@code files} via C-STORE to {@code destination} ("host:port"). Returns one
     * {@link StoreScuResult} per file that could be read and sent - a non-zero status is just
     * data in the result (the peer rejected that instance), not a raised error; this only raises
     * if the association itself could not be established, or none of {@code files} could be
     * read as DICOM at all.
     */
    public static List<StoreScuResult> storeScu(String destination, List<String> files, StoreScuOptions options)
        throws DcmnormException {
        String json = nativeStoreScu(destination, Json.encodeStringArray(files), options.toJson(), options.getOnLog());
        List<Object> entries = Json.asList(Json.parse(json));
        List<StoreScuResult> results = new ArrayList<>(entries.size());
        for (Object entry : entries) {
            Map<String, Object> map = Json.asMap(entry);
            results.add(new StoreScuResult(Json.asString(map.get("sopInstanceUid")), Json.asInt(map.get("status"))));
        }
        return results;
    }

    public static List<String> findScu(String destination, Map<String, String> query) throws DcmnormException {
        return findScu(destination, query, new FindScuOptions());
    }

    /**
     * Performs a C-FIND (Study Root Query/Retrieve) against {@code destination} ("host:port").
     * {@code query} values: an empty string is a universal-match "return key" (mirrors
     * findscu's bare {@code -k TAG}), non-empty constrains the match (mirrors {@code -k
     * TAG=value}); {@code QueryRetrieveLevel} defaults to {@code "STUDY"} if not given.
     *
     * @return one flat/hex-keyed DICOM JSON string per match (parse each with your own JSON
     *     library), same shape as {@code readJson(path, new
     *     ReadJsonOptions().format("flat").keyStyle("hex"))}
     */
    public static List<String> findScu(String destination, Map<String, String> query, FindScuOptions options)
        throws DcmnormException {
        Json.ObjectBuilder queryJson = Json.object();
        for (Map.Entry<String, String> entry : query.entrySet()) {
            queryJson.put(entry.getKey(), entry.getValue());
        }
        String json = nativeFindScu(destination, queryJson.build(), options.toJson(), options.getOnLog());
        return Json.asStringList(Json.parse(json));
    }

    public static MoveScuResult moveScu(String destination, String moveDestinationAe, String studyInstanceUid)
        throws DcmnormException {
        return moveScu(destination, moveDestinationAe, studyInstanceUid, new MoveScuOptions());
    }

    /**
     * Performs a C-MOVE (Study Root Query/Retrieve), asking {@code destination} ("host:port")
     * to push {@code studyInstanceUid} to {@code moveDestinationAe} (an AE title {@code
     * destination} already knows how to reach, not a socket address). Blocks until the retrieve
     * reaches a terminal status.
     */
    public static MoveScuResult moveScu(String destination, String moveDestinationAe, String studyInstanceUid,
        MoveScuOptions options) throws DcmnormException {
        String json = nativeMoveScu(destination, moveDestinationAe, studyInstanceUid, options.toJson(), options.getOnLog());
        Map<String, Object> map = Json.asMap(Json.parse(json));
        Object cancelledVia = map.get("cancelledVia");
        return new MoveScuResult(Json.asInt(map.get("status")), Json.asInt(map.get("completed")),
            Json.asInt(map.get("failed")), Json.asInt(map.get("warning")), Json.asInt(map.get("remaining")),
            Json.asBoolean(map.get("cancelled")), cancelledVia == null ? null : Json.asString(cancelledVia));
    }

    // -----------------------------------------------------------------------------------------
    // Rendering: renderFrame / renderMovie
    // -----------------------------------------------------------------------------------------

    public static RenderedFrame renderFrame(String filePath) throws DcmnormException {
        return renderFrame(filePath, new RenderFrameOptions());
    }

    /**
     * Renders a single frame of a DICOM file to JPEG or PNG. Mirrors {@code dcmnorm
     * --output-width ... --render-frame ... file.dcm out.jpg}.
     */
    public static RenderedFrame renderFrame(String filePath, RenderFrameOptions options) throws DcmnormException {
        return nativeRenderFrame(filePath, options.toJson());
    }

    public static RenderedMovie renderMovie(String filePath) throws DcmnormException {
        return renderMovie(filePath, new RenderMovieOptions());
    }

    /**
     * Renders every frame of a multi-frame DICOM file to an MP4 (requires {@code ffmpeg} on
     * {@code PATH}). Mirrors {@code dcmnorm --render-fps ... file.dcm out.mp4}. Does not
     * currently support the overlay options {@link #renderFrame} does.
     */
    public static RenderedMovie renderMovie(String filePath, RenderMovieOptions options) throws DcmnormException {
        return nativeRenderMovie(filePath, options.toJson());
    }

    // -----------------------------------------------------------------------------------------
    // MPR (Multiplanar Reformation): buildVolume
    // -----------------------------------------------------------------------------------------

    /**
     * Builds a 3D volume from a parallel stack of DICOM slice files (e.g. every image instance
     * in one CT/MR/PT series) sharing consistent {@code ImageOrientationPatient}. Slices are
     * spatially re-sorted internally by {@code ImagePositionPatient}, regardless of {@code
     * filePaths}' own order. Returns a {@link DicomVolumeHandle} for repeated {@code reformat()}
     * calls - <strong>must be closed</strong> (ideally via try-with-resources) once you are done
     * with it, see that class's own doc. Raises (rather than silently mis-rendering) for fewer
     * than 2 files, mismatched Rows/Columns, or a non-parallel/gantry-tilt-inconsistent stack.
     */
    public static DicomVolumeHandle buildVolume(List<String> filePaths) throws DcmnormException {
        long handle = nativeBuildVolume(Json.encodeStringArray(filePaths));
        return new DicomVolumeHandle(handle);
    }

    // -----------------------------------------------------------------------------------------
    // Texture export: exportFrameTexture / exportFrameStackTexture - see TextureExportResult's
    // own doc, and DicomVolumeHandle.exportTexture for the volume-native variant.
    // -----------------------------------------------------------------------------------------

    public static TextureExportResult exportFrameTexture(String filePath) throws DcmnormException {
        return exportFrameTexture(filePath, new TextureExportOptions());
    }

    /**
     * Packs a single frame's raw (unwindowed) physical values as a depth-1 "1-slice volume"
     * texture - lets a large diagnostic 2D image (e.g. DX/CR/mammography) reuse the exact same
     * client GPU texture/shader pipeline as an MPR volume. Mirrors {@code dcmnorm --render-frame
     * ... --output-type texture file.dcm out.gputex}.
     */
    public static TextureExportResult exportFrameTexture(String filePath, TextureExportOptions options)
        throws DcmnormException {
        return nativeExportFrameTexture(filePath, options.toJson());
    }

    public static TextureExportResult exportFrameStackTexture(List<FrameStackSource> sources) throws DcmnormException {
        return exportFrameStackTexture(sources, new TextureExportOptions());
    }

    /**
     * Packs several independent frames (from one or more files) as one texture-array upload -
     * one entry per source FILE, not per frame: a cine/multiframe instance supplies one source
     * with several frame indices (its file is parsed once), while a multi-image series supplies
     * one source per instance file. The result's layer order is the flattened source order
     * followed by each source's own frame-index order.
     */
    public static TextureExportResult exportFrameStackTexture(List<FrameStackSource> sources, TextureExportOptions options)
        throws DcmnormException {
        StringBuilder sourcesJson = new StringBuilder("[");
        for (int i = 0; i < sources.size(); i++) {
            if (i > 0) {
                sourcesJson.append(',');
            }
            sourcesJson.append(sources.get(i).toJson());
        }
        sourcesJson.append(']');
        return nativeExportFrameStackTexture(sourcesJson.toString(), options.toJson());
    }

    // -----------------------------------------------------------------------------------------
    // Native method declarations - implemented in bindings/java/src/lib.rs. Package-private
    // (rather than private) so DicomVolumeHandle can call the volume-handle natives directly.
    // -----------------------------------------------------------------------------------------

    private static native String nativeReadTags(String filePath, String tagsJson) throws DcmnormException;

    private static native String nativeReadJson(String filePath, String optionsJson) throws DcmnormException;

    private static native void nativeWriteJson(String json, String outputPath, String optionsJson) throws DcmnormException;

    private static native void nativeEditTags(String filePath, String optionsJson) throws DcmnormException;

    private static native void nativeTranscode(String filePath, String outputPath, String transferSyntaxUid)
        throws DcmnormException;

    private static native boolean nativeCheckDicom(String filePath);

    private static native int nativeEchoScu(String destination, String optionsJson, DimseLogger onLog)
        throws DcmnormException;

    private static native String nativeStoreScu(String destination, String filesJson, String optionsJson, DimseLogger onLog)
        throws DcmnormException;

    private static native String nativeFindScu(String destination, String queryJson, String optionsJson, DimseLogger onLog)
        throws DcmnormException;

    private static native String nativeMoveScu(String destination, String moveDestinationAe, String studyInstanceUid,
        String optionsJson, DimseLogger onLog) throws DcmnormException;

    private static native RenderedFrame nativeRenderFrame(String filePath, String optionsJson) throws DcmnormException;

    private static native RenderedMovie nativeRenderMovie(String filePath, String optionsJson) throws DcmnormException;

    private static native long nativeBuildVolume(String filePathsJson) throws DcmnormException;

    static native void nativeFreeVolume(long handle);

    static native int nativeVolumeRows(long handle);

    static native int nativeVolumeCols(long handle);

    static native int nativeVolumeNumSlices(long handle);

    static native double[] nativeVolumeNativeBasis(long handle);

    static native double[] nativeVolumeCenter(long handle);

    static native double nativeVolumeMinSpacingMm(long handle);

    static native RenderedFrame nativeVolumeReformat(long handle, String paramsJson) throws DcmnormException;

    static native TextureExportResult nativeVolumeExportTexture(long handle, String optionsJson) throws DcmnormException;

    private static native TextureExportResult nativeExportFrameTexture(String filePath, String optionsJson)
        throws DcmnormException;

    private static native TextureExportResult nativeExportFrameStackTexture(String sourcesJson, String optionsJson)
        throws DcmnormException;
}
