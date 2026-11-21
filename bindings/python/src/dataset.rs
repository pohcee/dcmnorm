//! PyO3 side of the typed dataset API (#6): the `_dataset_*` internals that `dataset.py`
//! (embedded below and run when `dcmnorm` is imported) wraps as `read`, `to_dicom_json`, `write`,
//! `render_pixels` and `transcode_to`, plus `transfer_syntax_support`, `keyword_of`, `vr_of`,
//! `tag_for_keyword` and the typed `DcmnormError` subclasses. Everything DICOM is in
//! `dcmnorm::dicom_io::dataset_api`; a data set crosses as `bytes` in the core's binary form,
//! decoded in Python.
//!
//! A source is a path (`str` / `os.PathLike`), `bytes` / `bytearray` / `memoryview`, or a binary
//! file-like object with `read(n)`. Paths and bytes are processed with the GIL released; a
//! file-like object is read (and a `bulk_data_uri_builder` called) with it held.

use std::cell::RefCell;
use std::io::Read;
use std::path::PathBuf;

use pyo3::create_exception;
use pyo3::exceptions::PyTypeError;
use pyo3::prelude::*;
use pyo3::pybacked::PyBackedBytes;
use pyo3::types::{PyBytes, PyDict, PyList};

use ::dcmnorm::dicom_io::{
    catch_api_panic, decode_document, dictionary_vr, document_to_dicom_json, encode_document,
    keyword_for_tag, list_transfer_syntax_support, read_dataset_document, render_source_frame,
    source_to_dicom_json, tag_for_keyword, transcode_source, transcode_source_to_path,
    write_dicom_bytes, write_document, write_document_to_path, ApiError, ApiErrorKind,
    BulkDataUriBuilder, BulkDataUriRequest, BulkSource, DatasetReadOptions, DicomJsonBulkDataMode,
    DicomJsonExportOptions, DicomSource, RenderFrameOutput, RenderPipelineOptions,
    WriteDocumentOptions,
};

use crate::DcmnormError;

create_exception!(
    dcmnorm,
    InvalidDicomError,
    DcmnormError,
    "Not DICOM, or malformed DICOM."
);
create_exception!(
    dcmnorm,
    UnsupportedTransferSyntaxError,
    DcmnormError,
    "A transfer syntax this build can't read, decode or encode, or an unknown UID."
);
create_exception!(
    dcmnorm,
    DecodeError,
    DcmnormError,
    "Pixel data could not be decoded."
);
create_exception!(
    dcmnorm,
    EncodeError,
    DcmnormError,
    "Pixel data could not be encoded, or the output not written."
);
create_exception!(
    dcmnorm,
    RenderError,
    DcmnormError,
    "A frame could not be rendered."
);
create_exception!(
    dcmnorm,
    DimseError,
    DcmnormError,
    "A DIMSE association or operation failed."
);

/// The `DcmnormError` subclass for an error kind, with `kind`, `source_transfer_syntax_uid`,
/// `target_transfer_syntax_uid` and `frame_index` set on the instance.
fn api_err(py: Python<'_>, error: ApiError) -> PyErr {
    let err = match error.kind {
        ApiErrorKind::InvalidDicom => InvalidDicomError::new_err(error.message.clone()),
        ApiErrorKind::UnsupportedTransferSyntax => {
            UnsupportedTransferSyntaxError::new_err(error.message.clone())
        }
        ApiErrorKind::Decode => DecodeError::new_err(error.message.clone()),
        ApiErrorKind::Encode => EncodeError::new_err(error.message.clone()),
        ApiErrorKind::Render => RenderError::new_err(error.message.clone()),
        ApiErrorKind::Dimse => DimseError::new_err(error.message.clone()),
        _ => DcmnormError::new_err(error.message.clone()),
    };
    let value = err.value(py);
    let _ = value.setattr("kind", error.kind.name());
    let _ = value.setattr(
        "source_transfer_syntax_uid",
        error.source_transfer_syntax_uid,
    );
    let _ = value.setattr(
        "target_transfer_syntax_uid",
        error.target_transfer_syntax_uid,
    );
    let _ = value.setattr("frame_index", error.frame_index);
    err
}

