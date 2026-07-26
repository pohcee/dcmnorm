//! Native DICOM Upper Layer / DIMSE service class user (SCU) operations: C-ECHO, C-STORE,
//! C-FIND, and C-MOVE. Ports the same request/response shapes as the dcmtk CLIs
//! (`echoscu`/`dcmsend`/`findscu`/`movescu`) this replaces, but as library calls that return
//! structured results instead of printing/exiting - callers decide what counts as a failure.
//!
//! `dicom-ul` (the DICOM Upper Layer/association crate used here) is purely transport-layer: it
//! knows about PDUs and presentation-context negotiation, nothing about DIMSE command semantics.
//! Every operation below hand-builds its own command dataset via
//! `InMemDicomObject::command_from_element_iter` + `dicom_dictionary_std::tags`, the same way
//! dcmtk/dicom-rs's example CLIs do - there is no higher-level DIMSE helper anywhere in this
//! dependency stack, including for C-MOVE (which has no reference implementation to port from;
//! its command dataset and response loop are built directly from PS3.7).
//!
//! `dicom-ul` 0.9.1's `ClientAssociation` does not release on `Drop` (older versions did) - every
//! function here explicitly calls `.release()` on success and `.abort()` on error paths.

use std::collections::HashMap;
use std::error::Error as StdError;
use std::fmt;
use std::io::Read;
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

use dicom_core::dictionary::{DataDictionary, DataDictionaryEntry};
use dicom_core::{dicom_value, DataElement, VR};
use dicom_dictionary_std::{tags, uids, StandardDataDictionary};
use dicom_encoding::{TransferSyntax, TransferSyntaxIndex};
use dicom_object::mem::InMemDicomObject;
use dicom_transfer_syntax_registry::TransferSyntaxRegistry;
use dicom_ul::{
    association::{client::ClientAssociationOptions, Error as AssociationError},
    pdu::{PDataValue, PDataValueType, Pdu, PresentationContextNegotiated},
    ClientAssociation,
};

use super::io::{read_dicom_file, transcode_dicom_object};
use super::json::write_dataset_as_dicom_json_with_options;
use super::types::{
    DicomJsonError, DicomJsonFormat, DicomJsonKeyStyle, DicomJsonWriteOptions, ReadError,
    TranscodeError, WriteError,
};

#[derive(Debug)]
pub enum DimseError {
    Association(AssociationError),
    Read(ReadError),
    Write(WriteError),
    Transcode(TranscodeError),
    Json(DicomJsonError),
    Io(std::io::Error),
    UnknownTransferSyntax(String),
    NoAcceptedPresentationContext,
    NoAcceptablePresentationContext { sop_class_uid: String, transfer_syntax_uid: String },
    NoFilesToSend,
    InvalidQueryKey(String),
    /// Something the peer sent didn't have the shape a well-behaved SCP response should have
    /// (missing Status, malformed PDU sequence, etc.) - not one of the specific, expected error
    /// paths above.
    Protocol(String),
}

impl fmt::Display for DimseError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Association(error) => write!(formatter, "DICOM association error: {error}"),
            Self::Read(error) => write!(formatter, "failed to read DICOM data: {error}"),
            Self::Write(error) => write!(formatter, "failed to write DICOM data: {error}"),
            Self::Transcode(error) => write!(formatter, "failed to transcode DICOM data: {error}"),
            Self::Json(error) => write!(formatter, "failed to build DICOM JSON: {error}"),
            Self::Io(error) => write!(formatter, "I/O error: {error}"),
            Self::UnknownTransferSyntax(uid) => write!(formatter, "unknown transfer syntax: {uid}"),
            Self::NoAcceptedPresentationContext => {
                write!(formatter, "peer did not accept any proposed presentation context")
            }
            Self::NoAcceptablePresentationContext { sop_class_uid, transfer_syntax_uid } => write!(
                formatter,
                "no accepted presentation context for SOP class {sop_class_uid} with transfer syntax {transfer_syntax_uid}"
            ),
            Self::NoFilesToSend => write!(formatter, "no valid DICOM files to send"),
            Self::InvalidQueryKey(key) => write!(formatter, "invalid query key '{key}'"),
            Self::Protocol(message) => write!(formatter, "DIMSE protocol error: {message}"),
        }
    }
}

