//! JNI bindings for dcmnorm, built with the [`jni`](https://docs.rs/jni) crate. This is the Java
//! counterpart of `bindings/node` (napi-rs) and `bindings/python` (PyO3) - same underlying
//! `dcmnorm` crate, same feature set, adapted to Java/JNI conventions. See `README.md` in this
//! directory for the full API and the design choices specific to this binding (most notably:
//! optional/structured call arguments cross the JNI boundary as a single JSON string rather than
//! as individual JNI object fields, to keep the amount of hand-written field-by-field JNI
//! marshaling - a common source of silent crashes when a method/field signature string typo goes
//! undetected until called - bounded and easy to review).
//!
//! Every entry point below follows the same three-phase shape:
//! 1. Pull plain Rust values out of the Java arguments (`JNIEnv::get_string`, `serde_json`).
//! 2. Run the actual dcmnorm call inside `guarded()`, which never touches a `JNIEnv` - so a
//!    `catch_unwind` around it is sound even though `JNIEnv` itself is not `UnwindSafe`.
//! 3. On `Err`, throw a `DcmnormException` and return a default/sentinel value (the JVM raises
//!    the pending exception as soon as the native method returns - the sentinel is never
//!    observed). On `Ok`, convert the Rust value into the JNI return type.

use std::collections::HashMap;
use std::panic::{self, AssertUnwindSafe};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use jni::objects::{GlobalRef, JClass, JObject, JString, JValue};
use jni::sys::{jboolean, jdouble, jdoubleArray, jint, jlong, jobject, jstring};
use jni::{JNIEnv, JavaVM};
use serde::Deserialize;

use dcmnorm::dicom_io::{
    apply_filter_to_object, build_volume as dcm_build_volume, dicom_file_to_json,
    echo_scu as dcm_echo_scu, find_scu as dcm_find_scu, move_scu as dcm_move_scu,
    pack_dicom_frame_stack_texture as dcm_pack_dicom_frame_stack_texture,
    pack_dicom_frame_texture as dcm_pack_dicom_frame_texture, pack_volume_texture as dcm_pack_volume_texture,
    parse_attribute_override, parse_filter_requests, parse_tag_key, probe_dicom_file_for_sop_class_uid,
    read_dcmnorm_object_for_filter, read_dicom_file, read_dicom_json_with_options, reformat_plane as dcm_reformat_plane,
    remove_attribute, remove_private_tags_inplace, render_dicom_frame as dcm_render_dicom_frame, set_attribute,
    store_scu as dcm_store_scu, transcode_dicom_file, write_dicom_file, write_dicom_json_with_options,
    write_dicom_video as dcm_write_dicom_video, CancelMode as DcmCancelMode, DicomJsonBulkDataMode, DicomJsonFormat,
    DicomJsonKeyStyle, DicomJsonReadOptions, DicomJsonWriteOptions, DimseLogger, EchoScuOptions as DcmEchoScuOptions,
    FindScuOptions as DcmFindScuOptions, Interpolation as DcmInterpolation, MoveScuOptions as DcmMoveScuOptions,
    PlaneParams as DcmPlaneParams, RenderOutputFormat as DcmRenderOutputFormat,
    RenderPipelineOptions as DcmRenderPipelineOptions, SlabProjection as DcmSlabProjection,
    StoreScuOptions as DcmStoreScuOptions, TextureCompression as DcmTextureCompression, Volume as DcmVolume,
};

const EXCEPTION_CLASS: &str = "com/pohcee/dcmnorm/DcmnormException";

// -------------------------------------------------------------------------------------------
// Shared plumbing: panic guarding, exception throwing, JSON option parsing
// -------------------------------------------------------------------------------------------

/// DICOM files arrive from arbitrary, sometimes-malformed vendor equipment. A subprocess-based
/// CLI call fails in isolation when one is bad; an in-process native library does not get that
/// for free, so every entry point runs its dcmnorm call through `catch_unwind` and turns a panic
/// into an `Err` (and from there, a `DcmnormException`) instead of taking the whole JVM process
/// down with it - mirrors `guarded()` in the Node/Python bindings. Deliberately takes a closure
/// that touches no `JNIEnv` (see the module doc) so this is sound without needing to reason about
/// `JNIEnv`'s own unwind-safety.
fn guarded<T>(f: impl FnOnce() -> Result<T, String>) -> Result<T, String> {
    match panic::catch_unwind(AssertUnwindSafe(f)) {
        Ok(result) => result,
        Err(payload) => {
            let message = payload
                .downcast_ref::<&str>()
                .map(|s| s.to_string())
                .or_else(|| payload.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "dcmnorm panicked while processing this call".to_string());
            Err(format!("dcmnorm internal error: {message}"))
        }
    }
}

/// Throws a `DcmnormException` carrying `message` and returns `T`'s default - every raw JNI
/// return type (`jstring`, `jobject`, `jlong`, `jint`, `jboolean`, `jdouble`, ...) implements
/// `Default` (null-pointer or zero), and the JVM discards whatever a native method returns once
/// an exception is actually pending, so the sentinel value itself is never observed by Java code.
fn throw<T: Default>(env: &mut JNIEnv, message: impl AsRef<str>) -> T {
    let _ = env.throw_new(EXCEPTION_CLASS, message.as_ref());
    T::default()
}

/// Like `throw`, but raises an unchecked `java.lang.RuntimeException` instead of
/// `DcmnormException` - for a JNI-allocation-failure edge case (e.g. `NewDoubleArray` under
/// extreme memory pressure) that has nothing to do with dcmnorm's own DICOM-processing errors,
/// so the Java-side method it backs is not declared `throws DcmnormException` at all.
fn throw_runtime<T: Default>(env: &mut JNIEnv, message: impl AsRef<str>) -> T {
    let _ = env.throw_new("java/lang/RuntimeException", message.as_ref());
    T::default()
}

/// Reads a Java `String` argument. Only fails if `value` is `null` or not valid UTF-16 - both
/// indicate a caller bug (every public Java method guards against passing `null` for a
/// non-optional `String`), not a DICOM-data problem, but handled the same way regardless.
fn get_string(env: &mut JNIEnv, value: &JString) -> Result<String, String> {
    env.get_string(value).map(|s| s.into()).map_err(|error| format!("invalid string argument: {error}"))
}

fn parse_json<T: for<'de> Deserialize<'de> + Default>(json: &str) -> Result<T, String> {
    if json.is_empty() {
        return Ok(T::default());
    }
    serde_json::from_str(json).map_err(|error| format!("invalid options: {error}"))
}

fn parse_json_format(value: Option<&str>) -> Result<DicomJsonFormat, String> {
    match value {
        None | Some("flat") => Ok(DicomJsonFormat::Flat),
        Some("standard") => Ok(DicomJsonFormat::Standard),
        Some(other) => Err(format!("invalid format '{other}'; expected 'flat' or 'standard'")),
    }
}

fn parse_key_style(value: Option<&str>) -> Result<DicomJsonKeyStyle, String> {
    match value {
        None | Some("name") => Ok(DicomJsonKeyStyle::Name),
        Some("hex") => Ok(DicomJsonKeyStyle::Hex),
        Some(other) => Err(format!("invalid keyStyle '{other}'; expected 'name' or 'hex'")),
    }
}

