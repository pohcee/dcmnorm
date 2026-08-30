//! DICOM Upper Layer Protocol Data Units (PS3.8) - the in-memory [`Pdu`] representation and its
//! wire encode/decode.
//!
//! Ported closely from `dicom-ul`'s `pdu::reader`/`pdu::writer` (byte offsets and field meanings
//! match PS3.8's own section references, kept in the comments below) since getting this
//! byte-exact wrong is exactly the kind of bug that only surfaces against a real external PACS,
//! not a fixture. Two deliberate simplifications from the original, both because `dcmnorm` never
//! needs the dropped detail (confirmed by grep: no `dcmnorm` code matches on a specific
//! rejection/abort sub-reason, just formats the whole `Pdu` via `{:?}`/`Display` in log lines):
//! - `AssociationRJ`'s source/reason and `AbortRQ`'s source/reason are kept as raw `(u8, u8)`
//!   codes with a `Display` impl, not the original's full nested enum hierarchy
//!   (`AssociationRJSource`/`AssociationRJServiceUserReason`/etc.) - same wire bytes, much less
//!   type surface to carry.
//! - `UserIdentityItem`/`SopClassExtendedNegotiationSubItem` user-variable sub-items are parsed
//!   generically as opaque bytes rather than modeled - `dcmnorm` never sends or reads either.
//!
//! Also uses plain `&[u8]` cursors instead of the original's `bytes::Buf` abstraction: that
//! abstraction exists there to let `read_pdu` be shared between the sync and async readers; since
//! `dcmnorm` only uses blocking `TcpStream`, the association layer (`association.rs`) always hands
//! this module one complete, already-fully-read PDU body, so there's no need to support "not
//! enough bytes yet" as a first-class outcome the way the original's shared sync/async reader did.

use std::fmt;

use dcmnorm_encoding::text::{DefaultCharacterSetCodec, TextCodec};

/// The length of the PDU header in bytes: PDU-type (1) + reserved (1) + PDU-length (4).
pub const PDU_HEADER_SIZE: u32 = 6;
/// The length of a PDV item header + message control header: item-length (4) + presentation
/// context ID (1) + message control header (1).
pub const PDV_HEADER_SIZE: u32 = 6;
/// The default maximum PDU size proposed/accepted when none is explicitly configured.
pub const DEFAULT_MAX_PDU: u32 = 16_384 - PDU_HEADER_SIZE;
/// A generous internal buffer size, used to avoid over-allocating for a max PDU length that's
/// unrealistically large.
pub(crate) const LARGE_PDU_SIZE: u32 = 262_144 - PDU_HEADER_SIZE;
/// Hard upper bound on `max_pdu_length`, enforced by both `ClientAssociationOptions` and
/// `ServerAssociationOptions` regardless of what a caller configures. `conn::receive_pdu` already
/// rejects any peer-declared PDU length above the *configured* `max_pdu_length` before
/// allocating its receive buffer - but that check is only as good as the ceiling it's checked
/// against. Without a floor here, a future caller (or a config regression) constructing options
/// with an unbounded `max_pdu_length` would remove that protection entirely, letting a hostile
/// peer force an allocation as large as whatever was configured. 64MiB is far beyond any real
/// DICOM association's negotiated PDU size (large transfers are already fragmented across many
/// PDVs/PDUs by the streaming design, not sent as one huge PDU) while still bounding worst-case
/// memory well below "attacker forces gigabytes."
pub const MAX_PDU_LENGTH_CEILING: u32 = 64 * 1024 * 1024;
/// Default read/write timeout applied when a caller never explicitly configures one. Not
/// `None` (block forever): a slow or hostile peer that dribbles bytes could otherwise hold a
/// thread open indefinitely. Generous enough not to interfere with legitimate large transfers,
/// which are bounded by their own progress (each PDV/PDU still needs to arrive within this
/// window, not the whole transfer).
pub const DEFAULT_IO_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

