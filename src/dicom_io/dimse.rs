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
//! function here explicitly releases once it has a terminal result (via `release_and_log` - see
//! its doc comment for why a failed release is only ever logged, never turned into an `Err`, and
//! can't fall back to `.abort()` the way every other fallible step does) and aborts on error paths
//! otherwise.

use std::collections::HashMap;
use std::error::Error as StdError;
use std::fmt;
use std::io::Read;
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

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

/// Which DICOM UL teardown verb an external `cancel` request is asking for - see
/// [`CancelSignal`]. `Release` sends a real A-RELEASE-RQ and waits (briefly) for the peer's
/// A-RELEASE-RP, matching a normal successful terminal response's own teardown path. `Abort`
/// sends A-ABORT and returns immediately without waiting on the peer at all - faster and more
/// certain when the caller needs to be assured the association is over as soon as possible.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CancelMode {
    Release,
    Abort,
}

/// A shared flag an external caller uses to ask an in-progress SCU call to stop waiting and
/// tear down its association - checked once per `poll_bounded` tick (see its doc comment).
/// Replaces a plain `AtomicBool` so a caller can ask for either DICOM UL teardown verb
/// ([`CancelMode`]), not just "cancel".
pub struct CancelSignal(AtomicU8);

impl CancelSignal {
    pub fn new() -> Arc<Self> {
        Arc::new(Self(AtomicU8::new(0)))
    }

    pub fn request(&self, mode: CancelMode) {
        let value = match mode {
            CancelMode::Release => 1,
            CancelMode::Abort => 2,
        };
        self.0.store(value, Ordering::SeqCst);
    }

    pub fn requested(&self) -> Option<CancelMode> {
        match self.0.load(Ordering::SeqCst) {
            1 => Some(CancelMode::Release),
            2 => Some(CancelMode::Abort),
            _ => None,
        }
    }
}

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
    /// The operation's absolute wall-clock ceiling (`*ScuOptions::timeout`) elapsed before it
    /// reached a terminal state. Distinct from `Io`'s per-read timeout: this fires even if
    /// individual reads keep succeeding - e.g. a peer sending periodic pending-status responses
    /// without ever making real progress - since it's checked against a fixed deadline computed
    /// once at the start of the call, not reset by activity. See `poll_bounded`.
    AbsoluteTimeout,
    /// A `move_scu` retrieve's own `stale_data_path` hasn't had a file created/modified within
    /// `stale_data_timeout`, even though the association with the source is still nominally
    /// alive (e.g. it keeps sending pending C-MOVE-RSPs) - a faster, more specific signal than
    /// `AbsoluteTimeout` for a retrieve that's stopped actually writing data to our own storage.
    StaleDataConnection { path: PathBuf },
    /// An external caller (`*ScuOptions::cancel`) asked this operation to stop waiting - not
    /// a peer/protocol failure. `move_scu` catches this itself and turns it into a successful,
    /// `cancelled: true` result rather than letting it propagate as an error to its caller (see
    /// `poll_bounded` and `move_scu`); `echo_scu`/`store_scu`/`find_scu` have no equivalent
    /// "we already know the outcome" case, so it propagates as a plain error for them.
    Cancelled(CancelMode),
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
            Self::AbsoluteTimeout => write!(formatter, "operation exceeded its absolute timeout"),
            Self::StaleDataConnection { path } => write!(
                formatter,
                "no new data written under {} within the configured staleness window; treating the connection as stale",
                path.display()
            ),
            Self::Cancelled(CancelMode::Release) => {
                write!(formatter, "operation was cancelled by the caller (graceful release)")
            }
            Self::Cancelled(CancelMode::Abort) => {
                write!(formatter, "operation was cancelled by the caller (hard abort)")
            }
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
            | Self::Protocol(_)
            | Self::AbsoluteTimeout
            | Self::StaleDataConnection { .. }
            | Self::Cancelled(_) => None,
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

