//! Native DICOM Upper Layer / DIMSE service class **provider** (SCP): a listener accepting
//! inbound associations and answering C-ECHO, C-STORE, C-FIND, and C-MOVE requests. Handles
//! association negotiation and C-ECHO/C-STORE entirely in Rust; C-FIND and C-MOVE (which need
//! to query/queue against GENERICAE's own HTTP API) and "association complete" (which needs to
//! POST the studies just stored) are delegated to a caller-supplied [`ScpHandlers`]
//! implementation, kept deliberately free of any napi/JS awareness here - the node bindings
//! crate is what adapts a JS callback into this trait.
//!
//! One OS thread per accepted connection (matching the fully-synchronous style the rest of this
//! crate's DIMSE code already uses - see `dimse.rs`'s SCU functions and their mock-SCP tests).

use std::collections::HashMap;
use std::fmt;
use std::io::Read;
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use dicom_core::{dicom_value, DataElement, VR};
use dicom_dictionary_std::{tags, uids};
use dicom_encoding::TransferSyntax;
use dicom_object::mem::InMemDicomObject;
use dicom_object::FileMetaTableBuilder;
use dicom_ul::{
    association::server::ServerAssociationOptions,
    pdu::{PDataValue, PDataValueType, Pdu},
    ServerAssociation,
};

use super::dimse::{implicit_vr_le, transfer_syntax};
use super::io::write_dicom_file;

/// Business logic an embedding application supplies for the DIMSE operations that need it - the
/// SCP itself only owns protocol/association mechanics and (for C-STORE) writing the received
/// file to disk. Implementations must be safe to call concurrently from multiple connection
/// threads at once.
pub trait ScpHandlers: Send + Sync {
    /// A C-FIND request's Identifier, reduced to whichever of StudyInstanceUID/StudyDate/
    /// PatientName/PatientID were actually present (DICOM keyword keys). Returns matched
    /// studies, each a keyword-keyed field map for whichever of StudyInstanceUID/
    /// AccessionNumber/PatientID/PatientName/PatientBirthDate/StudyDate/StudyTime/
    /// StudyDescription/ModalitiesInStudy/NumberOfStudyRelatedSeries/
    /// NumberOfStudyRelatedInstances it has values for.
    fn on_find(&self, filter: &HashMap<String, String>) -> Result<Vec<HashMap<String, String>>, String>;

    /// Returns `Ok(true)` if the move was successfully queued - mirrors a "fire and forget"
    /// C-MOVE that delegates the actual transfer elsewhere and only reports whether it was
    /// accepted for processing, not whether the transfer itself later succeeds.
    fn on_move(&self, study_instance_uid: &str, move_destination_ae: &str) -> Result<bool, String>;

    /// Called once per association, when it ends (cleanly or otherwise), if any C-STORE
    /// happened during it - maps each StudyInstanceUID to the on-disk paths of every instance
    /// stored for it during this association.
    fn on_association_complete(&self, stored_instances_by_study: &HashMap<String, Vec<String>>);
}

#[derive(Clone, Debug)]
pub struct ScpOptions {
    pub ae_title: String,
    pub max_pdu_length: u32,
    /// Applied as both the read and write timeout on each connection's socket - matches
    /// dcmjs-dimse's `pduTimeout` option (how long an established association may sit idle
    /// between PDUs before being dropped).
    pub idle_timeout: Duration,
}

impl Default for ScpOptions {
    fn default() -> Self {
        Self {
            ae_title: "ANY-SCP".to_owned(),
            max_pdu_length: 16384,
            idle_timeout: Duration::from_secs(300),
        }
    }
}

#[derive(Debug)]
pub enum ScpError {
    Io(std::io::Error),
}

impl fmt::Display for ScpError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "I/O error: {error}"),
        }
    }
}

impl std::error::Error for ScpError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
        }
    }
}

impl From<std::io::Error> for ScpError {
    fn from(value: std::io::Error) -> Self {
        Self::Io(value)
    }
}