// -------------------------------------------------------------------------------------------
// Sources
// -------------------------------------------------------------------------------------------

/// A binary file-like object as a `Read` (`read(n)`, called with the GIL held).
struct PyReader<'py> {
    file: Bound<'py, PyAny>,
}

impl Read for PyReader<'_> {
    fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
        let wanted = out.len().min(64 * 1024);
        let chunk = self.file.call_method1("read", (wanted,)).map_err(|error| {
            std::io::Error::new(std::io::ErrorKind::Other, format!("read() failed: {error}"))
        })?;
        if chunk.is_none() {
            return Ok(0);
        }
        let bytes: PyBackedBytes = chunk.extract().map_err(|_| {
            std::io::Error::new(
                std::io::ErrorKind::Other,
                "read() must return bytes (open the file in binary mode)",
            )
        })?;
        let n = bytes.len().min(out.len());
        out[..n].copy_from_slice(&bytes[..n]);
        Ok(n)
    }
}

enum Source<'py> {
    Path(PathBuf),
    Bytes(PyBackedBytes),
    File(Bound<'py, PyAny>),
}

fn source_from<'py>(value: &Bound<'py, PyAny>) -> PyResult<Source<'py>> {
    if let Ok(bytes) = value.extract::<PyBackedBytes>() {
        return Ok(Source::Bytes(bytes));
    }
    if let Ok(view) = value.downcast::<pyo3::types::PyMemoryView>() {
        let bytes: PyBackedBytes = view.call_method0("tobytes")?.extract()?;
        return Ok(Source::Bytes(bytes));
    }
    if value.hasattr("read")? {
        return Ok(Source::File(value.clone()));
    }
    if let Ok(path) = value.extract::<PathBuf>() {
        return Ok(Source::Path(path));
    }
    Err(PyTypeError::new_err(
        "a DICOM source is a path, bytes-like, or a binary file object",
    ))
}

/// Run `job` on the source: with the GIL released for a path or bytes, held for a file object.
fn with_source<T: Send>(
    py: Python<'_>,
    source: Source<'_>,
    job: impl for<'s> FnOnce(DicomSource<'s>) -> Result<T, ApiError> + Send,
) -> PyResult<T> {
    let result = match source {
        Source::Path(path) => {
            py.allow_threads(|| catch_api_panic(|| job(DicomSource::Path(&path))))
        }
        Source::Bytes(bytes) => {
            py.allow_threads(|| catch_api_panic(|| job(DicomSource::Bytes(&bytes))))
        }
        Source::File(file) => {
            let mut reader = PyReader { file };
            catch_api_panic(|| job(DicomSource::Reader(&mut reader)))
        }
    };
    result.map_err(|error| api_err(py, error))
}

fn bulk_source<'a>(path: &'a Option<PathBuf>, bytes: &'a Option<PyBackedBytes>) -> BulkSource<'a> {
    match (path, bytes) {
        (Some(path), _) => BulkSource::Path(path),
        (None, Some(bytes)) => BulkSource::Bytes(bytes),
        (None, None) => BulkSource::None,
    }
}

// -------------------------------------------------------------------------------------------
// Functions
// -------------------------------------------------------------------------------------------

/// Internal (see `dcmnorm.read`): the data set in the binary form.
#[pyfunction]
#[pyo3(name = "_dataset_read")]
fn dataset_read<'py>(
    py: Python<'py>,
    source: &Bound<'py, PyAny>,
    until_pixel_data: bool,
    load_bulk_data: bool,
) -> PyResult<Bound<'py, PyBytes>> {
    let options = DatasetReadOptions {
        until_pixel_data,
        load_bulk_data,
    };
    let encoded = with_source(py, source_from(source)?, |source| {
        Ok(encode_document(&read_dataset_document(source, options)?))
    })?;
    Ok(PyBytes::new(py, &encoded))
}

struct JsonArgs {
    include: Vec<u32>,
    exclude: Vec<u32>,
    bulk_data_mode: DicomJsonBulkDataMode,
    base: Option<String>,
    include_file_meta: bool,
    keyword_and_vm: bool,
}