// The dcmnorm CLI's --bulk-data flag defaults to Uri (a small "?offset=..&length=.." reference),
// overriding DicomJsonWriteOptions::default()'s InlineBinary at the CLI layer - so callers here
// need the same override, not the library default, or PixelData ends up fully base64-inlined
// (~1000x larger output for a typical image) instead of referenced. Mirrors the Node/Python
// bindings' own copy of this comment.
fn parse_bulk_data_mode(value: Option<&str>) -> Result<DicomJsonBulkDataMode, String> {
    match value {
        None | Some("uri") => Ok(DicomJsonBulkDataMode::Uri),
        Some("inline") => Ok(DicomJsonBulkDataMode::InlineBinary),
        Some(other) => Err(format!("invalid bulkData '{other}'; expected 'uri' or 'inline'")),
    }
}

fn parse_render_output_format(value: Option<&str>) -> Result<(DcmRenderOutputFormat, &'static str), String> {
    match value {
        None | Some("jpeg") | Some("jpg") => Ok((DcmRenderOutputFormat::Jpeg, "image/jpeg")),
        Some("png") => Ok((DcmRenderOutputFormat::Png, "image/png")),
        Some(other) => Err(format!("invalid format '{other}'; expected 'jpeg' or 'png'")),
    }
}

/// Parses `"#RRGGBB"` or `"R,G,B"` into an `[R, G, B]` byte triple - same two accepted shapes as
/// the Node/Python bindings.
fn parse_render_color(value: &str) -> Result<[u8; 3], String> {
    let trimmed = value.trim();

    if let Some(hex) = trimmed.strip_prefix('#') {
        if hex.len() != 6 {
            return Err(format!("invalid color '{value}'; hex color must be #RRGGBB"));
        }
        let channel = |range: std::ops::Range<usize>| {
            u8::from_str_radix(&hex[range], 16).map_err(|_| format!("invalid color '{value}'; not a valid hex color"))
        };
        return Ok([channel(0..2)?, channel(2..4)?, channel(4..6)?]);
    }

    let parts: Vec<&str> = trimmed.split(',').collect();
    if parts.len() != 3 {
        return Err(format!("invalid color '{value}'; expected R,G,B (0-255 each) or #RRGGBB"));
    }
    let mut channels = [0u8; 3];
    for (index, part) in parts.iter().enumerate() {
        channels[index] = part
            .trim()
            .parse::<u8>()
            .map_err(|_| format!("invalid color '{value}'; expected R,G,B (0-255 each) or #RRGGBB"))?;
    }
    Ok(channels)
}

fn overlay_pipeline_fields(
    show_overlays: Option<bool>,
    overlay_index: Option<u32>,
    overlay_color: Option<&str>,
) -> Result<(bool, Option<usize>, [u8; 3]), String> {
    let color = overlay_color.map(parse_render_color).transpose()?.unwrap_or([0, 255, 0]);
    Ok((show_overlays.unwrap_or(true), overlay_index.map(|value| value as usize), color))
}

fn parse_interpolation(value: Option<&str>) -> Result<DcmInterpolation, String> {
    match value {
        None | Some("trilinear") => Ok(DcmInterpolation::Trilinear),
        Some("nearest") => Ok(DcmInterpolation::Nearest),
        Some(other) => Err(format!("invalid interpolation '{other}'; expected 'trilinear' or 'nearest'")),
    }
}

fn parse_slab_projection(value: Option<&str>) -> Result<DcmSlabProjection, String> {
    match value {
        None | Some("mip") => Ok(DcmSlabProjection::MaximumIntensity),
        Some("minip") => Ok(DcmSlabProjection::MinimumIntensity),
        Some("average") => Ok(DcmSlabProjection::Average),
        Some(other) => Err(format!("invalid slabProjection '{other}'; expected 'mip', 'minip', or 'average'")),
    }
}

fn parse_texture_compression(value: Option<&str>) -> Result<DcmTextureCompression, String> {
    match value {
        None | Some("gzip") => Ok(DcmTextureCompression::Gzip),
        Some("none") => Ok(DcmTextureCompression::None),
        Some(other) => Err(format!("invalid compression '{other}'; expected 'gzip' or 'none'")),
    }
}

// -------------------------------------------------------------------------------------------
// DIMSE on_log bridging
// -------------------------------------------------------------------------------------------

/// Bridges dcmnorm's `DimseLogger` trait onto a Java `DimseLogger` callback object. Every `*Scu`
/// entry point in this crate is fully synchronous/blocking (unlike the Node bindings, which need
/// a `ThreadsafeFunction` bridge because a JS callback is inherently async, or even the Python
/// bindings' handle-based `start_*_scu` variants, which call back from a spawned background
/// thread) - the calling Java thread never leaves its own native call, so this never needs to
/// attach a new thread to the JVM. `JavaVM::get_env` just re-obtains a `JNIEnv` for the (already
/// attached) current thread.
struct JavaDimseLogger {
    vm: JavaVM,
    callback: GlobalRef,
}

impl DimseLogger for JavaDimseLogger {
    fn log(&self, message: String) {
        let Ok(mut env) = self.vm.get_env() else { return };
        let Ok(jmessage) = env.new_string(&message) else { return };
        let _ = env.call_method(&self.callback, "log", "(Ljava/lang/String;)V", &[JValue::Object(&jmessage)]);
        // A throwing `log` implementation leaves an exception pending on this thread - clear it
        // rather than letting it leak into whatever JNI call happens next (which would otherwise
        // misbehave/abort under `-Xcheck:jni`). `log` is documented as "exceptions are
        // swallowed", matching that contract.
        let _ = env.exception_clear();
    }
}

/// Takes `env` and the raw `on_log` argument (nullable `DimseLogger`) and produces the
/// `Option<Box<dyn DimseLogger>>` dcmnorm's SCU options expect. Must run BEFORE `guarded()` - it
/// needs `env`, which `guarded()`'s closure may not touch.
fn java_dimse_logger(env: &mut JNIEnv, on_log: &JObject) -> Result<Option<Box<dyn DimseLogger>>, String> {
    if on_log.is_null() {
        return Ok(None);
    }
    let vm = env.get_java_vm().map_err(|error| error.to_string())?;
    let callback = env.new_global_ref(on_log).map_err(|error| error.to_string())?;
    Ok(Some(Box::new(JavaDimseLogger { vm, callback }) as Box<dyn DimseLogger>))
}

// -------------------------------------------------------------------------------------------
// Result -> Java object construction
// -------------------------------------------------------------------------------------------

/// Constructs a `com.pohcee.dcmnorm.RenderedFrame` via its `(String, byte[])` constructor, which
/// parses `metadata_json` itself - see that class's source for the schema (mimeType/width/height/
/// overlays/selectedOverlayIndex).
fn new_rendered_frame(env: &mut JNIEnv, metadata_json: &str, data: &[u8]) -> Result<jobject, String> {
    let class = env.find_class("com/pohcee/dcmnorm/RenderedFrame").map_err(|error| error.to_string())?;
    let jmeta = env.new_string(metadata_json).map_err(|error| error.to_string())?;
    let jdata = env.byte_array_from_slice(data).map_err(|error| error.to_string())?;
    let object = env
        .new_object(class, "(Ljava/lang/String;[B)V", &[JValue::Object(&jmeta), JValue::Object(&jdata)])
        .map_err(|error| error.to_string())?;
    Ok(object.into_raw())
}

