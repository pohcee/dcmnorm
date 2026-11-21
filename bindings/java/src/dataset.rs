//! JNI side of the typed dataset API (#6): `Dcmnorm.read` / `toDicomJson` / `write` /
//! `renderPixels` / `transcode` overloads / `transferSyntaxSupport`, and `ElementDictionary`.
//! Everything DICOM is in `dcmnorm::dicom_io::dataset_api`; this file only moves arguments and
//! results across JNI and raises the `DcmnormException` subclass an [`ApiError`]'s kind names.
//!
//! A data set crosses as one `byte[]` in `dcmnorm::dicom_io::encode_document`'s binary form,
//! which `com.pohcee.dcmnorm.internal.DatasetCodec` decodes into plain Java objects (and encodes
//! back for writing): one JNI call per read, no native handles.
//!
//! Sources are `(String path, byte[] bytes, InputStream stream)` with exactly one non-null. A
//! `byte[]` is read in place where the JVM allows (`GetByteArrayElements`, never copied back); an
//! `InputStream` is read through a 64 KiB Java buffer, front to back, from inside the native call
//! (`JavaVM::get_env` on the calling thread, as the DIMSE logger does).

use std::cell::RefCell;
use std::io::{Read, Write};
use std::path::PathBuf;

use jni::objects::{
    AutoElements, GlobalRef, JByteArray, JClass, JIntArray, JObject, JString, JThrowable, JValue,
    ReleaseMode,
};
use jni::sys::{jboolean, jbyteArray, jint, jlong, jobject, jstring};
use jni::{JNIEnv, JavaVM};
use serde::Deserialize;

use dcmnorm::dicom_io::{
    catch_api_panic, decode_document, dictionary_vr, document_to_dicom_json, encode_document,
    keyword_for_tag, list_transfer_syntax_support, read_dataset_document, render_source_frame,
    source_to_dicom_json, tag_for_keyword, transcode_source, transcode_source_to_path,
    write_dicom_bytes, write_document, write_document_to_path, ApiError, ApiErrorKind,
    BulkDataUriBuilder, BulkDataUriRequest, BulkSource, DatasetReadOptions, DicomJsonBulkDataMode,
    DicomJsonExportOptions, DicomSource, RenderPipelineOptions, WriteDocumentOptions,
};

const BASE_EXCEPTION: &str = "com/pohcee/dcmnorm/DcmnormException";
const STREAM_BUFFER: i32 = 64 * 1024;

// -------------------------------------------------------------------------------------------
// Errors
// -------------------------------------------------------------------------------------------

fn exception_class(kind: ApiErrorKind) -> &'static str {
    match kind {
        ApiErrorKind::InvalidDicom => "com/pohcee/dcmnorm/InvalidDicomException",
        ApiErrorKind::UnsupportedTransferSyntax => {
            "com/pohcee/dcmnorm/UnsupportedTransferSyntaxException"
        }
        ApiErrorKind::Decode => "com/pohcee/dcmnorm/DecodeException",
        ApiErrorKind::Encode => "com/pohcee/dcmnorm/EncodeException",
        ApiErrorKind::Render => "com/pohcee/dcmnorm/RenderException",
        ApiErrorKind::Dimse => "com/pohcee/dcmnorm/DimseException",
        ApiErrorKind::Io | ApiErrorKind::InvalidArgument | ApiErrorKind::Internal => BASE_EXCEPTION,
    }
}

fn optional_jstring<'local>(
    env: &mut JNIEnv<'local>,
    value: Option<&str>,
) -> jni::errors::Result<JObject<'local>> {
    match value {
        Some(text) => Ok(env.new_string(text)?.into()),
        None => Ok(JObject::null()),
    }
}

/// Raise the `DcmnormException` subclass for `error`'s kind, carrying its context, and return
/// `T`'s default (never observed: the exception is pending when the native method returns).
pub(crate) fn throw_api_error<T: Default>(env: &mut JNIEnv, error: &ApiError) -> T {
    let raised = (|| -> jni::errors::Result<()> {
        let class = env.find_class(exception_class(error.kind))?;
        let message: JObject = env.new_string(&error.message)?.into();
        let kind: JObject = env.new_string(error.kind.name())?.into();
        let source = optional_jstring(env, error.source_transfer_syntax_uid.as_deref())?;
        let target = optional_jstring(env, error.target_transfer_syntax_uid.as_deref())?;
        let frame = error
            .frame_index
            .and_then(|index| i32::try_from(index).ok())
            .unwrap_or(-1);
        let exception = env.new_object(
            class,
            "(Ljava/lang/String;Ljava/lang/String;Ljava/lang/String;Ljava/lang/String;I)V",
            &[
                JValue::Object(&message),
                JValue::Object(&kind),
                JValue::Object(&source),
                JValue::Object(&target),
                JValue::Int(frame),
            ],
        )?;
        env.throw(JThrowable::from(exception))
    })();
    if raised.is_err() {
        let _ = env.exception_clear();
        let _ = env.throw_new(BASE_EXCEPTION, &error.message);
    }
    T::default()
}

