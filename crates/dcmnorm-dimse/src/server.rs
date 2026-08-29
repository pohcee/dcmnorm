//! The DICOM Upper Layer association from the accepting (SCP) side: [`ServerAssociationOptions`]
//! (builder) and [`ServerAssociation`] (the established connection).
//!
//! `dcmnorm`'s SCP always runs `.promiscuous(true)` (accept any abstract syntax) with no
//! transfer-syntax allow-list and no AE-title access control configured (confirmed by grep of
//! `scp.rs`) - so unlike `dicom-ul`'s server, which supports configuring all of those, this only
//! implements the promiscuous/unrestricted path. Narrower to `dcmnorm`'s actual usage, per the
//! phased dicom-rs removal plan's "lean toward the functionality dcmnorm adds" scope.

use std::net::TcpStream;
use std::time::Duration;

use dcmnorm_encoding::transfer_syntax::TransferSyntaxIndex;
use dcmnorm_transcode::TransferSyntaxRegistry;

use crate::conn;
use crate::error::{Error, Result};
use crate::pdu::{
    AbortRQSource, AssociationAC, AssociationRJ, AssociationRJResult, Pdu,
    PresentationContextNegotiated, PresentationContextResult, PresentationContextResultReason,
    UserVariableItem, DEFAULT_IO_TIMEOUT, DEFAULT_MAX_PDU, MAX_PDU_LENGTH_CEILING,
};

const APPLICATION_CONTEXT_NAME: &str = "1.2.840.10008.3.1.1.1";
const PROTOCOL_VERSION: u16 = 1;
const IMPLICIT_VR_LE: &str = "1.2.840.10008.1.2";

/// Builder for a server (SCP-side) association, negotiated over an already-accepted
/// [`TcpStream`].
#[derive(Debug, Clone)]
pub struct ServerAssociationOptions {
    ae_title: String,
    max_pdu_length: u32,
    read_timeout: Option<Duration>,
    write_timeout: Option<Duration>,
    /// Allow-listed abstract syntaxes. Empty + `promiscuous(true)` (production `scp.rs`'s
    /// mode) accepts any proposed abstract syntax. Non-empty (the test harness's mode, via
    /// `with_abstract_syntax`) restricts to exactly this set, regardless of `promiscuous`.
    abstract_syntax_uids: std::collections::HashSet<String>,
    promiscuous: bool,
    /// Allow-listed transfer syntaxes. Empty (production `scp.rs`'s mode - never configured)
    /// accepts the first transfer syntax per context the registry supports. Non-empty (the
    /// test harness's mode, via `with_transfer_syntax`) additionally requires membership here.
    transfer_syntax_uids: std::collections::HashSet<String>,
}

impl Default for ServerAssociationOptions {
    fn default() -> Self {
        ServerAssociationOptions {
            ae_title: "ANY-SCP".to_owned(),
            max_pdu_length: DEFAULT_MAX_PDU,
            read_timeout: Some(DEFAULT_IO_TIMEOUT),
            write_timeout: Some(DEFAULT_IO_TIMEOUT),
            abstract_syntax_uids: std::collections::HashSet::new(),
            promiscuous: false,
            transfer_syntax_uids: std::collections::HashSet::new(),
        }
    }
}

impl ServerAssociationOptions {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn ae_title(mut self, title: impl Into<String>) -> Self {
        self.ae_title = title.into();
        self
    }

    /// Restrict accepted associations to proposals whose abstract syntax is in this
    /// allow-list (repeatable). Overrides `promiscuous` for whichever context IDs it applies
    /// to - matches `dicom-ul`'s own precedence (an explicit allow-list is always checked
    /// first; `promiscuous` only matters when the allow-list is empty).
    pub fn with_abstract_syntax(mut self, abstract_syntax_uid: impl Into<String>) -> Self {
        self.abstract_syntax_uids.insert(abstract_syntax_uid.into());
        self
    }

    /// Restrict accepted transfer syntaxes to this allow-list (repeatable), in addition to
    /// requiring registry support. Empty (the default) accepts any registry-supported transfer
    /// syntax the peer proposes.
    pub fn with_transfer_syntax(mut self, transfer_syntax_uid: impl Into<String>) -> Self {
        self.transfer_syntax_uids.insert(transfer_syntax_uid.into());
        self
    }