#[derive(Debug)]
pub enum Error {
    Io(std::io::Error),
    UnexpectedEof,
    PduTooLarge { pdu_length: u32, max_pdu_length: u32 },
    InvalidField { field: &'static str },
    Text(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Io(e) => write!(f, "I/O error: {e}"),
            Error::UnexpectedEof => write!(f, "unexpected end of PDU data"),
            Error::PduTooLarge { pdu_length, max_pdu_length } => {
                write!(f, "incoming PDU too large: {pdu_length} bytes, maximum is {max_pdu_length}")
            }
            Error::InvalidField { field } => write!(f, "invalid or missing PDU field `{field}`"),
            Error::Text(msg) => write!(f, "{msg}"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Io(e)
    }
}

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Clone, Eq, PartialEq, Debug)]
pub struct PresentationContextProposed {
    pub id: u8,
    pub abstract_syntax: String,
    pub transfer_syntaxes: Vec<String>,
}

#[derive(Clone, Eq, PartialEq, Debug)]
pub struct PresentationContextResult {
    pub id: u8,
    pub reason: PresentationContextResultReason,
    pub transfer_syntax: String,
}

/// Like [`PresentationContextResult`], plus the abstract syntax carried forward from the
/// original proposal - what a negotiated association actually exposes to callers.
#[derive(Clone, Eq, PartialEq, Debug)]
pub struct PresentationContextNegotiated {
    pub id: u8,
    pub reason: PresentationContextResultReason,
    pub transfer_syntax: String,
    pub abstract_syntax: String,
}

#[derive(Clone, Copy, Eq, PartialEq, Debug)]
pub enum PresentationContextResultReason {
    Acceptance = 0,
    UserRejection = 1,
    NoReason = 2,
    AbstractSyntaxNotSupported = 3,
    TransferSyntaxesNotSupported = 4,
}

impl PresentationContextResultReason {
    fn from_u8(v: u8) -> Result<Self> {
        Ok(match v {
            0 => Self::Acceptance,
            1 => Self::UserRejection,
            2 => Self::NoReason,
            3 => Self::AbstractSyntaxNotSupported,
            4 => Self::TransferSyntaxesNotSupported,
            _ => return Err(Error::InvalidField { field: "Presentation-context Result/Reason" }),
        })
    }
}

impl fmt::Display for PresentationContextResultReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Acceptance => "acceptance",
            Self::UserRejection => "user rejection",
            Self::NoReason => "no reason",
            Self::AbstractSyntaxNotSupported => "abstract syntax not supported",
            Self::TransferSyntaxesNotSupported => "transfer syntaxes not supported",
        })
    }
}

#[derive(Clone, Copy, Eq, PartialEq, Debug)]
pub enum AssociationRJResult {
    Permanent = 1,
    Transient = 2,
}

#[derive(Clone, Eq, PartialEq, Debug)]
pub struct AssociationRJ {
    pub result: AssociationRJResult,
    /// Raw (source, reason) codes per PS3.8 Table 9-21 - see this module's doc comment for why
    /// these aren't modeled as a full enum.
    pub source: u8,
    pub reason: u8,
}

impl fmt::Display for AssociationRJ {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "result={:?} source={} reason={}", self.result, self.source, self.reason)
    }
}

#[derive(Clone, Eq, PartialEq, Debug)]
pub struct PDataValue {
    pub presentation_context_id: u8,
    pub value_type: PDataValueType,
    pub is_last: bool,
    pub data: Vec<u8>,
}

#[derive(Clone, Copy, Eq, PartialEq, Debug)]
pub enum PDataValueType {
    Command,
    Data,
}

/// A-ABORT source/reason, per PS3.8 Table 9-26. `0` = service-user, `2` = service-provider (with
/// `reason` significant); see this module's doc comment for why sub-reasons aren't modeled.
#[derive(Clone, Copy, Eq, PartialEq, Debug)]
pub struct AbortRQSource {
    pub source: u8,
    pub reason: u8,
}

impl AbortRQSource {
    pub const SERVICE_USER: Self = Self { source: 0x00, reason: 0x00 };
    /// Service-provider, reason-not-specified.
    pub const SERVICE_PROVIDER: Self = Self { source: 0x02, reason: 0x00 };
    pub const UNEXPECTED_PDU: Self = Self { source: 0x02, reason: 0x02 };
    pub const UNRECOGNIZED_PDU: Self = Self { source: 0x02, reason: 0x01 };
}