fn argument_error(message: impl Into<String>) -> ApiError {
    ApiError::invalid_argument(message)
}

/// The pending Java exception's `toString()`, cleared (for a callback that threw).
fn take_exception(env: &mut JNIEnv) -> String {
    let Ok(throwable) = env.exception_occurred() else {
        return "unknown Java exception".to_owned();
    };
    let _ = env.exception_clear();
    let text = env
        .call_method(&throwable, "toString", "()Ljava/lang/String;", &[])
        .and_then(|value| value.l())
        .ok()
        .and_then(|object| {
            env.get_string(&JString::from(object))
                .ok()
                .map(String::from)
        });
    let _ = env.exception_clear();
    text.unwrap_or_else(|| "Java exception".to_owned())
}

// -------------------------------------------------------------------------------------------
// Java streams as Rust Read/Write
// -------------------------------------------------------------------------------------------

/// A `java.io.InputStream` as a `Read`: `read(byte[], int, int)` into one reused Java buffer.
struct JavaInputStream {
    vm: JavaVM,
    stream: GlobalRef,
    buffer: GlobalRef,
}

impl JavaInputStream {
    fn new(env: &mut JNIEnv, stream: &JObject) -> Result<Self, ApiError> {
        let failed =
            |error: jni::errors::Error| ApiError::new(ApiErrorKind::Internal, error.to_string());
        let buffer = env.new_byte_array(STREAM_BUFFER).map_err(failed)?;
        Ok(Self {
            vm: env.get_java_vm().map_err(failed)?,
            stream: env.new_global_ref(stream).map_err(failed)?,
            buffer: env.new_global_ref(buffer).map_err(failed)?,
        })
    }
}

impl Read for JavaInputStream {
    fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
        if out.is_empty() {
            return Ok(0);
        }
        let io_error = |message: String| std::io::Error::new(std::io::ErrorKind::Other, message);
        let mut env = self
            .vm
            .get_env()
            .map_err(|error| io_error(error.to_string()))?;
        let wanted = out.len().min(STREAM_BUFFER as usize) as i32;
        let read = env.call_method(
            self.stream.as_obj(),
            "read",
            "([BII)I",
            &[
                JValue::Object(self.buffer.as_obj()),
                JValue::Int(0),
                JValue::Int(wanted),
            ],
        );
        let read = match read.and_then(|value| value.i()) {
            Ok(read) => read,
            Err(_) => {
                return Err(io_error(format!(
                    "InputStream.read failed: {}",
                    take_exception(&mut env)
                )))
            }
        };
        if read <= 0 {
            return Ok(0);
        }
        let read = (read as usize).min(out.len());
        // SAFETY: the global reference keeps the byte[] alive; a JByteArray made from its raw
        // handle doesn't own (or delete) the reference.
        let array = unsafe { JByteArray::from_raw(self.buffer.as_obj().as_raw()) };
        // SAFETY: u8 and i8 have the same layout.
        let target = unsafe { std::slice::from_raw_parts_mut(out.as_mut_ptr().cast::<i8>(), read) };
        env.get_byte_array_region(&array, 0, target)
            .map_err(|error| io_error(error.to_string()))?;
        Ok(read)
    }
}

/// A `java.io.OutputStream` as a `Write`, through one reused 64 KiB Java buffer.
struct JavaOutputStream {
    vm: JavaVM,
    stream: GlobalRef,
    buffer: GlobalRef,
}

impl JavaOutputStream {
    fn new(env: &mut JNIEnv, stream: &JObject) -> Result<Self, ApiError> {
        let failed =
            |error: jni::errors::Error| ApiError::new(ApiErrorKind::Internal, error.to_string());
        let buffer = env.new_byte_array(STREAM_BUFFER).map_err(failed)?;
        Ok(Self {
            vm: env.get_java_vm().map_err(failed)?,
            stream: env.new_global_ref(stream).map_err(failed)?,
            buffer: env.new_global_ref(buffer).map_err(failed)?,
        })
    }
}

