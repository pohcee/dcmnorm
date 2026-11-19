use std::collections::HashMap;
use std::panic::{self, AssertUnwindSafe};
use std::path::PathBuf;
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use napi::bindgen_prelude::*;
use napi_derive::napi;

use dcmnorm::dicom_io::{
    dicom_file_to_json,
    apply_filter_to_object, build_volume as dcm_build_volume, echo_scu as dcm_echo_scu,
    find_scu as dcm_find_scu, move_scu as dcm_move_scu, parse_attribute_override,
    parse_filter_requests, parse_tag_key, probe_dicom_file_for_sop_class_uid,
    read_dicom_file, read_dicom_file_for_frame, read_dicom_json_with_options, read_dcmnorm_object_for_filter,
    compute_frame_histogram as dcm_compute_frame_histogram,
    compute_instance_histograms as dcm_compute_instance_histograms,
    pack_dicom_frame_stack_texture as dcm_pack_dicom_frame_stack_texture,
    pack_dicom_frame_texture as dcm_pack_dicom_frame_texture,
    pack_dicom_rgb_frame_stack_texture as dcm_pack_dicom_rgb_frame_stack_texture,
    pack_dicom_rgb_frame_texture as dcm_pack_dicom_rgb_frame_texture,
    pack_volume_texture as dcm_pack_volume_texture,
    reformat_plane as dcm_reformat_plane, remove_attribute, remove_private_tags_inplace,
    render_dicom_frame as dcm_render_dicom_frame,
    try_extract_passthrough_jpeg_frame as dcm_try_extract_passthrough_jpeg_frame,
    set_attribute, start_scp as dcm_start_scp,
    store_scu as dcm_store_scu, transcode_dicom_file, write_dicom_file,
    write_dicom_json_with_options, write_dicom_video as dcm_write_dicom_video, CancelMode as DcmCancelMode,
    CancelSignal, DicomJsonBulkDataMode, DicomJsonFormat, DicomJsonKeyStyle, DicomJsonReadOptions,
    DicomJsonWriteOptions, DicomScp, DimseLogger, EchoScuOptions as DcmEchoScuOptions,
    FindScuOptions as DcmFindScuOptions, HistogramOptions as DcmHistogramOptions,
    Interpolation as DcmInterpolation,
    MoveScuOptions as DcmMoveScuOptions, OverlaySummary as DcmOverlaySummary,
    QueryRetrieveLevel as DcmQueryRetrieveLevel,
    PlaneParams as DcmPlaneParams, RenderOutputFormat as DcmRenderOutputFormat,
    RenderPipelineOptions as DcmRenderPipelineOptions, ScpHandlers as DcmScpHandlers,
    ScpOptions as DcmScpOptions, SlabProjection as DcmSlabProjection,
    StoreScuOptions as DcmStoreScuOptions, TextureCompression as DcmTextureCompression,
    TextureMeta as DcmTextureMeta, Volume as DcmVolume,
    write_dicomdir as dcm_write_dicomdir, DicomdirEntry as DcmDicomdirEntry,
    build_spatial_registration as dcm_build_spatial_registration, estimate_translation as dcm_estimate_translation,
    read_spatial_registration as dcm_read_spatial_registration, register_rigid as dcm_register_rigid,
    write_dicom_bytes as dcm_write_dicom_bytes, Initialization as DcmInitialization, MaskMode as DcmMaskMode,
    MetricChoice as DcmMetricChoice, RegistrationOptions as DcmRegistrationOptions,
    TranslationOptions as DcmTranslationOptions,
    DicomdirOptions as DcmDicomdirOptions, DicomdirReport as DcmDicomdirReport,
};
use napi::threadsafe_function::{ThreadsafeFunction, ThreadsafeFunctionCallMode};

mod dataset;

/// Read `path` to render (or texture-export) frame `frame_index` and nothing else: for a native
/// multi-frame file, only that frame's bytes are read (see dcmnorm's `read_dicom_file_for_frame`)
/// - e.g. ~130KB of a 29MB, 227-frame ultrasound cine instead of all of it. Anything that
/// shortcut doesn't cover (compressed, deflated, out-of-range frame, malformed, ...) falls back
/// to a normal full read, which also produces the usual error for a bad file.
fn read_dicom_file_for_single_frame(path: &std::path::Path, frame_index: usize) -> Result<dcmnorm_object::DefaultDicomObject> {
    match read_dicom_file_for_frame(path, frame_index) {
        Ok(Some(object)) => Ok(object),
        _ => read_dicom_file(path).map_err(to_napi_err),
    }
}

fn to_napi_err(err: impl std::fmt::Display) -> Error {
    Error::from_reason(err.to_string())
}

// DICOM files arrive from arbitrary, sometimes-malformed vendor equipment. A
// subprocess-based CLI call fails in isolation when one is bad; an in-process
// native addon does not get that for free, so every entry point runs through
// catch_unwind and turns a panic into a rejected Promise instead of taking
// the whole Node process down with it.
fn guarded<T>(f: impl FnOnce() -> Result<T>) -> Result<T> {
    match panic::catch_unwind(AssertUnwindSafe(f)) {
        Ok(result) => result,
        Err(payload) => {
            let message = payload
                .downcast_ref::<&str>()
                .map(|s| s.to_string())
                .or_else(|| payload.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "dcmnorm panicked while processing this file".to_string());
            Err(Error::from_reason(format!("dcmnorm internal error: {message}")))
        }
    }
}

fn parse_json_format(value: Option<&str>) -> Result<DicomJsonFormat> {
    match value {
        None | Some("flat") => Ok(DicomJsonFormat::Flat),
        Some("standard") => Ok(DicomJsonFormat::Standard),
        Some(other) => Err(Error::from_reason(format!(
            "invalid format '{other}'; expected 'flat' or 'standard'"
        ))),
    }
}

fn parse_key_style(value: Option<&str>) -> Result<DicomJsonKeyStyle> {
    match value {
        None | Some("name") => Ok(DicomJsonKeyStyle::Name),
        Some("hex") => Ok(DicomJsonKeyStyle::Hex),
        Some(other) => Err(Error::from_reason(format!(
            "invalid keyStyle '{other}'; expected 'name' or 'hex'"
        ))),
    }
}

// The dcmnorm CLI's --bulk-data flag defaults to Uri (a small "?offset=..&length=.."
// reference), overriding DicomJsonWriteOptions::default()'s InlineBinary at the CLI
// layer - so callers here need the same override, not the library default, or
// PixelData ends up fully base64-inlined (~1000x larger output for a typical image)
// instead of referenced. Confirmed empirically against the CLI's actual output.
fn parse_bulk_data_mode(value: Option<&str>) -> Result<DicomJsonBulkDataMode> {
    match value {
        None | Some("uri") => Ok(DicomJsonBulkDataMode::Uri),
        Some("inline") => Ok(DicomJsonBulkDataMode::InlineBinary),
        Some(other) => Err(Error::from_reason(format!(
            "invalid bulkData '{other}'; expected 'uri' or 'inline'"
        ))),
    }
}

/// Reads only the requested tags from a DICOM file, stopping as soon as the
/// highest one has been parsed. Mirrors `dcmnorm --filter ... --format flat
/// --keys hex`. Keys accept DICOM keywords (`StudyInstanceUID`) or tag
/// expressions (`(0020,000D)`); the returned JSON is keyed by bare hex tag
/// (`"0020000D"`). Returned as a JSON string (parse it JS-side) since
/// `Task::JsValue` requires `TypeName`, which `serde_json::Value` doesn't
/// implement.
pub struct ReadTagsTask {
    file_path: PathBuf,
    tags: Vec<String>,
}

impl Task for ReadTagsTask {
    type Output = String;
    type JsValue = String;

    fn compute(&mut self) -> Result<Self::Output> {
        guarded(|| {
            let requests = parse_filter_requests(&self.tags).map_err(to_napi_err)?;
            let mut object =
                read_dcmnorm_object_for_filter(&self.file_path, &requests).map_err(to_napi_err)?;
            apply_filter_to_object(&mut object, &requests);
            // bulk_data_mode: Uri needs the *source file bytes* to compute a valid
            // "?offset=..&length=.." reference (see read_json below) - which the
            // read_until fast path above deliberately doesn't read. That's fine for
            // every real caller today (short UID/string tags have no bulk-data VR to
            // begin with, so this never applies to them), but filtering for a
            // bulk-eligible tag (e.g. PixelData) will silently fall back to
            // inline-embedding its bytes rather than erroring or matching the CLI's
            // (slower - it always reads the whole file for this) URI behavior.
            write_dicom_json_with_options(
                &object,
                DicomJsonWriteOptions {
                    format: DicomJsonFormat::Flat,
                    key_style: DicomJsonKeyStyle::Hex,
                    bulk_data_mode: DicomJsonBulkDataMode::Uri,
                    ..Default::default()
                },
            )
            .map_err(to_napi_err)
        })
    }

    fn resolve(&mut self, _env: Env, output: Self::Output) -> Result<Self::JsValue> {
        Ok(output)
    }
}

#[napi]
pub fn read_tags(file_path: String, tags: Vec<String>) -> AsyncTask<ReadTagsTask> {
    AsyncTask::new(ReadTagsTask {
        file_path: PathBuf::from(file_path),
        tags,
    })
}

#[napi(object)]
#[derive(Default)]
pub struct ReadJsonOptions {
    pub format: Option<String>,
    pub key_style: Option<String>,
    /// 'uri' (default, matches the CLI) emits PixelData/other bulk elements as a
    /// small "?offset=..&length=.." reference; 'inline' base64-embeds them, which
    /// for a typical image is orders of magnitude larger.
    pub bulk_data: Option<String>,
    /// With format 'standard': omit the non-standard Keyword and VM members, leaving
    /// only what PS3.18 Annex F defines (the CLI's `--omit-keyword-vm`). Default false.
    pub omit_keyword_vm: Option<bool>,
}

pub struct ReadJsonTask {
    file_path: PathBuf,
    format: DicomJsonFormat,
    key_style: DicomJsonKeyStyle,
    bulk_data_mode: DicomJsonBulkDataMode,
    omit_keyword_vm: bool,
}

impl Task for ReadJsonTask {
    type Output = String;
    type JsValue = String;

    fn compute(&mut self) -> Result<Self::Output> {
        guarded(|| {
            // dicom_file_to_json mirrors the CLI: in Uri mode bulk values (PixelData etc.) are
            // skipped while reading and referenced by where the parser found them, instead of
            // reading the whole file and scanning it for their offsets.
            dicom_file_to_json(
                &self.file_path,
                DicomJsonWriteOptions {
                    format: self.format,
                    key_style: self.key_style,
                    bulk_data_mode: self.bulk_data_mode,
                    omit_keyword_vm: self.omit_keyword_vm,
                    ..Default::default()
                },
            )
            .map_err(to_napi_err)
        })
    }

    fn resolve(&mut self, _env: Env, output: Self::Output) -> Result<Self::JsValue> {
        Ok(output)
    }
}

/// Reads the full DICOM dataset as JSON. Mirrors plain `dcmnorm file.dcm`
/// (flat/name keys, bulk data as a URI reference, by default) or
/// `--format standard --keys hex --bulk-data inline`.
#[napi]
pub fn read_json(
    file_path: String,
    options: Option<ReadJsonOptions>,
) -> Result<AsyncTask<ReadJsonTask>> {
    let options = options.unwrap_or_default();
    let format = parse_json_format(options.format.as_deref())?;
    let key_style = parse_key_style(options.key_style.as_deref())?;
    let bulk_data_mode = parse_bulk_data_mode(options.bulk_data.as_deref())?;
    Ok(AsyncTask::new(ReadJsonTask {
        file_path: PathBuf::from(file_path),
        format,
        key_style,
        bulk_data_mode,
        omit_keyword_vm: options.omit_keyword_vm.unwrap_or(false),
    }))
}

#[napi(object)]
#[derive(Default)]
pub struct WriteJsonOptions {
    pub format: Option<String>,
    /// Resolves "?offset=..&length=.." BulkDataURIs (readJson's default 'uri'
    /// bulkData mode emits these for bulk elements already in the source file)
    /// against this file's bytes - mirrors the CLI's `--bulk-data-source`. Not
    /// needed for InlineBinary or "file://" BulkDataURI elements, which are
    /// self-contained and resolved independently of this option.
    pub bulk_data_source_path: Option<String>,
}

pub struct WriteJsonTask {
    json: String,
    output_path: PathBuf,
    format: DicomJsonFormat,
    bulk_data_source_path: Option<PathBuf>,
}

impl Task for WriteJsonTask {
    type Output = ();
    type JsValue = ();

    fn compute(&mut self) -> Result<Self::Output> {
        guarded(|| {
            let bulk_data_source = self
                .bulk_data_source_path
                .as_ref()
                .map(std::fs::read)
                .transpose()
                .map_err(to_napi_err)?;

            let mut object = read_dicom_json_with_options(
                &self.json,
                DicomJsonReadOptions {
                    format: self.format,
                    bulk_data_source: bulk_data_source.as_deref(),
                },
            )
            .map_err(to_napi_err)?;

            write_dicom_file(&mut object, &self.output_path).map_err(to_napi_err)
        })
    }

    fn resolve(&mut self, _env: Env, _output: Self::Output) -> Result<Self::JsValue> {
        Ok(())
    }
}

/// Writes a DICOM file from JSON (flat or standard format, auto never guessed -
/// pass the same `format` used to read it). Mirrors `dcmnorm dataset.json out.dcm`.
/// Sequence elements are always (re)written with undefined length, so this does
/// not carry forward stale defined-length byte counts from whatever encoding the
/// JSON was originally read from - see dicom_io::json's DataSetSequence handling.
#[napi]
pub fn write_json(
    json: String,
    output_path: String,
    options: Option<WriteJsonOptions>,
) -> Result<AsyncTask<WriteJsonTask>> {
    let options = options.unwrap_or_default();
    let format = parse_json_format(options.format.as_deref())?;
    Ok(AsyncTask::new(WriteJsonTask {
        json,
        output_path: PathBuf::from(output_path),
        format,
        bulk_data_source_path: options.bulk_data_source_path.map(PathBuf::from),
    }))
}

#[napi(object)]
#[derive(Default)]
pub struct EditTagsOptions {
    pub output_path: Option<String>,
    pub set: Option<HashMap<String, String>>,
    pub remove: Option<Vec<String>>,
    pub remove_private_tags: Option<bool>,
}

pub struct EditTagsTask {
    file_path: PathBuf,
    output_path: PathBuf,
    sets: HashMap<String, String>,
    removes: Vec<String>,
    remove_private_tags: bool,
}

impl Task for EditTagsTask {
    type Output = ();
    type JsValue = ();

    fn compute(&mut self) -> Result<Self::Output> {
        guarded(|| {
            let mut object = read_dicom_file(&self.file_path).map_err(to_napi_err)?;

            for (key, value) in &self.sets {
                let assignment = format!("{key}={value}");
                let (tag, vr, value) = parse_attribute_override(&assignment).map_err(to_napi_err)?;
                set_attribute(&mut object, tag, vr, value).map_err(to_napi_err)?;
            }

            for key in &self.removes {
                let tag = parse_tag_key(key).map_err(to_napi_err)?;
                remove_attribute(&mut object, tag);
            }

            if self.remove_private_tags {
                remove_private_tags_inplace(&mut object);
            }

            write_dicom_file(&mut object, &self.output_path).map_err(to_napi_err)
        })
    }

    fn resolve(&mut self, _env: Env, _output: Self::Output) -> Result<Self::JsValue> {
        Ok(())
    }
}

/// Sets/removes DICOM attributes, optionally stripping private tags. Mirrors
/// `dcmnorm --set KEY=VALUE --remove KEY --remove-private-tags`. Writes back
/// to `file_path` in place unless `options.output_path` is given.
#[napi]
pub fn edit_tags(file_path: String, options: Option<EditTagsOptions>) -> AsyncTask<EditTagsTask> {
    let options = options.unwrap_or_default();
    let output_path = options.output_path.unwrap_or_else(|| file_path.clone());
    AsyncTask::new(EditTagsTask {
        file_path: PathBuf::from(file_path),
        output_path: PathBuf::from(output_path),
        sets: options.set.unwrap_or_default(),
        removes: options.remove.unwrap_or_default(),
        remove_private_tags: options.remove_private_tags.unwrap_or(false),
    })
}