/// A running SCP. Dropping this without calling [`DicomScp::stop`] leaves the listener thread
/// running in the background (it is not tied to this handle's lifetime) - always call `stop()`
/// during shutdown.
pub struct DicomScp {
    stop_flag: Arc<AtomicBool>,
    local_addr: SocketAddr,
    accept_thread: Option<std::thread::JoinHandle<()>>,
}

impl DicomScp {
    pub fn local_port(&self) -> u16 {
        self.local_addr.port()
    }

    /// Stops accepting new associations and waits for the accept loop to exit. Associations
    /// already in progress on their own connection threads are not interrupted - each finishes
    /// (or times out via `idle_timeout`) independently.
    pub fn stop(mut self) {
        self.stop_flag.store(true, Ordering::SeqCst);
        // Unblock the listener thread's blocking accept() call with a throwaway local
        // connection - it's dropped immediately once the accept loop observes the stop flag.
        let _ = TcpStream::connect(("127.0.0.1", self.local_addr.port()));
        if let Some(handle) = self.accept_thread.take() {
            let _ = handle.join();
        }
    }
}

/// Starts listening on `port` (0 binds an ephemeral port - see [`DicomScp::local_port`]),
/// accepting DICOM associations and dispatching C-ECHO/C-STORE/C-FIND/C-MOVE requests.
/// C-STORE'd instances are written under `cache_path` as
/// `S_<StudyInstanceUID>/<Modality>_<SOPInstanceUID>.dcm`. Accepts every proposed presentation
/// context regardless of abstract syntax (`ServerAssociationOptions::promiscuous`), matching a
/// permissive "accept anything a real sender proposes" SCP rather than a curated allow-list.
pub fn start_scp(
    port: u16,
    cache_path: PathBuf,
    handlers: Arc<dyn ScpHandlers>,
    options: ScpOptions,
) -> Result<DicomScp, ScpError> {
    let listener = TcpListener::bind(("0.0.0.0", port))?;
    let local_addr = listener.local_addr()?;
    let stop_flag = Arc::new(AtomicBool::new(false));
    let stop_flag_thread = stop_flag.clone();

    let accept_thread = std::thread::spawn(move || loop {
        let stream = match listener.accept() {
            Ok((stream, _peer)) => stream,
            Err(_) => break,
        };
        if stop_flag_thread.load(Ordering::SeqCst) {
            break;
        }

        let handlers = handlers.clone();
        let cache_path = cache_path.clone();
        let options = options.clone();
        std::thread::spawn(move || {
            handle_association(stream, &cache_path, handlers.as_ref(), &options);
        });
    });

    Ok(DicomScp {
        stop_flag,
        local_addr,
        accept_thread: Some(accept_thread),
    })
}

fn handle_association(stream: TcpStream, cache_path: &Path, handlers: &dyn ScpHandlers, options: &ScpOptions) {
    let association_options = ServerAssociationOptions::new()
        .ae_title(options.ae_title.clone())
        .promiscuous(true)
        .max_pdu_length(options.max_pdu_length)
        .read_timeout(options.idle_timeout)
        .write_timeout(options.idle_timeout);

    let Ok(mut association) = association_options.establish(stream) else {
        return;
    };

    let mut stored_instances_by_study: HashMap<String, Vec<String>> = HashMap::new();

    loop {
        let pdu = match association.receive() {
            Ok(pdu) => pdu,
            Err(_) => break,
        };

        match pdu {
            Pdu::ReleaseRQ => {
                let _ = association.send(&Pdu::ReleaseRP);
                break;
            }
            Pdu::AbortRQ { .. } => break,
            Pdu::PData { data } => {
                let Some(command_pdv) = data.first() else {
                    break;
                };
                let pc_id = command_pdv.presentation_context_id;
                let Ok(command) =
                    InMemDicomObject::read_dataset_with_ts(command_pdv.data.as_slice(), implicit_vr_le())
                else {
                    break;
                };
                let Some(negotiated_ts) = negotiated_transfer_syntax(&association, pc_id) else {
                    break;
                };

                let outcome = match command_u16(&command, tags::COMMAND_FIELD) {
                    0x0030 => handle_echo(&mut association, pc_id, &command),
                    0x0001 => handle_store(
                        &mut association,
                        pc_id,
                        &command,
                        &data,
                        negotiated_ts,
                        cache_path,
                        &mut stored_instances_by_study,
                    ),
                    0x0020 => handle_find(&mut association, pc_id, &command, &data, negotiated_ts, handlers),
                    0x0021 => handle_move(&mut association, pc_id, &command, &data, negotiated_ts, handlers),
                    _ => Err(()),
                };
                if outcome.is_err() {
                    break;
                }
            }
            _ => break,
        }
    }

    if !stored_instances_by_study.is_empty() {
        handlers.on_association_complete(&stored_instances_by_study);
    }
}