impl Write for JavaOutputStream {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        if data.is_empty() {
            return Ok(0);
        }
        let io_error = |message: String| std::io::Error::new(std::io::ErrorKind::Other, message);
        let mut env = self
            .vm
            .get_env()
            .map_err(|error| io_error(error.to_string()))?;
        let chunk = &data[..data.len().min(STREAM_BUFFER as usize)];
        // SAFETY: see JavaInputStream::read.
        let array = unsafe { JByteArray::from_raw(self.buffer.as_obj().as_raw()) };
        let source =
            unsafe { std::slice::from_raw_parts(chunk.as_ptr().cast::<i8>(), chunk.len()) };
        env.set_byte_array_region(&array, 0, source)
            .map_err(|error| io_error(error.to_string()))?;
        let written = env.call_method(
            self.stream.as_obj(),
            "write",
            "([BII)V",
            &[
                JValue::Object(self.buffer.as_obj()),
                JValue::Int(0),
                JValue::Int(chunk.len() as i32),
            ],
        );
        if written.is_err() {
            return Err(io_error(format!(
                "OutputStream.write failed: {}",
                take_exception(&mut env)
            )));
        }
        Ok(chunk.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        let mut env = self
            .vm
            .get_env()
            .map_err(|error| std::io::Error::new(std::io::ErrorKind::Other, error.to_string()))?;
        if env
            .call_method(self.stream.as_obj(), "flush", "()V", &[])
            .is_err()
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Other,
                format!("OutputStream.flush failed: {}", take_exception(&mut env)),
            ));
        }
        Ok(())
    }
}

// -------------------------------------------------------------------------------------------
// Argument plumbing
// -------------------------------------------------------------------------------------------

fn get_optional_string(env: &mut JNIEnv, value: &JString) -> Result<Option<String>, ApiError> {
    if value.is_null() {
        return Ok(None);
    }
    env.get_string(value)
        .map(|text| Some(text.into()))
        .map_err(|error| argument_error(format!("invalid string argument: {error}")))
}

/// A `byte[]` argument's contents, borrowed in place where the JVM allows (pinned or a copy the
/// JVM makes; never copied back).
struct BorrowedBytes<'local, 'array> {
    elements: AutoElements<'local, 'local, 'array, i8>,
}

impl BorrowedBytes<'_, '_> {
    fn as_slice(&self) -> &[u8] {
        // SAFETY: u8 and i8 have the same layout; the elements live as long as `self`.
        unsafe {
            std::slice::from_raw_parts(self.elements.as_ptr().cast::<u8>(), self.elements.len())
        }
    }
}

fn borrow_bytes<'local, 'array>(
    env: &mut JNIEnv<'local>,
    array: &'array JByteArray<'local>,
) -> Result<Option<BorrowedBytes<'local, 'array>>, ApiError> {
    if array.is_null() {
        return Ok(None);
    }
    // SAFETY: the array is only read, and not by Java code, while the elements are held.
    let elements = unsafe { env.get_array_elements(array, ReleaseMode::NoCopyBack) }
        .map_err(|error| ApiError::new(ApiErrorKind::Internal, error.to_string()))?;
    Ok(Some(BorrowedBytes { elements }))
}

/// One of a source's three forms, ready to hand to the core as a [`DicomSource`].
enum SourceArg<'local, 'array> {
    Path(PathBuf),
    Bytes(BorrowedBytes<'local, 'array>),
    Stream(JavaInputStream),
}

impl SourceArg<'_, '_> {
    fn source(&mut self) -> DicomSource<'_> {
        match self {
            Self::Path(path) => DicomSource::Path(path),
            Self::Bytes(bytes) => DicomSource::Bytes(bytes.as_slice()),
            Self::Stream(stream) => DicomSource::Reader(stream),
        }
    }

    fn bulk_source(&self) -> BulkSource<'_> {
        match self {
            Self::Path(path) => BulkSource::Path(path),
            Self::Bytes(bytes) => BulkSource::Bytes(bytes.as_slice()),
            Self::Stream(_) => BulkSource::None,
        }
    }
}

fn source_arg<'local, 'array>(
    env: &mut JNIEnv<'local>,
    path: &JString<'local>,
    bytes: &'array JByteArray<'local>,
    stream: &JObject<'local>,
) -> Result<SourceArg<'local, 'array>, ApiError> {
    if let Some(path) = get_optional_string(env, path)? {
        return Ok(SourceArg::Path(PathBuf::from(path)));
    }
    if let Some(bytes) = borrow_bytes(env, bytes)? {
        return Ok(SourceArg::Bytes(bytes));
    }
    if !stream.is_null() {
        return Ok(SourceArg::Stream(JavaInputStream::new(env, stream)?));
    }
    Err(argument_error("no DICOM source given"))
}