fn json_args(options: &Bound<'_, PyDict>) -> PyResult<JsonArgs> {
    let get = |key: &str| {
        options
            .get_item(key)
            .ok()
            .flatten()
            .filter(|value| !value.is_none())
    };
    let bulk_data_mode = match get("bulk_data")
        .map(|v| v.extract::<String>())
        .transpose()?
        .as_deref()
    {
        None | Some("uri") => DicomJsonBulkDataMode::Uri,
        Some("inline") => DicomJsonBulkDataMode::InlineBinary,
        Some(other) => {
            return Err(DcmnormError::new_err(format!(
                "invalid bulk_data '{other}'; expected 'uri' or 'inline'"
            )))
        }
    };
    Ok(JsonArgs {
        include: get("include_tags")
            .map(|v| v.extract())
            .transpose()?
            .unwrap_or_default(),
        exclude: get("exclude_tags")
            .map(|v| v.extract())
            .transpose()?
            .unwrap_or_default(),
        bulk_data_mode,
        base: get("bulk_data_uri_base").map(|v| v.extract()).transpose()?,
        include_file_meta: get("include_file_meta")
            .map(|v| v.is_truthy())
            .transpose()?
            .unwrap_or(false),
        keyword_and_vm: get("keyword_and_vm")
            .map(|v| v.is_truthy())
            .transpose()?
            .unwrap_or(false),
    })
}

/// Calls a Python `bulk_data_uri_builder(ref)` with the GIL held; the first failure is kept and
/// raised after the JSON is built.
struct PyUriBuilder<'py> {
    callable: Bound<'py, PyAny>,
    bulk_data_ref: Bound<'py, PyAny>,
    failure: RefCell<Option<PyErr>>,
}

impl PyUriBuilder<'_> {
    fn build(&self, request: &BulkDataUriRequest) -> String {
        if self.failure.borrow().is_some() {
            return String::new();
        }
        let tag = (u32::from(request.tag.group()) << 16) | u32::from(request.tag.element());
        let length = request.length.map(|length| length as i64).unwrap_or(-1);
        let result = self
            .bulk_data_ref
            .call1((tag, request.vr.to_string(), request.offset, length))
            .and_then(|reference| self.callable.call1((reference,)))
            .and_then(|uri| uri.extract::<String>());
        match result {
            Ok(uri) => uri,
            Err(error) => {
                *self.failure.borrow_mut() = Some(error);
                String::new()
            }
        }
    }
}

fn run_json(
    py: Python<'_>,
    args: &JsonArgs,
    builder: Option<&PyUriBuilder<'_>>,
    write: impl FnOnce(&DicomJsonExportOptions<'_>) -> Result<String, ApiError>,
) -> PyResult<String> {
    let build =
        |request: &BulkDataUriRequest| builder.map(|b| b.build(request)).unwrap_or_default();
    let options = DicomJsonExportOptions {
        include_tags: &args.include,
        exclude_tags: &args.exclude,
        bulk_data_mode: args.bulk_data_mode,
        bulk_data_uri_base: args.base.as_deref(),
        bulk_data_uri_builder: builder.map(|_| BulkDataUriBuilder(&build)),
        include_file_meta: args.include_file_meta,
        keyword_and_vm: args.keyword_and_vm,
    };
    let json = catch_api_panic(|| write(&options)).map_err(|error| api_err(py, error))?;
    if let Some(error) = builder.and_then(|b| b.failure.borrow_mut().take()) {
        return Err(error);
    }
    Ok(json)
}

fn uri_builder<'py>(
    py: Python<'py>,
    callable: Option<Bound<'py, PyAny>>,
    bulk_data_ref: Bound<'py, PyAny>,
) -> Option<PyUriBuilder<'py>> {
    let _ = py;
    callable
        .filter(|c| !c.is_none())
        .map(|callable| PyUriBuilder {
            callable,
            bulk_data_ref,
            failure: RefCell::new(None),
        })
}

