use std::collections::HashMap;
use std::panic::{self, AssertUnwindSafe};
use std::path::PathBuf;
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use pyo3::create_exception;
use pyo3::exceptions::PyException;
use pyo3::prelude::*;
use pyo3::types::PyBytes;

use ::dcmnorm::dicom_io::{
    apply_filter_to_object, build_volume as dcm_build_volume, echo_scu as dcm_echo_scu,
    find_scu as dcm_find_scu, move_scu as dcm_move_scu, parse_attribute_override,
    parse_filter_requests, parse_tag_key, probe_dicom_file_for_sop_class_uid, read_dicom_bytes,
    read_dicom_file, read_dicom_json_with_options, read_dcmnorm_object_for_filter,
    pack_dicom_frame_stack_texture as dcm_pack_dicom_frame_stack_texture,
    pack_dicom_frame_texture as dcm_pack_dicom_frame_texture, pack_volume_texture as dcm_pack_volume_texture,
    reformat_plane as dcm_reformat_plane, remove_attribute, remove_private_tags_inplace,
    render_dicom_frame as dcm_render_dicom_frame, set_attribute, start_scp as dcm_start_scp,
    store_scu as dcm_store_scu, transcode_dicom_file, write_dicom_file,
    write_dicom_json_with_options, write_dicom_video as dcm_write_dicom_video, CancelMode as DcmCancelMode,
    CancelSignal, DicomJsonBulkDataMode, DicomJsonFormat, DicomJsonKeyStyle, DicomJsonReadOptions,
    DicomJsonWriteOptions, DicomScp, DimseLogger, EchoScuOptions as DcmEchoScuOptions,
    FindScuOptions as DcmFindScuOptions, Interpolation as DcmInterpolation,
    MoveScuOptions as DcmMoveScuOptions, OverlaySummary as DcmOverlaySummary,
    PlaneParams as DcmPlaneParams, RenderOutputFormat as DcmRenderOutputFormat,
    RenderPipelineOptions as DcmRenderPipelineOptions, ScpHandlers as DcmScpHandlers,
    ScpOptions as DcmScpOptions, SlabProjection as DcmSlabProjection,
    StoreScuOptions as DcmStoreScuOptions, TextureCompression as DcmTextureCompression,
    TextureMeta as DcmTextureMeta, Volume as DcmVolume,
};

create_exception!(dcmnorm, DcmnormError, PyException);

fn to_py_err(err: impl std::fmt::Display) -> PyErr {
    DcmnormError::new_err(err.to_string())
}

// DICOM files arrive from arbitrary, sometimes-malformed vendor equipment. A subprocess-based
// CLI call fails in isolation when one is bad; an in-process native extension does not get that
// for free, so every entry point runs through catch_unwind and turns a panic into a raised
// DcmnormError instead of taking the whole Python process down with it - mirrors `guarded()` in
// the Node bindings (`bindings/node/src/lib.rs`).
fn guarded<T>(f: impl FnOnce() -> PyResult<T>) -> PyResult<T> {
    match panic::catch_unwind(AssertUnwindSafe(f)) {
        Ok(result) => result,
        Err(payload) => {
            let message = payload
                .downcast_ref::<&str>()
                .map(|s| s.to_string())
                .or_else(|| payload.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "dcmnorm panicked while processing this file".to_string());
            Err(DcmnormError::new_err(format!("dcmnorm internal error: {message}")))
        }
    }
}

fn parse_json_format(value: Option<&str>) -> PyResult<DicomJsonFormat> {
    match value {
        None | Some("flat") => Ok(DicomJsonFormat::Flat),
        Some("standard") => Ok(DicomJsonFormat::Standard),
        Some(other) => Err(DcmnormError::new_err(format!(
            "invalid format '{other}'; expected 'flat' or 'standard'"
        ))),
    }
}

fn parse_key_style(value: Option<&str>) -> PyResult<DicomJsonKeyStyle> {
    match value {
        None | Some("name") => Ok(DicomJsonKeyStyle::Name),
        Some("hex") => Ok(DicomJsonKeyStyle::Hex),
        Some(other) => Err(DcmnormError::new_err(format!(
            "invalid key_style '{other}'; expected 'name' or 'hex'"
        ))),
    }
}

// The dcmnorm CLI's --bulk-data flag defaults to Uri (a small "?offset=..&length=.." reference),
// overriding DicomJsonWriteOptions::default()'s InlineBinary at the CLI layer - so callers here
// need the same override, not the library default, or PixelData ends up fully base64-inlined
// (~1000x larger output for a typical image) instead of referenced. Mirrors the Node bindings.
fn parse_bulk_data_mode(value: Option<&str>) -> PyResult<DicomJsonBulkDataMode> {
    match value {
        None | Some("uri") => Ok(DicomJsonBulkDataMode::Uri),
        Some("inline") => Ok(DicomJsonBulkDataMode::InlineBinary),
        Some(other) => Err(DcmnormError::new_err(format!(
            "invalid bulk_data '{other}'; expected 'uri' or 'inline'"
        ))),
    }
}

fn parse_render_output_format(value: Option<&str>) -> PyResult<(DcmRenderOutputFormat, &'static str)> {
    match value {
        None | Some("jpeg") | Some("jpg") => Ok((DcmRenderOutputFormat::Jpeg, "image/jpeg")),
        Some("png") => Ok((DcmRenderOutputFormat::Png, "image/png")),
        Some(other) => Err(DcmnormError::new_err(format!(
            "invalid format '{other}'; expected 'jpeg' or 'png'"
        ))),
    }
}

/// Parses `"#RRGGBB"` or `"R,G,B"` into an `[R, G, B]` byte triple.
fn parse_render_color(value: &str) -> PyResult<[u8; 3]> {
    let trimmed = value.trim();

    if let Some(hex) = trimmed.strip_prefix('#') {
        if hex.len() != 6 {
            return Err(DcmnormError::new_err(format!(
                "invalid color '{value}'; hex color must be #RRGGBB"
            )));
        }
        let channel = |range: std::ops::Range<usize>| {
            u8::from_str_radix(&hex[range], 16)
                .map_err(|_| DcmnormError::new_err(format!("invalid color '{value}'; not a valid hex color")))
        };
        return Ok([channel(0..2)?, channel(2..4)?, channel(4..6)?]);
    }

    let parts: Vec<&str> = trimmed.split(',').collect();
    if parts.len() != 3 {
        return Err(DcmnormError::new_err(format!(
            "invalid color '{value}'; expected R,G,B (0-255 each) or #RRGGBB"
        )));
    }
    let mut channels = [0u8; 3];
    for (index, part) in parts.iter().enumerate() {
        channels[index] = part.trim().parse::<u8>().map_err(|_| {
            DcmnormError::new_err(format!("invalid color '{value}'; expected R,G,B (0-255 each) or #RRGGBB"))
        })?;
    }
    Ok(channels)
}

fn overlay_pipeline_fields(
    show_overlays: Option<bool>,
    overlay_index: Option<u32>,
    overlay_color: Option<&str>,
) -> PyResult<(bool, Option<usize>, [u8; 3])> {
    let color = overlay_color.map(parse_render_color).transpose()?.unwrap_or([0, 255, 0]);
    Ok((show_overlays.unwrap_or(true), overlay_index.map(|value| value as usize), color))
}

fn parse_interpolation(value: Option<&str>) -> PyResult<DcmInterpolation> {
    match value {
        None | Some("trilinear") => Ok(DcmInterpolation::Trilinear),
        Some("nearest") => Ok(DcmInterpolation::Nearest),
        Some(other) => Err(DcmnormError::new_err(format!(
            "invalid interpolation '{other}'; expected 'trilinear' or 'nearest'"
        ))),
    }
}

fn parse_slab_projection(value: Option<&str>) -> PyResult<DcmSlabProjection> {
    match value {
        None | Some("mip") => Ok(DcmSlabProjection::MaximumIntensity),
        Some("minip") => Ok(DcmSlabProjection::MinimumIntensity),
        Some("average") => Ok(DcmSlabProjection::Average),
        Some(other) => Err(DcmnormError::new_err(format!(
            "invalid slab_projection '{other}'; expected 'mip', 'minip', or 'average'"
        ))),
    }
}