pub struct TranscodeTask {
    file_path: PathBuf,
    output_path: PathBuf,
    transfer_syntax_uid: String,
}

impl Task for TranscodeTask {
    type Output = ();
    type JsValue = ();

    fn compute(&mut self) -> Result<Self::Output> {
        guarded(|| {
            transcode_dicom_file(&self.file_path, &self.output_path, &self.transfer_syntax_uid)
                .map_err(to_napi_err)
        })
    }

    fn resolve(&mut self, _env: Env, _output: Self::Output) -> Result<Self::JsValue> {
        Ok(())
    }
}

/// Transcodes a DICOM file to the given transfer syntax UID. Mirrors
/// `dcmnorm --transfer-syntax UID in.dcm out.dcm`.
#[napi]
pub fn transcode(
    file_path: String,
    output_path: String,
    transfer_syntax_uid: String,
) -> AsyncTask<TranscodeTask> {
    AsyncTask::new(TranscodeTask {
        file_path: PathBuf::from(file_path),
        output_path: PathBuf::from(output_path),
        transfer_syntax_uid,
    })
}

pub struct CheckDicomTask {
    file_path: PathBuf,
}

impl Task for CheckDicomTask {
    type Output = bool;
    type JsValue = bool;

    fn compute(&mut self) -> Result<Self::Output> {
        guarded(|| Ok(probe_dicom_file_for_sop_class_uid(&self.file_path).unwrap_or(false)))
    }

    fn resolve(&mut self, _env: Env, output: Self::Output) -> Result<Self::JsValue> {
        Ok(output)
    }
}

/// Reports whether a file looks like valid DICOM. Mirrors `dcmnorm
/// --check-dicom`.
#[napi]
pub fn check_dicom(file_path: String) -> AsyncTask<CheckDicomTask> {
    AsyncTask::new(CheckDicomTask {
        file_path: PathBuf::from(file_path),
    })
}

// ---------------------------------------------------------------------------------------------
// DICOMDIR: writeDicomdir
// ---------------------------------------------------------------------------------------------

/// One file of the File-set for `writeDicomdir`: its File ID plus its header, either as DICOM
/// JSON (`json`) or a file to read it from (`path`; pixel data isn't loaded).
#[napi(object)]
pub struct DicomdirEntry {
    /// File ID components, e.g. `["DICOM", "ST000001", "SE000001", "IM000001"]`: 1-8
    /// components of 1-8 characters from A-Z, 0-9 and _ (PS3.10 8.2).
    pub file_id: Vec<String>,
    /// The referenced file's DICOM JSON (what `readJson` returns; BulkDataURI elements are
    /// ignored). One of `json` and `path` is required.
    pub json: Option<String>,
    /// 'flat' (default, `readJson`'s default) or 'standard', for `json`.
    pub format: Option<String>,
    /// A DICOM file to read the header from instead of `json`.
    pub path: Option<String>,
    /// The referenced file's transfer syntax. With `json`, defaults to the JSON's
    /// TransferSyntaxUID (0002,0010), else Explicit VR Little Endian; with `path`, the file's.
    pub transfer_syntax_uid: Option<String>,
}

#[napi(object)]
#[derive(Default)]
pub struct WriteDicomdirOptions {
    /// File-set ID (0004,1130): up to 16 characters from A-Z, 0-9 and _. Default empty.
    pub file_set_id: Option<String>,
    /// SOP Instance UID of the DICOMDIR itself. Generated when not given.
    pub sop_instance_uid: Option<String>,
}

/// A value `writeDicomdir` made up (or copied from another attribute) for a Type 1 key the
/// instances lacked (IHE RAD TF-2 Table 4.47.4-1).
#[napi(object)]
pub struct DicomdirSynthesized {
    /// e.g. "STUDY".
    pub record_type: String,
    /// The Study/Series/SOP Instance UID; for PATIENT the patient's name, or "patient of
    /// study <UID>" when the patient has no name either.
    pub entity: String,
    /// Attribute keyword, e.g. "StudyID".
    pub attribute: String,
    pub value: String,
    /// Keyword of the instance attribute it was copied from, if any (e.g. "SeriesDate").
    pub source: Option<String>,
}

#[napi(object)]
pub struct DicomdirRecordCount {
    pub record_type: String,
    pub count: u32,
}

#[napi(object)]
pub struct WriteDicomdirResult {
    /// Total Directory Records written.
    pub records: u32,
    /// Records per Directory Record Type, in first-seen order.
    pub record_types: Vec<DicomdirRecordCount>,
    pub synthesized: Vec<DicomdirSynthesized>,
    pub warnings: Vec<String>,
}

pub struct WriteDicomdirTask {
    entries: Vec<DicomdirEntry>,
    output_path: PathBuf,
    options: DcmDicomdirOptions,
}

impl Task for WriteDicomdirTask {
    type Output = DcmDicomdirReport;
    type JsValue = WriteDicomdirResult;

    fn compute(&mut self) -> Result<Self::Output> {
        guarded(|| {
            let mut entries = Vec::with_capacity(self.entries.len());
            for entry in &self.entries {
                let file_id = entry.file_id.clone();
                let built = match (&entry.json, &entry.path) {
                    (Some(json), None) => {
                        let format = parse_json_format(entry.format.as_deref())?;
                        DcmDicomdirEntry::from_json(file_id, json, format, entry.transfer_syntax_uid.as_deref())
                            .map_err(to_napi_err)?
                    }
                    (None, Some(path)) => {
                        let mut built = DcmDicomdirEntry::from_file(file_id, path).map_err(to_napi_err)?;
                        if let Some(ts) = entry.transfer_syntax_uid.as_deref().filter(|ts| !ts.is_empty()) {
                            built.transfer_syntax_uid = ts.to_owned();
                        }
                        built
                    }
                    _ => {
                        return Err(Error::from_reason(format!(
                            "DICOMDIR entry {}: give exactly one of json and path",
                            entry.file_id.join("\\")
                        )))
                    }
                };
                entries.push(built);
            }
            dcm_write_dicomdir(&entries, &self.output_path, &self.options).map_err(to_napi_err)
        })
    }

    fn resolve(&mut self, _env: Env, report: Self::Output) -> Result<Self::JsValue> {
        Ok(WriteDicomdirResult {
            records: report.record_count as u32,
            record_types: report
                .record_types
                .into_iter()
                .map(|(record_type, count)| DicomdirRecordCount { record_type, count: count as u32 })
                .collect(),
            synthesized: report
                .synthesized
                .into_iter()
                .map(|s| DicomdirSynthesized {
                    record_type: s.record_type,
                    entity: s.entity,
                    attribute: s.attribute,
                    value: s.value,
                    source: s.source,
                })
                .collect(),
            warnings: report.warnings,
        })
    }
}

/// Writes a DICOMDIR (Media Storage Directory, Explicit VR Little Endian) for a File-set:
/// PATIENT > STUDY > SERIES > one record per entry, its type from the SOP Class (IMAGE, SR
/// DOCUMENT, KEY OBJECT DOC, PRESENTATION, ENCAP DOC, ...; PS3.3 F.5). Type 1 keys the instances
/// lack are synthesized and reported in `synthesized`. Rejects bad File IDs / File-set IDs, a
/// Patient ID shared by two patients, a study under two patients, a series in two studies,
/// duplicate files or SOP Instance UIDs. Mirrors `dcmnorm dicomdir ROOT`, which scans a
/// directory instead of taking entries.
#[napi]
pub fn write_dicomdir(
    entries: Vec<DicomdirEntry>,
    output_path: String,
    options: Option<WriteDicomdirOptions>,
) -> AsyncTask<WriteDicomdirTask> {
    let options = options.unwrap_or_default();
    AsyncTask::new(WriteDicomdirTask {
        entries,
        output_path: PathBuf::from(output_path),
        options: DcmDicomdirOptions {
            file_set_id: options.file_set_id.unwrap_or_default(),
            sop_instance_uid: options.sop_instance_uid,
        },
    })
}

// ---------------------------------------------------------------------------------------------
// DIMSE: echoScu / storeScu / findScu / moveScu
// ---------------------------------------------------------------------------------------------

/// Bridges dcmnorm's `DimseLogger` trait onto a JS callback via a fire-and-forget
/// `ThreadsafeFunction` - unlike the SCP handlers below, a log line needs no return value, so
/// this skips the call_with_return_value/oneshot-channel bridging those use and just dispatches
/// non-blocking. Each `*_scu` call below runs on a napi worker thread (via `AsyncTask`), not the
/// JS thread, which is exactly what `ThreadsafeFunction` is for.
struct NapiDimseLogger {
    callback: ThreadsafeFunction<String>,
}

impl DimseLogger for NapiDimseLogger {
    fn log(&self, message: String) {
        self.callback.call(Ok(message), ThreadsafeFunctionCallMode::NonBlocking);
    }
}

fn dimse_logger(on_log: Option<ThreadsafeFunction<String>>) -> Option<Box<dyn DimseLogger>> {
    on_log.map(|callback| Box::new(NapiDimseLogger { callback }) as Box<dyn DimseLogger>)
}

// ---------------------------------------------------------------------------------------------
// Shared SCU call/handle machinery
//
// Every `echo`/`store`/`find`/`move` SCU call below is started on its own `std::thread::spawn`
// (not napi's `AsyncTask`/libuv threadpool - that pool is small, default 4, and shared by every
// native call in this addon; a long-running `move` call permanently occupying one of those slots
// via `Task::compute` would leave a concurrent `abort()`/`release()` - if implemented as another
// `AsyncTask` - queued behind unrelated threadpool work, undermining the very acknowledgement
// guarantee this exists to provide). The corresponding `*ScuHandle` returned to JS is a thin,
// synchronous wrapper around a `CancelSignal` (dcmnorm's low-level per-call cancel flag) and this
// `ScuCallState<T>`, which the handle's `result()`/`release()`/`abort()` methods each poll via a
// short-lived `AsyncTask<ScuWaitTask<T>>` - the *only* thing that touches the libuv threadpool per
// call, and only for as long as it takes to observe the background thread's own completion.
//
// napi-rs requires concrete (monomorphized) types crossing the FFI boundary - there is no single
// `dyn ScuHandle` type callable from JS. This is one implementation written once, instantiated
// four times (`EchoScuHandle`/`StoreScuHandle`/`FindScuHandle`/`MoveScuHandle` below) - the
// practical equivalent of a shared interface given that constraint.
pub struct ScuCallState<T> {
    // The error side is a `String`, not `dcmnorm::DimseError` - the latter wraps foreign
    // non-`Clone` types (`std::io::Error`, `dcmnorm_dimse`'s association error, ...), and every
    // waiter here needs its own owned copy of whichever outcome was recorded first.
    result: Mutex<Option<std::result::Result<T, String>>>,
    condvar: Condvar,
}

impl<T: Clone + Send + 'static> ScuCallState<T> {
    // Unlike every `Task::compute()` elsewhere in this file, `work` doesn't run inside napi's own
    // `guarded()`-wrapped call machinery (see the module doc above for why this runs on a bare
    // `std::thread::spawn` instead) - so a panic in `work` has to be caught right here. Without
    // this, a panic would unwind this thread before the `result`/`notify_all` lines below ever
    // run, leaving every current and future `result()`/`release()`/`abort()` waiter blocked on
    // the condvar forever instead of seeing a rejected Promise.
    fn spawn(work: impl FnOnce() -> std::result::Result<T, String> + Send + 'static) -> Arc<Self> {
        let state = Arc::new(Self { result: Mutex::new(None), condvar: Condvar::new() });
        let state_thread = state.clone();
        std::thread::spawn(move || {
            let outcome = panic::catch_unwind(AssertUnwindSafe(work)).unwrap_or_else(|payload| {
                let message = payload
                    .downcast_ref::<&str>()
                    .map(|s| s.to_string())
                    .or_else(|| payload.downcast_ref::<String>().cloned())
                    .unwrap_or_else(|| "dcmnorm panicked while processing this call".to_string());
                Err(format!("dcmnorm internal error: {message}"))
            });
            *state_thread.result.lock().unwrap() = Some(outcome);
            state_thread.condvar.notify_all();
        });
        state
    }

    /// Blocks the calling (napi worker-pool) thread - never the JS main thread - until `work`
    /// above has finished. Idempotent: any number of waiters just observe the same result.
    fn block_until_done(&self) -> std::result::Result<T, String> {
        let mut guard = self.result.lock().unwrap();
        while guard.is_none() {
            guard = self.condvar.wait(guard).unwrap();
        }
        guard.clone().unwrap()
    }
}

/// A `Task` whose `compute()` just calls `state.block_until_done()`, optionally requesting
/// cancellation first - shared by every `*ScuHandle`'s `result()`/`release()`/`abort()` methods,
/// differing only in what (if anything) `request_cancel` asks for.
pub struct ScuWaitTask<T> {
    state: Arc<ScuCallState<T>>,
    request_cancel: Option<(Arc<CancelSignal>, DcmCancelMode)>,
}

impl<T: Clone + Send + ToNapiValue + TypeName + 'static> Task for ScuWaitTask<T> {
    type Output = T;
    type JsValue = T;

    fn compute(&mut self) -> Result<Self::Output> {
        if let Some((signal, mode)) = self.request_cancel.take() {
            signal.request(mode);
        }
        self.state.block_until_done().map_err(Error::from_reason)
    }

    fn resolve(&mut self, _env: Env, output: Self::Output) -> Result<Self::JsValue> {
        Ok(output)
    }
}

#[napi(object)]
#[derive(Default)]
pub struct EchoScuOptions {
    pub calling_ae_title: Option<String>,
    pub called_ae_title: Option<String>,
    pub timeout_ms: Option<u32>,
}

pub struct EchoScuTask {
    destination: String,
    options: EchoScuOptions,
    on_log: Option<ThreadsafeFunction<String>>,
}

impl Task for EchoScuTask {
    type Output = u16;
    type JsValue = u32;

    fn compute(&mut self) -> Result<Self::Output> {
        guarded(|| {
            dcm_echo_scu(
                &self.destination,
                DcmEchoScuOptions {
                    calling_ae_title: self.options.calling_ae_title.clone().unwrap_or_else(|| "DCMNORM".to_owned()),
                    called_ae_title: self.options.called_ae_title.clone(),
                    timeout: self.options.timeout_ms.map(|ms| Duration::from_millis(ms as u64)),
                    on_log: dimse_logger(self.on_log.take()),
                    cancel: None,
                },
            )
            .map_err(to_napi_err)
        })
    }

    fn resolve(&mut self, _env: Env, output: Self::Output) -> Result<Self::JsValue> {
        Ok(output as u32)
    }
}

/// Performs a C-ECHO (DICOM Verification) against `destination` ("host:port"). Resolves with the
/// response Status code (0 = success); rejects only if the association itself could not be
/// established (unreachable host, no accepted presentation context, etc). `onLog`, if given, is
/// called (synchronously, no return value expected) with a debug line for each notable DIMSE
/// event - association open/close, the request sent, the response received.
#[napi]
pub fn echo_scu(
    destination: String,
    options: Option<EchoScuOptions>,
    on_log: Option<ThreadsafeFunction<String>>,
) -> AsyncTask<EchoScuTask> {
    AsyncTask::new(EchoScuTask { destination, options: options.unwrap_or_default(), on_log })
}

#[napi]
pub struct EchoScuHandle {
    cancel: Arc<CancelSignal>,
    state: Arc<ScuCallState<u32>>,
}

#[napi]
impl EchoScuHandle {
    /// The eventual Status code (0 = success), without requesting cancellation.
    #[napi(ts_return_type = "Promise<number>")]
    pub fn result(&self) -> AsyncTask<ScuWaitTask<u32>> {
        AsyncTask::new(ScuWaitTask { state: self.state.clone(), request_cancel: None })
    }

    /// Requests a graceful A-RELEASE and resolves once the association has genuinely closed -
    /// real acknowledgement, not a fire-and-forget signal. Idempotent.
    #[napi(ts_return_type = "Promise<number>")]
    pub fn release(&self) -> AsyncTask<ScuWaitTask<u32>> {
        AsyncTask::new(ScuWaitTask { state: self.state.clone(), request_cancel: Some((self.cancel.clone(), DcmCancelMode::Release)) })
    }