fn new_rendered_movie(env: &mut JNIEnv, mime_type: &str, data: &[u8]) -> Result<jobject, String> {
    let class = env.find_class("com/pohcee/dcmnorm/RenderedMovie").map_err(|error| error.to_string())?;
    let jmime = env.new_string(mime_type).map_err(|error| error.to_string())?;
    let jdata = env.byte_array_from_slice(data).map_err(|error| error.to_string())?;
    let object = env
        .new_object(class, "(Ljava/lang/String;[B)V", &[JValue::Object(&jmime), JValue::Object(&jdata)])
        .map_err(|error| error.to_string())?;
    Ok(object.into_raw())
}

/// Constructs a `com.pohcee.dcmnorm.TextureExportResult`. `meta_json` is
/// `dcmnorm::dicom_io::texture_export::TextureMeta::to_json()` - the single source of truth for
/// this payload's field names/casing already shared with the CLI sidecar and the render-server
/// (see that method's own doc comment) - so this binding reuses it exactly rather than
/// re-deriving an equivalent JSON shape by hand.
fn new_texture_export_result(env: &mut JNIEnv, meta_json: &str, data: &[u8]) -> Result<jobject, String> {
    let class = env.find_class("com/pohcee/dcmnorm/TextureExportResult").map_err(|error| error.to_string())?;
    let jmeta = env.new_string(meta_json).map_err(|error| error.to_string())?;
    let jdata = env.byte_array_from_slice(data).map_err(|error| error.to_string())?;
    let object = env
        .new_object(class, "(Ljava/lang/String;[B)V", &[JValue::Object(&jmeta), JValue::Object(&jdata)])
        .map_err(|error| error.to_string())?;
    Ok(object.into_raw())
}

fn render_pipeline_options_json(rendered: &dcmnorm::dicom_io::RenderFrameOutput, mime_type: &str) -> String {
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
    serde_json::json!({
        "mimeType": mime_type,
        "width": rendered.width,
        "height": rendered.height,
        "overlays": overlays,
        "selectedOverlayIndex": rendered.selected_overlay_index,
    })
    .to_string()
}

// -------------------------------------------------------------------------------------------
// Core: readTags / readJson / writeJson / editTags / transcode / checkDicom
// -------------------------------------------------------------------------------------------

/// Reads only the requested tags from a DICOM file, stopping as soon as the highest one has been
/// parsed. Mirrors `dcmnorm --filter ... --format flat --keys hex`. `tags_json` is a JSON array
/// of tag keywords/expressions; the returned JSON is keyed by bare hex tag (`"0020000D"`).
#[no_mangle]
pub extern "system" fn Java_com_pohcee_dcmnorm_Dcmnorm_nativeReadTags<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    file_path: JString<'local>,
    tags_json: JString<'local>,
) -> jstring {
    let file_path = match get_string(&mut env, &file_path) {
        Ok(value) => value,
        Err(message) => return throw(&mut env, message),
    };
    let tags_json = match get_string(&mut env, &tags_json) {
        Ok(value) => value,
        Err(message) => return throw(&mut env, message),
    };

    let result = guarded(|| {
        let tags: Vec<String> = serde_json::from_str(&tags_json).map_err(|error| error.to_string())?;
        let path = PathBuf::from(file_path);
        let requests = parse_filter_requests(&tags).map_err(|error| error.to_string())?;
        let mut object = read_dcmnorm_object_for_filter(&path, &requests).map_err(|error| error.to_string())?;
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
        .map_err(|error| error.to_string())
    });

    match result {
        Ok(json) => match env.new_string(json) {
            Ok(value) => value.into_raw(),
            Err(error) => throw(&mut env, error.to_string()),
        },
        Err(message) => throw(&mut env, message),
    }
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct ReadJsonOptionsIn {
    format: Option<String>,
    key_style: Option<String>,
    bulk_data: Option<String>,
}

/// Reads the full DICOM dataset as JSON. `options_json` (never null - callers pass `"{}"` for
/// defaults) mirrors `ReadJsonOptions`: `format`/`keyStyle`/`bulkData`. `bulkData` defaults to
/// `"uri"` (matching the CLI), not the Rust library's own default of inline-embedding bulk data -
/// getting this wrong makes a huge difference (`"inline"` base64-embeds e.g. `PixelData`
/// directly, ~1000x larger output for a typical image, instead of a small
/// `"?offset=..&length=.."` reference). Mirrors the Node/Python bindings' own copy of this
/// comment.
#[no_mangle]
pub extern "system" fn Java_com_pohcee_dcmnorm_Dcmnorm_nativeReadJson<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    file_path: JString<'local>,
    options_json: JString<'local>,
) -> jstring {
    let file_path = match get_string(&mut env, &file_path) {
        Ok(value) => value,
        Err(message) => return throw(&mut env, message),
    };
    let options_json = match get_string(&mut env, &options_json) {
        Ok(value) => value,
        Err(message) => return throw(&mut env, message),
    };

    let result = guarded(|| {
        let options: ReadJsonOptionsIn = parse_json(&options_json)?;
        let format = parse_json_format(options.format.as_deref())?;
        let key_style = parse_key_style(options.key_style.as_deref())?;
        let bulk_data_mode = parse_bulk_data_mode(options.bulk_data.as_deref())?;
        dicom_file_to_json(
            PathBuf::from(file_path),
            DicomJsonWriteOptions { format, key_style, bulk_data_mode, ..Default::default() },
        )
        .map_err(|error| error.to_string())
    });

    match result {
        Ok(json) => match env.new_string(json) {
            Ok(value) => value.into_raw(),
            Err(error) => throw(&mut env, error.to_string()),
        },
        Err(message) => throw(&mut env, message),
    }
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct WriteJsonOptionsIn {
    format: Option<String>,
    bulk_data_source_path: Option<String>,
}

/// Writes a DICOM file from JSON (flat or standard format, auto never guessed - pass the same
/// `format` used to read it). Mirrors `dcmnorm dataset.json out.dcm`. `bulkDataSourcePath`
/// resolves `"?offset=..&length=.."` BulkDataURIs against that file's bytes - mirrors the CLI's
/// `--bulk-data-source`.
#[no_mangle]
pub extern "system" fn Java_com_pohcee_dcmnorm_Dcmnorm_nativeWriteJson<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    json: JString<'local>,
    output_path: JString<'local>,
    options_json: JString<'local>,
) {
    let json = match get_string(&mut env, &json) {
        Ok(value) => value,
        Err(message) => return throw(&mut env, message),
    };
    let output_path = match get_string(&mut env, &output_path) {
        Ok(value) => value,
        Err(message) => return throw(&mut env, message),
    };
    let options_json = match get_string(&mut env, &options_json) {
        Ok(value) => value,
        Err(message) => return throw(&mut env, message),
    };

    let result = guarded(|| {
        let options: WriteJsonOptionsIn = parse_json(&options_json)?;
        let format = parse_json_format(options.format.as_deref())?;
        let bulk_data_source =
            options.bulk_data_source_path.as_ref().map(std::fs::read).transpose().map_err(|error| error.to_string())?;

        let mut object = read_dicom_json_with_options(
            &json,
            DicomJsonReadOptions { format, bulk_data_source: bulk_data_source.as_deref() },
        )
        .map_err(|error| error.to_string())?;

        write_dicom_file(&mut object, PathBuf::from(output_path)).map_err(|error| error.to_string())
    });

    if let Err(message) = result {
        throw::<()>(&mut env, message);
    }
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct EditTagsOptionsIn {
    output_path: Option<String>,
    set: Option<HashMap<String, String>>,
    remove: Option<Vec<String>>,
    remove_private_tags: Option<bool>,
}

/// Sets/removes DICOM attributes, optionally stripping private tags. Mirrors `dcmnorm --set
/// KEY=VALUE --remove KEY --remove-private-tags`. Writes back to `file_path` in place unless
/// `outputPath` is given.
#[no_mangle]
pub extern "system" fn Java_com_pohcee_dcmnorm_Dcmnorm_nativeEditTags<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    file_path: JString<'local>,
    options_json: JString<'local>,
) {
    let file_path = match get_string(&mut env, &file_path) {
        Ok(value) => value,
        Err(message) => return throw(&mut env, message),
    };
    let options_json = match get_string(&mut env, &options_json) {
        Ok(value) => value,
        Err(message) => return throw(&mut env, message),
    };

    let result = guarded(|| {
        let options: EditTagsOptionsIn = parse_json(&options_json)?;
        let output_path = options.output_path.unwrap_or_else(|| file_path.clone());
        let sets = options.set.unwrap_or_default();
        let removes = options.remove.unwrap_or_default();
        let remove_private_tags = options.remove_private_tags.unwrap_or(false);

        let mut object = read_dicom_file(PathBuf::from(&file_path)).map_err(|error| error.to_string())?;

        for (key, value) in &sets {
            let assignment = format!("{key}={value}");
            let (tag, vr, value) = parse_attribute_override(&assignment).map_err(|error| error.to_string())?;
            set_attribute(&mut object, tag, vr, value).map_err(|error| error.to_string())?;
        }

        for key in &removes {
            let tag = parse_tag_key(key).map_err(|error| error.to_string())?;
            remove_attribute(&mut object, tag);
        }

        if remove_private_tags {
            remove_private_tags_inplace(&mut object);
        }

        write_dicom_file(&mut object, PathBuf::from(&output_path)).map_err(|error| error.to_string())
    });

    if let Err(message) = result {
        throw::<()>(&mut env, message);
    }
}