#[derive(Clone, Eq, PartialEq, Debug)]
pub struct AssociationRQ {
    pub protocol_version: u16,
    pub calling_ae_title: String,
    pub called_ae_title: String,
    pub application_context_name: String,
    pub presentation_contexts: Vec<PresentationContextProposed>,
    pub user_variables: Vec<UserVariableItem>,
}

#[derive(Clone, Eq, PartialEq, Debug)]
pub struct AssociationAC {
    pub protocol_version: u16,
    pub calling_ae_title: String,
    pub called_ae_title: String,
    pub application_context_name: String,
    pub presentation_contexts: Vec<PresentationContextResult>,
    pub user_variables: Vec<UserVariableItem>,
}

#[derive(Clone, Eq, PartialEq, Debug)]
pub enum UserVariableItem {
    MaxLength(u32),
    ImplementationClassUID(String),
    ImplementationVersionName(String),
    /// Any other user-variable sub-item (`SopClassExtendedNegotiation`, `UserIdentityItem`,
    /// etc.) - `dcmnorm` neither sends nor reads these, so they're kept only as raw bytes for
    /// completeness of the parse (a peer that sends one must not desync the rest of the PDU).
    Other(u8, Vec<u8>),
}

/// An in-memory Protocol Data Unit.
#[derive(Clone, Eq, PartialEq, Debug)]
pub enum Pdu {
    AssociationRQ(AssociationRQ),
    AssociationAC(AssociationAC),
    AssociationRJ(AssociationRJ),
    PData { data: Vec<PDataValue> },
    ReleaseRQ,
    ReleaseRP,
    AbortRQ { source: AbortRQSource },
    /// An unrecognized PDU type - passed through so the association layer can abort cleanly
    /// with a clear reason instead of silently misinterpreting it as something else.
    Unknown { pdu_type: u8, data: Vec<u8> },
}

// ===== Reading =====

/// A cursor over an already-fully-buffered PDU body (see this module's doc comment for why a
/// partial-read-aware `Buf` abstraction isn't needed here).
struct Cur<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Cur<'a> {
    fn new(data: &'a [u8]) -> Self {
        Cur { data, pos: 0 }
    }
    fn remaining(&self) -> usize {
        self.data.len() - self.pos
    }
    fn u8(&mut self) -> Result<u8> {
        let b = *self.data.get(self.pos).ok_or(Error::UnexpectedEof)?;
        self.pos += 1;
        Ok(b)
    }
    fn u16(&mut self) -> Result<u16> {
        let bytes = self.bytes(2)?;
        Ok(u16::from_be_bytes([bytes[0], bytes[1]]))
    }
    fn u32(&mut self) -> Result<u32> {
        let bytes = self.bytes(4)?;
        Ok(u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    }
    fn bytes(&mut self, n: usize) -> Result<&'a [u8]> {
        if self.remaining() < n {
            return Err(Error::UnexpectedEof);
        }
        let out = &self.data[self.pos..self.pos + n];
        self.pos += n;
        Ok(out)
    }
    fn skip(&mut self, n: usize) -> Result<()> {
        self.bytes(n).map(|_| ())
    }
    fn text(&mut self, n: usize, codec: &dyn TextCodec, field: &'static str) -> Result<String> {
        let bytes = self.bytes(n)?;
        codec
            .decode(bytes)
            .map(|s| s.trim().to_string())
            .map_err(|_| Error::InvalidField { field })
    }
}