    /// Requests an immediate A-ABORT (no wait on the peer at all) and resolves once the
    /// association has genuinely closed. Idempotent.
    #[napi(ts_return_type = "Promise<number>")]
    pub fn abort(&self) -> AsyncTask<ScuWaitTask<u32>> {
        AsyncTask::new(ScuWaitTask { state: self.state.clone(), request_cancel: Some((self.cancel.clone(), DcmCancelMode::Abort)) })
    }
}

/// Same as `echoScu`, but returns immediately with a handle instead of blocking until the
/// C-ECHO completes - lets a caller `abort()`/`release()` it early. See `EchoScuHandle`.
#[napi]
pub fn start_echo_scu(
    destination: String,
    options: Option<EchoScuOptions>,
    on_log: Option<ThreadsafeFunction<String>>,
) -> EchoScuHandle {
    let options = options.unwrap_or_default();
    let cancel = CancelSignal::new();
    let cancel_for_call = cancel.clone();
    let calling_ae_title = options.calling_ae_title.unwrap_or_else(|| "DCMNORM".to_owned());
    let called_ae_title = options.called_ae_title;
    let timeout = options.timeout_ms.map(|ms| Duration::from_millis(ms as u64));
    let logger = dimse_logger(on_log);

    let state = ScuCallState::spawn(move || {
        dcm_echo_scu(
            &destination,
            DcmEchoScuOptions { calling_ae_title, called_ae_title, timeout, on_log: logger, cancel: Some(cancel_for_call) },
        )
        .map(|status| status as u32)
        .map_err(|error| error.to_string())
    });

    EchoScuHandle { cancel, state }
}

#[napi(object)]
#[derive(Default)]
pub struct StoreScuOptions {
    pub calling_ae_title: Option<String>,
    pub called_ae_title: Option<String>,
    pub max_pdu_length: Option<u32>,
    pub never_transcode: Option<bool>,
    /// Absolute ceiling (connect through release) for the whole call - not reset by activity.
    pub timeout_ms: Option<u32>,
}

#[napi(object)]
#[derive(Clone)]
pub struct StoreScuResult {
    pub sop_instance_uid: String,
    pub status: u32,
}

pub struct StoreScuTask {
    destination: String,
    files: Vec<PathBuf>,
    options: StoreScuOptions,
    on_log: Option<ThreadsafeFunction<String>>,
}

impl Task for StoreScuTask {
    type Output = Vec<StoreScuResult>;
    type JsValue = Vec<StoreScuResult>;

    fn compute(&mut self) -> Result<Self::Output> {
        guarded(|| {
            let results = dcm_store_scu(
                &self.destination,
                &self.files,
                DcmStoreScuOptions {
                    calling_ae_title: self.options.calling_ae_title.clone().unwrap_or_else(|| "DCMNORM".to_owned()),
                    called_ae_title: self.options.called_ae_title.clone(),
                    max_pdu_length: self.options.max_pdu_length.unwrap_or(16384),
                    never_transcode: self.options.never_transcode.unwrap_or(false),
                    timeout: self.options.timeout_ms.map(|ms| Duration::from_millis(ms as u64)),
                    on_log: dimse_logger(self.on_log.take()),
                    cancel: None,
                },
            )
            .map_err(to_napi_err)?;
            Ok(results
                .into_iter()
                .map(|result| StoreScuResult { sop_instance_uid: result.sop_instance_uid, status: result.status as u32 })
                .collect())
        })
    }

    fn resolve(&mut self, _env: Env, output: Self::Output) -> Result<Self::JsValue> {
        Ok(output)
    }
}

/// Sends each of `files` via C-STORE to `destination` ("host:port"). Resolves with one
/// `{sopInstanceUid, status}` per file that could be read and sent - a non-zero Status is just
/// data in the result (the peer rejected that instance), not a rejected Promise; this only
/// rejects if the association itself could not be established, or none of `files` could be read
/// as DICOM at all. `onLog`, if given, is called (synchronously, no return value expected) with a
/// debug line for each notable DIMSE event - association open/close, and each C-STORE-RQ/RSP.
#[napi]
pub fn store_scu(
    destination: String,
    files: Vec<String>,
    options: Option<StoreScuOptions>,
    on_log: Option<ThreadsafeFunction<String>>,
) -> AsyncTask<StoreScuTask> {
    AsyncTask::new(StoreScuTask {
        destination,
        files: files.into_iter().map(PathBuf::from).collect(),
        options: options.unwrap_or_default(),
        on_log,
    })
}

#[napi]
pub struct StoreScuHandle {
    cancel: Arc<CancelSignal>,
    state: Arc<ScuCallState<Vec<StoreScuResult>>>,
}

#[napi]
impl StoreScuHandle {
    /// The eventual per-file `{sopInstanceUid, status}` results, without requesting
    /// cancellation.
    #[napi(ts_return_type = "Promise<Array<StoreScuResult>>")]
    pub fn result(&self) -> AsyncTask<ScuWaitTask<Vec<StoreScuResult>>> {
        AsyncTask::new(ScuWaitTask { state: self.state.clone(), request_cancel: None })
    }

    /// Requests a graceful A-RELEASE and resolves once the association has genuinely closed -
    /// real acknowledgement, not a fire-and-forget signal. Idempotent.
    #[napi(ts_return_type = "Promise<Array<StoreScuResult>>")]
    pub fn release(&self) -> AsyncTask<ScuWaitTask<Vec<StoreScuResult>>> {
        AsyncTask::new(ScuWaitTask { state: self.state.clone(), request_cancel: Some((self.cancel.clone(), DcmCancelMode::Release)) })
    }

    /// Requests an immediate A-ABORT (no wait on the peer at all) and resolves once the
    /// association has genuinely closed. Idempotent.
    #[napi(ts_return_type = "Promise<Array<StoreScuResult>>")]
    pub fn abort(&self) -> AsyncTask<ScuWaitTask<Vec<StoreScuResult>>> {
        AsyncTask::new(ScuWaitTask { state: self.state.clone(), request_cancel: Some((self.cancel.clone(), DcmCancelMode::Abort)) })
    }
}

/// Same as `storeScu`, but returns immediately with a handle instead of blocking until every
/// file has been sent - lets a caller `abort()`/`release()` it early. See `StoreScuHandle`.
#[napi]
pub fn start_store_scu(
    destination: String,
    files: Vec<String>,
    options: Option<StoreScuOptions>,
    on_log: Option<ThreadsafeFunction<String>>,
) -> StoreScuHandle {
    let options = options.unwrap_or_default();
    let files: Vec<PathBuf> = files.into_iter().map(PathBuf::from).collect();
    let cancel = CancelSignal::new();
    let cancel_for_call = cancel.clone();
    let calling_ae_title = options.calling_ae_title.unwrap_or_else(|| "DCMNORM".to_owned());
    let called_ae_title = options.called_ae_title;
    let max_pdu_length = options.max_pdu_length.unwrap_or(16384);
    let never_transcode = options.never_transcode.unwrap_or(false);
    let timeout = options.timeout_ms.map(|ms| Duration::from_millis(ms as u64));
    let logger = dimse_logger(on_log);

    let state = ScuCallState::spawn(move || {
        dcm_store_scu(
            &destination,
            &files,
            DcmStoreScuOptions {
                calling_ae_title,
                called_ae_title,
                max_pdu_length,
                never_transcode,
                timeout,
                on_log: logger,
                cancel: Some(cancel_for_call),
            },
        )
        .map(|results| {
            results
                .into_iter()
                .map(|result| StoreScuResult { sop_instance_uid: result.sop_instance_uid, status: result.status as u32 })
                .collect()
        })
        .map_err(|error| error.to_string())
    });

    StoreScuHandle { cancel, state }
}

#[napi(object)]
#[derive(Default)]
pub struct FindScuOptions {
    pub calling_ae_title: Option<String>,
    pub called_ae_title: Option<String>,
    pub max_pdu_length: Option<u32>,
    pub timeout_ms: Option<u32>,
}

pub struct FindScuTask {
    destination: String,
    query: HashMap<String, String>,
    options: FindScuOptions,
    on_log: Option<ThreadsafeFunction<String>>,
}

impl Task for FindScuTask {
    type Output = Vec<String>;
    type JsValue = Vec<String>;

    fn compute(&mut self) -> Result<Self::Output> {
        guarded(|| {
            dcm_find_scu(
                &self.destination,
                &self.query,
                DcmFindScuOptions {
                    calling_ae_title: self.options.calling_ae_title.clone().unwrap_or_else(|| "DCMNORM".to_owned()),
                    called_ae_title: self.options.called_ae_title.clone(),
                    max_pdu_length: self.options.max_pdu_length.unwrap_or(16384),
                    timeout: self.options.timeout_ms.map(|ms| Duration::from_millis(ms as u64)),
                    on_log: dimse_logger(self.on_log.take()),
                    cancel: None,
                },
            )
            .map_err(to_napi_err)
        })
    }

    fn resolve(&mut self, _env: Env, output: Self::Output) -> Result<Self::JsValue> {
        Ok(output)
    }
}

/// Performs a C-FIND (Study Root Query/Retrieve) against `destination` ("host:port"). `query`
/// values: empty string is a universal-match "return key" (mirrors findscu's bare `-k TAG`),
/// non-empty constrains the match (mirrors `-k TAG=value`); `QueryRetrieveLevel` defaults to
/// `"STUDY"` if not given. Resolves with one flat/hex-keyed DICOM JSON string per match (parse
/// JS-side, same shape as `readJson({format: "flat", keyStyle: "hex"})`). `onLog`, if given, is
/// called (synchronously, no return value expected) with a debug line for each notable DIMSE
/// event - association open/close, and each C-FIND-RQ/RSP (query values are not logged, only the
/// tag keys queried, since the Identifier commonly carries PHI).
#[napi]
pub fn find_scu(
    destination: String,
    query: HashMap<String, String>,
    options: Option<FindScuOptions>,
    on_log: Option<ThreadsafeFunction<String>>,
) -> AsyncTask<FindScuTask> {
    AsyncTask::new(FindScuTask { destination, query, options: options.unwrap_or_default(), on_log })
}

#[napi]
pub struct FindScuHandle {
    cancel: Arc<CancelSignal>,
    state: Arc<ScuCallState<Vec<String>>>,
}

#[napi]
impl FindScuHandle {
    /// The eventual matched Identifiers (flat/hex-keyed DICOM JSON, one per match), without
    /// requesting cancellation.
    #[napi(ts_return_type = "Promise<Array<string>>")]
    pub fn result(&self) -> AsyncTask<ScuWaitTask<Vec<String>>> {
        AsyncTask::new(ScuWaitTask { state: self.state.clone(), request_cancel: None })
    }

    /// Requests a graceful A-RELEASE and resolves once the association has genuinely closed -
    /// real acknowledgement, not a fire-and-forget signal. Idempotent.
    #[napi(ts_return_type = "Promise<Array<string>>")]
    pub fn release(&self) -> AsyncTask<ScuWaitTask<Vec<String>>> {
        AsyncTask::new(ScuWaitTask { state: self.state.clone(), request_cancel: Some((self.cancel.clone(), DcmCancelMode::Release)) })
    }

    /// Requests an immediate A-ABORT (no wait on the peer at all) and resolves once the
    /// association has genuinely closed. Idempotent.
    #[napi(ts_return_type = "Promise<Array<string>>")]
    pub fn abort(&self) -> AsyncTask<ScuWaitTask<Vec<String>>> {
        AsyncTask::new(ScuWaitTask { state: self.state.clone(), request_cancel: Some((self.cancel.clone(), DcmCancelMode::Abort)) })
    }
}

/// Same as `findScu`, but returns immediately with a handle instead of blocking until the
/// C-FIND completes - lets a caller `abort()`/`release()` it early. See `FindScuHandle`.
#[napi]
pub fn start_find_scu(
    destination: String,
    query: HashMap<String, String>,
    options: Option<FindScuOptions>,
    on_log: Option<ThreadsafeFunction<String>>,
) -> FindScuHandle {
    let options = options.unwrap_or_default();
    let cancel = CancelSignal::new();
    let cancel_for_call = cancel.clone();
    let calling_ae_title = options.calling_ae_title.unwrap_or_else(|| "DCMNORM".to_owned());
    let called_ae_title = options.called_ae_title;
    let max_pdu_length = options.max_pdu_length.unwrap_or(16384);
    let timeout = options.timeout_ms.map(|ms| Duration::from_millis(ms as u64));
    let logger = dimse_logger(on_log);

    let state = ScuCallState::spawn(move || {
        dcm_find_scu(
            &destination,
            &query,
            DcmFindScuOptions { calling_ae_title, called_ae_title, max_pdu_length, timeout, on_log: logger, cancel: Some(cancel_for_call) },
        )
        .map_err(|error| error.to_string())
    });

    FindScuHandle { cancel, state }
}

#[napi(object)]
#[derive(Default)]
pub struct MoveScuOptions {
    pub calling_ae_title: Option<String>,
    pub called_ae_title: Option<String>,
    pub max_pdu_length: Option<u32>,
    /// Absolute ceiling (connect through release) for the whole call - not reset by activity.
    pub timeout_ms: Option<u32>,
    /// Directory to watch for local write progress (e.g. this retrieve's own cache destination)
    /// - see `staleDataTimeoutMs`. Both must be set for the watch to run.
    pub watch_path: Option<String>,
    /// How long `watchPath` may go without a new/modified file before the connection is
    /// considered stale and aborted - independent of, and typically much shorter than,
    /// `timeoutMs`.
    pub stale_data_timeout_ms: Option<u32>,
    /// `"STUDY"` (default: the whole study, same Identifier as before this option existed),
    /// `"SERIES"` (needs `seriesInstanceUid`) or `"IMAGE"` (needs `seriesInstanceUid` and a
    /// non-empty `sopInstanceUids`). An invalid combination rejects `.result()` without opening
    /// an association.
    pub query_retrieve_level: Option<String>,
    /// SeriesInstanceUID for a `"SERIES"`/`"IMAGE"` retrieve; ignored for `"STUDY"`.
    pub series_instance_uid: Option<String>,
    /// SOPInstanceUIDs for an `"IMAGE"` retrieve, sent as one multi-valued UID list; ignored
    /// otherwise.
    pub sop_instance_uids: Option<Vec<String>>,
}

#[napi(object)]
#[derive(Clone)]
pub struct MoveScuResult {
    pub status: u32,
    pub completed: u32,
    pub failed: u32,
    pub warning: u32,
    pub remaining: u32,
    pub cancelled: bool,
    /// `"release"`/`"abort"` when `cancelled` is true, indicating which teardown verb actually
    /// produced it (see `MoveScuHandle.release`/`.abort`) - `null` for a real terminal C-MOVE-RSP.
    pub cancelled_via: Option<String>,
}

#[napi]
pub struct MoveScuHandle {
    cancel: Arc<CancelSignal>,
    state: Arc<ScuCallState<MoveScuResult>>,
}

#[napi]
impl MoveScuHandle {
    /// The eventual terminal `MoveScuResult`, without requesting cancellation - for a caller
    /// that just wants to await completion.
    #[napi(ts_return_type = "Promise<MoveScuResult>")]
    pub fn result(&self) -> AsyncTask<ScuWaitTask<MoveScuResult>> {
        AsyncTask::new(ScuWaitTask { state: self.state.clone(), request_cancel: None })
    }

    /// Requests a graceful A-RELEASE and resolves once the association has genuinely closed -
    /// real acknowledgement, not a fire-and-forget signal that might land seconds later (or
    /// never, if missed). Idempotent: a second call just observes the same outcome.
    #[napi(ts_return_type = "Promise<MoveScuResult>")]
    pub fn release(&self) -> AsyncTask<ScuWaitTask<MoveScuResult>> {
        AsyncTask::new(ScuWaitTask { state: self.state.clone(), request_cancel: Some((self.cancel.clone(), DcmCancelMode::Release)) })
    }