fn parse_texture_compression(value: Option<&str>) -> PyResult<DcmTextureCompression> {
    match value {
        None | Some("gzip") => Ok(DcmTextureCompression::Gzip),
        Some("none") => Ok(DcmTextureCompression::None),
        Some(other) => Err(DcmnormError::new_err(format!(
            "invalid compression '{other}'; expected 'gzip' or 'none'"
        ))),
    }
}

fn parse_vec3(values: Vec<f64>, field_name: &'static str) -> PyResult<[f64; 3]> {
    if values.len() != 3 {
        return Err(DcmnormError::new_err(format!("{field_name} must have exactly 3 values (x, y, z)")));
    }
    Ok([values[0], values[1], values[2]])
}

// -------------------------------------------------------------------------------------------
// Core: read_tags / read_json / write_json / edit_tags / transcode / check_dicom
// -------------------------------------------------------------------------------------------

/// Reads only the requested tags from a DICOM file, stopping as soon as the highest one has been
/// parsed. Mirrors `dcmnorm --filter ... --format flat --keys hex`. Keys accept DICOM keywords
/// (`StudyInstanceUID`) or tag expressions (`(0020,000D)`); the returned JSON is keyed by bare
/// hex tag (`"0020000D"`). Returns a JSON string - parse it Python-side (`json.loads`).
#[pyfunction]
fn read_tags(py: Python<'_>, file_path: String, tags: Vec<String>) -> PyResult<String> {
    py.allow_threads(|| {
        guarded(|| {
            let path = PathBuf::from(file_path);
            let requests = parse_filter_requests(&tags).map_err(to_py_err)?;
            let mut object = read_dcmnorm_object_for_filter(&path, &requests).map_err(to_py_err)?;
            apply_filter_to_object(&mut object, &requests);
            write_dicom_json_with_options(
                &object,
                DicomJsonWriteOptions {
                    format: DicomJsonFormat::Flat,
                    key_style: DicomJsonKeyStyle::Hex,
                    bulk_data_mode: DicomJsonBulkDataMode::Uri,
                    ..Default::default()
                },
            )
            .map_err(to_py_err)
        })
    })
}

/// Reads the full DICOM dataset as JSON. Mirrors plain `dcmnorm file.dcm` (flat/name keys, bulk
/// data as a URI reference, by default) or `format="standard", key_style="hex",
/// bulk_data="inline"`. `bulk_data` defaults to `"uri"` (matching the CLI), not the Rust
/// library's own default of inline-embedding - getting this wrong makes a huge difference:
/// `"inline"` base64-embeds elements like `PixelData` directly, ~1000x larger output for a
/// typical image, instead of a small `"?offset=..&length=.."` reference.
#[pyfunction]
#[pyo3(signature = (file_path, format=None, key_style=None, bulk_data=None))]
fn read_json(
    py: Python<'_>,
    file_path: String,
    format: Option<String>,
    key_style: Option<String>,
    bulk_data: Option<String>,
) -> PyResult<String> {
    let format = parse_json_format(format.as_deref())?;
    let key_style = parse_key_style(key_style.as_deref())?;
    let bulk_data_mode = parse_bulk_data_mode(bulk_data.as_deref())?;
    py.allow_threads(|| {
        guarded(|| {
            let path = PathBuf::from(file_path);
            let (object, file_bytes) = if bulk_data_mode == DicomJsonBulkDataMode::Uri {
                let bytes = std::fs::read(&path).map_err(to_py_err)?;
                let object = read_dicom_bytes(&bytes).map_err(to_py_err)?;
                (object, Some(bytes))
            } else {
                (read_dicom_file(&path).map_err(to_py_err)?, None)
            };
            let bulk_scan_failed = std::cell::Cell::new(false);
            let bulk_scan_cursor = std::cell::Cell::new(0usize);
            write_dicom_json_with_options(
                &object,
                DicomJsonWriteOptions {
                    format,
                    key_style,
                    bulk_data_mode,
                    bulk_data_source: file_bytes.as_deref(),
                    bulk_scan_failed: Some(&bulk_scan_failed),
                    bulk_scan_cursor: Some(&bulk_scan_cursor),
                    ..Default::default()
                },
            )
            .map_err(to_py_err)
        })
    })
}

/// Writes a DICOM file from JSON (flat or standard format, auto never guessed - pass the same
/// `format` used to read it). Mirrors `dcmnorm dataset.json out.dcm`. `bulk_data_source_path`
/// resolves `"?offset=..&length=.."` BulkDataURIs (`read_json`'s default `"uri"` bulk_data mode
/// emits these for bulk elements already in the source file) against that file's bytes - mirrors
/// the CLI's `--bulk-data-source`.
#[pyfunction]
#[pyo3(signature = (json, output_path, format=None, bulk_data_source_path=None))]
fn write_json(
    py: Python<'_>,
    json: String,
    output_path: String,
    format: Option<String>,
    bulk_data_source_path: Option<String>,
) -> PyResult<()> {
    let format = parse_json_format(format.as_deref())?;
    py.allow_threads(|| {
        guarded(|| {
            let bulk_data_source = bulk_data_source_path
                .as_ref()
                .map(std::fs::read)
                .transpose()
                .map_err(to_py_err)?;

            let mut object = read_dicom_json_with_options(
                &json,
                DicomJsonReadOptions { format, bulk_data_source: bulk_data_source.as_deref() },
            )
            .map_err(to_py_err)?;

            write_dicom_file(&mut object, &PathBuf::from(output_path)).map_err(to_py_err)
        })
    })
}

/// Sets/removes DICOM attributes, optionally stripping private tags. Mirrors `dcmnorm --set
/// KEY=VALUE --remove KEY --remove-private-tags`. Writes back to `file_path` in place unless
/// `output_path` is given. `set` is a dict of `{tag_key: value}`.
#[pyfunction]
#[pyo3(signature = (file_path, output_path=None, set=None, remove=None, remove_private_tags=None))]
fn edit_tags(
    py: Python<'_>,
    file_path: String,
    output_path: Option<String>,
    set: Option<HashMap<String, String>>,
    remove: Option<Vec<String>>,
    remove_private_tags: Option<bool>,
) -> PyResult<()> {
    let output_path = output_path.unwrap_or_else(|| file_path.clone());
    let sets = set.unwrap_or_default();
    let removes = remove.unwrap_or_default();
    let remove_private_tags = remove_private_tags.unwrap_or(false);

    py.allow_threads(|| {
        guarded(|| {
            let mut object = read_dicom_file(&PathBuf::from(&file_path)).map_err(to_py_err)?;

            for (key, value) in &sets {
                let assignment = format!("{key}={value}");
                let (tag, vr, value) = parse_attribute_override(&assignment).map_err(to_py_err)?;
                set_attribute(&mut object, tag, vr, value).map_err(to_py_err)?;
            }

            for key in &removes {
                let tag = parse_tag_key(key).map_err(to_py_err)?;
                remove_attribute(&mut object, tag);
            }

            if remove_private_tags {
                remove_private_tags_inplace(&mut object);
            }

            write_dicom_file(&mut object, &PathBuf::from(&output_path)).map_err(to_py_err)
        })
    })
}

/// Transcodes a DICOM file to the given transfer syntax UID. Mirrors `dcmnorm --transfer-syntax
/// UID in.dcm out.dcm`.
#[pyfunction]
fn transcode(py: Python<'_>, file_path: String, output_path: String, transfer_syntax_uid: String) -> PyResult<()> {
    py.allow_threads(|| {
        guarded(|| {
            transcode_dicom_file(&PathBuf::from(file_path), &PathBuf::from(output_path), &transfer_syntax_uid)
                .map_err(to_py_err)
        })
    })
}