/// Transcodes a DICOM file to the given transfer syntax UID. Mirrors `dcmnorm --transfer-syntax
/// UID in.dcm out.dcm`.
#[no_mangle]
pub extern "system" fn Java_com_pohcee_dcmnorm_Dcmnorm_nativeTranscode<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    file_path: JString<'local>,
    output_path: JString<'local>,
    transfer_syntax_uid: JString<'local>,
) {
    let file_path = match get_string(&mut env, &file_path) {
        Ok(value) => value,
        Err(message) => return throw(&mut env, message),
    };
    let output_path = match get_string(&mut env, &output_path) {
        Ok(value) => value,
        Err(message) => return throw(&mut env, message),
    };
    let transfer_syntax_uid = match get_string(&mut env, &transfer_syntax_uid) {
        Ok(value) => value,
        Err(message) => return throw(&mut env, message),
    };

    let result = guarded(|| {
        transcode_dicom_file(PathBuf::from(file_path), PathBuf::from(output_path), &transfer_syntax_uid)
            .map_err(|error| error.to_string())
    });

    if let Err(message) = result {
        throw::<()>(&mut env, message);
    }
}

/// Reports whether a file looks like valid DICOM. Mirrors `dcmnorm --check-dicom`. Never raises -
/// an unreadable/malformed/non-DICOM file is simply `false`.
#[no_mangle]
pub extern "system" fn Java_com_pohcee_dcmnorm_Dcmnorm_nativeCheckDicom<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    file_path: JString<'local>,
) -> jboolean {
    let file_path = match get_string(&mut env, &file_path) {
        Ok(value) => value,
        Err(_) => return 0,
    };
    let is_dicom = panic::catch_unwind(AssertUnwindSafe(|| probe_dicom_file_for_sop_class_uid(PathBuf::from(file_path))))
        .ok()
        .and_then(|result| result.ok())
        .unwrap_or(false);
    jboolean::from(is_dicom)
}

// -------------------------------------------------------------------------------------------
// DIMSE: echoScu / storeScu / findScu / moveScu - every one blocks the calling Java thread until
// the call completes. Unlike the Python bindings' handle-based `start_*_scu` variants, there is
// no early abort()/release() here - see this crate's README for the scope decision.
// -------------------------------------------------------------------------------------------

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct EchoScuOptionsIn {
    calling_ae_title: Option<String>,
    called_ae_title: Option<String>,
    timeout_ms: Option<u32>,
}