    /// Requests an immediate A-ABORT (no wait on the peer at all) and resolves once the
    /// association has genuinely closed. Idempotent.
    #[napi(ts_return_type = "Promise<MoveScuResult>")]
    pub fn abort(&self) -> AsyncTask<ScuWaitTask<MoveScuResult>> {
        AsyncTask::new(ScuWaitTask { state: self.state.clone(), request_cancel: Some((self.cancel.clone(), DcmCancelMode::Abort)) })
    }
}

/// Performs a C-MOVE (Study Root Query/Retrieve), asking `destination` ("host:port") to push
/// `studyInstanceUid` - or one series of it / a list of its instances, per
/// `options.queryRetrieveLevel` - to `moveDestinationAe` (an AE title `destination` already knows how to
/// reach, not a socket address). Returns a handle immediately - the retrieve itself runs on its
/// own background thread - rather than blocking until it reaches a terminal status; call
/// `.result()` to await that terminal status and sub-operation counts (regardless of
/// success/warning/failure, mirroring the old behavior), or `.release()`/`.abort()` to close it
/// early once some other signal (e.g. the study already being confirmed fully received via a
/// separate channel) makes further waiting pointless. See `MoveScuHandle`.
///
/// `onLog`, if given, is called (synchronously, no return value expected) with a debug line for
/// each notable DIMSE event - association open/close, and each C-MOVE-RQ/RSP (including every
/// pending response, so a slow multi-instance move is visible sub-operation by sub-operation).
#[napi]
pub fn move_scu(
    destination: String,
    move_destination_ae: String,
    study_instance_uid: String,
    options: Option<MoveScuOptions>,
    on_log: Option<ThreadsafeFunction<String>>,
) -> MoveScuHandle {
    let options = options.unwrap_or_default();
    let cancel = CancelSignal::new();
    let cancel_for_call = cancel.clone();
    let calling_ae_title = options.calling_ae_title.unwrap_or_else(|| "DCMNORM".to_owned());
    let called_ae_title = options.called_ae_title;
    let max_pdu_length = options.max_pdu_length.unwrap_or(16384);
    let timeout = options.timeout_ms.map(|ms| Duration::from_millis(ms as u64));
    let stale_data_path = options.watch_path.map(PathBuf::from);
    let stale_data_timeout = options.stale_data_timeout_ms.map(|ms| Duration::from_millis(ms as u64));
    let query_retrieve_level = options.query_retrieve_level;
    let series_instance_uid = options.series_instance_uid;
    let sop_instance_uids = options.sop_instance_uids.unwrap_or_default();
    let logger = dimse_logger(on_log);

    let state = ScuCallState::spawn(move || {
        let query_retrieve_level = query_retrieve_level
            .map(|level| level.parse::<DcmQueryRetrieveLevel>().map_err(|error| error.to_string()))
            .transpose()?;
        dcm_move_scu(
            &destination,
            &move_destination_ae,
            &study_instance_uid,
            DcmMoveScuOptions {
                calling_ae_title,
                called_ae_title,
                max_pdu_length,
                timeout,
                stale_data_path,
                stale_data_timeout,
                on_log: logger,
                cancel: Some(cancel_for_call),
                query_retrieve_level,
                series_instance_uid,
                sop_instance_uids,
            },
        )
        .map(|result| MoveScuResult {
            status: result.status as u32,
            completed: result.completed as u32,
            failed: result.failed as u32,
            warning: result.warning as u32,
            remaining: result.remaining as u32,
            cancelled: result.cancelled,
            cancelled_via: result.cancelled_via.map(|mode| match mode {
                DcmCancelMode::Release => "release".to_owned(),
                DcmCancelMode::Abort => "abort".to_owned(),
            }),
        })
        .map_err(|error| error.to_string())
    });

    MoveScuHandle { cancel, state }
}

// ---------------------------------------------------------------------------------------------
// Rendering: renderFrame / renderMovie
// ---------------------------------------------------------------------------------------------

fn unique_temp_path(extension: &str) -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);

    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let sequence = COUNTER.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "dcmnorm-node-{}-{nanos}-{sequence}.{extension}",
        std::process::id()
    ))
}

#[napi(object)]
#[derive(Default)]
pub struct RenderFrameOptions {
    /// 'jpeg' (default) or 'png'.
    pub format: Option<String>,
    pub output_width: Option<u32>,
    pub output_height: Option<u32>,
    pub window_center: Option<f64>,
    pub window_width: Option<f64>,
    pub frame_index: Option<u32>,
    pub jpeg_quality: Option<u32>,
    /// Whether to composite an overlay plane, when the instance has one. Defaults to `true` -
    /// the first available overlay renders by default.
    pub show_overlays: Option<bool>,
    /// Which overlay to render, by its 0-based `OverlaySummary.index`. Defaults to the first
    /// available overlay. Ignored when `showOverlays` is `false`.
    pub overlay_index: Option<u32>,
    /// Fill color for rendered overlay pixels, as `"#RRGGBB"` or `"R,G,B"`. Defaults to green.
    pub overlay_color: Option<String>,
    /// When `true`, attempts to serve the frame's own original JPEG Baseline bytes untouched
    /// (no decode/re-encode) instead of the usual render pipeline - see
    /// `try_extract_passthrough_jpeg_frame`'s own doc for the exact eligibility conditions
    /// (color, JPEG Baseline source, no overlay, one fragment per frame). Falls through
    /// transparently to the ordinary render when any condition isn't met - `RenderedFrame.
    /// passthrough` tells the caller which path was actually taken. Defaults to `false`
    /// (unset/omitted callers get byte-for-byte identical behavior to before this option existed).
    pub passthrough: Option<bool>,
}

/// Mirrors `dcmnorm::dicom_io::OverlaySummary` for the JS side.
#[napi(object)]
pub struct OverlaySummary {
    pub index: u32,
    pub group: u32,
    pub rows: u32,
    pub columns: u32,
    pub overlay_type: Option<String>,
    pub label: Option<String>,
}

impl From<DcmOverlaySummary> for OverlaySummary {
    fn from(value: DcmOverlaySummary) -> Self {
        OverlaySummary {
            index: value.index as u32,
            group: value.group as u32,
            rows: value.rows as u32,
            columns: value.columns as u32,
            overlay_type: value.overlay_type,
            label: value.label,
        }
    }
}

#[napi(object)]
pub struct RenderedFrame {
    pub mime_type: String,
    pub width: u32,
    pub height: u32,
    pub data: Buffer,
    /// All overlay planes present on the source instance, regardless of whether one was
    /// rendered into `data`.
    pub overlays: Vec<OverlaySummary>,
    /// Which overlay (by `OverlaySummary.index`) was actually composited into `data`, if any.
    pub selected_overlay_index: Option<u32>,
    /// `true` when `data` is the frame's own original bytes, delivered untouched (see
    /// `RenderFrameOptions.passthrough`) - `false` for the ordinary decode/re-encode path,
    /// including whenever `passthrough` was requested but the frame didn't qualify.
    pub passthrough: bool,
}

fn parse_render_output_format(value: Option<&str>) -> Result<(DcmRenderOutputFormat, &'static str)> {
    match value {
        None | Some("jpeg") | Some("jpg") => Ok((DcmRenderOutputFormat::Jpeg, "image/jpeg")),
        Some("png") => Ok((DcmRenderOutputFormat::Png, "image/png")),
        Some(other) => Err(Error::from_reason(format!(
            "invalid format '{other}'; expected 'jpeg' or 'png'"
        ))),
    }
}

/// Parses `"#RRGGBB"` or `"R,G,B"` into an `[R, G, B]` byte triple. A standalone parser (not
/// shared with the CLI's `parse_redact_color` in `exec/dcmnorm`) since this crate doesn't depend
/// on the CLI crate.
fn parse_render_color(value: &str) -> Result<[u8; 3]> {
    let trimmed = value.trim();

    if let Some(hex) = trimmed.strip_prefix('#') {
        if hex.len() != 6 {
            return Err(Error::from_reason(format!(
                "invalid color '{value}'; hex color must be #RRGGBB"
            )));
        }
        let channel = |range: std::ops::Range<usize>| {
            u8::from_str_radix(&hex[range], 16)
                .map_err(|_| Error::from_reason(format!("invalid color '{value}'; not a valid hex color")))
        };
        return Ok([channel(0..2)?, channel(2..4)?, channel(4..6)?]);
    }

    let parts: Vec<&str> = trimmed.split(',').collect();
    if parts.len() != 3 {
        return Err(Error::from_reason(format!(
            "invalid color '{value}'; expected R,G,B (0-255 each) or #RRGGBB"
        )));
    }
    let mut channels = [0u8; 3];
    for (index, part) in parts.iter().enumerate() {
        channels[index] = part.trim().parse::<u8>().map_err(|_| {
            Error::from_reason(format!(
                "invalid color '{value}'; expected R,G,B (0-255 each) or #RRGGBB"
            ))
        })?;
    }
    Ok(channels)
}

fn overlay_pipeline_fields(
    show_overlays: Option<bool>,
    overlay_index: Option<u32>,
    overlay_color: Option<&str>,
) -> Result<(bool, Option<usize>, [u8; 3])> {
    let color = overlay_color.map(parse_render_color).transpose()?.unwrap_or([0, 255, 0]);
    Ok((
        show_overlays.unwrap_or(true),
        overlay_index.map(|value| value as usize),
        color,
    ))
}

pub struct RenderFrameTask {
    file_path: PathBuf,
    options: RenderFrameOptions,
}

impl Task for RenderFrameTask {
    type Output = RenderedFrame;
    type JsValue = RenderedFrame;

    fn compute(&mut self) -> Result<Self::Output> {
        guarded(|| {
            let (format, mime_type) = parse_render_output_format(self.options.format.as_deref())?;
            let (show_overlays, overlay_index, overlay_color) = overlay_pipeline_fields(
                self.options.show_overlays,
                self.options.overlay_index,
                self.options.overlay_color.as_deref(),
            )?;
            let frame_index = self.options.frame_index.unwrap_or(0) as usize;
            let object = read_dicom_file_for_single_frame(&self.file_path, frame_index)?;

            if self.options.passthrough.unwrap_or(false) {
                if let Some(frame) =
                    dcm_try_extract_passthrough_jpeg_frame(&object, frame_index).map_err(to_napi_err)?
                {
                    return Ok(RenderedFrame {
                        mime_type: "image/jpeg".to_owned(),
                        width: frame.width as u32,
                        height: frame.height as u32,
                        data: frame.bytes.into(),
                        overlays: frame.overlays.into_iter().map(OverlaySummary::from).collect(),
                        selected_overlay_index: frame.selected_overlay_index.map(|value| value as u32),
                        passthrough: true,
                    });
                }
            }

            let pipeline_options = DcmRenderPipelineOptions {
                frame_index,
                window_center: self.options.window_center,
                window_width: self.options.window_width,
                output_width: self.options.output_width,
                output_height: self.options.output_height,
                jpeg_quality: self.options.jpeg_quality.unwrap_or(90) as u8,
                show_overlays,
                overlay_index,
                overlay_color,
                ..Default::default()
            };
            let rendered = dcm_render_dicom_frame(&object, format, &pipeline_options).map_err(to_napi_err)?;
            Ok(RenderedFrame {
                mime_type: mime_type.to_owned(),
                width: rendered.width as u32,
                height: rendered.height as u32,
                data: rendered.bytes.into(),
                overlays: rendered.overlays.into_iter().map(OverlaySummary::from).collect(),
                selected_overlay_index: rendered.selected_overlay_index.map(|value| value as u32),
                passthrough: false,
            })
        })
    }

    fn resolve(&mut self, _env: Env, output: Self::Output) -> Result<Self::JsValue> {
        Ok(output)
    }
}

/// Renders a single frame of a DICOM file to JPEG or PNG. Mirrors `dcmnorm --output-width ...
/// --output-height ... --window-center ... --window-width ... --render-frame ... file.dcm out.jpg`.
#[napi]
pub fn render_frame(file_path: String, options: Option<RenderFrameOptions>) -> AsyncTask<RenderFrameTask> {
    AsyncTask::new(RenderFrameTask {
        file_path: PathBuf::from(file_path),
        options: options.unwrap_or_default(),
    })
}

#[napi(object)]
#[derive(Default)]
pub struct RenderMovieOptions {
    pub output_width: Option<u32>,
    pub output_height: Option<u32>,
    pub window_center: Option<f64>,
    pub window_width: Option<f64>,
    /// Defaults to 24 (matching the CLI's default when the DICOM file itself specifies no frame rate).
    pub fps: Option<f64>,
}

#[napi(object)]
pub struct RenderedMovie {
    pub mime_type: String,
    pub data: Buffer,
}

pub struct RenderMovieTask {
    file_path: PathBuf,
    options: RenderMovieOptions,
}

impl Task for RenderMovieTask {
    type Output = RenderedMovie;
    type JsValue = RenderedMovie;

    fn compute(&mut self) -> Result<Self::Output> {
        guarded(|| {
            let object = read_dicom_file(&self.file_path).map_err(to_napi_err)?;
            let pipeline_options = DcmRenderPipelineOptions {
                window_center: self.options.window_center,
                window_width: self.options.window_width,
                output_width: self.options.output_width,
                output_height: self.options.output_height,
                ..Default::default()
            };
            let fps = self.options.fps.unwrap_or(24.0);

            // write_dicom_video needs a real (seekable) output file for ffmpeg's muxer - there's
            // no way to mux MP4/MOV to an in-memory buffer or unseekable pipe. Render to a private
            // temp file, then read it back into the Buffer napi hands to JS, same trade-off the
            // CLI itself makes (it also always writes to a real file).
            let temp_path = unique_temp_path("mp4");
            let result = dcm_write_dicom_video(&object, &temp_path, &pipeline_options, fps).map_err(to_napi_err);
            let bytes = result.and_then(|_| std::fs::read(&temp_path).map_err(|e| to_napi_err(e)));
            let _ = std::fs::remove_file(&temp_path);

            Ok(RenderedMovie {
                mime_type: "video/mp4".to_owned(),
                data: bytes?.into(),
            })
        })
    }

    fn resolve(&mut self, _env: Env, output: Self::Output) -> Result<Self::JsValue> {
        Ok(output)
    }
}

/// Renders every frame of a multi-frame DICOM file to an MP4 (via a piped `ffmpeg` subprocess -
/// requires `ffmpeg` on `PATH`). Mirrors `dcmnorm --render-fps ... --output-width ... file.dcm out.mp4`.
#[napi]
pub fn render_movie(file_path: String, options: Option<RenderMovieOptions>) -> AsyncTask<RenderMovieTask> {
    AsyncTask::new(RenderMovieTask {
        file_path: PathBuf::from(file_path),
        options: options.unwrap_or_default(),
    })
}

// ---------------------------------------------------------------------------------------------
// Histogram: one bin-count array per frame, over the same modality-LUT-applied grayscale
// values render_frame/renderMovie decode from (so bins line up with what the viewer's hover
// probe already reports for the same frame - see dcmnorm::dicom_io::histogram's own doc).
// ---------------------------------------------------------------------------------------------

#[napi(object)]
#[derive(Default)]
pub struct HistogramOptions {
    /// Number of bins per frame histogram. Defaults to 256.
    pub bin_count: Option<u32>,
    /// Compute only this zero-based frame's histogram, instead of every frame in the instance.
    pub frame_index: Option<u32>,
    /// Lower bound of the binned value range. Defaults to each frame's own observed minimum.
    /// Must be set together with `maxValue`.
    pub min_value: Option<f64>,
    /// Upper bound of the binned value range. Defaults to each frame's own observed maximum.
    /// Must be set together with `minValue`.
    pub max_value: Option<f64>,
}

/// Mirrors `dcmnorm::dicom_io::FrameHistogram` for the JS side.
#[napi(object)]
pub struct FrameHistogram {
    pub frame_index: u32,
    /// `null` for a grayscale frame. `"red" | "green" | "blue"` for one channel of an RGB/color
    /// frame - `computeHistogram` returns three entries per frame for those, all sharing the same
    /// `frameIndex`, each over that channel's own decoded 8-bit (0-255) samples.
    pub channel: Option<String>,
    pub bin_count: u32,
    pub range_min: f64,
    pub range_max: f64,
    pub bin_width: f64,
    pub counts: Vec<u32>,
    pub pixel_count: u32,
    pub min_value: f64,
    pub max_value: f64,
    pub mean: f64,
    pub std_dev: f64,
}

