//! napi side of the typed dataset API (#6). These `dataset*` functions are the internals of
//! `dcmnorm.js` (the package's entry point), which wraps them as `read`, `toDicomJson`, `write`,
//! `renderPixels` and `transcodeBytes` with `DicomDataset` objects and typed errors. Everything
//! DICOM is in `dcmnorm::dicom_io::dataset_api`.
//!
//! Each runs on the libuv thread pool (an `AsyncTask`) and resolves to a `NativeResult`: the
//! payload, or `error` with the core's error kind and context, which the JS layer turns into a
//! `DcmnormError` subclass (a rejected napi error can carry only a code and a message). A data
//! set crosses as one `Buffer` in the core's binary form (`encode_document`), decoded in JS.

use std::path::PathBuf;

type StdResult<T, E> = std::result::Result<T, E>;

use napi::bindgen_prelude::*;
use napi_derive::napi;

use dcmnorm::dicom_io::{
    catch_api_panic, decode_document, dictionary_vr, document_to_dicom_json, encode_document,
    keyword_for_tag, list_transfer_syntax_support, read_dataset_document, render_source_frame,
    source_to_dicom_json, tag_for_keyword, transcode_source, transcode_source_to_path,
    write_dicom_bytes, write_document, write_document_to_path, ApiError, BulkDataUriBuilder,
    BulkDataUriRequest, BulkSource, DatasetReadOptions, DicomJsonBulkDataMode,
    DicomJsonExportOptions, DicomSource, RenderPipelineOptions, WriteDocumentOptions,
};

#[napi(object)]
pub struct NativeError {
    pub kind: String,
    pub message: String,
    pub source_transfer_syntax_uid: Option<String>,
    pub target_transfer_syntax_uid: Option<String>,
    pub frame_index: Option<i64>,
}

#[napi(object)]
pub struct NativeOverlay {
    pub index: u32,
    pub group: u32,
    pub rows: u32,
    pub columns: u32,
    pub overlay_type: Option<String>,
    pub label: Option<String>,
}

#[napi(object)]
pub struct NativeRendered {
    pub width: u32,
    pub height: u32,
    pub channels: u32,
    pub pixels: Buffer,
    pub overlays: Vec<NativeOverlay>,
    pub selected_overlay_index: Option<u32>,
}

/// What a `dataset*` call resolves to: `error`, or one of the payloads.
#[napi(object)]
pub struct NativeResult {
    pub error: Option<NativeError>,
    pub data: Option<Buffer>,
    pub text: Option<String>,
    pub rendered: Option<NativeRendered>,
}

fn native_error(error: ApiError) -> NativeError {
    NativeError {
        kind: error.kind.name().to_owned(),
        message: error.message,
        source_transfer_syntax_uid: error.source_transfer_syntax_uid,
        target_transfer_syntax_uid: error.target_transfer_syntax_uid,
        frame_index: error.frame_index.map(|index| index as i64),
    }
}

pub enum Payload {
    None,
    Data(Vec<u8>),
    Text(String),
    Rendered(dcmnorm::dicom_io::RenderFrameOutput),
}

/// A source as the JS layer passes it: a path or the object's bytes (a stream is collected
/// into a Buffer in JS first).
struct SourceArg {
    path: Option<PathBuf>,
    bytes: Option<Vec<u8>>,
}

impl SourceArg {
    fn new(path: Option<String>, bytes: Option<Buffer>) -> Self {
        Self {
            path: path.map(PathBuf::from),
            bytes: bytes.map(|buffer| buffer.to_vec()),
        }
    }

    fn source(&self) -> StdResult<DicomSource<'_>, ApiError> {
        match (&self.path, &self.bytes) {
            (Some(path), _) => Ok(DicomSource::Path(path)),
            (None, Some(bytes)) => Ok(DicomSource::Bytes(bytes)),
            (None, None) => Err(ApiError::invalid_argument("no DICOM source given")),
        }
    }

    fn bulk_source(&self) -> BulkSource<'_> {
        match (&self.path, &self.bytes) {
            (Some(path), _) => BulkSource::Path(path),
            (None, Some(bytes)) => BulkSource::Bytes(bytes),
            (None, None) => BulkSource::None,
        }
    }
}