/// Read one PDU from `body` (the PDU's variable-length content, i.e. everything after the
/// 6-byte PDU-type/reserved/length header - the caller reads that header and dispatches on
/// `pdu_type` itself, since only it knows how many more bytes to read off the socket first).
pub fn parse_pdu_body(pdu_type: u8, body: &[u8]) -> Result<Pdu> {
    let codec = DefaultCharacterSetCodec;
    let mut c = Cur::new(body);

    match pdu_type {
        0x01 | 0x02 => {
            // A-ASSOCIATE-RQ / A-ASSOCIATE-AC PDU Structure (PS3.8 §9.3.2 / §9.3.3) - identical
            // field layout, differing only in which side's semantics apply to each field.
            let protocol_version = c.u16()?;
            c.skip(2)?; // reserved
            let called_ae_title = c.text(16, &codec, "Called-AE-title")?;
            let calling_ae_title = c.text(16, &codec, "Calling-AE-title")?;
            c.skip(32)?; // reserved

            let mut application_context_name = String::new();
            let mut presentation_contexts_proposed = Vec::new();
            let mut presentation_contexts_result = Vec::new();
            let mut user_variables = Vec::new();

            while c.remaining() > 0 {
                let item_type = c.u8()?;
                c.skip(1)?; // reserved
                let item_length = c.u16()? as usize;
                let mut item = Cur::new(c.bytes(item_length)?);

                match item_type {
                    0x10 => {
                        application_context_name =
                            item.text(item.remaining(), &codec, "Application-context-name")?;
                    }
                    0x20 => {
                        presentation_contexts_proposed
                            .push(parse_presentation_context_proposed(&mut item, &codec)?);
                    }
                    0x21 => {
                        presentation_contexts_result
                            .push(parse_presentation_context_result(&mut item, &codec)?);
                    }
                    0x50 => {
                        user_variables = parse_user_variables(&mut item, &codec)?;
                    }
                    _ => {} // unknown top-level item - ignore, don't desync (length-prefixed)
                }
            }

            if pdu_type == 0x01 {
                Ok(Pdu::AssociationRQ(AssociationRQ {
                    protocol_version,
                    calling_ae_title,
                    called_ae_title,
                    application_context_name,
                    presentation_contexts: presentation_contexts_proposed,
                    user_variables,
                }))
            } else {
                Ok(Pdu::AssociationAC(AssociationAC {
                    protocol_version,
                    calling_ae_title,
                    called_ae_title,
                    application_context_name,
                    presentation_contexts: presentation_contexts_result,
                    user_variables,
                }))
            }
        }
        0x03 => {
            // A-ASSOCIATE-RJ PDU Structure (PS3.8 §9.3.4)
            c.skip(1)?; // reserved
            let result = match c.u8()? {
                1 => AssociationRJResult::Permanent,
                2 => AssociationRJResult::Transient,
                _ => return Err(Error::InvalidField { field: "AssociationRJ Result" }),
            };
            let source = c.u8()?;
            let reason = c.u8()?;
            Ok(Pdu::AssociationRJ(AssociationRJ { result, source, reason }))
        }
        0x04 => {
            // P-DATA-TF PDU Structure (PS3.8 §9.3.5)
            let mut values = Vec::new();
            while c.remaining() > 0 {
                let item_length = c.u32()? as usize;
                if item_length < 2 {
                    return Err(Error::InvalidField { field: "PDV item-length" });
                }
                let presentation_context_id = c.u8()?;
                let header = c.u8()?;
                let value_type =
                    if header & 0x01 != 0 { PDataValueType::Command } else { PDataValueType::Data };
                let is_last = header & 0x02 != 0;
                let data = c.bytes(item_length - 2)?.to_vec();
                values.push(PDataValue { presentation_context_id, value_type, is_last, data });
            }
            Ok(Pdu::PData { data: values })
        }
        0x05 => Ok(Pdu::ReleaseRQ),
        0x06 => Ok(Pdu::ReleaseRP),
        0x07 => {
            // A-ABORT PDU Structure (PS3.8 §9.3.8)
            c.skip(2)?; // reserved
            let source = c.u8()?;
            let reason = c.u8()?;
            Ok(Pdu::AbortRQ { source: AbortRQSource { source, reason } })
        }
        other => Ok(Pdu::Unknown { pdu_type: other, data: body.to_vec() }),
    }
}

fn parse_presentation_context_proposed(
    c: &mut Cur<'_>,
    codec: &dyn TextCodec,
) -> Result<PresentationContextProposed> {
    let id = c.u8()?;
    c.skip(3)?; // reserved x3
    let mut abstract_syntax = None;
    let mut transfer_syntaxes = Vec::new();
    while c.remaining() > 0 {
        let item_type = c.u8()?;
        c.skip(1)?;
        let item_length = c.u16()? as usize;
        let text = c.text(item_length, codec, "Abstract/Transfer-syntax-name")?;
        match item_type {
            0x30 => abstract_syntax = Some(text),
            0x40 => transfer_syntaxes.push(text),
            _ => {} // unknown sub-item - ignore, don't desync
        }
    }
    Ok(PresentationContextProposed {
        id,
        abstract_syntax: abstract_syntax.ok_or(Error::InvalidField { field: "Abstract-syntax-name" })?,
        transfer_syntaxes,
    })
}