impl From<dcmnorm::dicom_io::FrameHistogram> for FrameHistogram {
    fn from(value: dcmnorm::dicom_io::FrameHistogram) -> Self {
        FrameHistogram {
            frame_index: value.frame_index as u32,
            channel: value.channel,
            bin_count: value.bin_count,
            range_min: value.range_min,
            range_max: value.range_max,
            bin_width: value.bin_width,
            counts: value.counts,
            pixel_count: value.pixel_count,
            min_value: value.min_value,
            max_value: value.max_value,
            mean: value.mean,
            std_dev: value.std_dev,
        }
    }
}

pub struct ComputeHistogramTask {
    file_path: PathBuf,
    options: HistogramOptions,
}

impl Task for ComputeHistogramTask {
    type Output = Vec<FrameHistogram>;
    type JsValue = Vec<FrameHistogram>;

    fn compute(&mut self) -> Result<Self::Output> {
        guarded(|| {
            let dcm_options = DcmHistogramOptions {
                bin_count: self.options.bin_count.unwrap_or(256),
                min_value: self.options.min_value,
                max_value: self.options.max_value,
            };

            let histograms = match self.options.frame_index {
                // One frame: read only that frame's bytes where the file allows it, like
                // renderFrame / exportFrameTexture.
                Some(frame_index) => {
                    let frame_index = frame_index as usize;
                    let object = read_dicom_file_for_single_frame(&self.file_path, frame_index)?;
                    dcm_compute_frame_histogram(&object, frame_index, &dcm_options).map_err(to_napi_err)?
                }
                None => {
                    let object = read_dicom_file(&self.file_path).map_err(to_napi_err)?;
                    dcm_compute_instance_histograms(&object, &dcm_options).map_err(to_napi_err)?
                }
            };

            Ok(histograms.into_iter().map(FrameHistogram::from).collect())
        })
    }

    fn resolve(&mut self, _env: Env, output: Self::Output) -> Result<Self::JsValue> {
        Ok(output)
    }
}

/// Computes a pixel-value histogram for every frame of a DICOM file (or just one, via
/// `options.frameIndex`). Mirrors `dcmnorm --histogram --histogram-bins ... file.dcm`.
#[napi]
pub fn compute_histogram(file_path: String, options: Option<HistogramOptions>) -> AsyncTask<ComputeHistogramTask> {
    AsyncTask::new(ComputeHistogramTask {
        file_path: PathBuf::from(file_path),
        options: options.unwrap_or_default(),
    })
}

// ---------------------------------------------------------------------------------------------
// MPR (Multiplanar Reformation): buildVolume / DicomVolumeHandle.reformat
// ---------------------------------------------------------------------------------------------
//
// buildVolume is the expensive step (reads + decodes every slice in a series), so it's exposed as
// its own async call returning an opaque, reusable `DicomVolumeHandle` - the caller (render-server)
// builds a volume once per series and keeps the handle resident (see its own volume cache) so every
// subsequent `reformat()` call for a rotate/scroll/window-level change is cheap. `DicomVolumeHandle`
// wraps an `Arc<DcmVolume>`, which is read-only after `build_volume` returns, so concurrent
// `reformat()` calls need no locking - mirrors this file's existing stateful-handle convention
// (`EchoScuHandle`, `MoveScuHandle`, etc.) rather than inventing a new one.

pub struct BuildVolumeTask {
    file_paths: Vec<PathBuf>,
}

impl Task for BuildVolumeTask {
    type Output = DcmVolume;
    type JsValue = DicomVolumeHandle;

    fn compute(&mut self) -> Result<Self::Output> {
        guarded(|| dcm_build_volume(&self.file_paths).map_err(to_napi_err))
    }

    fn resolve(&mut self, _env: Env, output: Self::Output) -> Result<Self::JsValue> {
        Ok(DicomVolumeHandle::new(output))
    }
}

/// Builds a 3D volume from a parallel stack of DICOM slice files (e.g. every image instance in
/// one CT/MR/PT series) sharing consistent `ImageOrientationPatient`, or from Enhanced
/// multi-frame objects (one Enhanced CT/MR/PET file is enough; per-frame positions and rescale
/// are honoured). Slices are spatially re-sorted internally by `ImagePositionPatient`,
/// regardless of `filePaths`' own order; a gantry-tilted stack is desheared onto an orthogonal
/// grid. Returns a `DicomVolumeHandle` for repeated `reformat()` calls. Rejects (rather than
/// silently mis-rendering) fewer than 2 slices, mismatched Rows/Columns, or a non-parallel
/// stack.
#[napi]
pub fn build_volume(file_paths: Vec<String>) -> AsyncTask<BuildVolumeTask> {
    AsyncTask::new(BuildVolumeTask {
        file_paths: file_paths.into_iter().map(PathBuf::from).collect(),
    })
}

#[napi(object)]
#[derive(Default)]
pub struct ReformatPlaneOptions {
    /// World-space (mm, patient/LPS) point at the CENTER of the reformatted output image.
    pub origin: Vec<f64>,
    /// Unit vector: direction of travel across the output image as the column index increases.
    pub row_dir: Vec<f64>,
    /// Unit vector: direction of travel down the output image as the row index increases.
    pub col_dir: Vec<f64>,
    pub output_width: u32,
    pub output_height: u32,
    /// Physical size of one output pixel, in mm - the same in both output axes.
    pub spacing_mm: f64,
    pub window_center: Option<f64>,
    pub window_width: Option<f64>,
    /// 'jpeg' (default) or 'png'.
    pub format: Option<String>,
    pub jpeg_quality: Option<u32>,
    /// 'trilinear' (default) or 'nearest' - nearest is faster and intended for a live-drag
    /// preview frame, trilinear for the settled/idle frame.
    pub interpolation: Option<String>,
    /// Total slab thickness in mm, centered on `origin` along the plane's own normal. Omitted or
    /// 0 reformats an infinitely-thin plane (the original single-voxel-thick MPR behavior).
    pub slab_thickness_mm: Option<f64>,
    /// How the slab's samples combine when `slabThicknessMm > 0`: 'mip' (default, maximum
    /// intensity projection), 'minip' (minimum intensity projection), or 'average'.
    pub slab_projection: Option<String>,
}

fn parse_vec3(values: &[f64], field_name: &'static str) -> Result<[f64; 3]> {
    if values.len() != 3 {
        return Err(Error::from_reason(format!("{field_name} must have exactly 3 values (x, y, z)")));
    }
    Ok([values[0], values[1], values[2]])
}

fn parse_interpolation(value: Option<&str>) -> Result<DcmInterpolation> {
    match value {
        None | Some("trilinear") => Ok(DcmInterpolation::Trilinear),
        Some("nearest") => Ok(DcmInterpolation::Nearest),
        Some(other) => Err(Error::from_reason(format!(
            "invalid interpolation '{other}'; expected 'trilinear' or 'nearest'"
        ))),
    }
}

fn parse_slab_projection(value: Option<&str>) -> Result<DcmSlabProjection> {
    match value {
        None | Some("mip") => Ok(DcmSlabProjection::MaximumIntensity),
        Some("minip") => Ok(DcmSlabProjection::MinimumIntensity),
        Some("average") => Ok(DcmSlabProjection::Average),
        Some(other) => Err(Error::from_reason(format!(
            "invalid slabProjection '{other}'; expected 'mip', 'minip', or 'average'"
        ))),
    }
}

pub struct ReformatPlaneTask {
    volume: Arc<DcmVolume>,
    options: ReformatPlaneOptions,
}

impl Task for ReformatPlaneTask {
    type Output = RenderedFrame;
    type JsValue = RenderedFrame;

    fn compute(&mut self) -> Result<Self::Output> {
        guarded(|| {
            let (format, mime_type) = parse_render_output_format(self.options.format.as_deref())?;
            let params = DcmPlaneParams {
                origin: parse_vec3(&self.options.origin, "origin")?,
                row_dir: parse_vec3(&self.options.row_dir, "rowDir")?,
                col_dir: parse_vec3(&self.options.col_dir, "colDir")?,
                output_width: self.options.output_width,
                output_height: self.options.output_height,
                spacing_mm: self.options.spacing_mm,
                window_center: self.options.window_center,
                window_width: self.options.window_width,
                interpolation: parse_interpolation(self.options.interpolation.as_deref())?,
                slab_thickness_mm: self.options.slab_thickness_mm.unwrap_or(0.0),
                slab_projection: parse_slab_projection(self.options.slab_projection.as_deref())?,
            };
            let jpeg_quality = self.options.jpeg_quality.unwrap_or(90) as u8;
            let rendered = dcm_reformat_plane(&self.volume, &params, format, jpeg_quality).map_err(to_napi_err)?;
            Ok(RenderedFrame {
                mime_type: mime_type.to_owned(),
                width: u32::from(rendered.width),
                height: u32::from(rendered.height),
                data: rendered.bytes.into(),
                overlays: Vec::new(),
                selected_overlay_index: None,
                passthrough: false,
            })
        })
    }

    fn resolve(&mut self, _env: Env, output: Self::Output) -> Result<Self::JsValue> {
        Ok(output)
    }
}

#[napi]
pub struct DicomVolumeHandle {
    /// `None` once `release()` ran. Tasks already running hold their own `Arc` clone, so the
    /// samples are freed when the last of them finishes.
    volume: std::sync::Mutex<Option<Arc<DcmVolume>>>,
    rows: u32,
    cols: u32,
    num_slices: u32,
    native_basis: Vec<f64>,
    center: Vec<f64>,
    min_spacing_mm: f64,
    byte_length: f64,
    modality: Option<String>,
    frame_of_reference_uid: Option<String>,
    series_instance_uid: Option<String>,
    study_instance_uid: Option<String>,
    gantry_tilt_deg: f64,
    desheared: bool,
    irregular_spacing: bool,
    voxel_to_patient: Vec<f64>,
}

impl DicomVolumeHandle {
    fn new(volume: DcmVolume) -> Self {
        let mut native_basis = Vec::with_capacity(6);
        native_basis.extend_from_slice(&volume.row_vector);
        native_basis.extend_from_slice(&volume.col_vector);
        Self {
            rows: volume.rows,
            cols: volume.cols,
            num_slices: volume.num_slices,
            native_basis,
            center: volume.center().to_vec(),
            min_spacing_mm: volume.min_spacing_mm(),
            byte_length: (volume.samples.len() * std::mem::size_of::<f32>()) as f64,
            modality: volume.modality.clone(),
            frame_of_reference_uid: volume.frame_of_reference_uid.clone(),
            series_instance_uid: volume.series_instance_uid.clone(),
            study_instance_uid: volume.study_instance_uid.clone(),
            gantry_tilt_deg: volume.gantry_tilt_deg,
            desheared: volume.desheared,
            irregular_spacing: volume.has_irregular_spacing(),
            voxel_to_patient: volume.voxel_to_patient().to_vec(),
            volume: std::sync::Mutex::new(Some(Arc::new(volume))),
        }
    }

    fn volume(&self) -> Result<Arc<DcmVolume>> {
        self.volume
            .lock()
            .map_err(|_| Error::from_reason("volume lock poisoned"))?
            .clone()
            .ok_or_else(|| Error::from_reason("volume was released"))
    }
}

#[napi]
impl DicomVolumeHandle {
    /// Rows of the volume's slice grid (image height). The source images' Rows, except for a
    /// desheared (gantry-tilted) stack, whose grid covers every slice's footprint.
    #[napi(getter, js_name = "rows")]
    pub fn rows(&self) -> u32 {
        self.rows
    }
    /// Columns of the volume's slice grid (image width); see `rows`.
    #[napi(getter, js_name = "cols")]
    pub fn cols(&self) -> u32 {
        self.cols
    }
    /// Number of slices in the built volume.
    #[napi(getter, js_name = "numSlices")]
    pub fn num_slices(&self) -> u32 {
        self.num_slices
    }

    /// Bytes of voxel memory this volume holds (native memory, invisible to the JS heap), until
    /// `release()`.
    #[napi(getter, js_name = "byteLength")]
    pub fn byte_length(&self) -> f64 {
        if self.is_released() { 0.0 } else { self.byte_length }
    }

    /// Whether `release()` has run.
    #[napi(getter, js_name = "released")]
    pub fn is_released(&self) -> bool {
        self.volume.lock().map(|guard| guard.is_none()).unwrap_or(true)
    }

    /// Frees the voxel memory now instead of whenever the garbage collector finalizes this
    /// handle (which can be much later: the JS heap doesn't see native memory). Calls already
    /// running finish first; `reformat`/`exportTexture` afterwards throw. Idempotent.
    #[napi]
    pub fn release(&self) {
        if let Ok(mut guard) = self.volume.lock() {
            guard.take();
        }
    }

    /// The volume's own acquisition-native orientation, for seeding an "axial" reformat -
    /// `[rowDir(3), colDir(3)]`.
    #[napi(getter, js_name = "nativeBasis")]
    pub fn native_basis(&self) -> Vec<f64> {
        self.native_basis.clone()
    }

    /// The volume's own physical center, in patient/LPS mm - a reasonable default reformat origin.
    #[napi(getter, js_name = "center")]
    pub fn center(&self) -> Vec<f64> {
        self.center.clone()
    }

    /// The volume's own smallest voxel dimension, in mm - a reasonable default output spacing.
    #[napi(getter, js_name = "minSpacingMm")]
    pub fn min_spacing_mm(&self) -> f64 {
        self.min_spacing_mm
    }

    /// `Modality` of the source images (`CT`, `MR`, `PT`, ...), or `null`.
    #[napi(getter, js_name = "modality")]
    pub fn modality(&self) -> Option<String> {
        self.modality.clone()
    }

    /// `FrameOfReferenceUID` of the source images, or `null`.
    #[napi(getter, js_name = "frameOfReferenceUid")]
    pub fn frame_of_reference_uid(&self) -> Option<String> {
        self.frame_of_reference_uid.clone()
    }

    /// `SeriesInstanceUID` of the source images, or `null`.
    #[napi(getter, js_name = "seriesInstanceUid")]
    pub fn series_instance_uid(&self) -> Option<String> {
        self.series_instance_uid.clone()
    }

    /// `StudyInstanceUID` of the source images, or `null`.
    #[napi(getter, js_name = "studyInstanceUid")]
    pub fn study_instance_uid(&self) -> Option<String> {
        self.study_instance_uid.clone()
    }

    /// Gantry tilt of the acquisition, degrees: the angle between the slice-to-slice position
    /// step and the slice normal. `0` for an ordinary stack.
    #[napi(getter, js_name = "gantryTiltDeg")]
    pub fn gantry_tilt_deg(&self) -> f64 {
        self.gantry_tilt_deg
    }

    /// Whether the stack was sheared (gantry tilt) and resampled onto an orthogonal grid.
    #[napi(getter, js_name = "desheared")]
    pub fn desheared(&self) -> bool {
        self.desheared
    }

    /// Whether the slice gaps are irregular (a missing slice, variable pitch). Reformats
    /// interpolate by real position either way; `exportTexture` resamples to a uniform lattice.
    #[napi(getter, js_name = "irregularSpacing")]
    pub fn irregular_spacing(&self) -> bool {
        self.irregular_spacing
    }

    /// Row-major 4x4 matrix from voxel index `(col, row, slice, 1)` to patient LPS mm (slice
    /// axis at the nominal uniform spacing).
    #[napi(getter, js_name = "voxelToPatient")]
    pub fn voxel_to_patient(&self) -> Vec<f64> {
        self.voxel_to_patient.clone()
    }

    /// Resamples one plane through this volume and encodes it exactly like a normal 2D render
    /// (same `RenderedFrame` shape `renderFrame` returns), so callers can reuse their existing
    /// image-display code path unchanged.
    #[napi]
    pub fn reformat(&self, options: ReformatPlaneOptions) -> Result<AsyncTask<ReformatPlaneTask>> {
        Ok(AsyncTask::new(ReformatPlaneTask { volume: self.volume()?, options }))
    }