/// The bulk data source for writing / JSON: a path or bytes, or neither.
fn bulk_arg<'local, 'array>(
    env: &mut JNIEnv<'local>,
    path: &JString<'local>,
    bytes: &'array JByteArray<'local>,
) -> Result<Option<SourceArg<'local, 'array>>, ApiError> {
    if let Some(path) = get_optional_string(env, path)? {
        return Ok(Some(SourceArg::Path(PathBuf::from(path))));
    }
    Ok(borrow_bytes(env, bytes)?.map(SourceArg::Bytes))
}

fn int_array(env: &mut JNIEnv, array: &JIntArray) -> Result<Vec<u32>, ApiError> {
    if array.is_null() {
        return Ok(Vec::new());
    }
    let length = env
        .get_array_length(array)
        .map_err(|error| argument_error(error.to_string()))?;
    let mut values = vec![0i32; length as usize];
    env.get_int_array_region(array, 0, &mut values)
        .map_err(|error| argument_error(error.to_string()))?;
    Ok(values.into_iter().map(|value| value as u32).collect())
}

fn byte_array_result(env: &mut JNIEnv, bytes: &[u8]) -> jbyteArray {
    match env.byte_array_from_slice(bytes) {
        Ok(array) => array.into_raw(),
        Err(error) => throw_api_error(
            env,
            &ApiError::new(ApiErrorKind::Internal, error.to_string()),
        ),
    }
}

fn string_result(env: &mut JNIEnv, text: &str) -> jstring {
    match env.new_string(text) {
        Ok(value) => value.into_raw(),
        Err(error) => throw_api_error(
            env,
            &ApiError::new(ApiErrorKind::Internal, error.to_string()),
        ),
    }
}

// -------------------------------------------------------------------------------------------
// Transfer syntaxes and transcoding
// -------------------------------------------------------------------------------------------

/// `Dcmnorm.transferSyntaxSupport()`: a JSON array of this build's transfer syntax support.
#[no_mangle]
pub extern "system" fn Java_com_pohcee_dcmnorm_Dcmnorm_nativeTransferSyntaxSupport<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
) -> jstring {
    let result = catch_api_panic(|| {
        let entries: Vec<serde_json::Value> = list_transfer_syntax_support()
            .into_iter()
            .map(|ts| {
                serde_json::json!({
                    "uid": ts.uid,
                    "name": ts.name,
                    "encapsulated": ts.encapsulated_pixel_data,
                    "canReadDataset": ts.can_read_dataset,
                    "canWriteDataset": ts.can_write_dataset,
                    "canDecode": ts.can_decode_pixel_data,
                    "canEncode": ts.can_encode_pixel_data,
                    "canTranscodeTo": ts.can_transcode_to(),
                })
            })
            .collect();
        Ok(serde_json::Value::Array(entries).to_string())
    });
    match result {
        Ok(json) => string_result(&mut env, &json),
        Err(error) => throw_api_error(&mut env, &error),
    }
}

/// `Dcmnorm.transcode(...)` overloads: source as path/bytes/stream, output to a path, an
/// `OutputStream`, or (both null) returned as a `byte[]`.
#[no_mangle]
pub extern "system" fn Java_com_pohcee_dcmnorm_Dcmnorm_nativeTranscodeSource<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    path: JString<'local>,
    bytes: JByteArray<'local>,
    stream: JObject<'local>,
    output_path: JString<'local>,
    output_stream: JObject<'local>,
    transfer_syntax_uid: JString<'local>,
) -> jbyteArray {
    let prepared = (|| {
        let source = source_arg(&mut env, &path, &bytes, &stream)?;
        let output_path = get_optional_string(&mut env, &output_path)?;
        let output = if output_stream.is_null() {
            None
        } else {
            Some(JavaOutputStream::new(&mut env, &output_stream)?)
        };
        let uid = get_optional_string(&mut env, &transfer_syntax_uid)?
            .ok_or_else(|| argument_error("transferSyntaxUid is required"))?;
        Ok::<_, ApiError>((source, output_path, output, uid))
    })();
    let (mut source, output_path, output, uid) = match prepared {
        Ok(prepared) => prepared,
        Err(error) => return throw_api_error(&mut env, &error),
    };
    let result = catch_api_panic(|| {
        if let Some(path) = output_path {
            transcode_source_to_path(source.source(), &PathBuf::from(path), &uid)?;
            return Ok(None);
        }
        let mut object = transcode_source(source.source(), &uid)?;
        let encode =
            |error: dcmnorm::dicom_io::WriteError| ApiError::from(error).with_target_ts(Some(&uid));
        if let Some(mut output) = output {
            object.write_all(&mut output).map_err(encode)?;
            output
                .flush()
                .map_err(|error| ApiError::io(error.to_string()))?;
            Ok(None)
        } else {
            Ok(Some(write_dicom_bytes(&mut object).map_err(encode)?))
        }
    });
    match result {
        Ok(Some(bytes)) => byte_array_result(&mut env, &bytes),
        Ok(None) => std::ptr::null_mut(),
        Err(error) => throw_api_error(&mut env, &error),
    }
}