type Job = Box<dyn FnOnce() -> StdResult<Payload, ApiError> + Send>;

pub struct DatasetTask {
    job: Option<Job>,
}

impl Task for DatasetTask {
    type Output = StdResult<Payload, ApiError>;
    type JsValue = NativeResult;

    fn compute(&mut self) -> Result<Self::Output> {
        let job = self.job.take().expect("a task runs once");
        Ok(catch_api_panic(job))
    }

    fn resolve(&mut self, _env: Env, output: Self::Output) -> Result<Self::JsValue> {
        let mut result = NativeResult {
            error: None,
            data: None,
            text: None,
            rendered: None,
        };
        match output {
            Err(error) => result.error = Some(native_error(error)),
            Ok(Payload::None) => {}
            Ok(Payload::Data(data)) => result.data = Some(data.into()),
            Ok(Payload::Text(text)) => result.text = Some(text),
            Ok(Payload::Rendered(rendered)) => {
                result.rendered = Some(NativeRendered {
                    width: u32::from(rendered.width),
                    height: u32::from(rendered.height),
                    channels: u32::from(rendered.samples_per_pixel),
                    overlays: rendered
                        .overlays
                        .iter()
                        .map(|overlay| NativeOverlay {
                            index: overlay.index as u32,
                            group: u32::from(overlay.group),
                            rows: u32::from(overlay.rows),
                            columns: u32::from(overlay.columns),
                            overlay_type: overlay.overlay_type.clone(),
                            label: overlay.label.clone(),
                        })
                        .collect(),
                    selected_overlay_index: rendered
                        .selected_overlay_index
                        .map(|index| index as u32),
                    pixels: rendered.bytes.into(),
                })
            }
        }
        Ok(result)
    }
}

fn task(
    job: impl FnOnce() -> StdResult<Payload, ApiError> + Send + 'static,
) -> AsyncTask<DatasetTask> {
    AsyncTask::new(DatasetTask {
        job: Some(Box::new(job)),
    })
}

/// Internal (see `dcmnorm.js` `read`): the data set of a path or bytes, in the binary form.
#[napi]
pub fn dataset_read(
    path: Option<String>,
    bytes: Option<Buffer>,
    until_pixel_data: bool,
    load_bulk_data: bool,
) -> AsyncTask<DatasetTask> {
    let source = SourceArg::new(path, bytes);
    task(move || {
        let options = DatasetReadOptions {
            until_pixel_data,
            load_bulk_data,
        };
        Ok(Payload::Data(encode_document(&read_dataset_document(
            source.source()?,
            options,
        )?)))
    })
}

/// Options of `toDicomJson`, as `dcmnorm.js` passes them.
#[napi(object)]
pub struct NativeJsonOptions {
    pub include_tags: Option<Vec<u32>>,
    pub exclude_tags: Option<Vec<u32>>,
    pub bulk_data: Option<String>,
    pub bulk_data_uri_base: Option<String>,
    pub include_file_meta: Option<bool>,
    pub keyword_and_vm: Option<bool>,
    /// Write each BulkDataURI as the placeholder `dcmnorm-bulk:<TAG>:<VR>:<offset>:<length>`,
    /// for the JS layer to replace with its `bulkDataUriBuilder`'s result (a JS callback can't
    /// run on the thread pool).
    pub placeholder_uris: Option<bool>,
}

struct JsonArgs {
    include: Vec<u32>,
    exclude: Vec<u32>,
    bulk_data_mode: DicomJsonBulkDataMode,
    base: Option<String>,
    include_file_meta: bool,
    keyword_and_vm: bool,
    placeholders: bool,
}