/// Reports whether a file looks like valid DICOM. Mirrors `dcmnorm --check-dicom`.
#[pyfunction]
fn check_dicom(py: Python<'_>, file_path: String) -> bool {
    py.allow_threads(|| {
        panic::catch_unwind(AssertUnwindSafe(|| probe_dicom_file_for_sop_class_uid(&PathBuf::from(file_path))))
            .ok()
            .and_then(|result| result.ok())
            .unwrap_or(false)
    })
}

// -------------------------------------------------------------------------------------------
// DIMSE: echo_scu / store_scu / find_scu / move_scu
// -------------------------------------------------------------------------------------------

/// Bridges dcmnorm's `DimseLogger` trait onto a Python callable. Unlike the Node bindings (which
/// need a `ThreadsafeFunction`/Promise-based bridge because a JS callback is inherently async),
/// a Python callable is invoked synchronously by acquiring the GIL on whichever thread the log
/// event originates from - the calling thread itself for a blocking `*_scu` call, or the
/// call's own background thread for a `start_*_scu` handle.
struct PyDimseLogger {
    callback: Py<PyAny>,
}

impl DimseLogger for PyDimseLogger {
    fn log(&self, message: String) {
        Python::with_gil(|py| {
            let _ = self.callback.call1(py, (message,));
        });
    }
}

fn dimse_logger(on_log: Option<Py<PyAny>>) -> Option<Box<dyn DimseLogger>> {
    on_log.map(|callback| Box::new(PyDimseLogger { callback }) as Box<dyn DimseLogger>)
}

fn dimse_logger_arc(on_log: Option<Py<PyAny>>) -> Option<Arc<dyn DimseLogger>> {
    on_log.map(|callback| Arc::new(PyDimseLogger { callback }) as Arc<dyn DimseLogger>)
}

// ---------------------------------------------------------------------------------------------
// Shared SCU call/handle machinery - see the Node bindings' own doc comment on `ScuCallState` for
// the full rationale (a `start_*_scu` call runs on its own `std::thread::spawn`, not a shared
// pool, so a long-running `move` can't starve an unrelated concurrent `abort()`/`release()`).
// Ported to PyO3: `result()`/`release()`/`abort()` block the calling Python thread on a Condvar,
// with the GIL released for the duration via `py.allow_threads`, instead of Node's
// AsyncTask-per-poll approach - Python has no libuv threadpool to avoid occupying.
// ---------------------------------------------------------------------------------------------

struct ScuCallState<T> {
    // The error side is a `String`, not `dcmnorm::DimseError` - the latter wraps foreign
    // non-`Clone` types, and every waiter here needs its own owned copy of whichever outcome was
    // recorded first.
    result: Mutex<Option<std::result::Result<T, String>>>,
    condvar: Condvar,
}

impl<T: Clone + Send + 'static> ScuCallState<T> {
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

    /// Blocks the CURRENT thread until `work` above has finished. Idempotent: any number of
    /// callers just observe the same result. Callers are expected to have already released the
    /// GIL (`py.allow_threads`) before calling this.
    fn block_until_done(&self) -> std::result::Result<T, String> {
        let mut guard = self.result.lock().unwrap();
        while guard.is_none() {
            guard = self.condvar.wait(guard).unwrap();
        }
        guard.clone().unwrap()
    }
}

fn wait_scu<T: Clone + Send + 'static>(
    py: Python<'_>,
    state: &Arc<ScuCallState<T>>,
    request_cancel: Option<(&Arc<CancelSignal>, DcmCancelMode)>,
) -> PyResult<T> {
    if let Some((signal, mode)) = request_cancel {
        signal.request(mode);
    }
    let state = state.clone();
    py.allow_threads(|| state.block_until_done()).map_err(DcmnormError::new_err)
}

#[pyclass]
struct EchoScuHandle {
    cancel: Arc<CancelSignal>,
    state: Arc<ScuCallState<u16>>,
}

#[pymethods]
impl EchoScuHandle {
    /// The eventual Status code (0 = success), without requesting cancellation.
    fn result(&self, py: Python<'_>) -> PyResult<u16> {
        wait_scu(py, &self.state, None)
    }

    /// Requests a graceful A-RELEASE and returns once the association has genuinely closed - real
    /// acknowledgement, not a fire-and-forget signal. Idempotent.
    fn release(&self, py: Python<'_>) -> PyResult<u16> {
        wait_scu(py, &self.state, Some((&self.cancel, DcmCancelMode::Release)))
    }

    /// Requests an immediate A-ABORT (no wait on the peer at all) and returns once the
    /// association has genuinely closed. Idempotent.
    fn abort(&self, py: Python<'_>) -> PyResult<u16> {
        wait_scu(py, &self.state, Some((&self.cancel, DcmCancelMode::Abort)))
    }
}

/// Performs a C-ECHO (DICOM Verification) against `destination` ("host:port"). Returns the
/// response Status code (0 = success); raises `DcmnormError` only if the association itself
/// could not be established (unreachable host, no accepted presentation context, etc). `on_log`,
/// if given, is called (synchronously, no return value expected) with a debug line for each
/// notable DIMSE event - association open/close, the request sent, the response received.
#[pyfunction]
#[pyo3(signature = (destination, calling_ae_title=None, called_ae_title=None, timeout_ms=None, on_log=None))]
fn echo_scu(
    py: Python<'_>,
    destination: String,
    calling_ae_title: Option<String>,
    called_ae_title: Option<String>,
    timeout_ms: Option<u32>,
    on_log: Option<Py<PyAny>>,
) -> PyResult<u16> {
    py.allow_threads(|| {
        guarded(|| {
            dcm_echo_scu(
                &destination,
                DcmEchoScuOptions {
                    calling_ae_title: calling_ae_title.unwrap_or_else(|| "DCMNORM".to_owned()),
                    called_ae_title,
                    timeout: timeout_ms.map(|ms| Duration::from_millis(ms as u64)),
                    on_log: dimse_logger(on_log),
                    cancel: None,
                },
            )
            .map_err(to_py_err)
        })
    })
}

/// Same as `echo_scu`, but returns immediately with a handle instead of blocking until the C-ECHO
/// completes - lets a caller `abort()`/`release()` it early. See `EchoScuHandle`.
#[pyfunction]
#[pyo3(signature = (destination, calling_ae_title=None, called_ae_title=None, timeout_ms=None, on_log=None))]
fn start_echo_scu(
    destination: String,
    calling_ae_title: Option<String>,
    called_ae_title: Option<String>,
    timeout_ms: Option<u32>,
    on_log: Option<Py<PyAny>>,
) -> EchoScuHandle {
    let cancel = CancelSignal::new();
    let cancel_for_call = cancel.clone();
    let calling_ae_title = calling_ae_title.unwrap_or_else(|| "DCMNORM".to_owned());
    let timeout = timeout_ms.map(|ms| Duration::from_millis(ms as u64));
    let logger = dimse_logger(on_log);

    let state = ScuCallState::spawn(move || {
        dcm_echo_scu(
            &destination,
            DcmEchoScuOptions { calling_ae_title, called_ae_title, timeout, on_log: logger, cancel: Some(cancel_for_call) },
        )
        .map_err(|error| error.to_string())
    });

    EchoScuHandle { cancel, state }
}

#[pyclass(get_all)]
#[derive(Clone)]
struct StoreScuResult {
    sop_instance_uid: String,
    status: u16,
}

#[pyclass]
struct StoreScuHandle {
    cancel: Arc<CancelSignal>,
    state: Arc<ScuCallState<Vec<StoreScuResult>>>,
}

#[pymethods]
impl StoreScuHandle {
    /// The eventual per-file results, without requesting cancellation.
    fn result(&self, py: Python<'_>) -> PyResult<Vec<StoreScuResult>> {
        wait_scu(py, &self.state, None)
    }

    /// Requests a graceful A-RELEASE and returns once the association has genuinely closed - real
    /// acknowledgement, not a fire-and-forget signal. Idempotent.
    fn release(&self, py: Python<'_>) -> PyResult<Vec<StoreScuResult>> {
        wait_scu(py, &self.state, Some((&self.cancel, DcmCancelMode::Release)))
    }