/// Internal (see `dcmnorm.to_dicom_json`): DICOM JSON of a source.
#[pyfunction]
#[pyo3(name = "_dataset_to_json", signature = (source, options, builder, bulk_data_ref))]
fn dataset_to_json<'py>(
    py: Python<'py>,
    source: &Bound<'py, PyAny>,
    options: &Bound<'py, PyDict>,
    builder: Option<Bound<'py, PyAny>>,
    bulk_data_ref: Bound<'py, PyAny>,
) -> PyResult<String> {
    let args = json_args(options)?;
    let builder = uri_builder(py, builder, bulk_data_ref);
    let source = source_from(source)?;
    if builder.is_none() {
        // no Python callback: the GIL can be released (except for a file object)
        let (include, exclude, base) = (
            args.include.clone(),
            args.exclude.clone(),
            args.base.clone(),
        );
        let (mode, meta, keyword) = (
            args.bulk_data_mode,
            args.include_file_meta,
            args.keyword_and_vm,
        );
        return with_source(py, source, move |source| {
            source_to_dicom_json(
                source,
                &DicomJsonExportOptions {
                    include_tags: &include,
                    exclude_tags: &exclude,
                    bulk_data_mode: mode,
                    bulk_data_uri_base: base.as_deref(),
                    bulk_data_uri_builder: None,
                    include_file_meta: meta,
                    keyword_and_vm: keyword,
                },
            )
        });
    }
    match source {
        Source::Path(path) => run_json(py, &args, builder.as_ref(), |o| {
            source_to_dicom_json(DicomSource::Path(&path), o)
        }),
        Source::Bytes(bytes) => run_json(py, &args, builder.as_ref(), |o| {
            source_to_dicom_json(DicomSource::Bytes(&bytes), o)
        }),
        Source::File(file) => {
            let mut reader = PyReader { file };
            run_json(py, &args, builder.as_ref(), |o| {
                source_to_dicom_json(DicomSource::Reader(&mut reader), o)
            })
        }
    }
}

/// Internal (see `dcmnorm.to_dicom_json`): DICOM JSON of an encoded data set.
#[pyfunction]
#[pyo3(name = "_dataset_document_to_json", signature = (document, source_path, source_bytes, options, builder, bulk_data_ref))]
fn dataset_document_to_json<'py>(
    py: Python<'py>,
    document: PyBackedBytes,
    source_path: Option<PathBuf>,
    source_bytes: Option<PyBackedBytes>,
    options: &Bound<'py, PyDict>,
    builder: Option<Bound<'py, PyAny>>,
    bulk_data_ref: Bound<'py, PyAny>,
) -> PyResult<String> {
    let args = json_args(options)?;
    let builder = uri_builder(py, builder, bulk_data_ref);
    let document = decode_document(&document).map_err(|error| api_err(py, error))?;
    let bulk = bulk_source(&source_path, &source_bytes);
    run_json(py, &args, builder.as_ref(), |o| {
        document_to_dicom_json(&document, &bulk, o)
    })
}

/// Internal (see `dcmnorm.write`): write an encoded data set to `output_path`, or return the bytes.
#[pyfunction]
#[pyo3(name = "_dataset_write", signature = (document, source_path, source_bytes, output_path, transfer_syntax_uid))]
fn dataset_write<'py>(
    py: Python<'py>,
    document: PyBackedBytes,
    source_path: Option<PathBuf>,
    source_bytes: Option<PyBackedBytes>,
    output_path: Option<PathBuf>,
    transfer_syntax_uid: Option<String>,
) -> PyResult<Option<Bound<'py, PyBytes>>> {
    let result = py.allow_threads(|| {
        catch_api_panic(|| {
            let document = decode_document(&document)?;
            let bulk = bulk_source(&source_path, &source_bytes);
            let options = WriteDocumentOptions {
                transfer_syntax_uid: transfer_syntax_uid.as_deref(),
            };
            match &output_path {
                Some(path) => {
                    write_document_to_path(&document, &bulk, &options, path).map(|_| None)
                }
                None => {
                    let mut out = Vec::new();
                    write_document(&document, &bulk, &options, &mut out)?;
                    Ok(Some(out))
                }
            }
        })
    });
    match result {
        Ok(bytes) => Ok(bytes.map(|bytes| PyBytes::new(py, &bytes))),
        Err(error) => Err(api_err(py, error)),
    }
}