fn parse_presentation_context_result(
    c: &mut Cur<'_>,
    codec: &dyn TextCodec,
) -> Result<PresentationContextResult> {
    let id = c.u8()?;
    c.skip(1)?;
    let reason = PresentationContextResultReason::from_u8(c.u8()?)?;
    c.skip(1)?;
    let mut transfer_syntax = String::new();
    while c.remaining() > 0 {
        let item_type = c.u8()?;
        c.skip(1)?;
        let item_length = c.u16()? as usize;
        let text = c.text(item_length, codec, "Transfer-syntax-name")?;
        if item_type == 0x40 {
            transfer_syntax = text;
        }
    }
    Ok(PresentationContextResult { id, reason, transfer_syntax })
}

fn parse_user_variables(c: &mut Cur<'_>, codec: &dyn TextCodec) -> Result<Vec<UserVariableItem>> {
    let mut items = Vec::new();
    while c.remaining() > 0 {
        let item_type = c.u8()?;
        c.skip(1)?;
        let item_length = c.u16()? as usize;
        match item_type {
            0x51 => {
                if item_length != 4 {
                    return Err(Error::InvalidField { field: "Maximum-length-received" });
                }
                items.push(UserVariableItem::MaxLength(
                    u32::from_be_bytes(c.bytes(4)?.try_into().unwrap()),
                ));
            }
            0x52 => {
                items.push(UserVariableItem::ImplementationClassUID(c.text(
                    item_length,
                    codec,
                    "Implementation-class-uid",
                )?));
            }
            0x55 => {
                items.push(UserVariableItem::ImplementationVersionName(c.text(
                    item_length,
                    codec,
                    "Implementation-version-name",
                )?));
            }
            other => {
                items.push(UserVariableItem::Other(other, c.bytes(item_length)?.to_vec()));
            }
        }
    }
    Ok(items)
}

// ===== Writing =====

fn write_chunk_u32(out: &mut Vec<u8>, body: impl FnOnce(&mut Vec<u8>)) {
    let mut chunk = Vec::new();
    body(&mut chunk);
    out.extend_from_slice(&(chunk.len() as u32).to_be_bytes());
    out.extend_from_slice(&chunk);
}

fn write_chunk_u16(out: &mut Vec<u8>, body: impl FnOnce(&mut Vec<u8>)) {
    let mut chunk = Vec::new();
    body(&mut chunk);
    out.extend_from_slice(&(chunk.len() as u16).to_be_bytes());
    out.extend_from_slice(&chunk);
}

fn write_ae_title(out: &mut Vec<u8>, title: &str, codec: &dyn TextCodec) -> Result<()> {
    let mut bytes = codec.encode(title).map_err(|_| Error::InvalidField { field: "AE-title" })?;
    bytes.resize(16, b' ');
    out.extend_from_slice(&bytes);
    Ok(())
}