    /// Requests an immediate A-ABORT (no wait on the peer at all) and returns once the
    /// association has genuinely closed. Idempotent.
    fn abort(&self, py: Python<'_>) -> PyResult<Vec<StoreScuResult>> {
        wait_scu(py, &self.state, Some((&self.cancel, DcmCancelMode::Abort)))
    }
}

fn map_store_results(results: Vec<::dcmnorm::dicom_io::StoreScuResult>) -> Vec<StoreScuResult> {
    results
        .into_iter()
        .map(|result| StoreScuResult { sop_instance_uid: result.sop_instance_uid, status: result.status })
        .collect()
}

/// Sends each of `files` via C-STORE to `destination` ("host:port"). Returns one
/// `StoreScuResult` per file that could be read and sent - a non-zero status is just data in the
/// result (the peer rejected that instance), not a raised error; this only raises if the
/// association itself could not be established, or none of `files` could be read as DICOM at
/// all. `on_log`, if given, is called (synchronously) with a debug line for each notable DIMSE
/// event.
#[pyfunction]
#[pyo3(signature = (destination, files, calling_ae_title=None, called_ae_title=None, max_pdu_length=None, never_transcode=None, timeout_ms=None, on_log=None))]
#[allow(clippy::too_many_arguments)]
fn store_scu(
    py: Python<'_>,
    destination: String,
    files: Vec<String>,
    calling_ae_title: Option<String>,
    called_ae_title: Option<String>,
    max_pdu_length: Option<u32>,
    never_transcode: Option<bool>,
    timeout_ms: Option<u32>,
    on_log: Option<Py<PyAny>>,
) -> PyResult<Vec<StoreScuResult>> {
    let files: Vec<PathBuf> = files.into_iter().map(PathBuf::from).collect();
    py.allow_threads(|| {
        guarded(|| {
            let results = dcm_store_scu(
                &destination,
                &files,
                DcmStoreScuOptions {
                    calling_ae_title: calling_ae_title.unwrap_or_else(|| "DCMNORM".to_owned()),
                    called_ae_title,
                    max_pdu_length: max_pdu_length.unwrap_or(16384),
                    never_transcode: never_transcode.unwrap_or(false),
                    timeout: timeout_ms.map(|ms| Duration::from_millis(ms as u64)),
                    on_log: dimse_logger(on_log),
                    cancel: None,
                },
            )
            .map_err(to_py_err)?;
            Ok(map_store_results(results))
        })
    })
}

/// Same as `store_scu`, but returns immediately with a handle instead of blocking until every
/// file has been sent - lets a caller `abort()`/`release()` it early. See `StoreScuHandle`.
#[pyfunction]
#[pyo3(signature = (destination, files, calling_ae_title=None, called_ae_title=None, max_pdu_length=None, never_transcode=None, timeout_ms=None, on_log=None))]
#[allow(clippy::too_many_arguments)]
fn start_store_scu(
    destination: String,
    files: Vec<String>,
    calling_ae_title: Option<String>,
    called_ae_title: Option<String>,
    max_pdu_length: Option<u32>,
    never_transcode: Option<bool>,
    timeout_ms: Option<u32>,
    on_log: Option<Py<PyAny>>,
) -> StoreScuHandle {
    let files: Vec<PathBuf> = files.into_iter().map(PathBuf::from).collect();
    let cancel = CancelSignal::new();
    let cancel_for_call = cancel.clone();
    let calling_ae_title = calling_ae_title.unwrap_or_else(|| "DCMNORM".to_owned());
    let max_pdu_length = max_pdu_length.unwrap_or(16384);
    let never_transcode = never_transcode.unwrap_or(false);
    let timeout = timeout_ms.map(|ms| Duration::from_millis(ms as u64));
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
        .map(map_store_results)
        .map_err(|error| error.to_string())
    });

    StoreScuHandle { cancel, state }
}

#[pyclass]
struct FindScuHandle {
    cancel: Arc<CancelSignal>,
    state: Arc<ScuCallState<Vec<String>>>,
}

#[pymethods]
impl FindScuHandle {
    /// The eventual matched Identifiers (flat/hex-keyed DICOM JSON, one per match), without
    /// requesting cancellation.
    fn result(&self, py: Python<'_>) -> PyResult<Vec<String>> {
        wait_scu(py, &self.state, None)
    }

    /// Requests a graceful A-RELEASE and returns once the association has genuinely closed - real
    /// acknowledgement, not a fire-and-forget signal. Idempotent.
    fn release(&self, py: Python<'_>) -> PyResult<Vec<String>> {
        wait_scu(py, &self.state, Some((&self.cancel, DcmCancelMode::Release)))
    }

    /// Requests an immediate A-ABORT (no wait on the peer at all) and returns once the
    /// association has genuinely closed. Idempotent.
    fn abort(&self, py: Python<'_>) -> PyResult<Vec<String>> {
        wait_scu(py, &self.state, Some((&self.cancel, DcmCancelMode::Abort)))
    }
}

/// Performs a C-FIND (Study Root Query/Retrieve) against `destination` ("host:port"). `query`
/// values: empty string is a universal-match "return key" (mirrors findscu's bare `-k TAG`),
/// non-empty constrains the match (mirrors `-k TAG=value`); `QueryRetrieveLevel` defaults to
/// `"STUDY"` if not given. Returns one flat/hex-keyed DICOM JSON string per match (parse
/// Python-side, same shape as `read_json(format="flat", key_style="hex")`). `on_log`, if given,
/// is called (synchronously) with a debug line for each notable DIMSE event - association
/// open/close, and each C-FIND-RQ/RSP (query values are not logged, only the tag keys queried,
/// since the Identifier commonly carries PHI).
#[pyfunction]
#[pyo3(signature = (destination, query, calling_ae_title=None, called_ae_title=None, max_pdu_length=None, timeout_ms=None, on_log=None))]
#[allow(clippy::too_many_arguments)]
fn find_scu(
    py: Python<'_>,
    destination: String,
    query: HashMap<String, String>,
    calling_ae_title: Option<String>,
    called_ae_title: Option<String>,
    max_pdu_length: Option<u32>,
    timeout_ms: Option<u32>,
    on_log: Option<Py<PyAny>>,
) -> PyResult<Vec<String>> {
    py.allow_threads(|| {
        guarded(|| {
            dcm_find_scu(
                &destination,
                &query,
                DcmFindScuOptions {
                    calling_ae_title: calling_ae_title.unwrap_or_else(|| "DCMNORM".to_owned()),
                    called_ae_title,
                    max_pdu_length: max_pdu_length.unwrap_or(16384),
                    timeout: timeout_ms.map(|ms| Duration::from_millis(ms as u64)),
                    on_log: dimse_logger(on_log),
                    cancel: None,
                },
            )
            .map_err(to_py_err)
        })
    })
}

