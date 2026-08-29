//! The DICOM Upper Layer association from the requesting (SCU) side: [`ClientAssociationOptions`]
//! (builder) and [`ClientAssociation`] (the established connection).

use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;

use crate::conn;
use crate::error::{Error, Result};
use crate::pdu::{
    AbortRQSource, AssociationRQ, Pdu, PresentationContextNegotiated,
    PresentationContextProposed, PresentationContextResultReason, UserVariableItem,
    DEFAULT_MAX_PDU,
};

const APPLICATION_CONTEXT_NAME: &str = "1.2.840.10008.3.1.1.1";
const PROTOCOL_VERSION: u16 = 1;
const IMPLEMENTATION_CLASS_UID: &str = crate::IMPLEMENTATION_CLASS_UID;
const IMPLEMENTATION_VERSION_NAME: &str = crate::IMPLEMENTATION_VERSION_NAME;

/// Builder for a client (SCU-side) association.
#[derive(Debug, Clone)]
pub struct ClientAssociationOptions {
    calling_ae_title: String,
    called_ae_title: Option<String>,
    max_pdu_length: u32,
    presentation_contexts: Vec<(String, Vec<String>)>,
    read_timeout: Option<Duration>,
    write_timeout: Option<Duration>,
    connection_timeout: Option<Duration>,
}

impl Default for ClientAssociationOptions {
    fn default() -> Self {
        ClientAssociationOptions {
            calling_ae_title: "DCMNORM".to_owned(),
            called_ae_title: None,
            max_pdu_length: DEFAULT_MAX_PDU,
            presentation_contexts: Vec::new(),
            read_timeout: None,
            write_timeout: None,
            connection_timeout: None,
        }
    }
}

impl ClientAssociationOptions {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn calling_ae_title(mut self, title: impl Into<String>) -> Self {
        self.calling_ae_title = title.into();
        self
    }

    pub fn called_ae_title(mut self, title: impl Into<String>) -> Self {
        self.called_ae_title = Some(title.into());
        self
    }

    pub fn max_pdu_length(mut self, len: u32) -> Self {
        self.max_pdu_length = len;
        self
    }