// -------------------------------------------------------------------------------------------
// ElementDictionary
// -------------------------------------------------------------------------------------------

#[no_mangle]
pub extern "system" fn Java_com_pohcee_dcmnorm_ElementDictionary_nativeKeywordOf<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    tag: jint,
) -> jstring {
    match keyword_for_tag(tag as u32) {
        Some(keyword) => string_result(&mut env, keyword),
        None => std::ptr::null_mut(),
    }
}

#[no_mangle]
pub extern "system" fn Java_com_pohcee_dcmnorm_ElementDictionary_nativeVrOf<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    tag: jint,
) -> jstring {
    match dictionary_vr(tag as u32) {
        Some(vr) => string_result(&mut env, vr.to_string()),
        None => std::ptr::null_mut(),
    }
}

#[no_mangle]
pub extern "system" fn Java_com_pohcee_dcmnorm_ElementDictionary_nativeTagForKeyword<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    keyword: JString<'local>,
) -> jlong {
    match get_optional_string(&mut env, &keyword) {
        Ok(Some(keyword)) => tag_for_keyword(&keyword).map(i64::from).unwrap_or(-1),
        _ => -1,
    }
}

// -------------------------------------------------------------------------------------------
// Reading
// -------------------------------------------------------------------------------------------

/// `Dcmnorm.read(...)`: the data set in the binary form `DatasetCodec` decodes.
#[no_mangle]
pub extern "system" fn Java_com_pohcee_dcmnorm_Dcmnorm_nativeReadDataset<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    path: JString<'local>,
    bytes: JByteArray<'local>,
    stream: JObject<'local>,
    until_pixel_data: jboolean,
    load_bulk_data: jboolean,
) -> jbyteArray {
    let mut source = match source_arg(&mut env, &path, &bytes, &stream) {
        Ok(source) => source,
        Err(error) => return throw_api_error(&mut env, &error),
    };
    let options = DatasetReadOptions {
        until_pixel_data: until_pixel_data != 0,
        load_bulk_data: load_bulk_data != 0,
    };
    let result = catch_api_panic(|| {
        Ok(encode_document(&read_dataset_document(
            source.source(),
            options,
        )?))
    });
    match result {
        Ok(encoded) => byte_array_result(&mut env, &encoded),
        Err(error) => throw_api_error(&mut env, &error),
    }
}

// -------------------------------------------------------------------------------------------
// DICOM JSON
// -------------------------------------------------------------------------------------------

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct JsonOptionsIn {
    bulk_data: Option<String>,
    bulk_data_uri_base: Option<String>,
    include_file_meta: Option<bool>,
    keyword_and_vm: Option<bool>,
}

/// A `java.util.function.Function<BulkDataRef, String>` as the core's URI builder. A builder
/// that throws (or returns null) fails the whole call after the JSON is built.
struct JavaUriBuilder {
    vm: JavaVM,
    function: GlobalRef,
    failure: RefCell<Option<String>>,
}