/// Same as `find_scu`, but returns immediately with a handle instead of blocking until the
/// C-FIND completes - lets a caller `abort()`/`release()` it early. See `FindScuHandle`.
#[pyfunction]
#[pyo3(signature = (destination, query, calling_ae_title=None, called_ae_title=None, max_pdu_length=None, timeout_ms=None, on_log=None))]
#[allow(clippy::too_many_arguments)]
fn start_find_scu(
    destination: String,
    query: HashMap<String, String>,
    calling_ae_title: Option<String>,
    called_ae_title: Option<String>,
    max_pdu_length: Option<u32>,
    timeout_ms: Option<u32>,
    on_log: Option<Py<PyAny>>,
) -> FindScuHandle {
    let cancel = CancelSignal::new();
    let cancel_for_call = cancel.clone();
    let calling_ae_title = calling_ae_title.unwrap_or_else(|| "DCMNORM".to_owned());
    let max_pdu_length = max_pdu_length.unwrap_or(16384);
    let timeout = timeout_ms.map(|ms| Duration::from_millis(ms as u64));
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

#[pyclass(get_all)]
#[derive(Clone)]
struct MoveScuResult {
    status: u16,
    completed: u16,
    failed: u16,
    warning: u16,
    remaining: u16,
    cancelled: bool,
    /// `"release"`/`"abort"` when `cancelled` is true, indicating which teardown verb actually
    /// produced it - `None` for a real terminal C-MOVE-RSP.
    cancelled_via: Option<String>,
}

#[pyclass]
struct MoveScuHandle {
    cancel: Arc<CancelSignal>,
    state: Arc<ScuCallState<MoveScuResult>>,
}

#[pymethods]
impl MoveScuHandle {
    /// The eventual terminal `MoveScuResult`, without requesting cancellation - for a caller
    /// that just wants to wait for completion.
    fn result(&self, py: Python<'_>) -> PyResult<MoveScuResult> {
        wait_scu(py, &self.state, None)
    }

    /// Requests a graceful A-RELEASE and returns once the association has genuinely closed - real
    /// acknowledgement, not a fire-and-forget signal that might land seconds later (or never, if
    /// missed). Idempotent: a second call just observes the same outcome.
    fn release(&self, py: Python<'_>) -> PyResult<MoveScuResult> {
        wait_scu(py, &self.state, Some((&self.cancel, DcmCancelMode::Release)))
    }

    /// Requests an immediate A-ABORT (no wait on the peer at all) and returns once the
    /// association has genuinely closed. Idempotent.
    fn abort(&self, py: Python<'_>) -> PyResult<MoveScuResult> {
        wait_scu(py, &self.state, Some((&self.cancel, DcmCancelMode::Abort)))
    }
}

/// Performs a C-MOVE (Study Root Query/Retrieve), asking `destination` ("host:port") to push
/// `study_instance_uid` to `move_destination_ae` (an AE title `destination` already knows how to
/// reach, not a socket address). Returns a handle immediately - the retrieve itself runs on its
/// own background thread - rather than blocking until it reaches a terminal status; call
/// `.result()` to wait for that terminal status and sub-operation counts (regardless of
/// success/warning/failure), or `.release()`/`.abort()` to close it early once some other signal
/// (e.g. the study already being confirmed fully received via a separate channel) makes further
/// waiting pointless. See `MoveScuHandle`.
///
/// `on_log`, if given, is called (synchronously) with a debug line for each notable DIMSE event -
/// association open/close, and each C-MOVE-RQ/RSP (including every pending response, so a slow
/// multi-instance move is visible sub-operation by sub-operation).
#[pyfunction]
#[pyo3(signature = (
    destination, move_destination_ae, study_instance_uid,
    calling_ae_title=None, called_ae_title=None, max_pdu_length=None, timeout_ms=None,
    watch_path=None, stale_data_timeout_ms=None, on_log=None,
))]
#[allow(clippy::too_many_arguments)]
fn move_scu(
    destination: String,
    move_destination_ae: String,
    study_instance_uid: String,
    calling_ae_title: Option<String>,
    called_ae_title: Option<String>,
    max_pdu_length: Option<u32>,
    timeout_ms: Option<u32>,
    watch_path: Option<String>,
    stale_data_timeout_ms: Option<u32>,
    on_log: Option<Py<PyAny>>,
) -> MoveScuHandle {
    let cancel = CancelSignal::new();
    let cancel_for_call = cancel.clone();
    let calling_ae_title = calling_ae_title.unwrap_or_else(|| "DCMNORM".to_owned());
    let max_pdu_length = max_pdu_length.unwrap_or(16384);
    let timeout = timeout_ms.map(|ms| Duration::from_millis(ms as u64));
    let stale_data_path = watch_path.map(PathBuf::from);
    let stale_data_timeout = stale_data_timeout_ms.map(|ms| Duration::from_millis(ms as u64));
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
            status: result.status,
            completed: result.completed,
            failed: result.failed,
            warning: result.warning,
            remaining: result.remaining,
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
// Rendering: render_frame / render_movie
// ---------------------------------------------------------------------------------------------

fn unique_temp_path(extension: &str) -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);

    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let sequence = COUNTER.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!("dcmnorm-python-{}-{nanos}-{sequence}.{extension}", std::process::id()))
}

#[pyclass(get_all)]
#[derive(Clone)]
struct OverlaySummary {
    index: u32,
    group: u32,
    rows: u32,
    columns: u32,
    overlay_type: Option<String>,
    label: Option<String>,
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

#[pyclass(get_all)]
struct RenderedFrame {
    mime_type: String,
    width: u32,
    height: u32,
    data: Py<PyBytes>,
    /// All overlay planes present on the source instance, regardless of whether one was rendered
    /// into `data`.
    overlays: Vec<OverlaySummary>,
    /// Which overlay (by `OverlaySummary.index`) was actually composited into `data`, if any.
    selected_overlay_index: Option<u32>,
}

/// Renders a single frame of a DICOM file to JPEG or PNG. Mirrors `dcmnorm --output-width ...
/// --output-height ... --window-center ... --window-width ... --render-frame ... file.dcm
/// out.jpg`. If the instance has one or more DICOM overlay planes (group `60xx`), the first
/// available overlay composites onto the image by default; `overlay_index` selects a different
/// one (0-based, by `OverlaySummary.index`, matching the CLI's `--overlay-index`),
/// `show_overlays=False` disables overlay rendering, and `overlay_color` (`"R,G,B"` or
/// `"#RRGGBB"`, default green) sets the fill color. `overlays` in the result always lists every
/// overlay present on the instance (even when none was rendered); `selected_overlay_index` says
/// which one (if any) is actually in `data`.
#[pyfunction]
#[pyo3(signature = (
    file_path, format=None, output_width=None, output_height=None, window_center=None, window_width=None,
    frame_index=None, jpeg_quality=None, show_overlays=None, overlay_index=None, overlay_color=None,
))]
#[allow(clippy::too_many_arguments)]
fn render_frame(
    py: Python<'_>,
    file_path: String,
    format: Option<String>,
    output_width: Option<u32>,
    output_height: Option<u32>,
    window_center: Option<f64>,
    window_width: Option<f64>,
    frame_index: Option<u32>,
    jpeg_quality: Option<u32>,
    show_overlays: Option<bool>,
    overlay_index: Option<u32>,
    overlay_color: Option<String>,
) -> PyResult<RenderedFrame> {
    let (render_format, mime_type) = parse_render_output_format(format.as_deref())?;
    let (show_overlays, overlay_index_usize, overlay_color) =
        overlay_pipeline_fields(show_overlays, overlay_index, overlay_color.as_deref())?;

    let rendered = py.allow_threads(|| {
        guarded(|| {
            let object = read_dicom_file(&PathBuf::from(&file_path)).map_err(to_py_err)?;
            let pipeline_options = DcmRenderPipelineOptions {
                frame_index: frame_index.unwrap_or(0) as usize,
                window_center,
                window_width,
                output_width,
                output_height,
                jpeg_quality: jpeg_quality.unwrap_or(90) as u8,
                show_overlays,
                overlay_index: overlay_index_usize,
                overlay_color,
                ..Default::default()
            };
            dcm_render_dicom_frame(&object, render_format, &pipeline_options).map_err(to_py_err)
        })
    })?;

    Ok(RenderedFrame {
        mime_type: mime_type.to_owned(),
        width: rendered.width as u32,
        height: rendered.height as u32,
        data: PyBytes::new(py, &rendered.bytes).unbind(),
        overlays: rendered.overlays.into_iter().map(OverlaySummary::from).collect(),
        selected_overlay_index: rendered.selected_overlay_index.map(|value| value as u32),
    })
}

#[pyclass(get_all)]
struct RenderedMovie {
    mime_type: String,
    data: Py<PyBytes>,
}