    pub fn with_presentation_context(
        mut self,
        abstract_syntax: impl Into<String>,
        transfer_syntaxes: impl Into<Vec<String>>,
    ) -> Self {
        self.presentation_contexts.push((abstract_syntax.into(), transfer_syntaxes.into()));
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

    pub fn connection_timeout(mut self, timeout: Duration) -> Self {
        self.connection_timeout = Some(timeout);
        self
    }

    /// Connect to `address` and negotiate an association.
    pub fn establish_with(self, address: &str) -> Result<ClientAssociation<TcpStream>> {
        let addrs: Vec<_> = address
            .to_socket_addrs()
            .map_err(Error::Connect)?
            .collect();
        if addrs.is_empty() {
            return Err(Error::Connect(std::io::Error::new(
                std::io::ErrorKind::AddrNotAvailable,
                "no addresses resolved",
            )));
        }

        let stream = if let Some(timeout) = self.connection_timeout {
            let mut last_err = None;
            let mut connected = None;
            for addr in &addrs {
                match TcpStream::connect_timeout(addr, timeout) {
                    Ok(s) => {
                        connected = Some(s);
                        break;
                    }
                    Err(e) => last_err = Some(e),
                }
            }
            connected.ok_or_else(|| {
                Error::Connect(last_err.unwrap_or_else(|| {
                    std::io::Error::new(std::io::ErrorKind::TimedOut, "connection timed out")
                }))
            })?
        } else {
            TcpStream::connect(&addrs[..]).map_err(Error::Connect)?
        };

        stream.set_read_timeout(self.read_timeout).map_err(Error::Io)?;
        stream.set_write_timeout(self.write_timeout).map_err(Error::Io)?;

        self.establish_over(stream)
    }

    fn establish_over(self, mut stream: TcpStream) -> Result<ClientAssociation<TcpStream>> {
        if self.presentation_contexts.is_empty() {
            return Err(Error::MissingPresentationContexts);
        }

        let proposed: Vec<PresentationContextProposed> = self
            .presentation_contexts
            .iter()
            .enumerate()
            .map(|(i, (abstract_syntax, transfer_syntaxes))| PresentationContextProposed {
                id: (2 * i + 1) as u8,
                abstract_syntax: abstract_syntax.clone(),
                transfer_syntaxes: transfer_syntaxes.clone(),
            })
            .collect();

        let called_ae_title = self.called_ae_title.clone().unwrap_or_else(|| "ANY-SCP".to_owned());

        let rq = Pdu::AssociationRQ(AssociationRQ {
            protocol_version: PROTOCOL_VERSION,
            calling_ae_title: self.calling_ae_title.clone(),
            called_ae_title: called_ae_title.clone(),
            application_context_name: APPLICATION_CONTEXT_NAME.to_owned(),
            presentation_contexts: proposed.clone(),
            user_variables: vec![
                UserVariableItem::MaxLength(self.max_pdu_length),
                UserVariableItem::ImplementationClassUID(IMPLEMENTATION_CLASS_UID.to_owned()),
                UserVariableItem::ImplementationVersionName(IMPLEMENTATION_VERSION_NAME.to_owned()),
            ],
        });

        conn::send_pdu(&mut stream, &rq)?;
        let resp = conn::receive_pdu(&mut stream, self.max_pdu_length);

        let resp = match resp {
            Ok(pdu) => pdu,
            Err(e) => {
                let _ = conn::send_pdu(
                    &mut stream,
                    &Pdu::AbortRQ { source: AbortRQSource::SERVICE_USER },
                );
                return Err(e);
            }
        };

        match resp {
            Pdu::AssociationAC(ac) => {
                if ac.protocol_version != PROTOCOL_VERSION {
                    let _ = conn::send_pdu(
                        &mut stream,
                        &Pdu::AbortRQ { source: AbortRQSource::SERVICE_USER },
                    );
                    return Err(Error::ProtocolVersionMismatch {
                        expected: PROTOCOL_VERSION,
                        got: ac.protocol_version,
                    });
                }

                let acceptor_max_pdu_length = ac
                    .user_variables
                    .iter()
                    .find_map(|v| match v {
                        UserVariableItem::MaxLength(len) => Some(*len),
                        _ => None,
                    })
                    .unwrap_or(DEFAULT_MAX_PDU);
                let acceptor_max_pdu_length =
                    if acceptor_max_pdu_length == 0 { u32::MAX } else { acceptor_max_pdu_length };

                // Only accepted contexts are exposed to the caller - a context this side
                // proposed but the acceptor rejected isn't usable for anything, so there's
                // nothing meaningful to do with it here (unlike the server side, where every
                // negotiated outcome, accepted or not, is worth surfacing for logging - see
                // `server.rs`).
                let presentation_contexts: Vec<PresentationContextNegotiated> = ac
                    .presentation_contexts
                    .into_iter()
                    .filter(|pc| pc.reason == PresentationContextResultReason::Acceptance)
                    .filter_map(|pc| {
                        let abstract_syntax =
                            proposed.iter().find(|p| p.id == pc.id)?.abstract_syntax.clone();
                        Some(PresentationContextNegotiated {
                            id: pc.id,
                            reason: pc.reason,
                            transfer_syntax: pc.transfer_syntax,
                            abstract_syntax,
                        })
                    })
                    .collect();

                if presentation_contexts.is_empty() {
                    let _ = conn::send_pdu(
                        &mut stream,
                        &Pdu::AbortRQ { source: AbortRQSource::SERVICE_USER },
                    );
                    return Err(Error::NoAcceptedPresentationContexts);
                }

                Ok(ClientAssociation {
                    stream,
                    requestor_max_pdu_length: self.max_pdu_length,
                    acceptor_max_pdu_length,
                    presentation_contexts,
                })
            }
            Pdu::AssociationRJ(rj) => Err(Error::Rejected(rj)),
            other => {
                let _ = conn::send_pdu(
                    &mut stream,
                    &Pdu::AbortRQ { source: AbortRQSource::SERVICE_USER },
                );
                Err(Error::UnexpectedPdu(Box::new(other)))
            }
        }
    }
}

/// An established association from the requesting side.
#[derive(Debug)]
pub struct ClientAssociation<S> {
    stream: S,
    requestor_max_pdu_length: u32,
    acceptor_max_pdu_length: u32,
    presentation_contexts: Vec<PresentationContextNegotiated>,
}

impl<S: std::io::Read + std::io::Write> ClientAssociation<S> {
    pub fn presentation_contexts(&self) -> &[PresentationContextNegotiated] {
        &self.presentation_contexts
    }

    pub fn acceptor_max_pdu_length(&self) -> u32 {
        self.acceptor_max_pdu_length
    }

    pub fn requestor_max_pdu_length(&self) -> u32 {
        self.requestor_max_pdu_length
    }

    pub fn send(&mut self, pdu: &Pdu) -> Result<()> {
        conn::send_pdu(&mut self.stream, pdu)
    }

    pub fn receive(&mut self) -> Result<Pdu> {
        conn::receive_pdu(&mut self.stream, self.requestor_max_pdu_length)
    }

    pub fn send_pdata(&mut self, presentation_context_id: u8) -> crate::pdata::PDataWriter<&mut S> {
        crate::pdata::PDataWriter::new(
            &mut self.stream,
            presentation_context_id,
            crate::pdu::PDataValueType::Data,
            self.acceptor_max_pdu_length,
        )
    }

    pub fn receive_pdata(&mut self) -> crate::pdata::PDataReader<&mut S> {
        crate::pdata::PDataReader::new(&mut self.stream, self.requestor_max_pdu_length)
    }

    pub fn inner_stream(&mut self) -> &mut S {
        &mut self.stream
    }

    /// Gracefully release the association: A-RELEASE-RQ, wait for A-RELEASE-RP.
    pub fn release(mut self) -> Result<()> {
        self.send(&Pdu::ReleaseRQ)?;
        match self.receive()? {
            Pdu::ReleaseRP => Ok(()),
            other => Err(Error::UnexpectedPdu(Box::new(other))),
        }
    }

    /// Abruptly terminate the association: A-ABORT, no reply expected.
    pub fn abort(mut self) -> Result<()> {
        self.send(&Pdu::AbortRQ { source: AbortRQSource::SERVICE_USER })
    }
}