fn negotiated_transfer_syntax(
    association: &ServerAssociation<TcpStream>,
    pc_id: u8,
) -> Option<&'static TransferSyntax> {
    let pc = association.presentation_contexts().iter().find(|pc| pc.id == pc_id)?;
    transfer_syntax(&pc.transfer_syntax).ok()
}

fn command_u16(command: &InMemDicomObject, tag: dicom_core::Tag) -> u16 {
    command.element(tag).ok().and_then(|element| element.to_int::<u16>().ok()).unwrap_or(0)
}

fn command_str(command: &InMemDicomObject, tag: dicom_core::Tag) -> Option<String> {
    command.element(tag).ok().and_then(|element| element.to_str().ok()).map(|value| value.trim().to_owned())
}

fn element_str(object: &InMemDicomObject, tag: dicom_core::Tag) -> Option<String> {
    object.element(tag).ok().and_then(|element| element.to_str().ok()).map(|value| value.trim().to_owned())
}

fn send_command(
    association: &mut ServerAssociation<TcpStream>,
    pc_id: u8,
    command: &InMemDicomObject,
) -> Result<(), ()> {
    let mut data = Vec::with_capacity(128);
    command.write_dataset_with_ts(&mut data, implicit_vr_le()).map_err(|_| ())?;
    association
        .send(&Pdu::PData {
            data: vec![PDataValue { presentation_context_id: pc_id, value_type: PDataValueType::Command, is_last: true, data }],
        })
        .map_err(|_| ())
}

fn send_data(association: &mut ServerAssociation<TcpStream>, pc_id: u8, data: Vec<u8>) -> Result<(), ()> {
    association
        .send(&Pdu::PData {
            data: vec![PDataValue { presentation_context_id: pc_id, value_type: PDataValueType::Data, is_last: true, data }],
        })
        .map_err(|_| ())
}

fn receive_data(association: &mut ServerAssociation<TcpStream>) -> Result<Vec<u8>, ()> {
    let mut reader = association.receive_pdata();
    let mut buffer = Vec::new();
    reader.read_to_end(&mut buffer).map_err(|_| ())?;
    Ok(buffer)
}

/// Resolves the data set that follows a command, tolerating either of the two ways a sender may
/// deliver it: bundled into the same `Pdu::PData` as the command (a small payload might combine
/// both PDataValues into one PDU - see `store_scu`'s combined-PDU path), or sent as its own
/// separate PDU(s) afterward (read here via `receive_pdata()`, which transparently reassembles
/// a data set split across multiple PDU fragments).
fn resolve_data(association: &mut ServerAssociation<TcpStream>, initial_pdata: &[PDataValue]) -> Result<Vec<u8>, ()> {
    match initial_pdata.get(1) {
        Some(data_pdv) => Ok(data_pdv.data.clone()),
        None => receive_data(association),
    }
}