impl JavaUriBuilder {
    fn build(&self, request: &BulkDataUriRequest) -> String {
        if self.failure.borrow().is_some() {
            return String::new();
        }
        let result = (|| -> Result<String, String> {
            let mut env = self.vm.get_env().map_err(|error| error.to_string())?;
            let built = (|| -> jni::errors::Result<Option<String>> {
                let class = env.find_class("com/pohcee/dcmnorm/BulkDataRef")?;
                let vr: JObject = env.new_string(request.vr.to_string())?.into();
                let tag = ((u32::from(request.tag.group()) << 16)
                    | u32::from(request.tag.element())) as i32;
                let length = request.length.map(|length| length as i64).unwrap_or(-1);
                let reference = env.new_object(
                    class,
                    "(ILjava/lang/String;JJ)V",
                    &[
                        JValue::Int(tag),
                        JValue::Object(&vr),
                        JValue::Long(request.offset as i64),
                        JValue::Long(length),
                    ],
                )?;
                let uri = env
                    .call_method(
                        self.function.as_obj(),
                        "apply",
                        "(Ljava/lang/Object;)Ljava/lang/Object;",
                        &[JValue::Object(&reference)],
                    )?
                    .l()?;
                let text = if uri.is_null() {
                    None
                } else {
                    let uri = JString::from(uri);
                    let text: String = env.get_string(&uri)?.into();
                    env.delete_local_ref(uri)?;
                    Some(text)
                };
                env.delete_local_ref(reference)?;
                env.delete_local_ref(vr)?;
                Ok(text)
            })();
            match built {
                Ok(Some(text)) => Ok(text),
                Ok(None) => Err("bulkDataUriBuilder returned null".to_owned()),
                Err(_) => Err(format!(
                    "bulkDataUriBuilder failed: {}",
                    take_exception(&mut env)
                )),
            }
        })();
        result.unwrap_or_else(|message| {
            *self.failure.borrow_mut() = Some(message);
            String::new()
        })
    }
}

struct JsonArgs {
    options: JsonOptionsIn,
    include: Vec<u32>,
    exclude: Vec<u32>,
    builder: Option<JavaUriBuilder>,
}

fn json_args(
    env: &mut JNIEnv,
    options_json: &JString,
    include: &JIntArray,
    exclude: &JIntArray,
    builder: &JObject,
) -> Result<JsonArgs, ApiError> {
    let options_json = get_optional_string(env, options_json)?.unwrap_or_default();
    let options: JsonOptionsIn = if options_json.is_empty() {
        JsonOptionsIn::default()
    } else {
        serde_json::from_str(&options_json)
            .map_err(|error| argument_error(format!("invalid options: {error}")))?
    };
    let builder = if builder.is_null() {
        None
    } else {
        let failed =
            |error: jni::errors::Error| ApiError::new(ApiErrorKind::Internal, error.to_string());
        Some(JavaUriBuilder {
            vm: env.get_java_vm().map_err(failed)?,
            function: env.new_global_ref(builder).map_err(failed)?,
            failure: RefCell::new(None),
        })
    };
    Ok(JsonArgs {
        options,
        include: int_array(env, include)?,
        exclude: int_array(env, exclude)?,
        builder,
    })
}

impl JsonArgs {
    fn run(
        &self,
        write: impl FnOnce(&DicomJsonExportOptions<'_>) -> Result<String, ApiError>,
    ) -> Result<String, ApiError> {
        let bulk_data_mode = match self.options.bulk_data.as_deref() {
            None | Some("uri") => DicomJsonBulkDataMode::Uri,
            Some("inline") => DicomJsonBulkDataMode::InlineBinary,
            Some(other) => {
                return Err(argument_error(format!(
                    "invalid bulkData '{other}'; expected 'uri' or 'inline'"
                )))
            }
        };
        let build = |request: &BulkDataUriRequest| {
            self.builder
                .as_ref()
                .map(|b| b.build(request))
                .unwrap_or_default()
        };
        let options = DicomJsonExportOptions {
            include_tags: &self.include,
            exclude_tags: &self.exclude,
            bulk_data_mode,
            bulk_data_uri_base: self.options.bulk_data_uri_base.as_deref(),
            bulk_data_uri_builder: self.builder.as_ref().map(|_| BulkDataUriBuilder(&build)),
            include_file_meta: self.options.include_file_meta.unwrap_or(false),
            keyword_and_vm: self.options.keyword_and_vm.unwrap_or(false),
        };
        let json = write(&options)?;
        if let Some(message) = self
            .builder
            .as_ref()
            .and_then(|builder| builder.failure.borrow().clone())
        {
            return Err(argument_error(message));
        }
        Ok(json)
    }
}

/// `Dcmnorm.toDicomJson(source, JsonOptions)`.
#[no_mangle]
pub extern "system" fn Java_com_pohcee_dcmnorm_Dcmnorm_nativeSourceToDicomJson<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    path: JString<'local>,
    bytes: JByteArray<'local>,
    stream: JObject<'local>,
    options_json: JString<'local>,
    include: JIntArray<'local>,
    exclude: JIntArray<'local>,
    builder: JObject<'local>,
) -> jstring {
    let prepared = (|| {
        let source = source_arg(&mut env, &path, &bytes, &stream)?;
        Ok::<_, ApiError>((
            source,
            json_args(&mut env, &options_json, &include, &exclude, &builder)?,
        ))
    })();
    let (mut source, args) = match prepared {
        Ok(prepared) => prepared,
        Err(error) => return throw_api_error(&mut env, &error),
    };
    let result =
        catch_api_panic(|| args.run(|options| source_to_dicom_json(source.source(), options)));
    match result {
        Ok(json) => string_result(&mut env, &json),
        Err(error) => throw_api_error(&mut env, &error),
    }
}