    /// When no abstract-syntax allow-list is configured, accept any proposed abstract syntax
    /// (still negotiates transfer syntax per context against the registry - see `choose_ts`'s
    /// call site below). `dcmnorm`'s production SCP (`scp.rs`) always sets this; the test
    /// harness instead uses `with_abstract_syntax` to constrain the mock SCP to exactly what
    /// each test expects the SCU to propose.
    pub fn promiscuous(mut self, value: bool) -> Self {
        self.promiscuous = value;
        self
    }

    /// Clamped to [`MAX_PDU_LENGTH_CEILING`] regardless of what's requested - see that
    /// constant's doc comment for why.
    pub fn max_pdu_length(mut self, len: u32) -> Self {
        self.max_pdu_length = len.min(MAX_PDU_LENGTH_CEILING);
        self
    }

    pub fn read_timeout(mut self, timeout: Duration) -> Self {
        self.read_timeout = Some(timeout);
        self
    }

    pub fn write_timeout(mut self, timeout: Duration) -> Self {
        self.write_timeout = Some(timeout);
        self
    }

    /// Negotiate an association over an already-accepted TCP connection.
    pub fn establish(self, stream: TcpStream) -> Result<ServerAssociation<TcpStream>> {
        stream.set_read_timeout(self.read_timeout).map_err(Error::Io)?;
        stream.set_write_timeout(self.write_timeout).map_err(Error::Io)?;
        self.establish_over(stream)
    }

    fn establish_over(self, mut stream: TcpStream) -> Result<ServerAssociation<TcpStream>> {
        // The requestor's Maximum-length-received isn't known yet at this point, so the first
        // read (the A-ASSOCIATE-RQ itself) is bounded by our own advertised ceiling - matches
        // `dicom-ul`'s own behavior (see `ScpOptions::max_pdu_length`'s doc comment in
        // `scp.rs`: this can't be chosen per-requestor, so it must be generous up front).
        let rq = conn::receive_pdu(&mut stream, self.max_pdu_length)?;

        let rq = match rq {
            Pdu::AssociationRQ(rq) => rq,
            other => {
                let _ = conn::send_pdu(
                    &mut stream,
                    &Pdu::AbortRQ { source: AbortRQSource::UNEXPECTED_PDU },
                );
                return Err(Error::UnexpectedPdu(Box::new(other)));
            }
        };

        if rq.protocol_version != PROTOCOL_VERSION {
            let rj = AssociationRJ {
                result: AssociationRJResult::Permanent,
                source: 1, // service-user
                reason: 1, // no-reason-given
            };
            let _ = conn::send_pdu(&mut stream, &Pdu::AssociationRJ(rj.clone()));
            return Err(Error::Rejected(rj));
        }
        if rq.application_context_name != APPLICATION_CONTEXT_NAME {
            let rj = AssociationRJ {
                result: AssociationRJResult::Permanent,
                source: 1, // service-user
                reason: 2, // application-context-name-not-supported
            };
            let _ = conn::send_pdu(&mut stream, &Pdu::AssociationRJ(rj.clone()));
            return Err(Error::Rejected(rj));
        }

        let requestor_max_pdu_length = rq
            .user_variables
            .iter()
            .find_map(|v| match v {
                UserVariableItem::MaxLength(len) => Some(*len),
                _ => None,
            })
            .unwrap_or(DEFAULT_MAX_PDU);
        let requestor_max_pdu_length =
            if requestor_max_pdu_length == 0 { u32::MAX } else { requestor_max_pdu_length };

        // Promiscuous: accept every proposed abstract syntax; per context, accept the first
        // proposed transfer syntax this build's registry can actually decode/write - same
        // policy as `dicom_ul::association::server::choose_supported` (no transfer-syntax
        // allow-list configured, so `dcmnorm`'s SCP always took this branch already).
        let presentation_contexts: Vec<PresentationContextNegotiated> = rq
            .presentation_contexts
            .iter()
            .map(|pc| {
                let abstract_syntax_ok = self.abstract_syntax_uids.is_empty()
                    || self.abstract_syntax_uids.contains(&pc.abstract_syntax)
                    || self.abstract_syntax_uids.contains(pc.abstract_syntax.trim_end_matches(['\0', ' ']));
                if !abstract_syntax_ok && !self.promiscuous {
                    return PresentationContextNegotiated {
                        id: pc.id,
                        reason: PresentationContextResultReason::AbstractSyntaxNotSupported,
                        transfer_syntax: IMPLICIT_VR_LE.to_owned(),
                        abstract_syntax: pc.abstract_syntax.clone(),
                    };
                }

                let chosen = pc
                    .transfer_syntaxes
                    .iter()
                    .find(|ts| {
                        let trimmed = ts.trim_end_matches(['\0', ' ']);
                        (self.transfer_syntax_uids.is_empty()
                            || self.transfer_syntax_uids.contains(trimmed))
                            && TransferSyntaxRegistry
                                .get(trimmed)
                                .is_some_and(|ts| !ts.is_unsupported())
                    })
                    .cloned();
                match chosen {
                    Some(ts) => PresentationContextNegotiated {
                        id: pc.id,
                        reason: PresentationContextResultReason::Acceptance,
                        transfer_syntax: ts,
                        abstract_syntax: pc.abstract_syntax.clone(),
                    },
                    None => PresentationContextNegotiated {
                        id: pc.id,
                        reason: PresentationContextResultReason::TransferSyntaxesNotSupported,
                        transfer_syntax: IMPLICIT_VR_LE.to_owned(),
                        abstract_syntax: pc.abstract_syntax.clone(),
                    },
                }
            })
            .collect();

        let ac = Pdu::AssociationAC(AssociationAC {
            protocol_version: PROTOCOL_VERSION,
            calling_ae_title: rq.calling_ae_title.clone(),
            called_ae_title: rq.called_ae_title.clone(),
            application_context_name: APPLICATION_CONTEXT_NAME.to_owned(),
            presentation_contexts: presentation_contexts
                .iter()
                .map(|pc| PresentationContextResult {
                    id: pc.id,
                    reason: pc.reason,
                    transfer_syntax: pc.transfer_syntax.clone(),
                })
                .collect(),
            user_variables: vec![
                UserVariableItem::MaxLength(self.max_pdu_length),
                UserVariableItem::ImplementationClassUID(
                    crate::IMPLEMENTATION_CLASS_UID.to_owned(),
                ),
                UserVariableItem::ImplementationVersionName(
                    crate::IMPLEMENTATION_VERSION_NAME.to_owned(),
                ),
            ],
        });
        conn::send_pdu(&mut stream, &ac)?;

        Ok(ServerAssociation {
            stream,
            requestor_max_pdu_length,
            acceptor_max_pdu_length: self.max_pdu_length,
            presentation_contexts,
            peer_ae_title: rq.calling_ae_title,
        })
    }
}

