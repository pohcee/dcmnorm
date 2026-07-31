use std::collections::HashMap;
use std::panic::{self, AssertUnwindSafe};
use std::path::PathBuf;
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use napi::bindgen_prelude::*;
use napi_derive::napi;

use dcmnorm::dicom_io::{
    apply_filter_to_object, echo_scu as dcm_echo_scu, find_scu as dcm_find_scu,
    move_scu as dcm_move_scu, parse_attribute_override, parse_filter_requests, parse_tag_key,
    probe_dicom_file_for_sop_class_uid, read_dicom_bytes, read_dicom_file,
    read_dicom_json_with_options, read_dicom_object_for_filter, remove_attribute,
    remove_private_tags_inplace, render_dicom_frame as dcm_render_dicom_frame, set_attribute,
    start_scp as dcm_start_scp, store_scu as dcm_store_scu, transcode_dicom_file, write_dicom_file,
    write_dicom_json_with_options, write_dicom_video as dcm_write_dicom_video, CancelMode as DcmCancelMode,
    CancelSignal, DicomJsonBulkDataMode, DicomJsonFormat, DicomJsonKeyStyle, DicomJsonReadOptions,
    DicomJsonWriteOptions, DicomScp, DimseLogger, EchoScuOptions as DcmEchoScuOptions,
    FindScuOptions as DcmFindScuOptions, MoveScuOptions as DcmMoveScuOptions,
    RenderOutputFormat as DcmRenderOutputFormat, RenderPipelineOptions as DcmRenderPipelineOptions,
    ScpHandlers as DcmScpHandlers, ScpOptions as DcmScpOptions, StoreScuOptions as DcmStoreScuOptions,
};
use napi::threadsafe_function::{ThreadsafeFunction, ThreadsafeFunctionCallMode};

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
                read_dicom_object_for_filter(&self.file_path, &requests).map_err(to_napi_err)?;
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
}

pub struct ReadJsonTask {
    file_path: PathBuf,
    format: DicomJsonFormat,
    key_style: DicomJsonKeyStyle,
    bulk_data_mode: DicomJsonBulkDataMode,
}

impl Task for ReadJsonTask {
    type Output = String;
    type JsValue = String;

    fn compute(&mut self) -> Result<Self::Output> {
        guarded(|| {
            // Uri mode needs the source bytes to compute a valid "?offset=..&length=.."
            // reference for bulk-data elements (PixelData etc.) - mirrors exactly how
            // the CLI itself does this (see run_dicom_to_json_with_object in main.rs:
            // `bulk_data_source: if bulk_data_mode == Uri { input_bytes } else { None }`).
            // Without it, write_dicom_json_with_options silently falls back to
            // base64-inlining those elements instead - ~1000x larger output for a
            // typical image, confirmed empirically against the CLI's actual output.
            let (object, file_bytes) = if self.bulk_data_mode == DicomJsonBulkDataMode::Uri {
                let bytes = std::fs::read(&self.file_path).map_err(|e| to_napi_err(e))?;
                let object = read_dicom_bytes(&bytes).map_err(to_napi_err)?;
                (object, Some(bytes))
            } else {
                (read_dicom_file(&self.file_path).map_err(to_napi_err)?, None)
            };
            write_dicom_json_with_options(
                &object,
                DicomJsonWriteOptions {
                    format: self.format,
                    key_style: self.key_style,
                    bulk_data_mode: self.bulk_data_mode,
                    bulk_data_source: file_bytes.as_deref(),
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
    // non-`Clone` types (`std::io::Error`, `dicom_ul`'s association error, ...), and every waiter
    // here needs its own owned copy of whichever outcome was recorded first.
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
/// `studyInstanceUid` to `moveDestinationAe` (an AE title `destination` already knows how to
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
    let logger = dimse_logger(on_log);

    let state = ScuCallState::spawn(move || {
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
}

#[napi(object)]
pub struct RenderedFrame {
    pub mime_type: String,
    pub width: u32,
    pub height: u32,
    pub data: Buffer,
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
            let object = read_dicom_file(&self.file_path).map_err(to_napi_err)?;
            let pipeline_options = DcmRenderPipelineOptions {
                frame_index: self.options.frame_index.unwrap_or(0) as usize,
                window_center: self.options.window_center,
                window_width: self.options.window_width,
                output_width: self.options.output_width,
                output_height: self.options.output_height,
                jpeg_quality: self.options.jpeg_quality.unwrap_or(90) as u8,
                ..Default::default()
            };
            let rendered = dcm_render_dicom_frame(&object, format, &pipeline_options).map_err(to_napi_err)?;
            Ok(RenderedFrame {
                mime_type: mime_type.to_owned(),
                width: rendered.width as u32,
                height: rendered.height as u32,
                data: rendered.bytes.into(),
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
///   PatientName/PatientID keys the request's Identifier actually had values for. Must resolve
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
/// permissive "accept anything a real sender proposes" SCP, not a curated allow-list.
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
    };

    let scp = dcm_start_scp(port as u16, PathBuf::from(cache_path), handlers, options).map_err(to_napi_err)?;
    let local_port = scp.local_port();

    Ok(DicomServerHandle { inner: std::sync::Mutex::new(Some(scp)), local_port })
}