    /// Packs this volume's own NATIVE voxel lattice - not a resampled oblique plane, see
    /// `exportTexture`'s own doc - as a lossless GPU-upload-ready texture payload (16-bit
    /// samples, row-major, optionally gzip-compressed). Unlike `reformat()`, this is a one-shot
    /// call per volume, not one per interaction: the render-server ships the returned `data`
    /// once and the client does all further rotate/scroll/window-level manipulation itself in a
    /// WebGL2 shader.
    #[napi]
    pub fn export_texture(&self, options: Option<ExportVolumeTextureOptions>) -> Result<AsyncTask<ExportVolumeTextureTask>> {
        Ok(AsyncTask::new(ExportVolumeTextureTask {
            volume: self.volume()?,
            options: options.unwrap_or_default(),
        }))
    }
}

// ---------------------------------------------------------------------------------------------
// Registration: estimateTranslation / registerVolumes / RegistrationCancelToken
// ---------------------------------------------------------------------------------------------
//
// Every matrix is 16 numbers, row-major, mapping MOVING patient LPS mm to FIXED patient LPS mm -
// the same direction a DICOM Spatial Registration item stores (item frame = moving, the REG's
// own frame = fixed), so a result goes into writeSpatialRegistration unchanged.

fn parse_matrix(values: &[f64], field_name: &str) -> Result<[f64; 16]> {
    if values.len() != 16 || values.iter().any(|v| !v.is_finite()) {
        return Err(Error::from_reason(format!("{field_name} must be 16 finite numbers (row-major 4x4)")));
    }
    let mut m = [0.0; 16];
    m.copy_from_slice(values);
    Ok(m)
}

fn volume_pair(fixed: &DicomVolumeHandle, moving: &DicomVolumeHandle) -> Result<(Arc<DcmVolume>, Arc<DcmVolume>)> {
    Ok((fixed.volume()?, moving.volume()?))
}

#[napi(object)]
#[derive(Default)]
pub struct EstimateTranslationOptions {
    /// Overrides the fixed volume's Modality ('CT', 'MR', 'PT', ...).
    pub fixed_modality: Option<String>,
    /// Overrides the moving volume's Modality.
    pub moving_modality: Option<String>,
    /// Smallest S-I overlap of the two scans a z shift may imply, mm (default 60).
    pub min_overlap_mm: Option<f64>,
    /// Largest |z shift| considered, mm (default: anything with enough overlap).
    pub max_shift_mm: Option<f64>,
}

#[napi(object)]
pub struct TranslationEstimate {
    /// Moving -> fixed patient (a pure translation), row-major 4x4.
    pub matrix: Vec<f64>,
    pub translation_mm: Vec<f64>,
    /// 0..1: the correlation peak's height and its margin over the next peak 20+ mm away.
    /// Below ~0.3, fall back to capture-on-link.
    pub confidence: f64,
    pub peak_correlation: f64,
    pub second_peak_correlation: f64,
    pub overlap_mm: f64,
    /// The profiles correlated: 'body', 'lung', 'bone' (CT-CT), 'intensity' (MR-MR, PT-PT).
    pub profiles: Vec<String>,
    pub timing_ms: f64,
}

pub struct EstimateTranslationTask {
    fixed: Arc<DcmVolume>,
    moving: Arc<DcmVolume>,
    options: EstimateTranslationOptions,
}

impl Task for EstimateTranslationTask {
    type Output = TranslationEstimate;
    type JsValue = TranslationEstimate;

    fn compute(&mut self) -> Result<Self::Output> {
        guarded(|| {
            let defaults = DcmTranslationOptions::default();
            let options = DcmTranslationOptions {
                fixed_modality: self.options.fixed_modality.clone(),
                moving_modality: self.options.moving_modality.clone(),
                min_overlap_mm: self.options.min_overlap_mm.unwrap_or(defaults.min_overlap_mm),
                max_shift_mm: self.options.max_shift_mm,
            };
            let estimate = dcm_estimate_translation(&self.fixed, &self.moving, &options).map_err(to_napi_err)?;
            Ok(TranslationEstimate {
                matrix: estimate.matrix.to_vec(),
                translation_mm: estimate.translation_mm.to_vec(),
                confidence: estimate.confidence,
                peak_correlation: estimate.peak_correlation,
                second_peak_correlation: estimate.second_peak_correlation,
                overlap_mm: estimate.overlap_mm,
                profiles: estimate.profiles.iter().map(|p| (*p).to_owned()).collect(),
                timing_ms: estimate.timing_ms,
            })
        })
    }

    fn resolve(&mut self, _env: Env, output: Self::Output) -> Result<Self::JsValue> {
        Ok(output)
    }
}

/// A fast moving -> fixed translation: the S-I shift from cross-correlating per-height profiles
/// (body area; plus lung and bone area for CT-CT, integrated intensity for MR-MR / PT-PT), the
/// in-plane shift from body-mask centroids over the matched overlap. Works for any acquisition
/// orientation. Tens of milliseconds for a typical CT.
#[napi]
pub fn estimate_translation(
    fixed: &DicomVolumeHandle,
    moving: &DicomVolumeHandle,
    options: Option<EstimateTranslationOptions>,
) -> Result<AsyncTask<EstimateTranslationTask>> {
    let (fixed, moving) = volume_pair(fixed, moving)?;
    Ok(AsyncTask::new(EstimateTranslationTask { fixed, moving, options: options.unwrap_or_default() }))
}

#[napi(object)]
#[derive(Default)]
pub struct RegisterVolumesOptions {
    /// 'auto' (default: 'ncc' for the same modality, 'mattes-mi' across modalities), 'ncc' or
    /// 'mattes-mi'.
    pub metric: Option<String>,
    pub fixed_modality: Option<String>,
    pub moving_modality: Option<String>,
    /// Starting moving -> fixed transform (16 numbers, row-major), e.g. the viewer's translation.
    pub initial_matrix: Option<Vec<f64>>,
    /// Without `initialMatrix`: 'translation' (default; estimateTranslation, then the mask
    /// centroids when it is not confident), 'centroid' or 'none'.
    pub initialize: Option<String>,
    /// Working-grid spacing per pyramid level, coarse to fine, mm (default [8, 4, 2]).
    pub levels_mm: Option<Vec<f64>>,
    /// Optimizer iterations per level (default 40).
    pub max_iterations: Option<u32>,
    /// Wall-clock limit, ms (default 30000). On expiry the best result so far is returned with
    /// `stopReason: 'time-limit'`.
    pub time_limit_ms: Option<f64>,
    /// 'auto' (default: modality-aware body mask, CT table removed) or 'none'.
    pub mask: Option<String>,
    /// Optional box in FIXED patient coordinates the fixed mask is limited to (min corner).
    pub roi_min_mm: Option<Vec<f64>>,
    /// Max corner of `roiMinMm`'s box.
    pub roi_max_mm: Option<Vec<f64>>,
    /// Mutual-information histogram bins (default 32).
    pub histogram_bins: Option<u32>,
    /// Metric sample points per level (default 100000).
    pub max_samples: Option<u32>,
    /// Working-memory cap in bytes (default 268435456); a level that would exceed it is coarsened.
    pub max_memory_bytes: Option<f64>,
}

#[napi(object)]
pub struct RegistrationLevel {
    pub spacing_mm: f64,
    pub iterations: u32,
    pub metric_evaluations: u32,
    pub metric_value: f64,
}

#[napi(object)]
pub struct RegistrationResult {
    /// Moving -> fixed patient, row-major 4x4.
    pub matrix: Vec<f64>,
    /// Euler angles of the matrix's rotation, degrees (`R = Rz * Ry * Rx`).
    pub rotation_deg: Vec<f64>,
    /// The matrix's translation column, mm.
    pub translation_mm: Vec<f64>,
    /// Centre of rotation the optimizer used (fixed body centroid), fixed patient mm.
    pub center_mm: Vec<f64>,
    /// 'ncc' or 'mattes-mi'.
    pub metric: String,
    /// NCC correlation (-1..1), or mutual information (nats).
    pub metric_value: f64,
    pub initial_metric_value: f64,
    /// NCC correlation, or MI's entropy correlation coefficient (0..1).
    pub normalized_metric: f64,
    /// Fraction of the fixed body samples that land inside the moving volume.
    pub overlap_fraction: f64,
    /// 0..1 heuristic; show it, and never apply a 'poor' result silently.
    pub quality: f64,
    /// 'good' (>= 0.7), 'fair' (>= 0.4) or 'poor'.
    pub quality_label: String,
    pub iterations: u32,
    pub metric_evaluations: u32,
    pub levels: Vec<RegistrationLevel>,
    /// 'matrix', 'translation', 'centroid' or 'none'.
    pub initialization: String,
    pub converged: bool,
    /// 'converged', 'max-iterations' or 'time-limit'.
    pub stop_reason: String,
    pub timing_ms: f64,
    /// Peak bytes of working images and samples held.
    pub working_bytes: f64,
}

/// Cancels a running `registerVolumes`: pass it as the 4th argument and call `cancel()`; the
/// registration rejects with "registration cancelled" at its next metric evaluation.
#[napi]
pub struct RegistrationCancelToken {
    flag: Arc<std::sync::atomic::AtomicBool>,
}

#[napi]
impl RegistrationCancelToken {
    #[napi(constructor)]
    #[allow(clippy::new_without_default)]
    pub fn new() -> Self {
        Self { flag: Arc::new(std::sync::atomic::AtomicBool::new(false)) }
    }

    #[napi]
    pub fn cancel(&self) {
        self.flag.store(true, std::sync::atomic::Ordering::Relaxed);
    }

    #[napi(getter, js_name = "cancelled")]
    pub fn cancelled(&self) -> bool {
        self.flag.load(std::sync::atomic::Ordering::Relaxed)
    }
}

pub struct RegisterVolumesTask {
    fixed: Arc<DcmVolume>,
    moving: Arc<DcmVolume>,
    options: DcmRegistrationOptions,
}

impl Task for RegisterVolumesTask {
    type Output = RegistrationResult;
    type JsValue = RegistrationResult;

    fn compute(&mut self) -> Result<Self::Output> {
        guarded(|| {
            let result = dcm_register_rigid(&self.fixed, &self.moving, &self.options).map_err(to_napi_err)?;
            Ok(RegistrationResult {
                matrix: result.matrix.to_vec(),
                rotation_deg: result.rotation_deg.to_vec(),
                translation_mm: result.translation_mm.to_vec(),
                center_mm: result.center_mm.to_vec(),
                metric: result.metric.as_str().to_owned(),
                metric_value: result.metric_value,
                initial_metric_value: result.initial_metric_value,
                normalized_metric: result.normalized_metric,
                overlap_fraction: result.overlap_fraction,
                quality: result.quality,
                quality_label: result.quality_label.to_owned(),
                iterations: result.iterations,
                metric_evaluations: result.metric_evaluations,
                levels: result
                    .levels
                    .iter()
                    .map(|level| RegistrationLevel {
                        spacing_mm: level.spacing_mm,
                        iterations: level.iterations,
                        metric_evaluations: level.metric_evaluations,
                        metric_value: level.metric_value,
                    })
                    .collect(),
                initialization: result.initialization.to_owned(),
                converged: result.converged,
                stop_reason: result.stop_reason.as_str().to_owned(),
                timing_ms: result.timing_ms,
                working_bytes: result.working_bytes as f64,
            })
        })
    }

    fn resolve(&mut self, _env: Env, output: Self::Output) -> Result<Self::JsValue> {
        Ok(output)
    }
}

fn registration_options(options: RegisterVolumesOptions, cancel: Option<&RegistrationCancelToken>) -> Result<DcmRegistrationOptions> {
    let defaults = DcmRegistrationOptions::default();
    let metric = match options.metric.as_deref() {
        None | Some("auto") => DcmMetricChoice::Auto,
        Some("ncc") => DcmMetricChoice::Ncc,
        Some("mattes-mi") | Some("mi") => DcmMetricChoice::MattesMi,
        Some(other) => return Err(Error::from_reason(format!("invalid metric '{other}'; expected 'auto', 'ncc' or 'mattes-mi'"))),
    };
    let initialization = match options.initialize.as_deref() {
        None | Some("translation") => DcmInitialization::Translation,
        Some("centroid") => DcmInitialization::Centroid,
        Some("none") => DcmInitialization::None,
        Some(other) => return Err(Error::from_reason(format!("invalid initialize '{other}'; expected 'translation', 'centroid' or 'none'"))),
    };
    let mask = match options.mask.as_deref() {
        None | Some("auto") => DcmMaskMode::Auto,
        Some("none") => DcmMaskMode::None,
        Some(other) => return Err(Error::from_reason(format!("invalid mask '{other}'; expected 'auto' or 'none'"))),
    };
    let roi = match (&options.roi_min_mm, &options.roi_max_mm) {
        (None, None) => None,
        (Some(lo), Some(hi)) => Some((parse_vec3(lo, "roiMinMm")?, parse_vec3(hi, "roiMaxMm")?)),
        _ => return Err(Error::from_reason("roiMinMm and roiMaxMm go together")),
    };
    let time_limit = match options.time_limit_ms {
        None => defaults.time_limit,
        Some(ms) if ms.is_finite() && ms > 0.0 => Some(std::time::Duration::from_secs_f64(ms / 1000.0)),
        Some(_) => return Err(Error::from_reason("timeLimitMs must be a positive number")),
    };
    Ok(DcmRegistrationOptions {
        metric,
        fixed_modality: options.fixed_modality,
        moving_modality: options.moving_modality,
        initial_matrix: options.initial_matrix.as_deref().map(|m| parse_matrix(m, "initialMatrix")).transpose()?,
        initialization,
        levels_mm: options.levels_mm.unwrap_or(defaults.levels_mm),
        max_iterations: options.max_iterations.unwrap_or(defaults.max_iterations),
        time_limit,
        cancel: cancel.map(|token| token.flag.clone()),
        mask,
        roi,
        histogram_bins: options.histogram_bins.map(|b| b as usize).unwrap_or(defaults.histogram_bins),
        max_samples: options.max_samples.map(|s| s as usize).unwrap_or(defaults.max_samples),
        max_memory_bytes: options.max_memory_bytes.map(|b| b.max(0.0) as u64).unwrap_or(defaults.max_memory_bytes),
    })
}

/// Rigid (6-DOF) registration of `moving` onto `fixed`, any CT / MR / PT pairing: a multi-
/// resolution pyramid, modality-aware body masks (CT table removed), NCC for the same modality
/// and Mattes mutual information across modalities, Powell's optimizer. Deterministic, bounded
/// by `maxMemoryBytes` and `timeLimitMs`, cancellable through `cancel`. Resolves with the
/// moving -> fixed patient matrix, the metric, a quality score and timing; rejects when the
/// volumes have no body or do not overlap, on invalid options, or when cancelled.
#[napi]
pub fn register_volumes(
    fixed: &DicomVolumeHandle,
    moving: &DicomVolumeHandle,
    options: Option<RegisterVolumesOptions>,
    cancel: Option<&RegistrationCancelToken>,
) -> Result<AsyncTask<RegisterVolumesTask>> {
    let (fixed, moving) = volume_pair(fixed, moving)?;
    let options = registration_options(options.unwrap_or_default(), cancel)?;
    Ok(AsyncTask::new(RegisterVolumesTask { fixed, moving, options }))
}

// ---------------------------------------------------------------------------------------------
// DICOM Spatial Registration: readSpatialRegistration / writeSpatialRegistration
// ---------------------------------------------------------------------------------------------

pub struct ReadSpatialRegistrationTask {
    file_path: PathBuf,
}

impl Task for ReadSpatialRegistrationTask {
    type Output = String;
    type JsValue = String;

    fn compute(&mut self) -> Result<Self::Output> {
        guarded(|| {
            let object = read_dicom_file(&self.file_path).map_err(to_napi_err)?;
            let json = dcm_read_spatial_registration(&object).map_err(to_napi_err)?;
            serde_json::to_string(&json).map_err(to_napi_err)
        })
    }

    fn resolve(&mut self, _env: Env, output: Self::Output) -> Result<Self::JsValue> {
        Ok(output)
    }
}