/// Optional sink for DIMSE debug detail - association negotiation, each command sent/received,
/// and the eventual release/abort - that every `*_scu` function below reports through if its
/// Options carry one via `on_log`. A trait (rather than a plain `Fn(String)`) so the napi
/// binding can implement it once over a `ThreadsafeFunction`, matching how `ScpHandlers` bridges
/// its own callbacks across that same boundary.
pub trait DimseLogger: Send + Sync {
    fn log(&self, message: String);
}

/// Calls `logger.log(message())` if a logger is present - `message` is only built when
/// something is actually listening, so this costs nothing when no logger was supplied.
///
/// `pub(super)` (rather than private) so `scp.rs` - the SCP/server-side counterpart of this
/// module's SCU functions - can report the same association/presentation-context/per-message/
/// release detail through its own `ScpOptions::on_log`.
pub(super) fn log_event(logger: Option<&dyn DimseLogger>, message: impl FnOnce() -> String) {
    if let Some(logger) = logger {
        logger.log(message());
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
    logger: Option<&dyn DimseLogger>,
) -> Result<ClientAssociation<TcpStream>, DimseError> {
    log_event(logger, || {
        format!(
            "opening association to {destination} (calling AE {calling_ae_title:?}, called AE {:?}, max PDU {max_pdu_length}, {} presentation context(s) proposed)",
            called_ae_title.unwrap_or("<none>"),
            presentation_contexts.len()
        )
    });

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

    match options.establish_with(destination) {
        Ok(assoc) => {
            log_event(logger, || {
                let negotiated = assoc
                    .presentation_contexts()
                    .iter()
                    .map(|pc| format!("#{} {} / {}", pc.id, pc.abstract_syntax, pc.transfer_syntax))
                    .collect::<Vec<_>>()
                    .join(", ");
                format!("association established; negotiated presentation context(s): [{negotiated}]")
            });
            Ok(assoc)
        }
        Err(error) => {
            log_event(logger, || format!("association failed: {error}"));
            Err(error.into())
        }
    }
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

/// Sends a Command followed by its Data Set (the Identifier for C-FIND/C-MOVE, or the instance
/// for C-STORE) as a single P-DATA-TF PDU - one `assoc.send()`, one write - whenever both fit
/// under the negotiated PDU size. Splitting them into two separate PDUs/writes (as this used to
/// do unconditionally for C-FIND/C-MOVE) leaves a gap between them that, since `dicom-ul`'s
/// TcpStream never sets TCP_NODELAY, Nagle's algorithm can stretch out unpredictably - at least
/// one real-world SCP encountered in production doesn't tolerate that gap and drops or garbles
/// the association. Falls back to two PDUs (fragmenting the data set via `send_pdata`) only when
/// combined they don't fit in one.
fn send_command_with_data(
    assoc: &mut ClientAssociation<TcpStream>,
    pc_id: u8,
    command: &InMemDicomObject,
    data: Vec<u8>,
) -> Result<(), DimseError> {
    let mut command_data = Vec::with_capacity(128);
    command.write_dataset_with_ts(&mut command_data, implicit_vr_le())?;

    let combined_len = command_data.len() + data.len();
    if combined_len < assoc.acceptor_max_pdu_length().saturating_sub(100) as usize {
        assoc.send(&Pdu::PData {
            data: vec![
                PDataValue { presentation_context_id: pc_id, value_type: PDataValueType::Command, is_last: true, data: command_data },
                PDataValue { presentation_context_id: pc_id, value_type: PDataValueType::Data, is_last: true, data },
            ],
        })?;
    } else {
        assoc.send(&Pdu::PData {
            data: vec![PDataValue { presentation_context_id: pc_id, value_type: PDataValueType::Command, is_last: true, data: command_data }],
        })?;
        let mut writer = assoc.send_pdata(pc_id);
        std::io::Write::write_all(&mut writer, &data)?;
        drop(writer);
    }
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

/// Releases `assoc`, logging clearly if the release itself fails - unlike a bare
/// `assoc.release()?`, which this replaces at every call site and which failed *silently*: no log
/// line at all, just whatever generic error the top-level caller happened to surface, if any.
///
/// Deliberately does *not* return a `Result` - every call site calls this only after it already
/// has the operation's real outcome in hand (a validated terminal DIMSE response), and per PS3.7's
/// layering, the DIMSE operation is complete once that response is received; A-RELEASE is a
/// separate, purely transport-level teardown courtesy, not part of the operation's own success
/// criteria (this matches dcmtk: `movescu`'s reported exit status comes from the C-MOVE-RSP alone,
/// never from whether the later `ASC_releaseAssociation` succeeded). A caller that let a release
/// hiccup override an already-successful result would report the operation as failed - and for a
/// queue-backed caller like the move service, that means an unnecessary retry, re-driving a source
/// PACS through a whole retrieve it already completed. So this only ever logs; it can't fail the
/// call.
///
/// This also can't reuse the log+abort pattern every other fallible step in these SCU functions
/// uses, because `ClientAssociation::release`/`::abort` both take `self` by value - once
/// `release()` has consumed and failed on `assoc`, there is no association object left to call
/// `.abort()` on; that's a hard constraint of this dicom-ul version's API, not a choice made here.
/// `release()` sends A-RELEASE-RQ and then expects `Pdu::ReleaseRP` back; if the peer sends
/// anything else first (including a stray trailing `PData` - the same shape as the RamSoft "Failed
/// SOP Instance UID List sent as its own PDU" case `move_scu` already drains for, just now on a
/// success/terminal response instead of a failure one), `release()` returns `Err` without sending
/// an A-ABORT PDU. The underlying TCP socket still gets closed (via `TcpStream`'s own `Drop`, once
/// the consumed `assoc` goes out of scope here) - so the peer does see the connection go away -
/// just as an abrupt close rather than a graceful DICOM release/abort handshake.
fn release_and_log(assoc: ClientAssociation<TcpStream>, logger: Option<&dyn DimseLogger>, on_success: impl FnOnce() -> String) {
    match assoc.release() {
        Ok(()) => log_event(logger, on_success),
        Err(error) => log_event(logger, || {
            format!(
                "association release failed (peer did not send a clean A-RELEASE-RP), but the \
                 operation's own result was already determined and is not affected by this: {}",
                DimseError::from(error)
            )
        }),
    }
}

// ---------------------------------------------------------------------------------------------
// Absolute-deadline enforcement
//
// `establish`'s `read_timeout` (set once, via `TcpStream::set_read_timeout`) bounds each
// individual blocking read syscall, but a fresh call gets a fresh window - so a peer that keeps
// the association alive by responding periodically (e.g. repeated pending C-MOVE-RSPs) can hold
// a read loop open indefinitely even though nothing is really progressing. `poll_bounded` below
// converts that per-syscall timeout into a genuine absolute ceiling for the whole operation by
// re-deriving each read's timeout from however much of a fixed deadline is left, so it can only
// ever shrink - never reset - as the operation runs.
// ---------------------------------------------------------------------------------------------

/// How often `poll_bounded` re-checks the deadline/stale-data watch/cancel signal while waiting
/// for a read that hasn't produced anything yet. Short enough that a 60s stale-data window (see
/// `StaleDataWatch`) gets several checks, and that an external `cancel` request (see
/// `CancelSignal`) - which a caller may now be synchronously awaiting the real completion of, via
/// `abort()`/`release()` on the napi handle - is noticed well under a second rather than up to the
/// old 5s tick; still long enough not to matter as syscall/CPU overhead at this call frequency.
const POLL_INTERVAL: Duration = Duration::from_millis(500);

/// True if `error`'s source chain bottoms out in an `io::Error` of kind `WouldBlock` or
/// `TimedOut` - i.e. this was just a `poll_bounded` tick finding nothing to read yet, not a real
/// failure. Walks the chain rather than matching a specific `DimseError`/`AssociationError`/
/// `ReadError` shape since a read timeout can surface wrapped in any of several dicom-ul
/// variants (`ReceivePdu`, `ReceivePduItem`, ...) depending on exactly where mid-PDU it landed.
fn is_read_timeout(error: &DimseError) -> bool {
    let mut current: &(dyn StdError + 'static) = error;
    loop {
        if let Some(io_error) = current.downcast_ref::<std::io::Error>() {
            return matches!(io_error.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut);
        }
        match current.source() {
            Some(source) => current = source,
            None => return false,
        }
    }
}

/// Watches a local directory's newest file mtime on behalf of a `move_scu` retrieve, so a
/// connection that's technically still alive (the peer keeps responding) but has stopped
/// actually writing new data to our own storage - e.g. a C-STORE receiver stalled downstream, or
/// a peer sending no-progress keepalive pending statuses - can be caught well before the
/// absolute deadline. `move_scu` has no other way to observe this: the instances a C-MOVE
/// retrieve causes to be written arrive over a *separate* inbound association this call has no
/// visibility into (see `MoveScuOptions::stale_data_path`), so this polls the filesystem instead.
struct StaleDataWatch {
    path: PathBuf,
    stale_after: Duration,
    last_progress_at: Instant,
    last_mtime: Option<std::time::SystemTime>,
}

impl StaleDataWatch {
    fn new(path: PathBuf, stale_after: Duration) -> Self {
        Self { path, stale_after, last_progress_at: Instant::now(), last_mtime: None }
    }

    /// Newest mtime among the directory's files, or `None` if it doesn't exist yet (nothing has
    /// arrived at all) or is empty - either way, "no progress observed", not an error.
    fn newest_mtime(&self) -> Option<std::time::SystemTime> {
        std::fs::read_dir(&self.path).ok()?.filter_map(|entry| entry.ok()?.metadata().ok()?.modified().ok()).max()
    }

    fn check(&mut self) -> Result<(), DimseError> {
        let newest = self.newest_mtime();
        if newest.is_some() && newest != self.last_mtime {
            self.last_mtime = newest;
            self.last_progress_at = Instant::now();
        }
        if self.last_progress_at.elapsed() >= self.stale_after {
            return Err(DimseError::StaleDataConnection { path: self.path.clone() });
        }
        Ok(())
    }
}

/// Runs `op` (a `receive_command`/`read_pdata_to_end`-shaped blocking read), re-issuing it with
/// ever-shrinking read timeouts until it produces something other than a plain "nothing arrived
/// this tick" timeout. Between ticks, checks `deadline` (`AbsoluteTimeout` once passed),
/// `stale_watch` (`StaleDataConnection` once its window elapses), and `cancel` (`Cancelled` once
/// set) - any of the three aborts the wait immediately without another read attempt. A `deadline`
/// of `None` disables the absolute ceiling entirely (ticks stay at `POLL_INTERVAL`, matching
/// pre-existing behavior for callers that don't opt in).
fn poll_bounded<T>(
    assoc: &mut ClientAssociation<TcpStream>,
    deadline: Option<Instant>,
    mut stale_watch: Option<&mut StaleDataWatch>,
    cancel: Option<&CancelSignal>,
    mut op: impl FnMut(&mut ClientAssociation<TcpStream>) -> Result<T, DimseError>,
) -> Result<T, DimseError> {
    loop {
        if let Some(deadline) = deadline {
            if Instant::now() >= deadline {
                return Err(DimseError::AbsoluteTimeout);
            }
        }
        if let Some(watch) = stale_watch.as_mut() {
            watch.check()?;
        }
        if let Some(cancel) = cancel {
            if let Some(mode) = cancel.requested() {
                return Err(DimseError::Cancelled(mode));
            }
        }

        let tick = match deadline {
            Some(deadline) => POLL_INTERVAL.min(deadline.saturating_duration_since(Instant::now())).max(Duration::from_millis(1)),
            None => POLL_INTERVAL,
        };
        assoc.inner_stream().set_read_timeout(Some(tick)).map_err(DimseError::Io)?;

        match op(assoc) {
            Ok(value) => return Ok(value),
            Err(error) if is_read_timeout(&error) => continue,
            Err(error) => return Err(error),
        }
    }
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

/// Whether a received command's `CommandDataSetType` (PS3.7 E.2) says a data set follows it as
/// its own P-DATA-TF - `0x0101` means none, anything else (including the field being absent,
/// which shouldn't happen for a well-formed response but shouldn't be misread as "dataset
/// present" either) means one does.
fn command_has_dataset(command: &InMemDicomObject) -> bool {
    command
        .element(tags::COMMAND_DATA_SET_TYPE)
        .ok()
        .and_then(|element| element.to_int::<u16>().ok())
        .is_some_and(|value| value != 0x0101)
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
    pub on_log: Option<Box<dyn DimseLogger>>,
    /// Lets an external caller interrupt an in-progress echo - see `MoveScuOptions.cancel` for
    /// the general shape. Unlike move, there's no "we already know the outcome" success case
    /// here: a cancelled echo is just `Err(DimseError::Cancelled(_))`, same as any other failure.
    pub cancel: Option<Arc<CancelSignal>>,
}

/// Performs a C-ECHO (DICOM Verification) against `destination` ("host:port"). Returns the
/// response Status code (0 = success) - callers decide what threshold counts as "up".
pub fn echo_scu(destination: &str, options: EchoScuOptions) -> Result<u16, DimseError> {
    let logger = options.on_log.as_deref();
    let deadline = options.timeout.map(|timeout| Instant::now() + timeout);
    let cancel = options.cancel.as_deref();
    let mut assoc = establish(
        destination,
        &options.calling_ae_title,
        options.called_ae_title.as_deref(),
        dicom_ul::pdu::DEFAULT_MAX_PDU,
        &[(uids::VERIFICATION.to_owned(), default_transfer_syntaxes())],
        options.timeout,
        logger,
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

    log_event(logger, || "sending C-ECHO-RQ (message id 1)".to_owned());
    let result = send_command(&mut assoc, pc.id, &command)
        .and_then(|_| poll_bounded(&mut assoc, deadline, None, cancel, receive_command));

    let response = match result {
        Ok(response) => response,
        Err(error) => {
            log_event(logger, || format!("aborting association after error: {error}"));
            let _ = assoc.abort();
            return Err(error);
        }
    };

    let status = command_status(&response)?;
    log_event(logger, || format!("received C-ECHO-RSP: status=0x{status:04X}"));
    release_and_log(assoc, logger, || "association released".to_owned());
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
    /// Absolute ceiling for the whole call - connect through release - not reset by activity
    /// (see `poll_bounded`).
    pub timeout: Option<Duration>,
    pub on_log: Option<Box<dyn DimseLogger>>,
    /// Lets an external caller interrupt an in-progress send - see `MoveScuOptions.cancel` for
    /// the general shape. Unlike move, there's no "we already know the outcome" success case
    /// here: a cancelled send is just `Err(DimseError::Cancelled(_))`, same as any other failure.
    pub cancel: Option<Arc<CancelSignal>>,
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
    let logger = options.on_log.as_deref();
    let deadline = options.timeout.map(|timeout| Instant::now() + timeout);
    let cancel = options.cancel.as_deref();
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
        options.timeout,
        logger,
    )?;

    let negotiated = assoc.presentation_contexts().to_vec();

    let mut results = Vec::with_capacity(probed.len());
    for (index, file) in probed.iter().enumerate() {
        let message_id = (index + 1) as u16;
        match send_and_receive_store(&mut assoc, file, &negotiated, message_id, deadline, cancel, logger) {
            Ok(status) => results.push(StoreScuResult { sop_instance_uid: file.sop_instance_uid.clone(), status }),
            Err(error) => {
                log_event(logger, || format!("aborting association after error on {}: {error}", file.sop_instance_uid));
                let _ = assoc.abort();
                return Err(error);
            }
        }
    }

    release_and_log(assoc, logger, || format!("association released ({} instance(s) sent)", results.len()));
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
    deadline: Option<Instant>,
    cancel: Option<&CancelSignal>,
    logger: Option<&dyn DimseLogger>,
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

    log_event(logger, || {
        format!(
            "sending C-STORE-RQ #{message_id}: SOP instance {} (class {}, transfer syntax {})",
            file.sop_instance_uid, file.sop_class_uid, target_ts.uid()
        )
    });

    send_command_with_data(assoc, pc.id, &command, object_data)?;

    let response = poll_bounded(assoc, deadline, None, cancel, receive_command)?;
    let status = command_status(&response)?;
    log_event(logger, || format!("received C-STORE-RSP #{message_id}: status=0x{status:04X}"));
    Ok(status)
}

// ---------------------------------------------------------------------------------------------
// C-FIND
// ---------------------------------------------------------------------------------------------

pub struct FindScuOptions {
    pub calling_ae_title: String,
    pub called_ae_title: Option<String>,
    pub max_pdu_length: u32,
    /// Absolute ceiling for the whole call - connect through release - not reset by activity
    /// (see `poll_bounded`).
    pub timeout: Option<Duration>,
    pub on_log: Option<Box<dyn DimseLogger>>,
    /// Lets an external caller interrupt an in-progress query - see `MoveScuOptions.cancel` for
    /// the general shape. Unlike move, there's no "we already know the outcome" success case
    /// here: a cancelled query is just `Err(DimseError::Cancelled(_))`, same as any other failure.
    pub cancel: Option<Arc<CancelSignal>>,
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
    let logger = options.on_log.as_deref();
    let deadline = options.timeout.map(|timeout| Instant::now() + timeout);
    let cancel = options.cancel.as_deref();
    let abstract_syntax = uids::STUDY_ROOT_QUERY_RETRIEVE_INFORMATION_MODEL_FIND;

    let mut assoc = establish(
        destination,
        &options.calling_ae_title,
        options.called_ae_title.as_deref(),
        options.max_pdu_length,
        &[(abstract_syntax.to_owned(), default_transfer_syntaxes())],
        options.timeout,
        logger,
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

    // Logs query key names only, not values - an Identifier commonly carries PHI (PatientName,
    // PatientID) that a debug log shouldn't echo even at the debug tier.
    log_event(logger, || {
        let mut keys: Vec<&str> = query.keys().map(String::as_str).collect();
        keys.sort_unstable();
        format!("sending C-FIND-RQ (message id 1); query key(s): [{}]", keys.join(", "))
    });

    if let Err(error) = send_command_with_data(&mut assoc, pc.id, &command, identifier_data) {
        log_event(logger, || format!("aborting association after error: {error}"));
        let _ = assoc.abort();
        return Err(error);
    }

    let mut matches = Vec::new();
    loop {
        let response = match poll_bounded(&mut assoc, deadline, None, cancel, receive_command) {
            Ok(response) => response,
            Err(error) => {
                log_event(logger, || format!("aborting association after error: {error}"));
                let _ = assoc.abort();
                return Err(error);
            }
        };
        let status = match command_status(&response) {
            Ok(status) => status,
            Err(error) => {
                log_event(logger, || format!("aborting association after error: {error}"));
                let _ = assoc.abort();
                return Err(error);
            }
        };

        log_event(logger, || format!("received C-FIND-RSP: status=0x{status:04X}"));

        if status == 0x0000 {
            break;
        }
        if !is_pending_status(status) {
            log_event(logger, || format!("aborting association: C-FIND-RSP failed with status 0x{status:04X}"));
            let _ = assoc.abort();
            return Err(DimseError::Protocol(format!("C-FIND-RSP failed with status {status:#06X}")));
        }

        let bytes = match poll_bounded(&mut assoc, deadline, None, cancel, read_pdata_to_end) {
            Ok(bytes) => bytes,
            Err(error) => {
                log_event(logger, || format!("aborting association after error: {error}"));
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

    release_and_log(assoc, logger, || format!("association released ({} match(es))", matches.len()));
    Ok(matches)
}

// ---------------------------------------------------------------------------------------------
// C-MOVE
// ---------------------------------------------------------------------------------------------

pub struct MoveScuOptions {
    pub calling_ae_title: String,
    pub called_ae_title: Option<String>,
    pub max_pdu_length: u32,
    /// Absolute ceiling for the whole call - connect through release - not reset by activity
    /// (see `poll_bounded`). A peer that keeps the association alive with periodic pending
    /// C-MOVE-RSPs but never actually finishes still gets cut off once this elapses.
    pub timeout: Option<Duration>,
    /// Directory to watch for local write progress (see `StaleDataWatch`) - typically this
    /// retrieve's own cache destination (`<cachepath>/S_<studyInstanceUid>`). Both this and
    /// `stale_data_timeout` must be set for the watch to run; `None` disables it.
    pub stale_data_path: Option<PathBuf>,
    /// How long `stale_data_path` may go without a new/modified file before the connection is
    /// considered stale and aborted - independent of, and typically much shorter than, `timeout`.
    pub stale_data_timeout: Option<Duration>,
    pub on_log: Option<Box<dyn DimseLogger>>,
    /// Lets an external caller interrupt an in-progress retrieve once it's independently
    /// confirmed the study already arrived (e.g. a source PACS that pushes the instances over
    /// their own C-STORE association but never sends a terminal C-MOVE-RSP nor releases this
    /// one) - checked each `poll_bounded` tick alongside `timeout`/`stale_data_timeout`. Setting
    /// it produces a successful, `cancelled: true` result (see `move_scu`), not an error, since
    /// cancellation only happens when the data is already known to be in hand.
    pub cancel: Option<Arc<CancelSignal>>,
}

#[derive(Clone, Debug, Default)]
pub struct MoveScuResult {
    pub status: u16,
    pub completed: u16,
    pub failed: u16,
    pub warning: u16,
    pub remaining: u16,
    /// True when this result was produced by `options.cancel` firing rather than a terminal
    /// C-MOVE-RSP from the peer - the sub-operation counts above reflect the last pending
    /// response seen before cancellation, not a real terminal count.
    pub cancelled: bool,
    /// Which teardown verb actually produced `cancelled: true` - `None` when `cancelled` is
    /// `false` (a real terminal C-MOVE-RSP). Lets a caller/log line distinguish a graceful
    /// release from a hard abort instead of just seeing `cancelled: true` either way.
    pub cancelled_via: Option<CancelMode>,
}

/// Tears down `assoc` per the requested `mode` and builds the resulting `cancelled: true`
/// `MoveScuResult` - the shared tail of both `Cancelled` arms in `move_scu`'s response loop.
/// `Release` matches a normal successful terminal response's own teardown (a real A-RELEASE-RQ,
/// log-only on failure - see `release_and_log`). `Abort` sends A-ABORT and returns immediately
/// without waiting on the peer at all, for a caller that needs to be assured the association is
/// over as fast as possible rather than giving the peer a chance to ACK a clean release.
fn handle_move_cancelled(
    assoc: ClientAssociation<TcpStream>,
    logger: Option<&dyn DimseLogger>,
    mode: CancelMode,
    last_result: MoveScuResult,
) -> MoveScuResult {
    match mode {
        CancelMode::Release => {
            log_event(logger, || "cancelled by caller - releasing association".to_owned());
            release_and_log(assoc, logger, || "association released after cancellation".to_owned());
        }
        CancelMode::Abort => {
            log_event(logger, || "cancelled by caller - aborting association".to_owned());
            let _ = assoc.abort();
        }
    }
    MoveScuResult { cancelled: true, cancelled_via: Some(mode), status: 0, ..last_result }
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
    let logger = options.on_log.as_deref();
    let deadline = options.timeout.map(|timeout| Instant::now() + timeout);
    let mut stale_watch = match (options.stale_data_path, options.stale_data_timeout) {
        (Some(path), Some(stale_after)) => Some(StaleDataWatch::new(path, stale_after)),
        _ => None,
    };
    let cancel = options.cancel.as_deref();
    let abstract_syntax = uids::STUDY_ROOT_QUERY_RETRIEVE_INFORMATION_MODEL_MOVE;

    let mut assoc = establish(
        destination,
        &options.calling_ae_title,
        options.called_ae_title.as_deref(),
        options.max_pdu_length,
        &[(abstract_syntax.to_owned(), default_transfer_syntaxes())],
        options.timeout,
        logger,
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

    log_event(logger, || {
        format!("sending C-MOVE-RQ: study {study_instance_uid} -> move destination AE {move_destination_ae}")
    });

    if let Err(error) = send_command_with_data(&mut assoc, pc.id, &command, identifier_data) {
        log_event(logger, || format!("aborting association after error: {error}"));
        let _ = assoc.abort();
        return Err(error);
    }

    // Tracks the most recent pending C-MOVE-RSP's sub-operation counts so a cancellation (which,
    // unlike every other exit from this loop, isn't triggered by anything the peer sent) can
    // still report a meaningful `MoveScuResult` instead of an all-zero default.
    let mut last_result = MoveScuResult::default();

    loop {
        let response = match poll_bounded(&mut assoc, deadline, stale_watch.as_mut(), cancel, receive_command) {
            Ok(response) => response,
            Err(DimseError::Cancelled(mode)) => {
                return Ok(handle_move_cancelled(assoc, logger, mode, last_result));
            }
            Err(error) => {
                log_event(logger, || format!("aborting association after error: {error}"));
                let _ = assoc.abort();
                return Err(error);
            }
        };
        let status = match command_status(&response) {
            Ok(status) => status,
            Err(error) => {
                log_event(logger, || format!("aborting association after error: {error}"));
                let _ = assoc.abort();
                return Err(error);
            }
        };

        // PS3.7 Table 9.3-5: a C-MOVE-RSP with Failure/Warning/Cancel status may carry a Type 1C
        // Identifier (Failed SOP Instance UID List) as its own P-DATA-TF following the command -
        // some SCPs (e.g. RamSoft) send this even for a Failure that fails the whole study. If we
        // don't drain it here it's still sitting on the wire when `release()` next calls
        // `receive()` expecting A-RELEASE-RP, which dicom-ul then rejects as an unexpected PDU.
        if command_has_dataset(&response) {
            match poll_bounded(&mut assoc, deadline, stale_watch.as_mut(), cancel, read_pdata_to_end) {
                Ok(_) => {}
                Err(DimseError::Cancelled(mode)) => {
                    return Ok(handle_move_cancelled(assoc, logger, mode, last_result));
                }
                Err(error) => {
                    log_event(logger, || format!("aborting association after error: {error}"));
                    let _ = assoc.abort();
                    return Err(error);
                }
            }
        }

        let result = MoveScuResult {
            status,
            completed: command_u16(&response, tags::NUMBER_OF_COMPLETED_SUBOPERATIONS),
            failed: command_u16(&response, tags::NUMBER_OF_FAILED_SUBOPERATIONS),
            warning: command_u16(&response, tags::NUMBER_OF_WARNING_SUBOPERATIONS),
            remaining: command_u16(&response, tags::NUMBER_OF_REMAINING_SUBOPERATIONS),
            cancelled: false,
            cancelled_via: None,
        };

        log_event(logger, || {
            format!(
                "received C-MOVE-RSP: status=0x{status:04X} completed={} failed={} warning={} remaining={}",
                result.completed, result.failed, result.warning, result.remaining
            )
        });

        last_result = result.clone();

        if is_pending_status(status) {
            continue;
        }

        release_and_log(assoc, logger, || "association released".to_owned());
        return Ok(result);
    }
}