/// An established association from the accepting side.
#[derive(Debug)]
pub struct ServerAssociation<S> {
    stream: S,
    requestor_max_pdu_length: u32,
    acceptor_max_pdu_length: u32,
    /// Every negotiated context, accepted or not - unlike the client side, worth surfacing in
    /// full so a caller can log why a context wasn't usable (see this crate's module doc and
    /// `scp.rs`'s own comment about production incident 2026-07-28).
    presentation_contexts: Vec<PresentationContextNegotiated>,
    peer_ae_title: String,
}

impl<S: std::io::Read + std::io::Write> ServerAssociation<S> {
    pub fn presentation_contexts(&self) -> &[PresentationContextNegotiated] {
        &self.presentation_contexts
    }

    pub fn peer_ae_title(&self) -> &str {
        &self.peer_ae_title
    }

    pub fn send(&mut self, pdu: &Pdu) -> Result<()> {
        conn::send_pdu(&mut self.stream, pdu)
    }

    pub fn receive(&mut self) -> Result<Pdu> {
        conn::receive_pdu(&mut self.stream, self.acceptor_max_pdu_length)
    }

    pub fn send_pdata(&mut self, presentation_context_id: u8) -> crate::pdata::PDataWriter<&mut S> {
        crate::pdata::PDataWriter::new(
            &mut self.stream,
            presentation_context_id,
            crate::pdu::PDataValueType::Data,
            self.requestor_max_pdu_length,
        )
    }

    pub fn receive_pdata(&mut self) -> crate::pdata::PDataReader<&mut S> {
        crate::pdata::PDataReader::new(&mut self.stream, self.acceptor_max_pdu_length)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn max_pdu_length_is_clamped_to_the_ceiling_regardless_of_request() {
        let opts = ServerAssociationOptions::new().max_pdu_length(u32::MAX);
        assert_eq!(opts.max_pdu_length, MAX_PDU_LENGTH_CEILING);
    }

    #[test]
    fn defaults_apply_a_read_and_write_timeout_rather_than_blocking_forever() {
        let opts = ServerAssociationOptions::new();
        assert_eq!(opts.read_timeout, Some(DEFAULT_IO_TIMEOUT));
        assert_eq!(opts.write_timeout, Some(DEFAULT_IO_TIMEOUT));
    }
}