/// Performs a C-ECHO (DICOM Verification) against `destination` ("host:port"). Returns the
/// response Status code (0 = success); raises `DcmnormException` only if the association itself
/// could not be established.
#[no_mangle]
pub extern "system" fn Java_com_pohcee_dcmnorm_Dcmnorm_nativeEchoScu<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    destination: JString<'local>,
    options_json: JString<'local>,
    on_log: JObject<'local>,
) -> jint {
    let destination = match get_string(&mut env, &destination) {
        Ok(value) => value,
        Err(message) => return throw(&mut env, message),
    };
    let options_json = match get_string(&mut env, &options_json) {
        Ok(value) => value,
        Err(message) => return throw(&mut env, message),
    };
    let logger = match java_dimse_logger(&mut env, &on_log) {
        Ok(value) => value,
        Err(message) => return throw(&mut env, message),
    };

    let result = guarded(|| {
        let options: EchoScuOptionsIn = parse_json(&options_json)?;
        dcm_echo_scu(
            &destination,
            DcmEchoScuOptions {
                calling_ae_title: options.calling_ae_title.unwrap_or_else(|| "DCMNORM".to_owned()),
                called_ae_title: options.called_ae_title,
                timeout: options.timeout_ms.map(|ms| Duration::from_millis(ms as u64)),
                on_log: logger,
                cancel: None,
            },
        )
        .map_err(|error| error.to_string())
    });

    match result {
        Ok(status) => jint::from(status),
        Err(message) => throw(&mut env, message),
    }
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct StoreScuOptionsIn {
    calling_ae_title: Option<String>,
    called_ae_title: Option<String>,
    max_pdu_length: Option<u32>,
    never_transcode: Option<bool>,
    timeout_ms: Option<u32>,
}

/// Sends each of `files_json` (a JSON array of paths) via C-STORE to `destination`
/// ("host:port"). Returns a JSON array of `{"sopInstanceUid", "status"}` objects, one per file
/// that could be read and sent - a non-zero status is just data in the result (the peer rejected
/// that instance), not a raised error; this only raises if the association itself could not be
/// established, or none of the files could be read as DICOM at all.
#[no_mangle]
pub extern "system" fn Java_com_pohcee_dcmnorm_Dcmnorm_nativeStoreScu<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    destination: JString<'local>,
    files_json: JString<'local>,
    options_json: JString<'local>,
    on_log: JObject<'local>,
) -> jstring {
    let destination = match get_string(&mut env, &destination) {
        Ok(value) => value,
        Err(message) => return throw(&mut env, message),
    };
    let files_json = match get_string(&mut env, &files_json) {
        Ok(value) => value,
        Err(message) => return throw(&mut env, message),
    };
    let options_json = match get_string(&mut env, &options_json) {
        Ok(value) => value,
        Err(message) => return throw(&mut env, message),
    };
    let logger = match java_dimse_logger(&mut env, &on_log) {
        Ok(value) => value,
        Err(message) => return throw(&mut env, message),
    };

    let result = guarded(|| {
        let files: Vec<String> = serde_json::from_str(&files_json).map_err(|error| error.to_string())?;
        let files: Vec<PathBuf> = files.into_iter().map(PathBuf::from).collect();
        let options: StoreScuOptionsIn = parse_json(&options_json)?;

        let results = dcm_store_scu(
            &destination,
            &files,
            DcmStoreScuOptions {
                calling_ae_title: options.calling_ae_title.unwrap_or_else(|| "DCMNORM".to_owned()),
                called_ae_title: options.called_ae_title,
                max_pdu_length: options.max_pdu_length.unwrap_or(16384),
                never_transcode: options.never_transcode.unwrap_or(false),
                timeout: options.timeout_ms.map(|ms| Duration::from_millis(ms as u64)),
                on_log: logger,
                cancel: None,
            },
        )
        .map_err(|error| error.to_string())?;

        let values: Vec<serde_json::Value> = results
            .into_iter()
            .map(|result| serde_json::json!({"sopInstanceUid": result.sop_instance_uid, "status": result.status}))
            .collect();
        serde_json::to_string(&values).map_err(|error| error.to_string())
    });

    match result {
        Ok(json) => match env.new_string(json) {
            Ok(value) => value.into_raw(),
            Err(error) => throw(&mut env, error.to_string()),
        },
        Err(message) => throw(&mut env, message),
    }
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct FindScuOptionsIn {
    calling_ae_title: Option<String>,
    called_ae_title: Option<String>,
    max_pdu_length: Option<u32>,
    timeout_ms: Option<u32>,
}

/// Performs a C-FIND (Study Root Query/Retrieve) against `destination` ("host:port").
/// `query_json` is a JSON object of `{tagKeyOrKeyword: value}`: an empty string value is a
/// universal-match "return key" (mirrors findscu's bare `-k TAG`), non-empty constrains the match
/// (mirrors `-k TAG=value`); `QueryRetrieveLevel` defaults to `"STUDY"` if not given. Returns a
/// JSON array of flat/hex-keyed DICOM JSON strings, one per match (parse each Java-side, same
/// shape as `readJson(path, new ReadJsonOptions().format("flat").keyStyle("hex"))`).
#[no_mangle]
pub extern "system" fn Java_com_pohcee_dcmnorm_Dcmnorm_nativeFindScu<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    destination: JString<'local>,
    query_json: JString<'local>,
    options_json: JString<'local>,
    on_log: JObject<'local>,
) -> jstring {
    let destination = match get_string(&mut env, &destination) {
        Ok(value) => value,
        Err(message) => return throw(&mut env, message),
    };
    let query_json = match get_string(&mut env, &query_json) {
        Ok(value) => value,
        Err(message) => return throw(&mut env, message),
    };
    let options_json = match get_string(&mut env, &options_json) {
        Ok(value) => value,
        Err(message) => return throw(&mut env, message),
    };
    let logger = match java_dimse_logger(&mut env, &on_log) {
        Ok(value) => value,
        Err(message) => return throw(&mut env, message),
    };

    let result = guarded(|| {
        let query: HashMap<String, String> = serde_json::from_str(&query_json).map_err(|error| error.to_string())?;
        let options: FindScuOptionsIn = parse_json(&options_json)?;

        let matches = dcm_find_scu(
            &destination,
            &query,
            DcmFindScuOptions {
                calling_ae_title: options.calling_ae_title.unwrap_or_else(|| "DCMNORM".to_owned()),
                called_ae_title: options.called_ae_title,
                max_pdu_length: options.max_pdu_length.unwrap_or(16384),
                timeout: options.timeout_ms.map(|ms| Duration::from_millis(ms as u64)),
                on_log: logger,
                cancel: None,
            },
        )
        .map_err(|error| error.to_string())?;

        serde_json::to_string(&matches).map_err(|error| error.to_string())
    });

    match result {
        Ok(json) => match env.new_string(json) {
            Ok(value) => value.into_raw(),
            Err(error) => throw(&mut env, error.to_string()),
        },
        Err(message) => throw(&mut env, message),
    }
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct MoveScuOptionsIn {
    calling_ae_title: Option<String>,
    called_ae_title: Option<String>,
    max_pdu_length: Option<u32>,
    timeout_ms: Option<u32>,
    watch_path: Option<String>,
    stale_data_timeout_ms: Option<u32>,
}

/// Performs a C-MOVE (Study Root Query/Retrieve), asking `destination` ("host:port") to push
/// `studyInstanceUid` to `moveDestinationAe` (an AE title `destination` already knows how to
/// reach, not a socket address). Blocks until the retrieve reaches a terminal status. Returns a
/// JSON object `{"status","completed","failed","warning","remaining","cancelled","cancelledVia"}`
/// regardless of success/warning/failure.
#[no_mangle]
pub extern "system" fn Java_com_pohcee_dcmnorm_Dcmnorm_nativeMoveScu<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    destination: JString<'local>,
    move_destination_ae: JString<'local>,
    study_instance_uid: JString<'local>,
    options_json: JString<'local>,
    on_log: JObject<'local>,
) -> jstring {
    let destination = match get_string(&mut env, &destination) {
        Ok(value) => value,
        Err(message) => return throw(&mut env, message),
    };
    let move_destination_ae = match get_string(&mut env, &move_destination_ae) {
        Ok(value) => value,
        Err(message) => return throw(&mut env, message),
    };
    let study_instance_uid = match get_string(&mut env, &study_instance_uid) {
        Ok(value) => value,
        Err(message) => return throw(&mut env, message),
    };
    let options_json = match get_string(&mut env, &options_json) {
        Ok(value) => value,
        Err(message) => return throw(&mut env, message),
    };
    let logger = match java_dimse_logger(&mut env, &on_log) {
        Ok(value) => value,
        Err(message) => return throw(&mut env, message),
    };

    let result = guarded(|| {
        let options: MoveScuOptionsIn = parse_json(&options_json)?;

        let outcome = dcm_move_scu(
            &destination,
            &move_destination_ae,
            &study_instance_uid,
            DcmMoveScuOptions {
                calling_ae_title: options.calling_ae_title.unwrap_or_else(|| "DCMNORM".to_owned()),
                called_ae_title: options.called_ae_title,
                max_pdu_length: options.max_pdu_length.unwrap_or(16384),
                timeout: options.timeout_ms.map(|ms| Duration::from_millis(ms as u64)),
                stale_data_path: options.watch_path.map(PathBuf::from),
                stale_data_timeout: options.stale_data_timeout_ms.map(|ms| Duration::from_millis(ms as u64)),
                on_log: logger,
                cancel: None,
            },
        )
        .map_err(|error| error.to_string())?;

        let cancelled_via = outcome.cancelled_via.map(|mode| match mode {
            DcmCancelMode::Release => "release",
            DcmCancelMode::Abort => "abort",
        });
        serde_json::to_string(&serde_json::json!({
            "status": outcome.status,
            "completed": outcome.completed,
            "failed": outcome.failed,
            "warning": outcome.warning,
            "remaining": outcome.remaining,
            "cancelled": outcome.cancelled,
            "cancelledVia": cancelled_via,
        }))
        .map_err(|error| error.to_string())
    });

    match result {
        Ok(json) => match env.new_string(json) {
            Ok(value) => value.into_raw(),
            Err(error) => throw(&mut env, error.to_string()),
        },
        Err(message) => throw(&mut env, message),
    }
}