/// Renders every frame of a multi-frame DICOM file to an MP4 (via a piped `ffmpeg` subprocess -
/// requires `ffmpeg` on `PATH`). Mirrors `dcmnorm --render-fps ... --output-width ... file.dcm
/// out.mp4`. Does not currently support the overlay options `render_frame` does.
#[pyfunction]
#[pyo3(signature = (file_path, output_width=None, output_height=None, window_center=None, window_width=None, fps=None))]
fn render_movie(
    py: Python<'_>,
    file_path: String,
    output_width: Option<u32>,
    output_height: Option<u32>,
    window_center: Option<f64>,
    window_width: Option<f64>,
    fps: Option<f64>,
) -> PyResult<RenderedMovie> {
    let bytes = py.allow_threads(|| {
        guarded(|| {
            let object = read_dicom_file(&PathBuf::from(&file_path)).map_err(to_py_err)?;
            let pipeline_options =
                DcmRenderPipelineOptions { window_center, window_width, output_width, output_height, ..Default::default() };
            let fps = fps.unwrap_or(24.0);

            // write_dicom_video needs a real (seekable) output file for ffmpeg's muxer - render
            // to a private temp file, then read it back, same trade-off the Node bindings and
            // the CLI itself make.
            let temp_path = unique_temp_path("mp4");
            let result = dcm_write_dicom_video(&object, &temp_path, &pipeline_options, fps).map_err(to_py_err);
            let bytes = result.and_then(|_| std::fs::read(&temp_path).map_err(to_py_err));
            let _ = std::fs::remove_file(&temp_path);
            bytes
        })
    })?;

    Ok(RenderedMovie { mime_type: "video/mp4".to_owned(), data: PyBytes::new(py, &bytes).unbind() })
}

// ---------------------------------------------------------------------------------------------
// MPR (Multiplanar Reformation): build_volume / DicomVolumeHandle.reformat
// ---------------------------------------------------------------------------------------------
//
// build_volume is the expensive step (reads + decodes every slice in a series), so it's exposed
// as its own call returning an opaque, reusable `DicomVolumeHandle` - the caller builds a volume
// once per series and keeps the handle resident (e.g. its own volume cache) so every subsequent
// `reformat()` call for a rotate/scroll/window-level change is cheap. `DicomVolumeHandle` wraps
// an `Arc<DcmVolume>`, which is read-only after `build_volume` returns, so concurrent
// `reformat()` calls need no locking.

/// Builds a 3D volume from a parallel stack of DICOM slice files (e.g. every image instance in
/// one CT/MR/PT series) sharing consistent `ImageOrientationPatient`. Slices are spatially
/// re-sorted internally by `ImagePositionPatient`, regardless of `file_paths`' own order. Returns
/// a `DicomVolumeHandle` for repeated `reformat()` calls. Raises (rather than silently
/// mis-rendering) for fewer than 2 files, mismatched Rows/Columns, or a
/// non-parallel/gantry-tilt-inconsistent stack.
#[pyfunction]
fn build_volume(py: Python<'_>, file_paths: Vec<String>) -> PyResult<DicomVolumeHandle> {
    let paths: Vec<PathBuf> = file_paths.into_iter().map(PathBuf::from).collect();
    py.allow_threads(|| guarded(|| dcm_build_volume(&paths).map_err(to_py_err)))
        .map(|volume| DicomVolumeHandle { volume: Arc::new(volume) })
}

#[pyclass]
struct DicomVolumeHandle {
    volume: Arc<DcmVolume>,
}

#[pymethods]
impl DicomVolumeHandle {
    /// Rows in the source slices (image height).
    #[getter]
    fn rows(&self) -> u32 {
        self.volume.rows
    }
    /// Columns in the source slices (image width).
    #[getter]
    fn cols(&self) -> u32 {
        self.volume.cols
    }
    /// Number of slices in the built volume.
    #[getter]
    fn num_slices(&self) -> u32 {
        self.volume.num_slices
    }

    /// The volume's own acquisition-native orientation, for seeding an "axial" reformat -
    /// `[row_dir(3), col_dir(3)]`.
    #[getter]
    fn native_basis(&self) -> Vec<f64> {
        let mut basis = Vec::with_capacity(6);
        basis.extend_from_slice(&self.volume.row_vector);
        basis.extend_from_slice(&self.volume.col_vector);
        basis
    }

    /// The volume's own physical center, in patient/LPS mm - a reasonable default reformat
    /// origin.
    #[getter]
    fn center(&self) -> Vec<f64> {
        self.volume.center().to_vec()
    }

    /// The volume's own smallest voxel dimension, in mm - a reasonable default output spacing.
    #[getter]
    fn min_spacing_mm(&self) -> f64 {
        self.volume.min_spacing_mm()
    }

    /// Resamples one plane through this volume and encodes it exactly like a normal 2D render
    /// (same `RenderedFrame` shape `render_frame` returns), so callers can reuse their existing
    /// image-display code path unchanged. `interpolation` defaults to `"trilinear"`; use
    /// `"nearest"` (faster) for a live-drag preview frame. `slab_thickness_mm` (default 0, an
    /// infinitely-thin plane) turns on a thick-slab reformat centered on `origin`, combined per
    /// `slab_projection` (default `"mip"`).
    #[pyo3(signature = (
        origin, row_dir, col_dir, output_width, output_height, spacing_mm,
        window_center=None, window_width=None, format=None, jpeg_quality=None,
        interpolation=None, slab_thickness_mm=None, slab_projection=None,
    ))]
    #[allow(clippy::too_many_arguments)]
    fn reformat(
        &self,
        py: Python<'_>,
        origin: Vec<f64>,
        row_dir: Vec<f64>,
        col_dir: Vec<f64>,
        output_width: u32,
        output_height: u32,
        spacing_mm: f64,
        window_center: Option<f64>,
        window_width: Option<f64>,
        format: Option<String>,
        jpeg_quality: Option<u32>,
        interpolation: Option<String>,
        slab_thickness_mm: Option<f64>,
        slab_projection: Option<String>,
    ) -> PyResult<RenderedFrame> {
        let (render_format, mime_type) = parse_render_output_format(format.as_deref())?;
        let params = DcmPlaneParams {
            origin: parse_vec3(origin, "origin")?,
            row_dir: parse_vec3(row_dir, "row_dir")?,
            col_dir: parse_vec3(col_dir, "col_dir")?,
            output_width,
            output_height,
            spacing_mm,
            window_center,
            window_width,
            interpolation: parse_interpolation(interpolation.as_deref())?,
            slab_thickness_mm: slab_thickness_mm.unwrap_or(0.0),
            slab_projection: parse_slab_projection(slab_projection.as_deref())?,
        };
        let jpeg_quality = jpeg_quality.unwrap_or(90) as u8;
        let volume = self.volume.clone();

        let rendered = py.allow_threads(|| {
            guarded(|| dcm_reformat_plane(&volume, &params, render_format, jpeg_quality).map_err(to_py_err))
        })?;

        Ok(RenderedFrame {
            mime_type: mime_type.to_owned(),
            width: u32::from(rendered.width),
            height: u32::from(rendered.height),
            data: PyBytes::new(py, &rendered.bytes).unbind(),
            overlays: Vec::new(),
            selected_overlay_index: None,
        })
    }

    /// Packs this volume's own NATIVE voxel lattice - not a resampled oblique plane, see
    /// `export_frame_texture`'s own doc - as a lossless GPU-upload-ready texture payload (16-bit
    /// samples, row-major, optionally gzip-compressed). Unlike `reformat()`, this is a one-shot
    /// call per volume, not one per interaction: ship the returned `data` once and do all further
    /// rotate/scroll/window-level manipulation client-side in a WebGL2 shader.
    #[pyo3(signature = (target_max_dim=None, compression=None, window_center=None, window_width=None))]
    fn export_texture(
        &self,
        py: Python<'_>,
        target_max_dim: Option<u32>,
        compression: Option<String>,
        window_center: Option<f64>,
        window_width: Option<f64>,
    ) -> PyResult<TextureExportResult> {
        let compression = parse_texture_compression(compression.as_deref())?;
        let default_window = match (window_center, window_width) {
            (Some(center), Some(width)) => Some((center, width)),
            _ => None,
        };
        let volume = self.volume.clone();

        let packed = py.allow_threads(|| {
            guarded(|| dcm_pack_volume_texture(&volume, target_max_dim, default_window, compression).map_err(to_py_err))
        })?;

        Ok(texture_export_result(py, &packed.meta, packed.payload))
    }
}