impl StdError for DimseError {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        match self {
            Self::Association(error) => Some(error),
            Self::Read(error) => Some(error),
            Self::Write(error) => Some(error),
            Self::Transcode(error) => Some(error),
            Self::Json(error) => Some(error),
            Self::Io(error) => Some(error),
            Self::UnknownTransferSyntax(_)
            | Self::NoAcceptedPresentationContext
            | Self::NoAcceptablePresentationContext { .. }
            | Self::NoFilesToSend
            | Self::InvalidQueryKey(_)
            | Self::Protocol(_) => None,
        }
    }
}

impl From<AssociationError> for DimseError {
    fn from(value: AssociationError) -> Self {
        Self::Association(value)
    }
}

impl From<ReadError> for DimseError {
    fn from(value: ReadError) -> Self {
        Self::Read(value)
    }
}

impl From<WriteError> for DimseError {
    fn from(value: WriteError) -> Self {
        Self::Write(value)
    }
}

impl From<TranscodeError> for DimseError {
    fn from(value: TranscodeError) -> Self {
        Self::Transcode(value)
    }
}

impl From<DicomJsonError> for DimseError {
    fn from(value: DicomJsonError) -> Self {
        Self::Json(value)
    }
}

impl From<std::io::Error> for DimseError {
    fn from(value: std::io::Error) -> Self {
        Self::Io(value)
    }
}

pub(super) fn transfer_syntax(uid: &str) -> Result<&'static TransferSyntax, DimseError> {
    TransferSyntaxRegistry
        .get(uid)
        .ok_or_else(|| DimseError::UnknownTransferSyntax(uid.to_owned()))
}

pub(super) fn implicit_vr_le() -> &'static TransferSyntax {
    transfer_syntax(uids::IMPLICIT_VR_LITTLE_ENDIAN).expect("Implicit VR Little Endian is always registered")
}

fn default_transfer_syntaxes() -> Vec<String> {
    vec![
        uids::EXPLICIT_VR_LITTLE_ENDIAN.to_owned(),
        uids::IMPLICIT_VR_LITTLE_ENDIAN.to_owned(),
    ]
}

/// Establishes an association proposing the given (abstract syntax, transfer syntaxes) pairs,
/// each as its own presentation context - mirrors how `store_scu` needs one context per (SOP
/// class, transfer syntax) pair it might send, while `echo_scu`/`find_scu`/`move_scu` each pass
/// a single pair.
fn establish(
    destination: &str,
    calling_ae_title: &str,
    called_ae_title: Option<&str>,
    max_pdu_length: u32,
    presentation_contexts: &[(String, Vec<String>)],
    timeout: Option<Duration>,
) -> Result<ClientAssociation<TcpStream>, DimseError> {
    let mut options = ClientAssociationOptions::new()
        .calling_ae_title(calling_ae_title.to_owned())
        .max_pdu_length(max_pdu_length);

    for (abstract_syntax, transfer_syntaxes) in presentation_contexts {
        options = options.with_presentation_context(abstract_syntax.clone(), transfer_syntaxes.clone());
    }

    if let Some(called_ae_title) = called_ae_title {
        options = options.called_ae_title(called_ae_title.to_owned());
    }

    if let Some(timeout) = timeout {
        options = options.read_timeout(timeout).write_timeout(timeout).connection_timeout(timeout);
    }

    Ok(options.establish_with(destination)?)
}

fn send_command(
    assoc: &mut ClientAssociation<TcpStream>,
    pc_id: u8,
    command: &InMemDicomObject,
) -> Result<(), DimseError> {
    let mut data = Vec::with_capacity(128);
    command.write_dataset_with_ts(&mut data, implicit_vr_le())?;
    assoc.send(&Pdu::PData {
        data: vec![PDataValue {
            presentation_context_id: pc_id,
            value_type: PDataValueType::Command,
            is_last: true,
            data,
        }],
    })?;
    Ok(())
}