// -------------------------------------------------------------------------------------------
// Rendering: renderFrame / renderMovie
// -------------------------------------------------------------------------------------------

fn unique_temp_path(extension: &str) -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);

    let nanos =
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
    let sequence = COUNTER.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!("dcmnorm-java-{}-{nanos}-{sequence}.{extension}", std::process::id()))
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct RenderFrameOptionsIn {
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
}

/// Renders a single frame of a DICOM file to JPEG or PNG. Mirrors `dcmnorm --output-width ...
/// --render-frame ... file.dcm out.jpg`. If the instance has one or more DICOM overlay planes
/// (group `60xx`), the first available overlay composites onto the image by default;
/// `overlayIndex` selects a different one, `showOverlays: false` disables overlay rendering, and
/// `overlayColor` (`"R,G,B"` or `"#RRGGBB"`, default green) sets the fill color.
#[no_mangle]
pub extern "system" fn Java_com_pohcee_dcmnorm_Dcmnorm_nativeRenderFrame<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    file_path: JString<'local>,
    options_json: JString<'local>,
) -> jobject {
    let file_path = match get_string(&mut env, &file_path) {
        Ok(value) => value,
        Err(message) => return throw(&mut env, message),
    };
    let options_json = match get_string(&mut env, &options_json) {
        Ok(value) => value,
        Err(message) => return throw(&mut env, message),
    };

    let result = guarded(|| {
        let options: RenderFrameOptionsIn = parse_json(&options_json)?;
        let (render_format, mime_type) = parse_render_output_format(options.format.as_deref())?;
        let (show_overlays, overlay_index, overlay_color) =
            overlay_pipeline_fields(options.show_overlays, options.overlay_index, options.overlay_color.as_deref())?;

        let object = read_dicom_file(PathBuf::from(&file_path)).map_err(|error| error.to_string())?;
        let pipeline_options = DcmRenderPipelineOptions {
            frame_index: options.frame_index.unwrap_or(0) as usize,
            window_center: options.window_center,
            window_width: options.window_width,
            output_width: options.output_width,
            output_height: options.output_height,
            jpeg_quality: options.jpeg_quality.unwrap_or(90) as u8,
            show_overlays,
            overlay_index,
            overlay_color,
            ..Default::default()
        };
        let rendered = dcm_render_dicom_frame(&object, render_format, &pipeline_options).map_err(|error| error.to_string())?;
        let metadata_json = render_pipeline_options_json(&rendered, mime_type);
        Ok((metadata_json, rendered.bytes))
    });

    match result {
        Ok((metadata_json, bytes)) => match new_rendered_frame(&mut env, &metadata_json, &bytes) {
            Ok(object) => object,
            Err(message) => throw(&mut env, message),
        },
        Err(message) => throw(&mut env, message),
    }
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct RenderMovieOptionsIn {
    output_width: Option<u32>,
    output_height: Option<u32>,
    window_center: Option<f64>,
    window_width: Option<f64>,
    fps: Option<f64>,
}

/// Renders every frame of a multi-frame DICOM file to an MP4 (via a piped `ffmpeg` subprocess -
/// requires `ffmpeg` on `PATH`). Mirrors `dcmnorm --render-fps ... file.dcm out.mp4`. Does not
/// currently support the overlay options `renderFrame` does.
#[no_mangle]
pub extern "system" fn Java_com_pohcee_dcmnorm_Dcmnorm_nativeRenderMovie<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    file_path: JString<'local>,
    options_json: JString<'local>,
) -> jobject {
    let file_path = match get_string(&mut env, &file_path) {
        Ok(value) => value,
        Err(message) => return throw(&mut env, message),
    };
    let options_json = match get_string(&mut env, &options_json) {
        Ok(value) => value,
        Err(message) => return throw(&mut env, message),
    };

    let result = guarded(|| {
        let options: RenderMovieOptionsIn = parse_json(&options_json)?;
        let object = read_dicom_file(PathBuf::from(&file_path)).map_err(|error| error.to_string())?;
        let pipeline_options = DcmRenderPipelineOptions {
            window_center: options.window_center,
            window_width: options.window_width,
            output_width: options.output_width,
            output_height: options.output_height,
            ..Default::default()
        };
        let fps = options.fps.unwrap_or(24.0);

        // write_dicom_video needs a real (seekable) output file for ffmpeg's muxer - render to a
        // private temp file, then read it back, same trade-off the Node/Python bindings make.
        let temp_path = unique_temp_path("mp4");
        let render_result = dcm_write_dicom_video(&object, &temp_path, &pipeline_options, fps).map_err(|error| error.to_string());
        let bytes = render_result.and_then(|_| std::fs::read(&temp_path).map_err(|error| error.to_string()));
        let _ = std::fs::remove_file(&temp_path);
        bytes
    });

    match result {
        Ok(bytes) => match new_rendered_movie(&mut env, "video/mp4", &bytes) {
            Ok(object) => object,
            Err(message) => throw(&mut env, message),
        },
        Err(message) => throw(&mut env, message),
    }
}

// -------------------------------------------------------------------------------------------
// MPR (Multiplanar Reformation): buildVolume / DicomVolumeHandle
// -------------------------------------------------------------------------------------------
//
// buildVolume is the expensive step (reads + decodes every slice in a series), so it's exposed
// as an opaque native handle (a leaked `Arc<Volume>` pointer, boxed as a jlong) rather than a
// Java object owning Rust-managed memory directly - the caller builds a volume once per series
// and keeps `DicomVolumeHandle` resident, then calls `nativeFreeVolume` (via `close()`/
// `AutoCloseable`) exactly once when done. Unlike napi-rs's `Env::wrap`/PyO3's `#[pyclass]` (both
// of which tie the Rust value's lifetime to their own GC/refcounting), the JVM has no destructor
// hook a native library can rely on running promptly - see this crate's README for why
// `DicomVolumeHandle` must be used in a try-with-resources block rather than left to finalization.