/// `Dcmnorm.toDicomJson(DicomDataset, JsonOptions)`.
#[no_mangle]
pub extern "system" fn Java_com_pohcee_dcmnorm_Dcmnorm_nativeDatasetToDicomJson<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    document: JByteArray<'local>,
    source_path: JString<'local>,
    source_bytes: JByteArray<'local>,
    options_json: JString<'local>,
    include: JIntArray<'local>,
    exclude: JIntArray<'local>,
    builder: JObject<'local>,
) -> jstring {
    let prepared = (|| {
        let document = borrow_bytes(&mut env, &document)?
            .ok_or_else(|| argument_error("no data set given"))?;
        let bulk = bulk_arg(&mut env, &source_path, &source_bytes)?;
        Ok::<_, ApiError>((
            document,
            bulk,
            json_args(&mut env, &options_json, &include, &exclude, &builder)?,
        ))
    })();
    let (document, bulk, args) = match prepared {
        Ok(prepared) => prepared,
        Err(error) => return throw_api_error(&mut env, &error),
    };
    let result = catch_api_panic(|| {
        let document = decode_document(document.as_slice())?;
        let bulk_source = bulk
            .as_ref()
            .map(SourceArg::bulk_source)
            .unwrap_or(BulkSource::None);
        args.run(|options| document_to_dicom_json(&document, &bulk_source, options))
    });
    match result {
        Ok(json) => string_result(&mut env, &json),
        Err(error) => throw_api_error(&mut env, &error),
    }
}

// -------------------------------------------------------------------------------------------
// Writing
// -------------------------------------------------------------------------------------------

/// `Dcmnorm.write(...)` / `toBytes(...)`: the encoded data set written with File Meta
/// Information created for it, bulk data references read from the source path/bytes, to a path,
/// an `OutputStream`, or (both null) returned as a `byte[]`.
#[no_mangle]
pub extern "system" fn Java_com_pohcee_dcmnorm_Dcmnorm_nativeWriteDataset<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    document: JByteArray<'local>,
    source_path: JString<'local>,
    source_bytes: JByteArray<'local>,
    output_path: JString<'local>,
    output_stream: JObject<'local>,
    transfer_syntax_uid: JString<'local>,
) -> jbyteArray {
    let prepared = (|| {
        let document = borrow_bytes(&mut env, &document)?
            .ok_or_else(|| argument_error("no data set given"))?;
        let bulk = bulk_arg(&mut env, &source_path, &source_bytes)?;
        let output_path = get_optional_string(&mut env, &output_path)?;
        let output = if output_stream.is_null() {
            None
        } else {
            Some(JavaOutputStream::new(&mut env, &output_stream)?)
        };
        let uid = get_optional_string(&mut env, &transfer_syntax_uid)?;
        Ok::<_, ApiError>((document, bulk, output_path, output, uid))
    })();
    let (document, bulk, output_path, output, uid) = match prepared {
        Ok(prepared) => prepared,
        Err(error) => return throw_api_error(&mut env, &error),
    };
    let result = catch_api_panic(|| {
        let document = decode_document(document.as_slice())?;
        let bulk_source = bulk
            .as_ref()
            .map(SourceArg::bulk_source)
            .unwrap_or(BulkSource::None);
        let options = WriteDocumentOptions {
            transfer_syntax_uid: uid.as_deref(),
        };
        if let Some(path) = output_path {
            write_document_to_path(
                &document,
                &bulk_source,
                &options,
                std::path::Path::new(&path),
            )?;
            Ok(None)
        } else if let Some(mut output) = output {
            write_document(&document, &bulk_source, &options, &mut output)?;
            output
                .flush()
                .map_err(|error| ApiError::io(error.to_string()))?;
            Ok(None)
        } else {
            let mut out = Vec::new();
            write_document(&document, &bulk_source, &options, &mut out)?;
            Ok(Some(out))
        }
    });
    match result {
        Ok(Some(bytes)) => byte_array_result(&mut env, &bytes),
        Ok(None) => std::ptr::null_mut(),
        Err(error) => throw_api_error(&mut env, &error),
    }
}