/// Write one PDU's full wire representation (header + body) to `out`.
pub fn write_pdu(out: &mut Vec<u8>, pdu: &Pdu) -> Result<()> {
    let codec = DefaultCharacterSetCodec;
    match pdu {
        Pdu::AssociationRQ(rq) => {
            out.push(0x01);
            out.push(0x00);
            write_chunk_u32(out, |b| {
                b.extend_from_slice(&rq.protocol_version.to_be_bytes());
                b.extend_from_slice(&[0, 0]);
                let _ = write_ae_title(b, &rq.called_ae_title, &codec);
                let _ = write_ae_title(b, &rq.calling_ae_title, &codec);
                b.extend_from_slice(&[0u8; 32]);
                write_application_context(b, &rq.application_context_name, &codec);
                for pc in &rq.presentation_contexts {
                    write_presentation_context_proposed(b, pc, &codec);
                }
                write_user_variables(b, &rq.user_variables, &codec);
            });
        }
        Pdu::AssociationAC(ac) => {
            out.push(0x02);
            out.push(0x00);
            write_chunk_u32(out, |b| {
                b.extend_from_slice(&ac.protocol_version.to_be_bytes());
                b.extend_from_slice(&[0, 0]);
                let _ = write_ae_title(b, &ac.called_ae_title, &codec);
                let _ = write_ae_title(b, &ac.calling_ae_title, &codec);
                b.extend_from_slice(&[0u8; 32]);
                write_application_context(b, &ac.application_context_name, &codec);
                for pc in &ac.presentation_contexts {
                    write_presentation_context_result(b, pc, &codec);
                }
                write_user_variables(b, &ac.user_variables, &codec);
            });
        }
        Pdu::AssociationRJ(rj) => {
            out.push(0x03);
            out.push(0x00);
            write_chunk_u32(out, |b| {
                b.push(0);
                b.push(match rj.result {
                    AssociationRJResult::Permanent => 1,
                    AssociationRJResult::Transient => 2,
                });
                b.push(rj.source);
                b.push(rj.reason);
            });
        }
        Pdu::PData { data } => {
            out.push(0x04);
            out.push(0x00);
            write_chunk_u32(out, |b| {
                for pdv in data {
                    write_chunk_u32(b, |b| {
                        b.push(pdv.presentation_context_id);
                        let mut header = 0u8;
                        if let PDataValueType::Command = pdv.value_type {
                            header |= 0x01;
                        }
                        if pdv.is_last {
                            header |= 0x02;
                        }
                        b.push(header);
                        b.extend_from_slice(&pdv.data);
                    });
                }
            });
        }
        Pdu::ReleaseRQ => {
            out.push(0x05);
            out.push(0x00);
            write_chunk_u32(out, |b| b.extend_from_slice(&[0u8; 4]));
        }
        Pdu::ReleaseRP => {
            out.push(0x06);
            out.push(0x00);
            write_chunk_u32(out, |b| b.extend_from_slice(&[0u8; 4]));
        }
        Pdu::AbortRQ { source } => {
            out.push(0x07);
            out.push(0x00);
            write_chunk_u32(out, |b| {
                b.extend_from_slice(&[0, 0]);
                b.push(source.source);
                b.push(source.reason);
            });
        }
        Pdu::Unknown { pdu_type, data } => {
            out.push(*pdu_type);
            out.push(0x00);
            write_chunk_u32(out, |b| b.extend_from_slice(data));
        }
    }
    Ok(())
}

fn write_application_context(out: &mut Vec<u8>, name: &str, codec: &dyn TextCodec) {
    out.push(0x10);
    out.push(0x00);
    write_chunk_u16(out, |b| {
        if let Ok(bytes) = codec.encode(name) {
            b.extend_from_slice(&bytes);
        }
    });
}

fn write_presentation_context_proposed(
    out: &mut Vec<u8>,
    pc: &PresentationContextProposed,
    codec: &dyn TextCodec,
) {
    out.push(0x20);
    out.push(0x00);
    write_chunk_u16(out, |b| {
        b.push(pc.id);
        b.extend_from_slice(&[0, 0, 0]);

        b.push(0x30);
        b.push(0x00);
        write_chunk_u16(b, |b| {
            if let Ok(bytes) = codec.encode(&pc.abstract_syntax) {
                b.extend_from_slice(&bytes);
            }
        });

        for ts in &pc.transfer_syntaxes {
            b.push(0x40);
            b.push(0x00);
            write_chunk_u16(b, |b| {
                if let Ok(bytes) = codec.encode(ts) {
                    b.extend_from_slice(&bytes);
                }
            });
        }
    });
}

fn write_presentation_context_result(
    out: &mut Vec<u8>,
    pc: &PresentationContextResult,
    codec: &dyn TextCodec,
) {
    out.push(0x21);
    out.push(0x00);
    write_chunk_u16(out, |b| {
        b.push(pc.id);
        b.push(0);
        b.push(match pc.reason {
            PresentationContextResultReason::Acceptance => 0,
            PresentationContextResultReason::UserRejection => 1,
            PresentationContextResultReason::NoReason => 2,
            PresentationContextResultReason::AbstractSyntaxNotSupported => 3,
            PresentationContextResultReason::TransferSyntaxesNotSupported => 4,
        });
        b.push(0);

        b.push(0x40);
        b.push(0x00);
        write_chunk_u16(b, |b| {
            if let Ok(bytes) = codec.encode(&pc.transfer_syntax) {
                b.extend_from_slice(&bytes);
            }
        });
    });
}