/// Builds a 3D volume from a parallel stack of DICOM slice files (e.g. every image instance in
/// one CT/MR/PT series) sharing consistent `ImageOrientationPatient`. Slices are spatially
/// re-sorted internally by `ImagePositionPatient`, regardless of input order. Raises (rather than
/// silently mis-rendering) for fewer than 2 files, mismatched Rows/Columns, or a
/// non-parallel/gantry-tilt-inconsistent stack.
#[no_mangle]
pub extern "system" fn Java_com_pohcee_dcmnorm_Dcmnorm_nativeBuildVolume<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    file_paths_json: JString<'local>,
) -> jlong {
    let file_paths_json = match get_string(&mut env, &file_paths_json) {
        Ok(value) => value,
        Err(message) => return throw(&mut env, message),
    };

    let result = guarded(|| {
        let paths: Vec<String> = serde_json::from_str(&file_paths_json).map_err(|error| error.to_string())?;
        let paths: Vec<PathBuf> = paths.into_iter().map(PathBuf::from).collect();
        dcm_build_volume(&paths).map_err(|error| error.to_string())
    });

    match result {
        Ok(volume) => Arc::into_raw(Arc::new(volume)) as jlong,
        Err(message) => throw(&mut env, message),
    }
}

/// Reconstructs a (refcount-bumped) `Arc<Volume>` from a handle previously returned by
/// `nativeBuildVolume`. The returned `Arc` is just a borrowed-for-this-call view - letting it drop
/// at the end of the calling function decrements the count right back down; it never frees the
/// volume the Java-side handle still owns. Callers must not pass a handle that `nativeFreeVolume`
/// has already consumed - `DicomVolumeHandle` enforces this Java-side (see `checkOpen()`).
unsafe fn volume_from_handle(handle: jlong) -> Arc<DcmVolume> {
    let ptr = handle as *const DcmVolume;
    Arc::increment_strong_count(ptr);
    Arc::from_raw(ptr)
}

#[no_mangle]
pub extern "system" fn Java_com_pohcee_dcmnorm_Dcmnorm_nativeFreeVolume<'local>(
    _env: JNIEnv<'local>,
    _class: JClass<'local>,
    handle: jlong,
) {
    if handle != 0 {
        unsafe {
            drop(Arc::from_raw(handle as *const DcmVolume));
        }
    }
}

#[no_mangle]
pub extern "system" fn Java_com_pohcee_dcmnorm_Dcmnorm_nativeVolumeRows<'local>(
    _env: JNIEnv<'local>,
    _class: JClass<'local>,
    handle: jlong,
) -> jint {
    let volume = unsafe { volume_from_handle(handle) };
    volume.rows as jint
}

#[no_mangle]
pub extern "system" fn Java_com_pohcee_dcmnorm_Dcmnorm_nativeVolumeCols<'local>(
    _env: JNIEnv<'local>,
    _class: JClass<'local>,
    handle: jlong,
) -> jint {
    let volume = unsafe { volume_from_handle(handle) };
    volume.cols as jint
}

#[no_mangle]
pub extern "system" fn Java_com_pohcee_dcmnorm_Dcmnorm_nativeVolumeNumSlices<'local>(
    _env: JNIEnv<'local>,
    _class: JClass<'local>,
    handle: jlong,
) -> jint {
    let volume = unsafe { volume_from_handle(handle) };
    volume.num_slices as jint
}

fn new_double_array(env: &mut JNIEnv, values: &[f64]) -> Result<jdoubleArray, String> {
    let array = env.new_double_array(values.len() as i32).map_err(|error| error.to_string())?;
    env.set_double_array_region(&array, 0, values).map_err(|error| error.to_string())?;
    Ok(array.into_raw())
}

/// The volume's own acquisition-native orientation, for seeding an "axial" reformat -
/// `[rowDir(3), colDir(3)]`, 6 elements.
#[no_mangle]
pub extern "system" fn Java_com_pohcee_dcmnorm_Dcmnorm_nativeVolumeNativeBasis<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    handle: jlong,
) -> jdoubleArray {
    let volume = unsafe { volume_from_handle(handle) };
    let mut basis = Vec::with_capacity(6);
    basis.extend_from_slice(&volume.row_vector);
    basis.extend_from_slice(&volume.col_vector);
    match new_double_array(&mut env, &basis) {
        Ok(array) => array,
        Err(message) => throw_runtime(&mut env, message),
    }
}

/// The volume's own physical center, in patient/LPS mm (`[x, y, z]`) - a reasonable default
/// reformat origin.
#[no_mangle]
pub extern "system" fn Java_com_pohcee_dcmnorm_Dcmnorm_nativeVolumeCenter<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    handle: jlong,
) -> jdoubleArray {
    let volume = unsafe { volume_from_handle(handle) };
    match new_double_array(&mut env, &volume.center()) {
        Ok(array) => array,
        Err(message) => throw_runtime(&mut env, message),
    }
}

#[no_mangle]
pub extern "system" fn Java_com_pohcee_dcmnorm_Dcmnorm_nativeVolumeMinSpacingMm<'local>(
    _env: JNIEnv<'local>,
    _class: JClass<'local>,
    handle: jlong,
) -> jdouble {
    let volume = unsafe { volume_from_handle(handle) };
    volume.min_spacing_mm()
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ReformatParamsIn {
    origin: [f64; 3],
    row_dir: [f64; 3],
    col_dir: [f64; 3],
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
}

/// Resamples one plane through this volume and encodes it exactly like a normal 2D render (the
/// same `RenderedFrame` shape `Dcmnorm.renderFrame` returns), so callers can reuse their existing
/// image-display code path unchanged. `interpolation` defaults to `"trilinear"`; use `"nearest"`
/// (faster) for a live-drag preview frame. `slabThicknessMm` (default 0, an infinitely-thin
/// plane) turns on a thick-slab reformat centered on `origin`, combined per `slabProjection`
/// (default `"mip"`).
#[no_mangle]
pub extern "system" fn Java_com_pohcee_dcmnorm_Dcmnorm_nativeVolumeReformat<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    handle: jlong,
    params_json: JString<'local>,
) -> jobject {
    let params_json = match get_string(&mut env, &params_json) {
        Ok(value) => value,
        Err(message) => return throw(&mut env, message),
    };
    let volume = unsafe { volume_from_handle(handle) };

    let result = guarded(|| {
        let params: ReformatParamsIn = serde_json::from_str(&params_json).map_err(|error| error.to_string())?;
        let (render_format, mime_type) = parse_render_output_format(params.format.as_deref())?;
        let plane_params = DcmPlaneParams {
            origin: params.origin,
            row_dir: params.row_dir,
            col_dir: params.col_dir,
            output_width: params.output_width,
            output_height: params.output_height,
            spacing_mm: params.spacing_mm,
            window_center: params.window_center,
            window_width: params.window_width,
            interpolation: parse_interpolation(params.interpolation.as_deref())?,
            slab_thickness_mm: params.slab_thickness_mm.unwrap_or(0.0),
            slab_projection: parse_slab_projection(params.slab_projection.as_deref())?,
        };
        let jpeg_quality = params.jpeg_quality.unwrap_or(90) as u8;

        let rendered =
            dcm_reformat_plane(&volume, &plane_params, render_format, jpeg_quality).map_err(|error| error.to_string())?;
        let metadata_json = serde_json::json!({
            "mimeType": mime_type,
            "width": rendered.width,
            "height": rendered.height,
            "overlays": [],
            "selectedOverlayIndex": null,
        })
        .to_string();
        Ok((metadata_json, rendered.bytes))
    });

    match result {
        Ok((metadata_json, bytes)) => match new_rendered_frame(&mut env, &metadata_json, &bytes) {
            Ok(object) => object,
            Err(message) => throw(&mut env, message),
        },
        Err(message) => throw(&mut env, message),
    }
}

