//! The shared PDU send/receive transport, used by both [`crate::client::ClientAssociation`] and
//! [`crate::server::ServerAssociation`] once an association is established. Reads a PDU by
//! blocking on exactly the 6-byte header, then exactly `length` more bytes, then parsing that
//! complete buffer - simpler than `dicom-ul`'s incremental buffer-fill loop, which exists there
//! to share code with an async reader dcmnorm doesn't need (see `pdu.rs`'s doc comment).

use std::io::{Read, Write};

use crate::error::{Error, Result};
use crate::pdu::{self, Pdu, PDU_HEADER_SIZE};

pub(crate) fn send_pdu<W: Write>(stream: &mut W, pdu: &Pdu) -> Result<()> {
    let mut buf = Vec::new();
    pdu::write_pdu(&mut buf, pdu)?;
    stream.write_all(&buf)?;
    Ok(())
}

/// Read exactly one PDU from `stream`, enforcing `max_pdu_length` (our own receive ceiling) on
/// the declared PDU length up front, before attempting to read/allocate the body.
pub(crate) fn receive_pdu<R: Read>(stream: &mut R, max_pdu_length: u32) -> Result<Pdu> {
    let mut header = [0u8; PDU_HEADER_SIZE as usize];
    stream.read_exact(&mut header).map_err(|e| {
        if e.kind() == std::io::ErrorKind::UnexpectedEof {
            // A clean close with no bytes at all reads the same as a genuine protocol error
            // here; callers that need to distinguish "peer hung up between PDUs" from "peer
            // sent a garbled header" already treat any receive() error as connection loss.
            Error::Io(e)
        } else {
            Error::Io(e)
        }
    })?;
    let pdu_type = header[0];
    let length = u32::from_be_bytes([header[2], header[3], header[4], header[5]]);

    if length > max_pdu_length {
        return Err(Error::Pdu(pdu::Error::PduTooLarge { pdu_length: length, max_pdu_length }));
    }

    let mut body = vec![0u8; length as usize];
    stream.read_exact(&mut body)?;
    Ok(pdu::parse_pdu_body(pdu_type, &body)?)
}