/// Internal (see `dcmnorm.transcode_to`).
#[pyfunction]
#[pyo3(name = "_dataset_transcode", signature = (source, output_path, transfer_syntax_uid))]
fn dataset_transcode<'py>(
    py: Python<'py>,
    source: &Bound<'py, PyAny>,
    output_path: Option<PathBuf>,
    transfer_syntax_uid: String,
) -> PyResult<Option<Bound<'py, PyBytes>>> {
    let bytes = with_source(py, source_from(source)?, |source| match &output_path {
        Some(path) => {
            transcode_source_to_path(source, path, &transfer_syntax_uid)?;
            Ok(None)
        }
        None => {
            let mut object = transcode_source(source, &transfer_syntax_uid)?;
            let encode = |error| ApiError::from(error).with_target_ts(Some(&transfer_syntax_uid));
            write_dicom_bytes(&mut object).map(Some).map_err(encode)
        }
    })?;
    Ok(bytes.map(|bytes| PyBytes::new(py, &bytes)))
}

/// Internal (see `dcmnorm.render_pixels`): `(width, height, channels, pixels, overlays,
/// selected_overlay_index)`.
#[pyfunction]
#[pyo3(name = "_dataset_render", signature = (source, frame_index, params))]
fn dataset_render<'py>(
    py: Python<'py>,
    source: &Bound<'py, PyAny>,
    frame_index: usize,
    params: &Bound<'py, PyDict>,
) -> PyResult<Bound<'py, PyAny>> {
    let get = |key: &str| {
        params
            .get_item(key)
            .ok()
            .flatten()
            .filter(|value| !value.is_none())
    };
    let flag =
        |key: &str| -> PyResult<Option<bool>> { get(key).map(|v| v.is_truthy()).transpose() };
    let overlay_color = match get("overlay_color")
        .map(|v| v.extract::<String>())
        .transpose()?
    {
        Some(color) => crate::parse_render_color(&color)?,
        None => [0, 255, 0],
    };
    let pipeline = RenderPipelineOptions {
        frame_index,
        window_center: get("window_center").map(|v| v.extract()).transpose()?,
        window_width: get("window_width").map(|v| v.extract()).transpose()?,
        apply_voi_lut: flag("voi_lut")?.unwrap_or(true),
        apply_modality_lut: flag("modality_lut")?.unwrap_or(true),
        apply_icc_profile: flag("icc_profile")?.unwrap_or(true),
        output_width: get("output_width").map(|v| v.extract()).transpose()?,
        output_height: get("output_height").map(|v| v.extract()).transpose()?,
        show_overlays: flag("show_overlays")?.unwrap_or(true),
        overlay_index: get("overlay_index").map(|v| v.extract()).transpose()?,
        overlay_color,
        ..Default::default()
    };
    let rendered: RenderFrameOutput = with_source(py, source_from(source)?, |source| {
        render_source_frame(source, &pipeline)
    })?;
    let overlays = PyList::empty(py);
    for overlay in &rendered.overlays {
        let entry = PyDict::new(py);
        entry.set_item("index", overlay.index)?;
        entry.set_item("group", overlay.group)?;
        entry.set_item("rows", overlay.rows)?;
        entry.set_item("columns", overlay.columns)?;
        entry.set_item("overlay_type", overlay.overlay_type.clone())?;
        entry.set_item("label", overlay.label.clone())?;
        overlays.append(entry)?;
    }
    let tuple = (
        rendered.width,
        rendered.height,
        rendered.samples_per_pixel,
        PyBytes::new(py, &rendered.bytes),
        overlays,
        rendered.selected_overlay_index,
    );
    Ok(tuple.into_pyobject(py)?.into_any())
}