// -------------------------------------------------------------------------------------------
// Texture export: DicomVolumeHandle.exportTexture / Dcmnorm.exportFrameTexture /
// Dcmnorm.exportFrameStackTexture
// -------------------------------------------------------------------------------------------

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct TextureExportOptionsIn {
    frame_index: Option<u32>,
    target_max_dim: Option<u32>,
    compression: Option<String>,
    window_center: Option<f64>,
    window_width: Option<f64>,
}

/// Packs this volume's own NATIVE voxel lattice - not a resampled oblique plane, see
/// `Dcmnorm.exportFrameTexture`'s own doc - as a lossless GPU-upload-ready texture payload
/// (16-bit samples, row-major, optionally gzip-compressed). `targetMaxDim` caps the longest of
/// width/height/depth, proportionally downsampling if the native volume exceeds it.
#[no_mangle]
pub extern "system" fn Java_com_pohcee_dcmnorm_Dcmnorm_nativeVolumeExportTexture<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    handle: jlong,
    options_json: JString<'local>,
) -> jobject {
    let options_json = match get_string(&mut env, &options_json) {
        Ok(value) => value,
        Err(message) => return throw(&mut env, message),
    };
    let volume = unsafe { volume_from_handle(handle) };

    let result = guarded(|| {
        let options: TextureExportOptionsIn = parse_json(&options_json)?;
        let compression = parse_texture_compression(options.compression.as_deref())?;
        let default_window = match (options.window_center, options.window_width) {
            (Some(center), Some(width)) => Some((center, width)),
            _ => None,
        };
        let packed = dcm_pack_volume_texture(&volume, options.target_max_dim, default_window, compression)
            .map_err(|error| error.to_string())?;
        let meta_json = serde_json::to_string(&packed.meta.to_json()).map_err(|error| error.to_string())?;
        Ok((meta_json, packed.payload))
    });

    match result {
        Ok((meta_json, payload)) => match new_texture_export_result(&mut env, &meta_json, &payload) {
            Ok(object) => object,
            Err(message) => throw(&mut env, message),
        },
        Err(message) => throw(&mut env, message),
    }
}

/// Packs a single frame's raw (unwindowed) physical values as a depth-1 "1-slice volume"
/// texture, letting a large diagnostic 2D image (e.g. DX/CR/mammography) reuse the exact same
/// client GPU texture/shader pipeline as an MPR volume. Mirrors `dcmnorm --render-frame ...
/// --output-type texture file.dcm out.gputex`. `frameIndex` defaults to 0.
#[no_mangle]
pub extern "system" fn Java_com_pohcee_dcmnorm_Dcmnorm_nativeExportFrameTexture<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    file_path: JString<'local>,
    options_json: JString<'local>,
) -> jobject {
    let file_path = match get_string(&mut env, &file_path) {
        Ok(value) => value,
        Err(message) => return throw(&mut env, message),
    };
    let options_json = match get_string(&mut env, &options_json) {
        Ok(value) => value,
        Err(message) => return throw(&mut env, message),
    };

    let result = guarded(|| {
        let options: TextureExportOptionsIn = parse_json(&options_json)?;
        let compression = parse_texture_compression(options.compression.as_deref())?;
        let default_window = match (options.window_center, options.window_width) {
            (Some(center), Some(width)) => Some((center, width)),
            _ => None,
        };

        let object = read_dicom_file(PathBuf::from(&file_path)).map_err(|error| error.to_string())?;
        let packed = dcm_pack_dicom_frame_texture(
            &object,
            options.frame_index.unwrap_or(0) as usize,
            options.target_max_dim,
            default_window,
            compression,
        )
        .map_err(|error| error.to_string())?;
        let meta_json = serde_json::to_string(&packed.meta.to_json()).map_err(|error| error.to_string())?;
        Ok((meta_json, packed.payload))
    });

    match result {
        Ok((meta_json, payload)) => match new_texture_export_result(&mut env, &meta_json, &payload) {
            Ok(object) => object,
            Err(message) => throw(&mut env, message),
        },
        Err(message) => throw(&mut env, message),
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct FrameStackSourceIn {
    file_path: String,
    frame_indices: Option<Vec<u32>>,
}

/// Packs several independent frames (from one or more files) as one texture-array upload.
/// `sources_json` is a JSON array of `{"filePath", "frameIndices"}` - one entry per source FILE,
/// not per frame: a cine/multiframe instance supplies one source with several `frameIndices`
/// (its file is parsed once), while a multi-image series supplies one source per instance file
/// (`frameIndices` defaulting to `[0]`). The result's layer order is the flattened source order
/// followed by each source's own `frameIndices` order.
#[no_mangle]
pub extern "system" fn Java_com_pohcee_dcmnorm_Dcmnorm_nativeExportFrameStackTexture<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    sources_json: JString<'local>,
    options_json: JString<'local>,
) -> jobject {
    let sources_json = match get_string(&mut env, &sources_json) {
        Ok(value) => value,
        Err(message) => return throw(&mut env, message),
    };
    let options_json = match get_string(&mut env, &options_json) {
        Ok(value) => value,
        Err(message) => return throw(&mut env, message),
    };

    let result = guarded(|| {
        let sources: Vec<FrameStackSourceIn> = serde_json::from_str(&sources_json).map_err(|error| error.to_string())?;
        let options: TextureExportOptionsIn = parse_json(&options_json)?;
        let compression = parse_texture_compression(options.compression.as_deref())?;
        let default_window = match (options.window_center, options.window_width) {
            (Some(center), Some(width)) => Some((center, width)),
            _ => None,
        };

        // Read each distinct source file exactly once, regardless of how many frame indices it
        // contributes - avoids re-parsing the same multi-MB file per frame for a cine loop.
        let objects = sources
            .iter()
            .map(|source| read_dicom_file(PathBuf::from(&source.file_path)).map_err(|error| error.to_string()))
            .collect::<Result<Vec<_>, _>>()?;

        let mut frame_refs: Vec<(&dcmnorm_object::DefaultDicomObject, usize)> = Vec::new();
        for (source, object) in sources.iter().zip(objects.iter()) {
            let indices = source.frame_indices.clone().unwrap_or_else(|| vec![0]);
            for index in indices {
                frame_refs.push((object, index as usize));
            }
        }

        let packed = dcm_pack_dicom_frame_stack_texture(&frame_refs, default_window, compression).map_err(|error| error.to_string())?;
        let meta_json = serde_json::to_string(&packed.meta.to_json()).map_err(|error| error.to_string())?;
        Ok((meta_json, packed.payload))
    });

    match result {
        Ok((meta_json, payload)) => match new_texture_export_result(&mut env, &meta_json, &payload) {
            Ok(object) => object,
            Err(message) => throw(&mut env, message),
        },
        Err(message) => throw(&mut env, message),
    }
}