fn send_data(assoc: &mut ClientAssociation<TcpStream>, pc_id: u8, data: Vec<u8>) -> Result<(), DimseError> {
    assoc.send(&Pdu::PData {
        data: vec![PDataValue {
            presentation_context_id: pc_id,
            value_type: PDataValueType::Data,
            is_last: true,
            data,
        }],
    })?;
    Ok(())
}

fn receive_command(assoc: &mut ClientAssociation<TcpStream>) -> Result<InMemDicomObject, DimseError> {
    match assoc.receive()? {
        Pdu::PData { data } => {
            let value = data
                .first()
                .ok_or_else(|| DimseError::Protocol("empty P-Data response".to_owned()))?;
            Ok(InMemDicomObject::read_dataset_with_ts(value.data.as_slice(), implicit_vr_le())?)
        }
        other => Err(DimseError::Protocol(format!("unexpected response PDU: {other:?}"))),
    }
}

fn read_pdata_to_end(assoc: &mut ClientAssociation<TcpStream>) -> Result<Vec<u8>, DimseError> {
    let mut reader = assoc.receive_pdata();
    let mut buffer = Vec::new();
    reader.read_to_end(&mut buffer)?;
    Ok(buffer)
}

fn command_status(command: &InMemDicomObject) -> Result<u16, DimseError> {
    command
        .element(tags::STATUS)
        .map_err(|error| DimseError::Protocol(format!("response is missing Status: {error}")))?
        .to_int::<u16>()
        .map_err(|error| DimseError::Protocol(format!("Status is not a valid integer: {error}")))
}

fn command_u16(command: &InMemDicomObject, tag: dicom_core::Tag) -> u16 {
    command.element(tag).ok().and_then(|element| element.to_int::<u16>().ok()).unwrap_or(0)
}

/// `0xFF00` ("pending, matches follow") and `0xFF01` ("pending, optional keys not supported")
/// are the two DICOM-defined pending statuses for C-FIND/C-MOVE responses - see PS3.7 C.4.
fn is_pending_status(status: u16) -> bool {
    status == 0xFF00 || status == 0xFF01
}

// ---------------------------------------------------------------------------------------------
// C-ECHO
// ---------------------------------------------------------------------------------------------

pub struct EchoScuOptions {
    pub calling_ae_title: String,
    pub called_ae_title: Option<String>,
    pub timeout: Option<Duration>,
}

/// Performs a C-ECHO (DICOM Verification) against `destination` ("host:port"). Returns the
/// response Status code (0 = success) - callers decide what threshold counts as "up".
pub fn echo_scu(destination: &str, options: EchoScuOptions) -> Result<u16, DimseError> {
    let mut assoc = establish(
        destination,
        &options.calling_ae_title,
        options.called_ae_title.as_deref(),
        dicom_ul::pdu::DEFAULT_MAX_PDU,
        &[(uids::VERIFICATION.to_owned(), default_transfer_syntaxes())],
        options.timeout,
    )?;

    let pc = assoc
        .presentation_contexts()
        .first()
        .cloned()
        .ok_or(DimseError::NoAcceptedPresentationContext)?;

    let command = InMemDicomObject::command_from_element_iter([
        DataElement::new(tags::AFFECTED_SOP_CLASS_UID, VR::UI, dicom_value!(Str, uids::VERIFICATION)),
        DataElement::new(tags::COMMAND_FIELD, VR::US, dicom_value!(U16, [0x0030])),
        DataElement::new(tags::MESSAGE_ID, VR::US, dicom_value!(U16, [1])),
        DataElement::new(tags::COMMAND_DATA_SET_TYPE, VR::US, dicom_value!(U16, [0x0101])),
    ]);

    let result = send_command(&mut assoc, pc.id, &command).and_then(|_| receive_command(&mut assoc));

    let response = match result {
        Ok(response) => response,
        Err(error) => {
            let _ = assoc.abort();
            return Err(error);
        }
    };

    let status = command_status(&response)?;
    assoc.release()?;
    Ok(status)
}

// ---------------------------------------------------------------------------------------------
// C-STORE
// ---------------------------------------------------------------------------------------------