/// What this build can read, write, decode and encode, per transfer syntax UID: a list of dicts
/// (`uid`, `name`, `encapsulated`, `can_read_dataset`, `can_write_dataset`, `can_decode`,
/// `can_encode`, `can_transcode_to`).
#[pyfunction]
fn transfer_syntax_support<'py>(py: Python<'py>) -> PyResult<Bound<'py, PyList>> {
    let list = PyList::empty(py);
    for ts in list_transfer_syntax_support() {
        let entry = PyDict::new(py);
        entry.set_item("can_transcode_to", ts.can_transcode_to())?;
        entry.set_item("uid", ts.uid)?;
        entry.set_item("name", ts.name)?;
        entry.set_item("encapsulated", ts.encapsulated_pixel_data)?;
        entry.set_item("can_read_dataset", ts.can_read_dataset)?;
        entry.set_item("can_write_dataset", ts.can_write_dataset)?;
        entry.set_item("can_decode", ts.can_decode_pixel_data)?;
        entry.set_item("can_encode", ts.can_encode_pixel_data)?;
        list.append(entry)?;
    }
    Ok(list)
}

/// The standard keyword of a tag (`0x0020000D` -> `"StudyInstanceUID"`), or None.
#[pyfunction]
fn keyword_of(tag: u32) -> Option<&'static str> {
    keyword_for_tag(tag)
}

/// The dictionary VR of a tag, or None.
#[pyfunction]
fn vr_of(tag: u32) -> Option<&'static str> {
    dictionary_vr(tag).map(|vr| vr.to_string())
}

/// The tag of a standard keyword or tag expression, or None.
#[pyfunction]
#[pyo3(name = "tag_for_keyword")]
fn tag_of_keyword(keyword: &str) -> Option<u32> {
    tag_for_keyword(keyword)
}

const CONSTANTS_PY: &str = include_str!("dicom_constants.py");
const DATASET_PY: &str = include_str!("dataset.py");

/// Register the dataset API on the `dcmnorm` module: the native functions, the exception
/// classes, and the Python layer (generated constants, then `dataset.py`).
pub(crate) fn register(module: &Bound<'_, PyModule>) -> PyResult<()> {
    let py = module.py();
    module.add("InvalidDicomError", py.get_type::<InvalidDicomError>())?;
    module.add(
        "UnsupportedTransferSyntaxError",
        py.get_type::<UnsupportedTransferSyntaxError>(),
    )?;
    module.add("DecodeError", py.get_type::<DecodeError>())?;
    module.add("EncodeError", py.get_type::<EncodeError>())?;
    module.add("RenderError", py.get_type::<RenderError>())?;
    module.add("DimseError", py.get_type::<DimseError>())?;
    module.add_function(wrap_pyfunction!(dataset_read, module)?)?;
    module.add_function(wrap_pyfunction!(dataset_to_json, module)?)?;
    module.add_function(wrap_pyfunction!(dataset_document_to_json, module)?)?;
    module.add_function(wrap_pyfunction!(dataset_write, module)?)?;
    module.add_function(wrap_pyfunction!(dataset_transcode, module)?)?;
    module.add_function(wrap_pyfunction!(dataset_render, module)?)?;
    module.add_function(wrap_pyfunction!(transfer_syntax_support, module)?)?;
    module.add_function(wrap_pyfunction!(keyword_of, module)?)?;
    module.add_function(wrap_pyfunction!(vr_of, module)?)?;
    module.add_function(wrap_pyfunction!(tag_of_keyword, module)?)?;

    for (code, file, name) in [
        (
            CONSTANTS_PY,
            c"dcmnorm/dicom_constants.py",
            c"dcmnorm._dicom_constants",
        ),
        (DATASET_PY, c"dcmnorm/dataset.py", c"dcmnorm._dataset"),
    ] {
        let code = std::ffi::CString::new(code).expect("no NUL in the embedded Python");
        let layer = PyModule::from_code(py, &code, file, name)?;
        if let Ok(bind) = layer.getattr("_bind") {
            bind.call1((module,))?;
        }
        let exported: Vec<String> = layer.getattr("__all__")?.extract()?;
        for name in exported {
            module.add(name.as_str(), layer.getattr(name.as_str())?)?;
        }
    }
    Ok(())
}