fn write_user_variables(out: &mut Vec<u8>, vars: &[UserVariableItem], codec: &dyn TextCodec) {
    if vars.is_empty() {
        return;
    }
    out.push(0x50);
    out.push(0x00);
    write_chunk_u16(out, |b| {
        for var in vars {
            match var {
                UserVariableItem::MaxLength(len) => {
                    b.push(0x51);
                    b.push(0x00);
                    write_chunk_u16(b, |b| b.extend_from_slice(&len.to_be_bytes()));
                }
                UserVariableItem::ImplementationClassUID(uid) => {
                    b.push(0x52);
                    b.push(0x00);
                    write_chunk_u16(b, |b| {
                        if let Ok(bytes) = codec.encode(uid) {
                            b.extend_from_slice(&bytes);
                        }
                    });
                }
                UserVariableItem::ImplementationVersionName(name) => {
                    b.push(0x55);
                    b.push(0x00);
                    write_chunk_u16(b, |b| {
                        if let Ok(bytes) = codec.encode(name) {
                            b.extend_from_slice(&bytes);
                        }
                    });
                }
                UserVariableItem::Other(item_type, data) => {
                    b.push(*item_type);
                    b.push(0x00);
                    write_chunk_u16(b, |b| b.extend_from_slice(data));
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `write_pdu` emits the 6-byte header (type, reserved, u32 body-length) itself; strip it back
    /// off and hand the body to `parse_pdu_body`, exactly as `conn.rs` does against a real socket.
    fn roundtrip(pdu: &Pdu) -> Pdu {
        let mut bytes = Vec::new();
        write_pdu(&mut bytes, pdu).expect("write_pdu should not fail for an in-memory buffer");
        let pdu_type = bytes[0];
        let body = &bytes[PDU_HEADER_SIZE as usize..];
        parse_pdu_body(pdu_type, body).expect("parse_pdu_body should read back what write_pdu wrote")
    }

    #[test]
    fn association_rq_roundtrips_presentation_contexts_and_user_variables() {
        let rq = AssociationRQ {
            protocol_version: 1,
            calling_ae_title: "CALLING_AE".to_owned(),
            called_ae_title: "CALLED_AE".to_owned(),
            application_context_name: "1.2.840.10008.3.1.1.1".to_owned(),
            presentation_contexts: vec![
                PresentationContextProposed {
                    id: 1,
                    abstract_syntax: "1.2.840.10008.1.1".to_owned(),
                    transfer_syntaxes: vec![
                        "1.2.840.10008.1.2".to_owned(),
                        "1.2.840.10008.1.2.1".to_owned(),
                    ],
                },
                PresentationContextProposed {
                    id: 3,
                    abstract_syntax: "1.2.840.10008.5.1.4.1.1.1.2".to_owned(),
                    transfer_syntaxes: vec!["1.2.840.10008.1.2.4.50".to_owned()],
                },
            ],
            user_variables: vec![
                UserVariableItem::MaxLength(16_384),
                UserVariableItem::ImplementationClassUID("1.2.3.4.5.6.7".to_owned()),
                UserVariableItem::ImplementationVersionName("DCMNORM_1_0".to_owned()),
                // An unmodeled sub-item (e.g. what upstream calls SopClassExtendedNegotiation)
                // must still round-trip as opaque bytes rather than desyncing the rest of the PDU.
                UserVariableItem::Other(0x56, vec![1, 2, 3, 4, 5]),
            ],
        };

        let Pdu::AssociationRQ(got) = roundtrip(&Pdu::AssociationRQ(rq.clone())) else {
            panic!("expected AssociationRQ back");
        };
        assert_eq!(got, rq);
    }

    #[test]
    fn association_ac_roundtrips_every_presentation_context_result_reason() {
        let ac = AssociationAC {
            protocol_version: 1,
            calling_ae_title: "CALLING_AE".to_owned(),
            called_ae_title: "CALLED_AE".to_owned(),
            application_context_name: "1.2.840.10008.3.1.1.1".to_owned(),
            presentation_contexts: vec![
                PresentationContextResult {
                    id: 1,
                    reason: PresentationContextResultReason::Acceptance,
                    transfer_syntax: "1.2.840.10008.1.2".to_owned(),
                },
                PresentationContextResult {
                    id: 3,
                    reason: PresentationContextResultReason::AbstractSyntaxNotSupported,
                    transfer_syntax: String::new(),
                },
                PresentationContextResult {
                    id: 5,
                    reason: PresentationContextResultReason::TransferSyntaxesNotSupported,
                    transfer_syntax: String::new(),
                },
                PresentationContextResult {
                    id: 7,
                    reason: PresentationContextResultReason::UserRejection,
                    transfer_syntax: String::new(),
                },
                PresentationContextResult {
                    id: 9,
                    reason: PresentationContextResultReason::NoReason,
                    transfer_syntax: String::new(),
                },
            ],
            user_variables: vec![UserVariableItem::MaxLength(16_384)],
        };

        let Pdu::AssociationAC(got) = roundtrip(&Pdu::AssociationAC(ac.clone())) else {
            panic!("expected AssociationAC back");
        };
        assert_eq!(got, ac);
    }

    #[test]
    fn association_rj_roundtrips_result_and_raw_source_reason_codes() {
        let rj = AssociationRJ { result: AssociationRJResult::Permanent, source: 1, reason: 2 };
        let Pdu::AssociationRJ(got) = roundtrip(&Pdu::AssociationRJ(rj.clone())) else {
            panic!("expected AssociationRJ back");
        };
        assert_eq!(got, rj);

        let rj = AssociationRJ { result: AssociationRJResult::Transient, source: 2, reason: 4 };
        let Pdu::AssociationRJ(got) = roundtrip(&Pdu::AssociationRJ(rj.clone())) else {
            panic!("expected AssociationRJ back");
        };
        assert_eq!(got, rj);
    }

    #[test]
    fn pdata_roundtrips_multiple_command_and_data_pdvs_with_is_last() {
        let pdu = Pdu::PData {
            data: vec![
                PDataValue {
                    presentation_context_id: 1,
                    value_type: PDataValueType::Command,
                    is_last: true,
                    data: vec![0xde, 0xad, 0xbe, 0xef],
                },
                PDataValue {
                    presentation_context_id: 1,
                    value_type: PDataValueType::Data,
                    is_last: false,
                    data: vec![0; 512],
                },
                PDataValue {
                    presentation_context_id: 1,
                    value_type: PDataValueType::Data,
                    is_last: true,
                    data: vec![1, 2, 3],
                },
            ],
        };

        let Pdu::PData { data: got } = roundtrip(&pdu) else {
            panic!("expected PData back");
        };
        let Pdu::PData { data: want } = pdu else { unreachable!() };
        assert_eq!(got, want);
    }

    #[test]
    fn release_rq_and_rp_roundtrip() {
        assert!(matches!(roundtrip(&Pdu::ReleaseRQ), Pdu::ReleaseRQ));
        assert!(matches!(roundtrip(&Pdu::ReleaseRP), Pdu::ReleaseRP));
    }

    #[test]
    fn abort_rq_roundtrips_every_predefined_source() {
        for source in [
            AbortRQSource::SERVICE_USER,
            AbortRQSource::SERVICE_PROVIDER,
            AbortRQSource::UNEXPECTED_PDU,
            AbortRQSource::UNRECOGNIZED_PDU,
        ] {
            let Pdu::AbortRQ { source: got } = roundtrip(&Pdu::AbortRQ { source }) else {
                panic!("expected AbortRQ back");
            };
            assert_eq!(got, source);
        }
    }

    #[test]
    fn unknown_pdu_type_passes_its_raw_body_through_unmodified() {
        let pdu = Pdu::Unknown { pdu_type: 0x99, data: vec![1, 2, 3, 4, 5] };
        let Pdu::Unknown { pdu_type, data } = roundtrip(&pdu) else {
            panic!("expected Unknown back");
        };
        assert_eq!(pdu_type, 0x99);
        assert_eq!(data, vec![1, 2, 3, 4, 5]);
    }
}