pub struct StoreScuOptions {
    pub calling_ae_title: String,
    pub called_ae_title: Option<String>,
    pub max_pdu_length: u32,
    /// When true, only the file's own transfer syntax is proposed - a peer that doesn't support
    /// it fails that file rather than receiving a transcoded copy.
    pub never_transcode: bool,
}

#[derive(Clone, Debug)]
pub struct StoreScuResult {
    pub sop_instance_uid: String,
    pub status: u16,
}

struct StoreFile {
    path: PathBuf,
    sop_class_uid: String,
    sop_instance_uid: String,
    transfer_syntax_uid: String,
}

fn probe_store_file(path: &Path) -> Result<StoreFile, DimseError> {
    let object = read_dicom_file(path)?;
    let meta = object.meta();
    Ok(StoreFile {
        path: path.to_owned(),
        sop_class_uid: meta.media_storage_sop_class_uid.trim_end_matches(['\0', ' ']).to_owned(),
        sop_instance_uid: meta.media_storage_sop_instance_uid.trim_end_matches(['\0', ' ']).to_owned(),
        transfer_syntax_uid: meta.transfer_syntax.trim_end_matches(['\0', ' ']).to_owned(),
    })
}

/// Sends each of `files` via C-STORE to `destination` ("host:port"). Presentation contexts
/// propose each file's own SOP class + transfer syntax, plus (unless `never_transcode`) the same
/// SOP class under Explicit/Implicit VR Little Endian as a transcoding fallback - mirrors the
/// storescu CLI this replaces, except the fallback here reuses dcmnorm's own
/// [`transcode_dicom_object`], which (via openjpeg/kakadu/jpeg-ls/ffmpeg) can transcode a
/// strictly wider set of source transfer syntaxes than that CLI's uncompressed-only fallback did.
///
/// Returns one [`StoreScuResult`] per file that could be probed and sent; only association-level
/// failure (can't connect, no acceptable presentation context at all) is a hard `Err` - a
/// per-file non-success Status is just data in the result, not a thrown error, since the caller
/// is in a better position to decide which statuses are retryable/fatal.
pub fn store_scu(
    destination: &str,
    files: &[PathBuf],
    options: StoreScuOptions,
) -> Result<Vec<StoreScuResult>, DimseError> {
    let probed: Vec<StoreFile> = files.iter().filter_map(|path| probe_store_file(path).ok()).collect();
    if probed.is_empty() {
        return Err(DimseError::NoFilesToSend);
    }

    let mut presentation_contexts: Vec<(String, Vec<String>)> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for file in &probed {
        let mut transfer_syntaxes = vec![file.transfer_syntax_uid.clone()];
        if !options.never_transcode {
            for fallback in default_transfer_syntaxes() {
                if fallback != file.transfer_syntax_uid {
                    transfer_syntaxes.push(fallback);
                }
            }
        }
        if seen.insert(file.sop_class_uid.clone()) {
            presentation_contexts.push((file.sop_class_uid.clone(), transfer_syntaxes));
        }
    }

    let mut assoc = establish(
        destination,
        &options.calling_ae_title,
        options.called_ae_title.as_deref(),
        options.max_pdu_length,
        &presentation_contexts,
        None,
    )?;

    let negotiated = assoc.presentation_contexts().to_vec();

    let mut results = Vec::with_capacity(probed.len());
    for (index, file) in probed.iter().enumerate() {
        let message_id = (index + 1) as u16;
        match send_and_receive_store(&mut assoc, file, &negotiated, message_id) {
            Ok(status) => results.push(StoreScuResult { sop_instance_uid: file.sop_instance_uid.clone(), status }),
            Err(error) => {
                let _ = assoc.abort();
                return Err(error);
            }
        }
    }

    assoc.release()?;
    Ok(results)
}