fn handle_echo(association: &mut ServerAssociation<TcpStream>, pc_id: u8, command: &InMemDicomObject) -> Result<(), ()> {
    let message_id = command_u16(command, tags::MESSAGE_ID);
    let response = InMemDicomObject::command_from_element_iter([
        DataElement::new(tags::AFFECTED_SOP_CLASS_UID, VR::UI, dicom_value!(Str, uids::VERIFICATION)),
        DataElement::new(tags::COMMAND_FIELD, VR::US, dicom_value!(U16, [0x8030])),
        DataElement::new(tags::MESSAGE_ID_BEING_RESPONDED_TO, VR::US, dicom_value!(U16, [message_id])),
        DataElement::new(tags::COMMAND_DATA_SET_TYPE, VR::US, dicom_value!(U16, [0x0101])),
        DataElement::new(tags::STATUS, VR::US, dicom_value!(U16, [0x0000])),
    ]);
    send_command(association, pc_id, &response)
}

fn handle_store(
    association: &mut ServerAssociation<TcpStream>,
    pc_id: u8,
    command: &InMemDicomObject,
    initial_pdata: &[PDataValue],
    negotiated_ts: &TransferSyntax,
    cache_path: &Path,
    stored_instances_by_study: &mut HashMap<String, Vec<String>>,
) -> Result<(), ()> {
    let message_id = command_u16(command, tags::MESSAGE_ID);
    let affected_sop_class_uid = command_str(command, tags::AFFECTED_SOP_CLASS_UID).unwrap_or_default();
    let affected_sop_instance_uid = command_str(command, tags::AFFECTED_SOP_INSTANCE_UID).unwrap_or_default();

    let dataset_bytes = resolve_data(association, initial_pdata)?;
    let status = store_received_dataset(
        &dataset_bytes,
        negotiated_ts,
        &affected_sop_class_uid,
        &affected_sop_instance_uid,
        cache_path,
        stored_instances_by_study,
    )
    // 0xC000: "Processing failure" (PS3.7 Annex C, generic non-specific failure) - matches the
    // Status this SCP's predecessor (dcmjs-dimse) used for any C-STORE write failure.
    .unwrap_or(0xC000);

    let response = InMemDicomObject::command_from_element_iter([
        DataElement::new(tags::AFFECTED_SOP_CLASS_UID, VR::UI, dicom_value!(Str, affected_sop_class_uid)),
        DataElement::new(tags::COMMAND_FIELD, VR::US, dicom_value!(U16, [0x8001])),
        DataElement::new(tags::MESSAGE_ID_BEING_RESPONDED_TO, VR::US, dicom_value!(U16, [message_id])),
        DataElement::new(tags::COMMAND_DATA_SET_TYPE, VR::US, dicom_value!(U16, [0x0101])),
        DataElement::new(tags::STATUS, VR::US, dicom_value!(U16, [status])),
        DataElement::new(tags::AFFECTED_SOP_INSTANCE_UID, VR::UI, dicom_value!(Str, affected_sop_instance_uid)),
    ]);
    send_command(association, pc_id, &response)
}

fn store_received_dataset(
    dataset_bytes: &[u8],
    negotiated_ts: &TransferSyntax,
    affected_sop_class_uid: &str,
    affected_sop_instance_uid: &str,
    cache_path: &Path,
    stored_instances_by_study: &mut HashMap<String, Vec<String>>,
) -> Result<u16, Box<dyn std::error::Error>> {
    let dataset = InMemDicomObject::read_dataset_with_ts(dataset_bytes, negotiated_ts)?;
    let study_instance_uid = element_str(&dataset, tags::STUDY_INSTANCE_UID).unwrap_or_default();
    let modality = element_str(&dataset, tags::MODALITY).unwrap_or_else(|| "UN".to_owned());

    let meta = FileMetaTableBuilder::new()
        .transfer_syntax(negotiated_ts.uid())
        .media_storage_sop_class_uid(affected_sop_class_uid)
        .media_storage_sop_instance_uid(affected_sop_instance_uid);
    let mut file_object = dataset.with_meta(meta)?;

    let study_dir = cache_path.join(format!("S_{study_instance_uid}"));
    std::fs::create_dir_all(&study_dir)?;
    let file_path = study_dir.join(format!("{modality}_{affected_sop_instance_uid}.dcm"));
    write_dicom_file(&mut file_object, &file_path)?;

    stored_instances_by_study
        .entry(study_instance_uid)
        .or_default()
        .push(file_path.to_string_lossy().into_owned());

    Ok(0x0000)
}