impl JsonArgs {
    fn new(options: NativeJsonOptions) -> Result<Self> {
        let bulk_data_mode = match options.bulk_data.as_deref() {
            None | Some("uri") => DicomJsonBulkDataMode::Uri,
            Some("inline") => DicomJsonBulkDataMode::InlineBinary,
            Some(other) => {
                return Err(Error::from_reason(format!(
                    "invalid bulkData '{other}'; expected 'uri' or 'inline'"
                )))
            }
        };
        Ok(Self {
            include: options.include_tags.unwrap_or_default(),
            exclude: options.exclude_tags.unwrap_or_default(),
            bulk_data_mode,
            base: options.bulk_data_uri_base,
            include_file_meta: options.include_file_meta.unwrap_or(false),
            keyword_and_vm: options.keyword_and_vm.unwrap_or(false),
            placeholders: options.placeholder_uris.unwrap_or(false),
        })
    }

    fn run(
        &self,
        write: impl FnOnce(&DicomJsonExportOptions<'_>) -> StdResult<String, ApiError>,
    ) -> StdResult<String, ApiError> {
        let placeholder = |request: &BulkDataUriRequest| {
            let tag = (u32::from(request.tag.group()) << 16) | u32::from(request.tag.element());
            let length = request.length.map(|length| length as i64).unwrap_or(-1);
            format!(
                "dcmnorm-bulk:{tag:08X}:{}:{}:{length}",
                request.vr.to_string(),
                request.offset
            )
        };
        write(&DicomJsonExportOptions {
            include_tags: &self.include,
            exclude_tags: &self.exclude,
            bulk_data_mode: self.bulk_data_mode,
            bulk_data_uri_base: self.base.as_deref(),
            bulk_data_uri_builder: self
                .placeholders
                .then_some(BulkDataUriBuilder(&placeholder)),
            include_file_meta: self.include_file_meta,
            keyword_and_vm: self.keyword_and_vm,
        })
    }
}

/// Internal (see `dcmnorm.js` `toDicomJson`): DICOM JSON of a source.
#[napi]
pub fn dataset_source_to_json(
    path: Option<String>,
    bytes: Option<Buffer>,
    options: NativeJsonOptions,
) -> Result<AsyncTask<DatasetTask>> {
    let source = SourceArg::new(path, bytes);
    let args = JsonArgs::new(options)?;
    Ok(task(move || {
        Ok(Payload::Text(args.run(|options| {
            source_to_dicom_json(source.source()?, options)
        })?))
    }))
}

/// Internal (see `dcmnorm.js` `toDicomJson`): DICOM JSON of an encoded data set, bulk data from
/// its source path/bytes in inline mode.
#[napi]
pub fn dataset_document_to_json(
    document: Buffer,
    source_path: Option<String>,
    source_bytes: Option<Buffer>,
    options: NativeJsonOptions,
) -> Result<AsyncTask<DatasetTask>> {
    let document = document.to_vec();
    let source = SourceArg::new(source_path, source_bytes);
    let args = JsonArgs::new(options)?;
    Ok(task(move || {
        let document = decode_document(&document)?;
        Ok(Payload::Text(args.run(|options| {
            document_to_dicom_json(&document, &source.bulk_source(), options)
        })?))
    }))
}

/// Internal (see `dcmnorm.js` `write`): write an encoded data set to `outputPath`, or (no path)
/// resolve to the file's bytes.
#[napi]
pub fn dataset_write(
    document: Buffer,
    source_path: Option<String>,
    source_bytes: Option<Buffer>,
    output_path: Option<String>,
    transfer_syntax_uid: Option<String>,
) -> AsyncTask<DatasetTask> {
    let document = document.to_vec();
    let source = SourceArg::new(source_path, source_bytes);
    task(move || {
        let document = decode_document(&document)?;
        let options = WriteDocumentOptions {
            transfer_syntax_uid: transfer_syntax_uid.as_deref(),
        };
        match output_path {
            Some(path) => {
                write_document_to_path(
                    &document,
                    &source.bulk_source(),
                    &options,
                    std::path::Path::new(&path),
                )?;
                Ok(Payload::None)
            }
            None => {
                let mut out = Vec::new();
                write_document(&document, &source.bulk_source(), &options, &mut out)?;
                Ok(Payload::Data(out))
            }
        }
    })
}

/// Internal (see `dcmnorm.js` `transcode`): transcode a source to `outputPath`, or resolve to
/// the bytes.
#[napi]
pub fn dataset_transcode(
    path: Option<String>,
    bytes: Option<Buffer>,
    output_path: Option<String>,
    transfer_syntax_uid: String,
) -> AsyncTask<DatasetTask> {
    let source = SourceArg::new(path, bytes);
    task(move || match output_path {
        Some(path) => {
            transcode_source_to_path(source.source()?, &PathBuf::from(path), &transfer_syntax_uid)?;
            Ok(Payload::None)
        }
        None => {
            let mut object = transcode_source(source.source()?, &transfer_syntax_uid)?;
            let encode = |error| ApiError::from(error).with_target_ts(Some(&transfer_syntax_uid));
            Ok(Payload::Data(
                write_dicom_bytes(&mut object).map_err(encode)?,
            ))
        }
    })
}

#[napi(object)]
pub struct NativeRenderParams {
    pub window_center: Option<f64>,
    pub window_width: Option<f64>,
    pub voi_lut: Option<bool>,
    pub modality_lut: Option<bool>,
    pub icc_profile: Option<bool>,
    pub output_width: Option<u32>,
    pub output_height: Option<u32>,
    pub show_overlays: Option<bool>,
    pub overlay_index: Option<u32>,
    pub overlay_color: Option<String>,
}

/// Internal (see `dcmnorm.js` `renderPixels`): one frame's 8-bit display pixels.
#[napi]
pub fn dataset_render_pixels(
    path: Option<String>,
    bytes: Option<Buffer>,
    frame_index: u32,
    params: NativeRenderParams,
) -> Result<AsyncTask<DatasetTask>> {
    let overlay_color = match params.overlay_color.as_deref() {
        Some(color) => crate::parse_render_color(color)?,
        None => [0, 255, 0],
    };
    let pipeline = RenderPipelineOptions {
        frame_index: frame_index as usize,
        window_center: params.window_center,
        window_width: params.window_width,
        apply_voi_lut: params.voi_lut.unwrap_or(true),
        apply_modality_lut: params.modality_lut.unwrap_or(true),
        apply_icc_profile: params.icc_profile.unwrap_or(true),
        output_width: params.output_width,
        output_height: params.output_height,
        show_overlays: params.show_overlays.unwrap_or(true),
        overlay_index: params.overlay_index.map(|index| index as usize),
        overlay_color,
        ..Default::default()
    };
    let source = SourceArg::new(path, bytes);
    Ok(task(move || {
        Ok(Payload::Rendered(render_source_frame(
            source.source()?,
            &pipeline,
        )?))
    }))
}

#[napi(object)]
pub struct TransferSyntaxSupportInfo {
    pub uid: String,
    pub name: String,
    pub encapsulated: bool,
    pub can_read_dataset: bool,
    pub can_write_dataset: bool,
    pub can_decode: bool,
    pub can_encode: bool,
    pub can_transcode_to: bool,
}

/// What this build can read, write, decode and encode, per transfer syntax UID.
#[napi]
pub fn transfer_syntax_support() -> Vec<TransferSyntaxSupportInfo> {
    list_transfer_syntax_support()
        .into_iter()
        .map(|ts| TransferSyntaxSupportInfo {
            can_transcode_to: ts.can_transcode_to(),
            uid: ts.uid,
            name: ts.name,
            encapsulated: ts.encapsulated_pixel_data,
            can_read_dataset: ts.can_read_dataset,
            can_write_dataset: ts.can_write_dataset,
            can_decode: ts.can_decode_pixel_data,
            can_encode: ts.can_encode_pixel_data,
        })
        .collect()
}

/// The standard keyword of a tag (`0x0020000D` -> `"StudyInstanceUID"`), or null.
#[napi]
pub fn keyword_of(tag: u32) -> Option<String> {
    keyword_for_tag(tag).map(str::to_owned)
}

/// The dictionary VR of a tag, or null.
#[napi]
pub fn vr_of(tag: u32) -> Option<String> {
    dictionary_vr(tag).map(|vr| vr.to_string().to_owned())
}

/// The tag of a standard keyword or tag expression, or null.
#[napi(js_name = "tagForKeyword")]
pub fn tag_of_keyword(keyword: String) -> Option<u32> {
    tag_for_keyword(&keyword)
}