fn select_store_presentation_context<'a>(
    file: &StoreFile,
    negotiated: &'a [PresentationContextNegotiated],
) -> Result<(&'a PresentationContextNegotiated, &'static TransferSyntax), DimseError> {
    let exact = negotiated
        .iter()
        .find(|pc| pc.abstract_syntax == file.sop_class_uid && pc.transfer_syntax == file.transfer_syntax_uid);
    if let Some(pc) = exact {
        return Ok((pc, transfer_syntax(&pc.transfer_syntax)?));
    }

    let fallback = negotiated.iter().find(|pc| pc.abstract_syntax == file.sop_class_uid);
    match fallback {
        Some(pc) => Ok((pc, transfer_syntax(&pc.transfer_syntax)?)),
        None => Err(DimseError::NoAcceptablePresentationContext {
            sop_class_uid: file.sop_class_uid.clone(),
            transfer_syntax_uid: file.transfer_syntax_uid.clone(),
        }),
    }
}

fn send_and_receive_store(
    assoc: &mut ClientAssociation<TcpStream>,
    file: &StoreFile,
    negotiated: &[PresentationContextNegotiated],
    message_id: u16,
) -> Result<u16, DimseError> {
    let (pc, target_ts) = select_store_presentation_context(file, negotiated)?;

    let object = read_dicom_file(&file.path)?;
    let object = if target_ts.uid() == file.transfer_syntax_uid {
        object
    } else {
        transcode_dicom_object(&object, target_ts.uid())?
    };

    let mut object_data = Vec::with_capacity(2048);
    object.write_dataset_with_ts(&mut object_data, target_ts)?;

    let command = InMemDicomObject::command_from_element_iter([
        DataElement::new(tags::AFFECTED_SOP_CLASS_UID, VR::UI, dicom_value!(Str, &file.sop_class_uid)),
        DataElement::new(tags::COMMAND_FIELD, VR::US, dicom_value!(U16, [0x0001])),
        DataElement::new(tags::MESSAGE_ID, VR::US, dicom_value!(U16, [message_id])),
        DataElement::new(tags::PRIORITY, VR::US, dicom_value!(U16, [0x0000])),
        DataElement::new(tags::COMMAND_DATA_SET_TYPE, VR::US, dicom_value!(U16, [0x0000])),
        DataElement::new(tags::AFFECTED_SOP_INSTANCE_UID, VR::UI, dicom_value!(Str, &file.sop_instance_uid)),
    ]);

    let mut command_data = Vec::with_capacity(128);
    command.write_dataset_with_ts(&mut command_data, implicit_vr_le())?;

    let combined_len = command_data.len() + object_data.len();
    if combined_len < assoc.acceptor_max_pdu_length().saturating_sub(100) as usize {
        assoc.send(&Pdu::PData {
            data: vec![
                PDataValue { presentation_context_id: pc.id, value_type: PDataValueType::Command, is_last: true, data: command_data },
                PDataValue { presentation_context_id: pc.id, value_type: PDataValueType::Data, is_last: true, data: object_data },
            ],
        })?;
    } else {
        assoc.send(&Pdu::PData {
            data: vec![PDataValue { presentation_context_id: pc.id, value_type: PDataValueType::Command, is_last: true, data: command_data }],
        })?;
        let mut writer = assoc.send_pdata(pc.id);
        std::io::Write::write_all(&mut writer, &object_data)?;
        drop(writer);
    }

    let response = receive_command(assoc)?;
    command_status(&response)
}

// ---------------------------------------------------------------------------------------------
// C-FIND
// ---------------------------------------------------------------------------------------------

pub struct FindScuOptions {
    pub calling_ae_title: String,
    pub called_ae_title: Option<String>,
    pub max_pdu_length: u32,
    pub timeout: Option<Duration>,
}

fn put_query_attribute(identifier: &mut InMemDicomObject, key: &str, value: &str) -> Result<(), DimseError> {
    let tag = StandardDataDictionary
        .parse_tag(key)
        .ok_or_else(|| DimseError::InvalidQueryKey(key.to_owned()))?;
    let vr = StandardDataDictionary
        .by_tag(tag)
        .map(|entry| entry.vr().relaxed())
        .ok_or_else(|| DimseError::InvalidQueryKey(key.to_owned()))?;
    identifier.put_str(tag, vr, value.to_owned());
    Ok(())
}