/// Reads a DICOM Spatial Registration object (1.2.840.10008.5.1.4.1.1.66.1; for a Deformable
/// one, .66.3, only its pre/post-deformation matrices) and resolves with its JSON form as a
/// string: the REG's own (registered) Frame of Reference, one entry per Registration Sequence
/// item with its Frame of Reference, the composed item -> REG matrix (row-major), its type and
/// referenced images, the Common Instance Reference series, and `warnings` (IOD problems).
/// Rejects anything that is not a registration object.
#[napi]
pub fn read_spatial_registration(file_path: String) -> AsyncTask<ReadSpatialRegistrationTask> {
    AsyncTask::new(ReadSpatialRegistrationTask { file_path: PathBuf::from(file_path) })
}

#[napi(object)]
pub struct WrittenSpatialRegistration {
    /// The written object in readSpatialRegistration's JSON form (a string).
    pub json: String,
    /// The written object as standard PS3.18 DICOM JSON (hex tag keys, `{vr, Value}`), a
    /// string, ready for `writeJson`.
    pub dataset: String,
    pub sop_instance_uid: String,
    pub series_instance_uid: String,
    /// The Part 10 file's bytes, when no `outputPath` was given.
    pub data: Option<Buffer>,
}

pub struct WriteSpatialRegistrationTask {
    spec_json: String,
    output_path: Option<PathBuf>,
}

pub struct WrittenSpatialRegistrationOutput {
    json: String,
    dataset: String,
    sop_instance_uid: String,
    series_instance_uid: String,
    data: Option<Vec<u8>>,
}

impl Task for WriteSpatialRegistrationTask {
    type Output = WrittenSpatialRegistrationOutput;
    type JsValue = WrittenSpatialRegistration;

    fn compute(&mut self) -> Result<Self::Output> {
        guarded(|| {
            let spec: serde_json::Value = serde_json::from_str(&self.spec_json).map_err(|e| Error::from_reason(format!("spec is not valid JSON: {e}")))?;
            let copy_from = match spec.get("copyPatientStudyFrom").and_then(|v| v.as_str()) {
                Some(path) => Some(read_dicom_file(path).map_err(to_napi_err)?),
                None => None,
            };
            let mut object = dcm_build_spatial_registration(&spec, copy_from.as_ref()).map_err(to_napi_err)?;
            let json = dcm_read_spatial_registration(&object).map_err(to_napi_err)?;
            let dataset = write_dicom_json_with_options(
                &object,
                DicomJsonWriteOptions { format: DicomJsonFormat::Standard, key_style: DicomJsonKeyStyle::Hex, ..Default::default() },
            )
            .map_err(to_napi_err)?;
            let data = match &self.output_path {
                Some(path) => {
                    write_dicom_file(&mut object, path).map_err(to_napi_err)?;
                    None
                }
                None => Some(dcm_write_dicom_bytes(&mut object).map_err(to_napi_err)?),
            };
            let text = |key: &str| json.get(key).and_then(|v| v.as_str()).unwrap_or_default().to_owned();
            Ok(WrittenSpatialRegistrationOutput {
                sop_instance_uid: text("sopInstanceUid"),
                series_instance_uid: text("seriesInstanceUid"),
                json: serde_json::to_string(&json).map_err(to_napi_err)?,
                dataset,
                data,
            })
        })
    }

    fn resolve(&mut self, _env: Env, output: Self::Output) -> Result<Self::JsValue> {
        Ok(WrittenSpatialRegistration {
            json: output.json,
            dataset: output.dataset,
            sop_instance_uid: output.sop_instance_uid,
            series_instance_uid: output.series_instance_uid,
            data: output.data.map(Buffer::from),
        })
    }
}

/// Builds a DICOM Spatial Registration instance from `specJson` (a JSON string) and writes it to
/// `outputPath`, or returns its bytes as `data` when `outputPath` is omitted. The REG goes in the
/// FIXED study and Frame of Reference: item 1 maps the fixed frame with the identity, item 2 the
/// moving frame with `matrix` (moving -> fixed, row-major, what registerVolumes returns).
///
/// spec: `{ matrix: number[16], matrixType?: 'RIGID' (default) | 'RIGID_SCALE' | 'AFFINE',
/// registrationType?: 'auto' | 'manual' | 'identity', fixed: { frameOfReferenceUid,
/// studyInstanceUid, seriesInstanceUid, instances?: [{ sopClassUid, sopInstanceUid }] }, moving:
/// { ...same }, copyPatientStudyFrom?: path to one fixed-study DICOM file, patient?: {
/// patientName, patientId, patientBirthDate, patientSex }, study?: { studyDate, studyTime,
/// accessionNumber, referringPhysicianName, studyId, studyDescription }, sopInstanceUid?,
/// seriesInstanceUid?, seriesNumber?, seriesDescription?, instanceNumber?, contentLabel?,
/// contentDescription?, contentCreatorName?, manufacturer?, comment? }`. Rejects a spec that would
/// not produce a conformant object (missing UIDs, a non-rigid RIGID matrix, fixed and moving in
/// one Frame of Reference, ...).
#[napi]
pub fn write_spatial_registration(spec_json: String, output_path: Option<String>) -> AsyncTask<WriteSpatialRegistrationTask> {
    AsyncTask::new(WriteSpatialRegistrationTask { spec_json, output_path: output_path.map(PathBuf::from) })
}

// ---------------------------------------------------------------------------------------------
// Texture export: DicomVolumeHandle.exportTexture / exportFrameTexture
// ---------------------------------------------------------------------------------------------
//
// See dcmnorm::dicom_io::texture_export's own module doc for the format contract (metadata +
// raw row-major int16/uint16 payload, optionally gzip-compressed). `exportTexture` (volume) and
// `exportFrameTexture` (a single decoded 2D frame, packed as a depth-1 "1-slice volume" so the
// client can reuse the exact same GPU texture/shader code for both) share the same options shape
// and result-mapping helper below.

#[napi(object)]
#[derive(Default)]
pub struct ExportVolumeTextureOptions {
    /// Caps the longest of width/height/depth at this many samples, proportionally downsampling
    /// (trilinear) if the native volume exceeds it. Omitted means no cap (full native
    /// resolution) - the caller (render-server) is expected to derive this from the client's
    /// reported `MAX_3D_TEXTURE_SIZE`, or from a small fixed value for a fast "progressive"
    /// first pass ahead of the full-resolution texture.
    pub target_max_dim: Option<u32>,
    /// 'gzip' (default) or 'none'.
    pub compression: Option<String>,
    /// Optional default window/level to carry through to `TextureExportResult.defaultWindowCenter`
    /// /`defaultWindowWidth` - purely informational for the client's initial render, since the
    /// exported samples themselves are never windowed.
    pub window_center: Option<f64>,
    pub window_width: Option<f64>,
}

#[napi(object)]
#[derive(Default)]
pub struct ExportFrameTextureOptions {
    /// Zero-based frame index for a multi-frame file. Defaults to 0.
    pub frame_index: Option<u32>,
    pub target_max_dim: Option<u32>,
    /// 'gzip' (default) or 'none'.
    pub compression: Option<String>,
    pub window_center: Option<f64>,
    pub window_width: Option<f64>,
}

#[napi(object)]
pub struct TextureExportResult {
    /// 'volume', 'image2d', or 'framestack'.
    pub content_kind: String,
    /// 'int16' or 'uint16' for a grayscale texture, 'rgb8' for a color texture (see
    /// `exportFrameTextureRgb`/`exportFrameStackTextureRgb`) - `data` is interleaved 8-bit-per-
    /// channel RGB in that case, never quantized (`lossless` is unconditionally `true`).
    pub sample_format: String,
    /// 'none' or 'gzip' - matches how `data` below is actually encoded.
    pub compression: String,
    /// `false` means `data` is a bounded-error quantization (see `rescaleSlope`/
    /// `rescaleIntercept`), not an exact round-trip of the source samples.
    pub lossless: bool,
    pub width: u32,
    pub height: u32,
    /// Always 1 for `contentKind: 'image2d'`.
    pub depth: u32,
    /// `texel * rescaleSlope + rescaleIntercept` recovers the physical value (e.g. HU).
    pub rescale_slope: f64,
    pub rescale_intercept: f64,
    pub row_spacing_mm: f64,
    pub col_spacing_mm: f64,
    /// `0` for `contentKind: 'image2d'`.
    pub slice_spacing_mm: f64,
    /// `[x, y, z]`, LPS mm, center of voxel (0,0,0).
    pub origin: Vec<f64>,
    pub row_dir: Vec<f64>,
    pub col_dir: Vec<f64>,
    pub normal_dir: Vec<f64>,
    pub default_window_center: Option<f64>,
    pub default_window_width: Option<f64>,
    /// Whether the client shader should display `1.0 - windowedIntensity` rather than
    /// `windowedIntensity` - the invert decision `dcmnorm::dicom_io::render::resolve_grayscale_invert`
    /// makes (PresentationLUTShape overriding PhotometricInterpretation's MONOCHROME1/2-derived
    /// default). Always `false` for `contentKind: 'volume'` - see `TextureMeta::invert`'s own doc.
    pub invert: bool,
    /// `framestack` only (empty otherwise): each layer's own default window and invert decision,
    /// index-aligned with the layers - see `TextureMeta::layer_default_windows`. Prefer these over
    /// the stack-wide `default_window_*`/`invert` whenever present.
    pub layer_window_centers: Vec<f64>,
    pub layer_window_widths: Vec<f64>,
    pub layer_invert: Vec<bool>,
    pub native_width: u32,
    pub native_height: u32,
    pub native_depth: u32,
    pub downsampled: bool,
    /// Uncompressed byte length of `data`'s content - an exact integer, safely representable as
    /// `f64` at any real texture size (well under 2^53).
    pub payload_bytes_raw: f64,
    /// `data.length` - included on the result too (not just derivable from `data` client-side)
    /// so the render-server can log/cap transfer size without touching the buffer itself.
    pub payload_bytes_stored: f64,
    pub data: Buffer,
}

fn parse_texture_compression(value: Option<&str>) -> Result<DcmTextureCompression> {
    match value {
        None | Some("gzip") => Ok(DcmTextureCompression::Gzip),
        Some("none") => Ok(DcmTextureCompression::None),
        Some(other) => Err(Error::from_reason(format!(
            "invalid compression '{other}'; expected 'gzip' or 'none'"
        ))),
    }
}

fn texture_export_result(meta: &DcmTextureMeta, payload: Vec<u8>) -> TextureExportResult {
    let (content_kind, sample_format, compression) = (
        match meta.content_kind {
            dcmnorm::dicom_io::ContentKind::Volume => "volume",
            dcmnorm::dicom_io::ContentKind::Image2D => "image2d",
            dcmnorm::dicom_io::ContentKind::FrameStack => "framestack",
        },
        match meta.sample_format {
            dcmnorm::dicom_io::SampleFormat::Int16 => "int16",
            dcmnorm::dicom_io::SampleFormat::Uint16 => "uint16",
            dcmnorm::dicom_io::SampleFormat::Rgb8 => "rgb8",
        },
        match meta.compression {
            DcmTextureCompression::None => "none",
            DcmTextureCompression::Gzip => "gzip",
        },
    );
    TextureExportResult {
        content_kind: content_kind.to_owned(),
        sample_format: sample_format.to_owned(),
        compression: compression.to_owned(),
        lossless: meta.lossless,
        width: meta.width,
        height: meta.height,
        depth: meta.depth,
        rescale_slope: meta.rescale_slope,
        rescale_intercept: meta.rescale_intercept,
        row_spacing_mm: meta.row_spacing_mm,
        col_spacing_mm: meta.col_spacing_mm,
        slice_spacing_mm: meta.slice_spacing_mm,
        origin: meta.origin.to_vec(),
        row_dir: meta.row_dir.to_vec(),
        col_dir: meta.col_dir.to_vec(),
        normal_dir: meta.normal_dir.to_vec(),
        default_window_center: meta.default_window_center,
        default_window_width: meta.default_window_width,
        invert: meta.invert,
        layer_window_centers: meta.layer_default_windows.iter().map(|(center, _)| *center).collect(),
        layer_window_widths: meta.layer_default_windows.iter().map(|(_, width)| *width).collect(),
        layer_invert: meta.layer_invert.clone(),
        native_width: meta.native_dims.0,
        native_height: meta.native_dims.1,
        native_depth: meta.native_dims.2,
        downsampled: meta.downsampled,
        payload_bytes_raw: meta.payload_bytes_raw as f64,
        payload_bytes_stored: meta.payload_bytes_stored as f64,
        data: payload.into(),
    }
}

pub struct ExportVolumeTextureTask {
    volume: Arc<DcmVolume>,
    options: ExportVolumeTextureOptions,
}

impl Task for ExportVolumeTextureTask {
    type Output = TextureExportResult;
    type JsValue = TextureExportResult;

    fn compute(&mut self) -> Result<Self::Output> {
        guarded(|| {
            let compression = parse_texture_compression(self.options.compression.as_deref())?;
            let default_window = match (self.options.window_center, self.options.window_width) {
                (Some(center), Some(width)) => Some((center, width)),
                _ => None,
            };
            let packed = dcm_pack_volume_texture(&self.volume, self.options.target_max_dim, default_window, compression)
                .map_err(to_napi_err)?;
            Ok(texture_export_result(&packed.meta, packed.payload))
        })
    }

    fn resolve(&mut self, _env: Env, output: Self::Output) -> Result<Self::JsValue> {
        Ok(output)
    }
}

pub struct ExportFrameTextureTask {
    file_path: PathBuf,
    options: ExportFrameTextureOptions,
}

impl Task for ExportFrameTextureTask {
    type Output = TextureExportResult;
    type JsValue = TextureExportResult;

    fn compute(&mut self) -> Result<Self::Output> {
        guarded(|| {
            let compression = parse_texture_compression(self.options.compression.as_deref())?;
            let default_window = match (self.options.window_center, self.options.window_width) {
                (Some(center), Some(width)) => Some((center, width)),
                _ => None,
            };
            let frame_index = self.options.frame_index.unwrap_or(0) as usize;
            let object = read_dicom_file_for_single_frame(&self.file_path, frame_index)?;
            let packed = dcm_pack_dicom_frame_texture(
                &object,
                frame_index,
                self.options.target_max_dim,
                default_window,
                compression,
            )
            .map_err(to_napi_err)?;
            Ok(texture_export_result(&packed.meta, packed.payload))
        })
    }

    fn resolve(&mut self, _env: Env, output: Self::Output) -> Result<Self::JsValue> {
        Ok(output)
    }
}

/// Packs a single frame's raw (unwindowed) physical values as a depth-1 "1-slice volume" texture
/// - lets a large diagnostic 2D image (e.g. DX/CR/mammography) reuse the exact same client GPU
/// texture/shader pipeline as an MPR volume, instead of the lossy zoomed-JPEG path `renderFrame`
/// produces. Mirrors `dcmnorm --render-frame ... --output-type texture file.dcm out.gputex`.
#[napi]
pub fn export_frame_texture(file_path: String, options: Option<ExportFrameTextureOptions>) -> AsyncTask<ExportFrameTextureTask> {
    AsyncTask::new(ExportFrameTextureTask {
        file_path: PathBuf::from(file_path),
        options: options.unwrap_or_default(),
    })
}

#[napi(object)]
#[derive(Default)]
pub struct ExportFrameTextureRgbOptions {
    /// Zero-based frame index for a multi-frame file. Defaults to 0.
    pub frame_index: Option<u32>,
    pub target_max_dim: Option<u32>,
    /// 'gzip' (default) or 'none'.
    pub compression: Option<String>,
    // No windowCenter/windowWidth - color has no window/level concept (see
    // dcmnorm::dicom_io::pack_rgb_frame_texture's own doc).
}

pub struct ExportFrameTextureRgbTask {
    file_path: PathBuf,
    options: ExportFrameTextureRgbOptions,
}

impl Task for ExportFrameTextureRgbTask {
    type Output = TextureExportResult;
    type JsValue = TextureExportResult;