const FIND_IDENTIFIER_KEYS: &[(&str, dicom_core::Tag)] = &[
    ("StudyInstanceUID", tags::STUDY_INSTANCE_UID),
    ("StudyDate", tags::STUDY_DATE),
    ("PatientName", tags::PATIENT_NAME),
    ("PatientID", tags::PATIENT_ID),
];

const FIND_RESPONSE_FIELDS: &[(&str, dicom_core::Tag, VR)] = &[
    ("StudyInstanceUID", tags::STUDY_INSTANCE_UID, VR::UI),
    ("AccessionNumber", tags::ACCESSION_NUMBER, VR::SH),
    ("PatientID", tags::PATIENT_ID, VR::LO),
    ("PatientName", tags::PATIENT_NAME, VR::PN),
    ("PatientBirthDate", tags::PATIENT_BIRTH_DATE, VR::DA),
    ("StudyDate", tags::STUDY_DATE, VR::DA),
    ("StudyTime", tags::STUDY_TIME, VR::TM),
    ("StudyDescription", tags::STUDY_DESCRIPTION, VR::LO),
    ("ModalitiesInStudy", tags::MODALITIES_IN_STUDY, VR::CS),
    ("NumberOfStudyRelatedSeries", tags::NUMBER_OF_STUDY_RELATED_SERIES, VR::IS),
    ("NumberOfStudyRelatedInstances", tags::NUMBER_OF_STUDY_RELATED_INSTANCES, VR::IS),
];

fn handle_find(
    association: &mut ServerAssociation<TcpStream>,
    pc_id: u8,
    command: &InMemDicomObject,
    initial_pdata: &[PDataValue],
    negotiated_ts: &TransferSyntax,
    handlers: &dyn ScpHandlers,
) -> Result<(), ()> {
    let message_id = command_u16(command, tags::MESSAGE_ID);
    let affected_sop_class_uid = command_str(command, tags::AFFECTED_SOP_CLASS_UID).unwrap_or_default();

    let identifier_bytes = resolve_data(association, initial_pdata)?;
    let identifier = InMemDicomObject::read_dataset_with_ts(identifier_bytes.as_slice(), negotiated_ts).map_err(|_| ())?;

    let mut filter = HashMap::new();
    for (keyword, tag) in FIND_IDENTIFIER_KEYS {
        if let Some(value) = element_str(&identifier, *tag) {
            if !value.is_empty() {
                filter.insert((*keyword).to_owned(), value);
            }
        }
    }

    let final_status = match handlers.on_find(&filter) {
        Ok(studies) => {
            for study in &studies {
                let elements: Vec<_> = FIND_RESPONSE_FIELDS
                    .iter()
                    .filter_map(|(keyword, tag, vr)| {
                        study.get(*keyword).map(|value| DataElement::new(*tag, *vr, dicom_value!(Str, value.clone())))
                    })
                    .collect();
                let match_dataset = InMemDicomObject::from_element_iter(elements);
                let mut match_bytes = Vec::new();
                match_dataset.write_dataset_with_ts(&mut match_bytes, negotiated_ts).map_err(|_| ())?;

                let pending = InMemDicomObject::command_from_element_iter([
                    DataElement::new(tags::AFFECTED_SOP_CLASS_UID, VR::UI, dicom_value!(Str, affected_sop_class_uid.clone())),
                    DataElement::new(tags::COMMAND_FIELD, VR::US, dicom_value!(U16, [0x8020])),
                    DataElement::new(tags::MESSAGE_ID_BEING_RESPONDED_TO, VR::US, dicom_value!(U16, [message_id])),
                    DataElement::new(tags::COMMAND_DATA_SET_TYPE, VR::US, dicom_value!(U16, [0x0001])),
                    // 0xFF00: Pending, matches follow (PS3.7 C.4.1) - the standard "another match
                    // is coming next" status for a C-FIND-RSP.
                    DataElement::new(tags::STATUS, VR::US, dicom_value!(U16, [0xFF00])),
                ]);
                send_command(association, pc_id, &pending)?;
                send_data(association, pc_id, match_bytes)?;
            }
            0x0000u16
        }
        // 0xC000: "Unable to process" (generic C-FIND failure). Logged, not just swallowed - the
        // handler's own error (e.g. a malformed match JS returned, an HTTP failure it hit) is the
        // only trace of *why* a C-FIND failed, since the DIMSE status code itself carries none of
        // that detail to the SCU.
        Err(error) => {
            eprintln!("[dicom-scp] C-FIND on_find handler failed: {error}");
            0xC000u16
        }
    };

    let final_response = InMemDicomObject::command_from_element_iter([
        DataElement::new(tags::AFFECTED_SOP_CLASS_UID, VR::UI, dicom_value!(Str, affected_sop_class_uid)),
        DataElement::new(tags::COMMAND_FIELD, VR::US, dicom_value!(U16, [0x8020])),
        DataElement::new(tags::MESSAGE_ID_BEING_RESPONDED_TO, VR::US, dicom_value!(U16, [message_id])),
        DataElement::new(tags::COMMAND_DATA_SET_TYPE, VR::US, dicom_value!(U16, [0x0101])),
        DataElement::new(tags::STATUS, VR::US, dicom_value!(U16, [final_status])),
    ]);
    send_command(association, pc_id, &final_response)
}