/// Builds a C-FIND/C-MOVE Identifier dataset from `query` (tag keyword/hex -> value; an empty
/// value is a universal-match "return key", mirroring findscu's bare `-k TAG` vs `-k TAG=value`).
/// `QueryRetrieveLevel` defaults to `"STUDY"` when not present in `query`.
fn build_identifier(query: &HashMap<String, String>) -> Result<InMemDicomObject, DimseError> {
    let mut identifier = InMemDicomObject::new_empty();
    for (key, value) in query {
        put_query_attribute(&mut identifier, key, value)?;
    }
    if identifier.element(tags::QUERY_RETRIEVE_LEVEL).is_err() {
        put_query_attribute(&mut identifier, "QueryRetrieveLevel", "STUDY")?;
    }
    Ok(identifier)
}

/// Performs a C-FIND (Study Root Query/Retrieve) against `destination`, returning each matched
/// Identifier as flat/hex-keyed DICOM JSON (same shape as [`super::read_json`]'s default).
pub fn find_scu(
    destination: &str,
    query: &HashMap<String, String>,
    options: FindScuOptions,
) -> Result<Vec<String>, DimseError> {
    let abstract_syntax = uids::STUDY_ROOT_QUERY_RETRIEVE_INFORMATION_MODEL_FIND;

    let mut assoc = establish(
        destination,
        &options.calling_ae_title,
        options.called_ae_title.as_deref(),
        options.max_pdu_length,
        &[(abstract_syntax.to_owned(), default_transfer_syntaxes())],
        options.timeout,
    )?;

    let pc = assoc
        .presentation_contexts()
        .first()
        .cloned()
        .ok_or(DimseError::NoAcceptedPresentationContext)?;
    let ts = transfer_syntax(&pc.transfer_syntax)?;

    let identifier = build_identifier(query)?;
    let mut identifier_data = Vec::with_capacity(128);
    identifier.write_dataset_with_ts(&mut identifier_data, ts)?;

    let command = InMemDicomObject::command_from_element_iter([
        DataElement::new(tags::AFFECTED_SOP_CLASS_UID, VR::UI, dicom_value!(Str, abstract_syntax)),
        DataElement::new(tags::COMMAND_FIELD, VR::US, dicom_value!(U16, [0x0020])),
        DataElement::new(tags::MESSAGE_ID, VR::US, dicom_value!(U16, [1])),
        DataElement::new(tags::PRIORITY, VR::US, dicom_value!(U16, [0x0000])),
        DataElement::new(tags::COMMAND_DATA_SET_TYPE, VR::US, dicom_value!(U16, [0x0001])),
    ]);

    if let Err(error) = send_command(&mut assoc, pc.id, &command).and_then(|_| send_data(&mut assoc, pc.id, identifier_data)) {
        let _ = assoc.abort();
        return Err(error);
    }

    let mut matches = Vec::new();
    loop {
        let response = match receive_command(&mut assoc) {
            Ok(response) => response,
            Err(error) => {
                let _ = assoc.abort();
                return Err(error);
            }
        };
        let status = match command_status(&response) {
            Ok(status) => status,
            Err(error) => {
                let _ = assoc.abort();
                return Err(error);
            }
        };

        if status == 0x0000 {
            break;
        }
        if !is_pending_status(status) {
            let _ = assoc.abort();
            return Err(DimseError::Protocol(format!("C-FIND-RSP failed with status {status:#06X}")));
        }

        let bytes = match read_pdata_to_end(&mut assoc) {
            Ok(bytes) => bytes,
            Err(error) => {
                let _ = assoc.abort();
                return Err(error);
            }
        };
        let matched = InMemDicomObject::read_dataset_with_ts(bytes.as_slice(), ts)?;
        let json = write_dataset_as_dicom_json_with_options(
            matched,
            ts.uid(),
            DicomJsonWriteOptions { key_style: DicomJsonKeyStyle::Hex, format: DicomJsonFormat::Flat, ..Default::default() },
        )?;
        matches.push(json);
    }

    assoc.release()?;
    Ok(matches)
}

// ---------------------------------------------------------------------------------------------
// C-MOVE
// ---------------------------------------------------------------------------------------------