// ---------------------------------------------------------------------------------------------
// Texture export: DicomVolumeHandle.export_texture / export_frame_texture / export_frame_stack_texture
// ---------------------------------------------------------------------------------------------
//
// See dcmnorm::dicom_io::texture_export's own module doc for the format contract (metadata + raw
// row-major int16/uint16 payload, optionally gzip-compressed).

#[pyclass(get_all)]
struct TextureExportResult {
    /// 'volume', 'image2d', or 'framestack'.
    content_kind: String,
    /// 'int16' or 'uint16'.
    sample_format: String,
    /// 'none' or 'gzip' - matches how `data` below is actually encoded.
    compression: String,
    /// `False` means `data` is a bounded-error quantization (see `rescale_slope`/
    /// `rescale_intercept`), not an exact round-trip of the source samples.
    lossless: bool,
    width: u32,
    height: u32,
    /// Always 1 for `content_kind: 'image2d'`.
    depth: u32,
    /// `texel * rescale_slope + rescale_intercept` recovers the physical value (e.g. HU).
    rescale_slope: f64,
    rescale_intercept: f64,
    row_spacing_mm: f64,
    col_spacing_mm: f64,
    /// `0` for `content_kind: 'image2d'`.
    slice_spacing_mm: f64,
    /// `[x, y, z]`, LPS mm, center of voxel (0,0,0).
    origin: Vec<f64>,
    row_dir: Vec<f64>,
    col_dir: Vec<f64>,
    normal_dir: Vec<f64>,
    default_window_center: Option<f64>,
    default_window_width: Option<f64>,
    /// Whether the client shader should display `1.0 - windowed_intensity` rather than
    /// `windowed_intensity` - the invert decision `dcmnorm::dicom_io::render::resolve_grayscale_invert`
    /// makes (PresentationLUTShape overriding PhotometricInterpretation's MONOCHROME1/2-derived
    /// default). Always `False` for `content_kind: 'volume'` - see `TextureMeta::invert`'s own doc.
    invert: bool,
    native_width: u32,
    native_height: u32,
    native_depth: u32,
    downsampled: bool,
    /// Uncompressed byte length of `data`'s content.
    payload_bytes_raw: u64,
    /// `len(data)` - included on the result too (not just derivable from `data` Python-side) so
    /// a caller can log/cap transfer size without touching the buffer itself.
    payload_bytes_stored: u64,
    data: Py<PyBytes>,
}