fn handle_move(
    association: &mut ServerAssociation<TcpStream>,
    pc_id: u8,
    command: &InMemDicomObject,
    initial_pdata: &[PDataValue],
    negotiated_ts: &TransferSyntax,
    handlers: &dyn ScpHandlers,
) -> Result<(), ()> {
    let message_id = command_u16(command, tags::MESSAGE_ID);
    let affected_sop_class_uid = command_str(command, tags::AFFECTED_SOP_CLASS_UID).unwrap_or_default();
    let move_destination = command_str(command, tags::MOVE_DESTINATION).unwrap_or_default();

    let identifier_bytes = resolve_data(association, initial_pdata)?;
    let identifier = InMemDicomObject::read_dataset_with_ts(identifier_bytes.as_slice(), negotiated_ts).map_err(|_| ())?;
    let study_instance_uid = element_str(&identifier, tags::STUDY_INSTANCE_UID).unwrap_or_default();

    let status: u16 = if study_instance_uid.is_empty() {
        // 0x0120: "Missing Attribute" (PS3.7 Annex C, general status - no StudyInstanceUID given).
        0x0120
    } else if move_destination.is_empty() {
        // 0xA801: "Move Destination unknown" (C-MOVE-specific status, PS3.7 C.4.2.1.5).
        0xA801
    } else {
        match handlers.on_move(&study_instance_uid, &move_destination) {
            Ok(true) => 0x0000,
            // 0xC000: "Unable to process" (generic C-MOVE failure - matches the queueing failure
            // this SCP's predecessor reported here, since the actual transfer's own success/
            // failure is reported separately and asynchronously by whatever performs it).
            Ok(false) => 0xC000,
            Err(error) => {
                eprintln!("[dicom-scp] C-MOVE on_move handler failed: {error}");
                0xC000
            }
        }
    };

    let response = InMemDicomObject::command_from_element_iter([
        DataElement::new(tags::AFFECTED_SOP_CLASS_UID, VR::UI, dicom_value!(Str, affected_sop_class_uid)),
        DataElement::new(tags::COMMAND_FIELD, VR::US, dicom_value!(U16, [0x8021])),
        DataElement::new(tags::MESSAGE_ID_BEING_RESPONDED_TO, VR::US, dicom_value!(U16, [message_id])),
        DataElement::new(tags::COMMAND_DATA_SET_TYPE, VR::US, dicom_value!(U16, [0x0101])),
        DataElement::new(tags::STATUS, VR::US, dicom_value!(U16, [status])),
    ]);
    send_command(association, pc_id, &response)
}