pub struct MoveScuOptions {
    pub calling_ae_title: String,
    pub called_ae_title: Option<String>,
    pub max_pdu_length: u32,
    /// Per-PDU-wait read timeout - a slow-but-progressing move keeps resetting this on every
    /// pending C-MOVE-RSP, so this should be generous rather than a hard overall deadline.
    pub timeout: Option<Duration>,
}

#[derive(Clone, Debug, Default)]
pub struct MoveScuResult {
    pub status: u16,
    pub completed: u16,
    pub failed: u16,
    pub warning: u16,
    pub remaining: u16,
}

/// Performs a C-MOVE (Study Root Query/Retrieve), asking `destination` to push
/// `study_instance_uid` to `move_destination_ae` (an AE title `destination` already knows how to
/// reach - not a socket address). Blocks until the move reaches a terminal status; returns the
/// terminal [`MoveScuResult`] regardless of whether it was success/warning/failure so the caller
/// can decide what's retryable, mirroring [`store_scu`]'s "return data, don't decide policy" shape.
pub fn move_scu(
    destination: &str,
    move_destination_ae: &str,
    study_instance_uid: &str,
    options: MoveScuOptions,
) -> Result<MoveScuResult, DimseError> {
    let abstract_syntax = uids::STUDY_ROOT_QUERY_RETRIEVE_INFORMATION_MODEL_MOVE;

    let mut assoc = establish(
        destination,
        &options.calling_ae_title,
        options.called_ae_title.as_deref(),
        options.max_pdu_length,
        &[(abstract_syntax.to_owned(), default_transfer_syntaxes())],
        options.timeout,
    )?;

    let pc = assoc
        .presentation_contexts()
        .first()
        .cloned()
        .ok_or(DimseError::NoAcceptedPresentationContext)?;
    let ts = transfer_syntax(&pc.transfer_syntax)?;

    let mut identifier = InMemDicomObject::new_empty();
    put_query_attribute(&mut identifier, "QueryRetrieveLevel", "STUDY")?;
    put_query_attribute(&mut identifier, "StudyInstanceUID", study_instance_uid)?;
    let mut identifier_data = Vec::with_capacity(128);
    identifier.write_dataset_with_ts(&mut identifier_data, ts)?;

    let command = InMemDicomObject::command_from_element_iter([
        DataElement::new(tags::AFFECTED_SOP_CLASS_UID, VR::UI, dicom_value!(Str, abstract_syntax)),
        DataElement::new(tags::COMMAND_FIELD, VR::US, dicom_value!(U16, [0x0021])),
        DataElement::new(tags::MESSAGE_ID, VR::US, dicom_value!(U16, [1])),
        DataElement::new(tags::PRIORITY, VR::US, dicom_value!(U16, [0x0000])),
        DataElement::new(tags::MOVE_DESTINATION, VR::AE, dicom_value!(Str, move_destination_ae)),
        DataElement::new(tags::COMMAND_DATA_SET_TYPE, VR::US, dicom_value!(U16, [0x0001])),
    ]);

    if let Err(error) = send_command(&mut assoc, pc.id, &command).and_then(|_| send_data(&mut assoc, pc.id, identifier_data)) {
        let _ = assoc.abort();
        return Err(error);
    }

    loop {
        let response = match receive_command(&mut assoc) {
            Ok(response) => response,
            Err(error) => {
                let _ = assoc.abort();
                return Err(error);
            }
        };
        let status = match command_status(&response) {
            Ok(status) => status,
            Err(error) => {
                let _ = assoc.abort();
                return Err(error);
            }
        };

        let result = MoveScuResult {
            status,
            completed: command_u16(&response, tags::NUMBER_OF_COMPLETED_SUBOPERATIONS),
            failed: command_u16(&response, tags::NUMBER_OF_FAILED_SUBOPERATIONS),
            warning: command_u16(&response, tags::NUMBER_OF_WARNING_SUBOPERATIONS),
            remaining: command_u16(&response, tags::NUMBER_OF_REMAINING_SUBOPERATIONS),
        };

        if is_pending_status(status) {
            continue;
        }

        assoc.release()?;
        return Ok(result);
    }
}