fn texture_export_result(py: Python<'_>, meta: &DcmTextureMeta, payload: Vec<u8>) -> TextureExportResult {
    let (content_kind, sample_format, compression) = (
        match meta.content_kind {
            ::dcmnorm::dicom_io::ContentKind::Volume => "volume",
            ::dcmnorm::dicom_io::ContentKind::Image2D => "image2d",
            ::dcmnorm::dicom_io::ContentKind::FrameStack => "framestack",
        },
        match meta.sample_format {
            ::dcmnorm::dicom_io::SampleFormat::Int16 => "int16",
            ::dcmnorm::dicom_io::SampleFormat::Uint16 => "uint16",
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
        native_width: meta.native_dims.0,
        native_height: meta.native_dims.1,
        native_depth: meta.native_dims.2,
        downsampled: meta.downsampled,
        payload_bytes_raw: meta.payload_bytes_raw,
        payload_bytes_stored: meta.payload_bytes_stored,
        data: PyBytes::new(py, &payload).unbind(),
    }
}

/// Packs a single frame's raw (unwindowed) physical values as a depth-1 "1-slice volume" texture
/// - lets a large diagnostic 2D image (e.g. DX/CR/mammography) reuse the exact same client GPU
/// texture/shader pipeline as an MPR volume, instead of the lossy zoomed-JPEG path `render_frame`
/// produces. Mirrors `dcmnorm --render-frame ... --output-type texture file.dcm out.gputex`.
/// `frame_index` defaults to 0.
#[pyfunction]
#[pyo3(signature = (file_path, frame_index=None, target_max_dim=None, compression=None, window_center=None, window_width=None))]
fn export_frame_texture(
    py: Python<'_>,
    file_path: String,
    frame_index: Option<u32>,
    target_max_dim: Option<u32>,
    compression: Option<String>,
    window_center: Option<f64>,
    window_width: Option<f64>,
) -> PyResult<TextureExportResult> {
    let compression = parse_texture_compression(compression.as_deref())?;
    let default_window = match (window_center, window_width) {
        (Some(center), Some(width)) => Some((center, width)),
        _ => None,
    };

    let packed = py.allow_threads(|| {
        guarded(|| {
            let object = read_dicom_file(&PathBuf::from(&file_path)).map_err(to_py_err)?;
            dcm_pack_dicom_frame_texture(&object, frame_index.unwrap_or(0) as usize, target_max_dim, default_window, compression)
                .map_err(to_py_err)
        })
    })?;

    Ok(texture_export_result(py, &packed.meta, packed.payload))
}

/// One source file (and, for a cine/multiframe instance, the specific frames within it) to pack
/// into a frame-stack texture - see `export_frame_stack_texture`.
#[pyclass(get_all)]
#[derive(Clone)]
struct FrameStackSource {
    file_path: String,
    frame_indices: Option<Vec<u32>>,
}

#[pymethods]
impl FrameStackSource {
    /// `frame_indices` defaults to `[0]` if not given.
    #[new]
    #[pyo3(signature = (file_path, frame_indices=None))]
    fn new(file_path: String, frame_indices: Option<Vec<u32>>) -> Self {
        FrameStackSource { file_path, frame_indices }
    }
}

/// Packs several independent frames (from one or more files) as one texture-array upload - see
/// `dcmnorm::dicom_io::pack_frame_stack_texture`'s own doc for the full contract (no resampling,
/// no cross-layer interpolation, no physical geometry). Mirrors `export_frame_texture`'s
/// options/result shape, generalized to many sources. One entry per source FILE, not per frame -
/// a cine/multiframe instance uses ONE source with several `frame_indices` (its file is parsed
/// once, however many of its frames end up in the stack), while a multi-image series uses one
/// source per instance file (`frame_indices` defaulting to `[0]`). The result's layer order is
/// the flattened source order followed by each source's own `frame_indices` order - callers must
/// supply sources in the exact order the client's own frame/instance index expects.
#[pyfunction]
#[pyo3(signature = (sources, compression=None, window_center=None, window_width=None))]
fn export_frame_stack_texture(
    py: Python<'_>,
    sources: Vec<FrameStackSource>,
    compression: Option<String>,
    window_center: Option<f64>,
    window_width: Option<f64>,
) -> PyResult<TextureExportResult> {
    let compression = parse_texture_compression(compression.as_deref())?;
    let default_window = match (window_center, window_width) {
        (Some(center), Some(width)) => Some((center, width)),
        _ => None,
    };

    let packed = py.allow_threads(|| {
        guarded(|| {
            // Read each distinct source file exactly once, regardless of how many frame indices
            // it contributes - avoids re-parsing the same multi-MB file per frame for a cine loop.
            let objects = sources
                .iter()
                .map(|source| read_dicom_file(PathBuf::from(&source.file_path)).map_err(to_py_err))
                .collect::<PyResult<Vec<_>>>()?;

            let mut frame_refs: Vec<(&dcmnorm_object::DefaultDicomObject, usize)> = Vec::new();
            for (source, object) in sources.iter().zip(objects.iter()) {
                let indices = source.frame_indices.clone().unwrap_or_else(|| vec![0]);
                for index in indices {
                    frame_refs.push((object, index as usize));
                }
            }

            dcm_pack_dicom_frame_stack_texture(&frame_refs, default_window, compression).map_err(to_py_err)
        })
    })?;

    Ok(texture_export_result(py, &packed.meta, packed.payload))
}

// ---------------------------------------------------------------------------------------------
// DICOM SCP: start_dicom_server
// ---------------------------------------------------------------------------------------------
//
// C-FIND/C-MOVE/"association complete" business logic is delegated to Python via plain callables,
// invoked synchronously (GIL acquired via `Python::with_gil`) from each connection's own OS
// thread (spawned inside dcmnorm's start_scp) - dcmnorm's `ScpHandlers` trait itself is
// synchronous, so unlike the Node bindings (whose JS callbacks are inherently async and need a
// ThreadsafeFunction/Promise/oneshot-channel bridge), a Python callable's return value is used
// directly, no bridging required.

struct PyScpHandlers {
    on_find: Py<PyAny>,
    on_move: Py<PyAny>,
    on_association_complete: Py<PyAny>,
}

impl DcmScpHandlers for PyScpHandlers {
    fn on_find(&self, filter: &HashMap<String, String>) -> std::result::Result<Vec<HashMap<String, String>>, String> {
        Python::with_gil(|py| {
            let result = self.on_find.call1(py, (filter.clone(),)).map_err(|error| error.to_string())?;
            result.extract::<Vec<HashMap<String, String>>>(py).map_err(|error| error.to_string())
        })
    }

    fn on_move(&self, study_instance_uid: &str, move_destination_ae: &str) -> std::result::Result<bool, String> {
        Python::with_gil(|py| {
            let result = self
                .on_move
                .call1(py, (study_instance_uid, move_destination_ae))
                .map_err(|error| error.to_string())?;
            result.extract::<bool>(py).map_err(|error| error.to_string())
        })
    }

    fn on_association_complete(&self, stored_instances_by_study: &HashMap<String, Vec<String>>) {
        Python::with_gil(|py| {
            let _ = self.on_association_complete.call1(py, (stored_instances_by_study.clone(),));
        });
    }
}

#[pyclass]
struct DicomServerHandle {
    inner: Mutex<Option<DicomScp>>,
    local_port: u16,
}

#[pymethods]
impl DicomServerHandle {
    #[getter]
    fn local_port(&self) -> u32 {
        self.local_port as u32
    }

    /// Stops accepting new associations and waits for the listener to shut down. Associations
    /// already in progress finish independently (or hit their own idle timeout) - this does not
    /// interrupt them. Safe to call more than once.
    fn close(&self, py: Python<'_>) {
        let scp = self.inner.lock().unwrap().take();
        if let Some(scp) = scp {
            py.allow_threads(|| scp.stop());
        }
    }
}

/// Starts a DICOM SCP (association acceptor) on `port` (0 picks an ephemeral port - see the
/// returned handle's `local_port`), answering C-ECHO, C-STORE, C-FIND, and C-MOVE. C-ECHO and
/// C-STORE are handled entirely natively (C-STORE writes each received instance under
/// `cache_path` as `S_<StudyInstanceUID>/<Modality>_<SOPInstanceUID>.dcm`); C-FIND, C-MOVE, and
/// "association complete" (fired once per association that stored at least one instance) are
/// delegated to the given Python callables:
///
/// - `on_find(filter: dict) -> list[dict]` - `filter` has whichever of StudyInstanceUID/
///   StudyDate/PatientName/PatientID keys the request's Identifier actually had values for. Must
///   return a list of matched-study dicts (whichever of StudyInstanceUID/AccessionNumber/
///   PatientID/PatientName/PatientBirthDate/StudyDate/StudyTime/StudyDescription/
///   ModalitiesInStudy/NumberOfStudyRelatedSeries/NumberOfStudyRelatedInstances each has).
/// - `on_move(study_instance_uid: str, move_destination_ae: str) -> bool` - whether the move was
///   successfully queued.
/// - `on_association_complete(stored_instances_by_study: dict[str, list[str]])` - maps each
///   StudyInstanceUID to the on-disk paths of every instance C-STORE'd for it during that
///   association. Return value is ignored.
///
/// `on_log`, if given, is called (from whichever connection thread is handling the association at
/// the time) with a debug-detail line per association accept/negotiation, each request/response,
/// and release/abort - the SCP-side counterpart of `on_log` on `echo_scu`/`store_scu`/
/// `find_scu`/`move_scu`.
///
/// Accepts every proposed presentation context regardless of abstract syntax - this is a
/// permissive "accept anything a real sender proposes" SCP, not a curated allow-list.
#[pyfunction]
#[pyo3(signature = (
    port, cache_path, ae_title, on_find, on_move, on_association_complete,
    max_pdu_length=None, idle_timeout_ms=None, on_log=None,
))]
#[allow(clippy::too_many_arguments)]
fn start_dicom_server(
    py: Python<'_>,
    port: u16,
    cache_path: String,
    ae_title: String,
    on_find: Py<PyAny>,
    on_move: Py<PyAny>,
    on_association_complete: Py<PyAny>,
    max_pdu_length: Option<u32>,
    idle_timeout_ms: Option<u32>,
    on_log: Option<Py<PyAny>>,
) -> PyResult<DicomServerHandle> {
    let handlers: Arc<dyn DcmScpHandlers> = Arc::new(PyScpHandlers { on_find, on_move, on_association_complete });
    let options = DcmScpOptions {
        ae_title,
        // 256 KiB, matching ScpOptions::default() - see that struct's doc comment for why this
        // ceiling has to be generous rather than per-requestor.
        max_pdu_length: max_pdu_length.unwrap_or(262_144),
        idle_timeout: Duration::from_millis(u64::from(idle_timeout_ms.unwrap_or(300_000))),
        on_log: dimse_logger_arc(on_log),
    };

    let scp = py
        .allow_threads(|| dcm_start_scp(port, PathBuf::from(cache_path), handlers, options))
        .map_err(to_py_err)?;
    let local_port = scp.local_port();

    Ok(DicomServerHandle { inner: Mutex::new(Some(scp)), local_port })
}

#[pymodule]
fn dcmnorm(module: &Bound<'_, PyModule>) -> PyResult<()> {
    module.add("DcmnormError", module.py().get_type::<DcmnormError>())?;

    module.add_function(wrap_pyfunction!(read_tags, module)?)?;
    module.add_function(wrap_pyfunction!(read_json, module)?)?;
    module.add_function(wrap_pyfunction!(write_json, module)?)?;
    module.add_function(wrap_pyfunction!(edit_tags, module)?)?;
    module.add_function(wrap_pyfunction!(transcode, module)?)?;
    module.add_function(wrap_pyfunction!(check_dicom, module)?)?;

    module.add_function(wrap_pyfunction!(echo_scu, module)?)?;
    module.add_function(wrap_pyfunction!(start_echo_scu, module)?)?;
    module.add_function(wrap_pyfunction!(store_scu, module)?)?;
    module.add_function(wrap_pyfunction!(start_store_scu, module)?)?;
    module.add_function(wrap_pyfunction!(find_scu, module)?)?;
    module.add_function(wrap_pyfunction!(start_find_scu, module)?)?;
    module.add_function(wrap_pyfunction!(move_scu, module)?)?;
    module.add_class::<EchoScuHandle>()?;
    module.add_class::<StoreScuHandle>()?;
    module.add_class::<StoreScuResult>()?;
    module.add_class::<FindScuHandle>()?;
    module.add_class::<MoveScuHandle>()?;
    module.add_class::<MoveScuResult>()?;

    module.add_function(wrap_pyfunction!(render_frame, module)?)?;
    module.add_function(wrap_pyfunction!(render_movie, module)?)?;
    module.add_class::<RenderedFrame>()?;
    module.add_class::<RenderedMovie>()?;
    module.add_class::<OverlaySummary>()?;

    module.add_function(wrap_pyfunction!(build_volume, module)?)?;
    module.add_class::<DicomVolumeHandle>()?;

    module.add_function(wrap_pyfunction!(export_frame_texture, module)?)?;
    module.add_function(wrap_pyfunction!(export_frame_stack_texture, module)?)?;
    module.add_class::<FrameStackSource>()?;
    module.add_class::<TextureExportResult>()?;

    module.add_function(wrap_pyfunction!(start_dicom_server, module)?)?;
    module.add_class::<DicomServerHandle>()?;

    Ok(())
}
