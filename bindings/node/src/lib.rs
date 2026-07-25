use std::collections::HashMap;
use std::panic::{self, AssertUnwindSafe};
use std::path::PathBuf;

use napi::bindgen_prelude::*;
use napi_derive::napi;

use dcmnorm::dicom_io::{
    apply_filter_to_object, parse_attribute_override, parse_filter_requests, parse_tag_key,
    probe_dicom_file_for_sop_class_uid, read_dicom_bytes, read_dicom_file,
    read_dicom_json_with_options, read_dicom_object_for_filter, remove_attribute,
    remove_private_tags_inplace, set_attribute, transcode_dicom_file, write_dicom_file,
    write_dicom_json_with_options, DicomJsonBulkDataMode, DicomJsonFormat, DicomJsonKeyStyle,
    DicomJsonReadOptions, DicomJsonWriteOptions,
};

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
                set_attribute(&mut object, tag, vr, value);
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