    fn compute(&mut self) -> Result<Self::Output> {
        guarded(|| {
            let compression = parse_texture_compression(self.options.compression.as_deref())?;
            let frame_index = self.options.frame_index.unwrap_or(0) as usize;
            let object = read_dicom_file_for_single_frame(&self.file_path, frame_index)?;
            let packed = dcm_pack_dicom_rgb_frame_texture(
                &object,
                frame_index,
                self.options.target_max_dim,
                compression,
            )
            .map_err(to_napi_err)?;
            Ok(texture_export_result(&packed.meta, packed.payload))
        })
    }

    fn resolve(&mut self, _env: Env, output: Self::Output) -> Result<Self::JsValue> {
        Ok(output)
    }
}

/// Color counterpart to `exportFrameTexture` - packs a single color frame (RGB/YBR variants, or
/// PALETTE COLOR) as an `rgb8` depth-1 texture instead of a quantized grayscale one. Same
/// result shape (`TextureExportResult`), no window options (color is never windowed).
#[napi]
pub fn export_frame_texture_rgb(
    file_path: String,
    options: Option<ExportFrameTextureRgbOptions>,
) -> AsyncTask<ExportFrameTextureRgbTask> {
    AsyncTask::new(ExportFrameTextureRgbTask {
        file_path: PathBuf::from(file_path),
        options: options.unwrap_or_default(),
    })
}

// ---------------------------------------------------------------------------------------------
// Texture export: exportFrameStackTexture
// ---------------------------------------------------------------------------------------------
//
// One entry per source FILE, not per frame - a cine/multiframe instance uses ONE source with
// several frameIndices (its file is parsed once, however many of its frames end up in the
// stack), while a multi-image series uses one source per instance file (frameIndices defaulting
// to just [0]). Either way, the resulting layer order is the flattened source order followed by
// each source's own frameIndices order - the caller (render-server) is responsible for supplying
// sources in the exact order the client's own frame/instance index expects, since a mismatch here
// would silently show the wrong layer for a given index client-side.

#[napi(object)]
pub struct FrameStackSource {
    pub file_path: String,
    /// Zero-based frame indices to include from this file, in order. Defaults to `[0]`.
    pub frame_indices: Option<Vec<u32>>,
}

#[napi(object)]
#[derive(Default)]
pub struct ExportFrameStackTextureOptions {
    /// 'gzip' (default) or 'none'.
    pub compression: Option<String>,
    pub window_center: Option<f64>,
    pub window_width: Option<f64>,
}

pub struct ExportFrameStackTextureTask {
    sources: Vec<FrameStackSource>,
    options: ExportFrameStackTextureOptions,
}

impl Task for ExportFrameStackTextureTask {
    type Output = TextureExportResult;
    type JsValue = TextureExportResult;

    fn compute(&mut self) -> Result<Self::Output> {
        guarded(|| {
            let compression = parse_texture_compression(self.options.compression.as_deref())?;
            let default_window = match (self.options.window_center, self.options.window_width) {
                (Some(center), Some(width)) => Some((center, width)),
                _ => None,
            };

            // Read each distinct source file exactly once, regardless of how many frame indices
            // it contributes - avoids re-parsing the same multi-MB file per frame for a cine loop.
            let objects = self
                .sources
                .iter()
                .map(|source| read_dicom_file(PathBuf::from(&source.file_path)).map_err(to_napi_err))
                .collect::<Result<Vec<_>>>()?;

            let mut frame_refs: Vec<(&dcmnorm_object::DefaultDicomObject, usize)> = Vec::new();
            for (source, object) in self.sources.iter().zip(objects.iter()) {
                let indices = source.frame_indices.clone().unwrap_or_else(|| vec![0]);
                for index in indices {
                    frame_refs.push((object, index as usize));
                }
            }

            let packed = dcm_pack_dicom_frame_stack_texture(&frame_refs, default_window, compression).map_err(to_napi_err)?;
            Ok(texture_export_result(&packed.meta, packed.payload))
        })
    }

    fn resolve(&mut self, _env: Env, output: Self::Output) -> Result<Self::JsValue> {
        Ok(output)
    }
}

/// Packs several independent frames (from one or more files) as one texture-array upload - see
/// `dcmnorm::dicom_io::pack_frame_stack_texture`'s own doc for the full contract (no resampling,
/// no cross-layer interpolation, no physical geometry). Mirrors `exportFrameTexture`'s
/// options/result shape, generalized to many sources.
#[napi]
pub fn export_frame_stack_texture(
    sources: Vec<FrameStackSource>,
    options: Option<ExportFrameStackTextureOptions>,
) -> AsyncTask<ExportFrameStackTextureTask> {
    AsyncTask::new(ExportFrameStackTextureTask { sources, options: options.unwrap_or_default() })
}

#[napi(object)]
#[derive(Default)]
pub struct ExportFrameStackTextureRgbOptions {
    /// 'gzip' (default) or 'none'.
    pub compression: Option<String>,
    // No windowCenter/windowWidth - color has no window/level concept.
}

pub struct ExportFrameStackTextureRgbTask {
    sources: Vec<FrameStackSource>,
    options: ExportFrameStackTextureRgbOptions,
}

impl Task for ExportFrameStackTextureRgbTask {
    type Output = TextureExportResult;
    type JsValue = TextureExportResult;

    fn compute(&mut self) -> Result<Self::Output> {
        guarded(|| {
            let compression = parse_texture_compression(self.options.compression.as_deref())?;

            let objects = self
                .sources
                .iter()
                .map(|source| read_dicom_file(PathBuf::from(&source.file_path)).map_err(to_napi_err))
                .collect::<Result<Vec<_>>>()?;

            let mut frame_refs: Vec<(&dcmnorm_object::DefaultDicomObject, usize)> = Vec::new();
            for (source, object) in self.sources.iter().zip(objects.iter()) {
                let indices = source.frame_indices.clone().unwrap_or_else(|| vec![0]);
                for index in indices {
                    frame_refs.push((object, index as usize));
                }
            }

            let packed = dcm_pack_dicom_rgb_frame_stack_texture(&frame_refs, compression).map_err(to_napi_err)?;
            Ok(texture_export_result(&packed.meta, packed.payload))
        })
    }

    fn resolve(&mut self, _env: Env, output: Self::Output) -> Result<Self::JsValue> {
        Ok(output)
    }
}

/// Color counterpart to `exportFrameStackTexture` - packs several independent color frames as one
/// `rgb8` texture-array upload. Same options/result shape (minus window, which color never has).
#[napi]
pub fn export_frame_stack_texture_rgb(
    sources: Vec<FrameStackSource>,
    options: Option<ExportFrameStackTextureRgbOptions>,
) -> AsyncTask<ExportFrameStackTextureRgbTask> {
    AsyncTask::new(ExportFrameStackTextureRgbTask { sources, options: options.unwrap_or_default() })
}

// ---------------------------------------------------------------------------------------------
// DICOM SCP: startDicomServer
// ---------------------------------------------------------------------------------------------
//
// C-FIND/C-MOVE/"association complete" business logic is delegated to JS via napi
// ThreadsafeFunctions, one per callback, bridged synchronously from each connection's own OS
// thread (spawned inside dcmnorm's start_scp, not napi's async-task pool) via
// `call_with_return_value` + a oneshot channel: the JS callback must be an `async function`,
// its resolved value (a JSON string) is what the connection thread blocks on. This mirrors the
// existing Task/guarded()/to_napi_err conventions used everywhere else in this file, adapted for
// a callback that's invoked repeatedly and unpredictably (once per matching request) rather than
// once per napi call.

type JsonCallback = ThreadsafeFunction<String, PromiseRaw<'static, String>>;
type TwoStringCallback = ThreadsafeFunction<(String, String), PromiseRaw<'static, String>>;

/// Bridges a `call_with_return_value` invocation's settlement (success, synchronous throw, or
/// asynchronous Promise rejection) into a single `oneshot` send - shared by both
/// `call_json_callback` and `call_two_string_callback` below, which differ only in the
/// ThreadsafeFunction's argument type.
fn bridge_promise_result(
    promise_result: Result<PromiseRaw<String>>,
    tx: std::sync::Arc<std::sync::Mutex<Option<futures::channel::oneshot::Sender<std::result::Result<String, String>>>>>,
) {
    match promise_result {
        Ok(promise) => {
            let tx_then = tx.clone();
            let _ = promise.then(move |ctx: CallbackContext<String>| {
                if let Some(sender) = tx_then.lock().unwrap().take() {
                    let _ = sender.send(Ok(ctx.value));
                }
                Ok(())
            });
            let _ = promise.catch(move |_ctx: CallbackContext<Unknown>| {
                if let Some(sender) = tx.lock().unwrap().take() {
                    let _ = sender.send(Err("DIMSE server callback's returned promise rejected".to_owned()));
                }
                Ok(())
            });
        }
        Err(error) => {
            if let Some(sender) = tx.lock().unwrap().take() {
                let _ = sender.send(Err(error.to_string()));
            }
        }
    }
}

/// Calls `callback` with `payload` from whatever thread this is invoked on, and blocks that
/// thread until the callback's returned Promise settles - `Ok(value)` if it resolved (`value` is
/// the resolved string), `Err(message)` if the call itself failed, the callback threw
/// synchronously, or the returned Promise rejected.
fn call_json_callback(callback: &JsonCallback, payload: String) -> std::result::Result<String, String> {
    let (tx, rx) = futures::channel::oneshot::channel();
    let tx = std::sync::Arc::new(std::sync::Mutex::new(Some(tx)));
    callback.call_with_return_value(Ok(payload), ThreadsafeFunctionCallMode::NonBlocking, move |result, _env| {
        bridge_promise_result(result, tx);
        Ok(())
    });
    futures::executor::block_on(rx).unwrap_or_else(|_| Err("DIMSE server callback was dropped before responding".to_owned()))
}

/// As [`call_json_callback`], for the two-plain-string-argument callback (`onMove`).
fn call_two_string_callback(
    callback: &TwoStringCallback,
    payload: (String, String),
) -> std::result::Result<String, String> {
    let (tx, rx) = futures::channel::oneshot::channel();
    let tx = std::sync::Arc::new(std::sync::Mutex::new(Some(tx)));
    callback.call_with_return_value(Ok(payload), ThreadsafeFunctionCallMode::NonBlocking, move |result, _env| {
        bridge_promise_result(result, tx);
        Ok(())
    });
    futures::executor::block_on(rx).unwrap_or_else(|_| Err("DIMSE server callback was dropped before responding".to_owned()))
}

struct NapiScpHandlers {
    on_find: JsonCallback,
    on_move: TwoStringCallback,
    on_association_complete: JsonCallback,
}

impl DcmScpHandlers for NapiScpHandlers {
    fn on_find(
        &self,
        filter: &HashMap<String, String>,
    ) -> std::result::Result<Vec<HashMap<String, String>>, String> {
        let payload = serde_json::to_string(filter).map_err(|e| e.to_string())?;
        let response = call_json_callback(&self.on_find, payload)?;
        serde_json::from_str(&response).map_err(|e| e.to_string())
    }

    /// The request's SOP class rides along in the filter object as `AffectedSOPClassUID` (a command-set keyword, so it
    /// can never collide with an Identifier key); callers that only read the identifier keys are unaffected.
    fn on_find_with_sop_class(
        &self,
        sop_class_uid: &str,
        filter: &HashMap<String, String>,
    ) -> std::result::Result<Vec<HashMap<String, String>>, String> {
        let mut request = filter.clone();
        request.insert("AffectedSOPClassUID".to_owned(), sop_class_uid.to_owned());
        self.on_find(&request)
    }

    fn on_move(&self, study_instance_uid: &str, move_destination_ae: &str) -> std::result::Result<bool, String> {
        let response = call_two_string_callback(
            &self.on_move,
            (study_instance_uid.to_owned(), move_destination_ae.to_owned()),
        )?;
        serde_json::from_str(&response).map_err(|e| e.to_string())
    }

    fn on_association_complete(&self, stored_instances_by_study: &HashMap<String, Vec<String>>) {
        if let Ok(payload) = serde_json::to_string(stored_instances_by_study) {
            let _ = call_json_callback(&self.on_association_complete, payload);
        }
    }
}

#[napi]
pub struct DicomServerHandle {
    inner: std::sync::Mutex<Option<DicomScp>>,
    local_port: u16,
}

#[napi]
impl DicomServerHandle {
    #[napi(getter)]
    pub fn local_port(&self) -> u32 {
        self.local_port as u32
    }

    /// Stops accepting new associations and waits for the listener to shut down. Associations
    /// already in progress finish independently (or hit their own idle timeout) - this does not
    /// interrupt them. Safe to call more than once.
    #[napi]
    pub fn close(&self) {
        if let Some(scp) = self.inner.lock().unwrap().take() {
            scp.stop();
        }
    }
}

/// Starts a DICOM SCP (association acceptor) on `port` (0 picks an ephemeral port - see the
/// returned handle's `localPort`), answering C-ECHO, C-STORE, C-FIND, and C-MOVE. C-ECHO and
/// C-STORE are handled entirely natively (C-STORE writes each received instance under
/// `cachePath` as `S_<StudyInstanceUID>/<Modality>_<SOPInstanceUID>.dcm`); C-FIND, C-MOVE, and
/// "association complete" (fired once per association that stored at least one instance) are
/// delegated to the given `async function` callbacks:
///
/// - `onFind(filterJson)` - `filterJson` has whichever of StudyInstanceUID/StudyDate/
///   PatientName/PatientID keys the request's Identifier actually had values for, plus
///   `AffectedSOPClassUID` (Study Root or Patient Root Query/Retrieve FIND; any other C-FIND,
///   e.g. Modality Worklist, is answered 0x0122 "SOP class not supported" without calling
///   `onFind`). Must resolve
///   to a JSON array of matched-study objects (whichever of StudyInstanceUID/AccessionNumber/
///   PatientID/PatientName/PatientBirthDate/StudyDate/StudyTime/StudyDescription/
///   ModalitiesInStudy/NumberOfStudyRelatedSeries/NumberOfStudyRelatedInstances each has).
/// - `onMove(studyInstanceUid, moveDestinationAe)` - must resolve to `"true"`/`"false"`
///   (JSON-booleans-as-strings) indicating whether the move was successfully queued.
/// - `onAssociationComplete(storedInstancesByStudyJson)` - `storedInstancesByStudyJson` maps
///   each StudyInstanceUID to the on-disk paths of every instance C-STORE'd for it during that
///   association. Return value is ignored.
///
/// `onLog`, if given, is called (fire-and-forget, from whichever connection thread is handling
/// the association at the time) with a debug-detail line per association accept/negotiation,
/// each request/response, and release/abort - the SCP-side counterpart of `onLog` on
/// `echoScu`/`storeScu`/`findScu`/`moveScu`.
///
/// Accepts every proposed presentation context regardless of abstract syntax - this is a
/// permissive "accept anything a real sender proposes" SCP, not a curated allow-list. A
/// C-CANCEL-RQ during a C-FIND stops the matches and answers 0xFE00.
#[napi]
pub fn start_dicom_server(
    port: u32,
    cache_path: String,
    ae_title: String,
    max_pdu_length: Option<u32>,
    idle_timeout_ms: Option<u32>,
    on_find: JsonCallback,
    on_move: TwoStringCallback,
    on_association_complete: JsonCallback,
    on_log: Option<ThreadsafeFunction<String>>,
) -> Result<DicomServerHandle> {
    let handlers: std::sync::Arc<dyn DcmScpHandlers> = std::sync::Arc::new(NapiScpHandlers {
        on_find,
        on_move,
        on_association_complete,
    });
    let options = DcmScpOptions {
        ae_title,
        // 256 KiB, matching ScpOptions::default() in dicom_io/scp.rs - see that struct's doc
        // comment for why this ceiling has to be generous rather than per-requestor.
        max_pdu_length: max_pdu_length.unwrap_or(262_144),
        idle_timeout: Duration::from_millis(u64::from(idle_timeout_ms.unwrap_or(300_000))),
        on_log: on_log.map(|callback| std::sync::Arc::new(NapiDimseLogger { callback }) as std::sync::Arc<dyn DimseLogger>),
        ..Default::default()
    };

    let scp = dcm_start_scp(port as u16, PathBuf::from(cache_path), handlers, options).map_err(to_napi_err)?;
    let local_port = scp.local_port();

    Ok(DicomServerHandle { inner: std::sync::Mutex::new(Some(scp)), local_port })
}