// -------------------------------------------------------------------------------------------
// Rendering
// -------------------------------------------------------------------------------------------

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct RenderParamsIn {
    window_center: Option<f64>,
    window_width: Option<f64>,
    voi_lut: Option<bool>,
    modality_lut: Option<bool>,
    icc_profile: Option<bool>,
    output_width: Option<u32>,
    output_height: Option<u32>,
    show_overlays: Option<bool>,
    overlay_index: Option<u32>,
    overlay_color: Option<String>,
}

/// `Dcmnorm.renderPixels` / `renderImage`: one frame's 8-bit display pixels, as a
/// `RenderedPixels` built by its `(String, byte[])` constructor. With `bgr`, 3-channel pixels
/// are swapped to B,G,R so the `byte[]` can back a `TYPE_3BYTE_BGR` `BufferedImage` as it is.
#[no_mangle]
pub extern "system" fn Java_com_pohcee_dcmnorm_Dcmnorm_nativeRenderPixels<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    path: JString<'local>,
    bytes: JByteArray<'local>,
    stream: JObject<'local>,
    frame_index: jint,
    options_json: JString<'local>,
    bgr: jboolean,
) -> jobject {
    let prepared = (|| {
        let source = source_arg(&mut env, &path, &bytes, &stream)?;
        let options_json = get_optional_string(&mut env, &options_json)?.unwrap_or_default();
        let options: RenderParamsIn = if options_json.is_empty() {
            RenderParamsIn::default()
        } else {
            serde_json::from_str(&options_json)
                .map_err(|error| argument_error(format!("invalid options: {error}")))?
        };
        if frame_index < 0 {
            return Err(argument_error(format!(
                "frame index {frame_index} is negative"
            )));
        }
        Ok::<_, ApiError>((source, options))
    })();
    let (mut source, options) = match prepared {
        Ok(prepared) => prepared,
        Err(error) => return throw_api_error(&mut env, &error),
    };
    let result = catch_api_panic(|| {
        let overlay_color = options
            .overlay_color
            .as_deref()
            .map(crate::parse_render_color)
            .transpose()
            .map_err(argument_error)?
            .unwrap_or([0, 255, 0]);
        let pipeline = RenderPipelineOptions {
            frame_index: frame_index as usize,
            window_center: options.window_center,
            window_width: options.window_width,
            apply_voi_lut: options.voi_lut.unwrap_or(true),
            apply_modality_lut: options.modality_lut.unwrap_or(true),
            apply_icc_profile: options.icc_profile.unwrap_or(true),
            output_width: options.output_width,
            output_height: options.output_height,
            show_overlays: options.show_overlays.unwrap_or(true),
            overlay_index: options.overlay_index.map(|index| index as usize),
            overlay_color,
            ..Default::default()
        };
        let mut rendered = render_source_frame(source.source(), &pipeline)?;
        if bgr != 0 && rendered.samples_per_pixel == 3 {
            for pixel in rendered.bytes.chunks_exact_mut(3) {
                pixel.swap(0, 2);
            }
        }
        let overlays: Vec<serde_json::Value> = rendered
            .overlays
            .iter()
            .map(|overlay| {
                serde_json::json!({
                    "index": overlay.index,
                    "group": overlay.group,
                    "rows": overlay.rows,
                    "columns": overlay.columns,
                    "overlayType": overlay.overlay_type,
                    "label": overlay.label,
                })
            })
            .collect();
        let metadata = serde_json::json!({
            "width": rendered.width,
            "height": rendered.height,
            "channels": rendered.samples_per_pixel,
            "bgr": bgr != 0 && rendered.samples_per_pixel == 3,
            "frameIndex": frame_index,
            "overlays": overlays,
            "selectedOverlayIndex": rendered.selected_overlay_index,
        })
        .to_string();
        Ok((metadata, rendered.bytes))
    });
    match result {
        Ok((metadata, pixels)) => {
            let built = (|| -> jni::errors::Result<jobject> {
                let class = env.find_class("com/pohcee/dcmnorm/RenderedPixels")?;
                let metadata: JObject = env.new_string(&metadata)?.into();
                let pixels: JObject = env.byte_array_from_slice(&pixels)?.into();
                let object = env.new_object(
                    class,
                    "(Ljava/lang/String;[B)V",
                    &[JValue::Object(&metadata), JValue::Object(&pixels)],
                )?;
                Ok(object.into_raw())
            })();
            match built {
                Ok(object) => object,
                Err(error) => throw_api_error(
                    &mut env,
                    &ApiError::new(ApiErrorKind::Internal, error.to_string()),
                ),
            }
        }
        Err(error) => throw_api_error(&mut env, &error),
    }
}
