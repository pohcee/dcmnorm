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

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    /// A hostile peer can send a 6-byte PDU header declaring an arbitrary length. This must be
    /// rejected before `vec![0u8; length]` is ever allocated - proves the check at the top of
    /// this function actually runs before the allocation, not after.
    #[test]
    fn receive_pdu_rejects_declared_length_exceeding_max_pdu_length() {
        let max_pdu_length = 1024u32;
        // PDU type 0x04 (P-DATA-TF), reserved byte, length = way beyond max_pdu_length.
        let mut header = vec![0x04, 0x00];
        header.extend_from_slice(&(max_pdu_length + 1).to_be_bytes());
        let mut stream = Cursor::new(header); // no body bytes at all follow

        let result = receive_pdu(&mut stream, max_pdu_length);
        assert!(
            matches!(result, Err(Error::Pdu(pdu::Error::PduTooLarge { .. }))),
            "expected PduTooLarge, got {result:?}"
        );
    }

    /// Even a length within the allowed ceiling must be rejected cleanly (not panic) if the
    /// stream doesn't actually contain that many bytes.
    #[test]
    fn receive_pdu_errors_cleanly_on_truncated_body() {
        let max_pdu_length = 1024u32;
        let mut header = vec![0x04, 0x00];
        header.extend_from_slice(&100u32.to_be_bytes()); // declares 100 bytes of body
        header.extend_from_slice(b"only 10ba"); // far fewer than declared
        let mut stream = Cursor::new(header);

        let result = receive_pdu(&mut stream, max_pdu_length);
        assert!(matches!(result, Err(Error::Io(_))), "expected a clean I/O error, got {result:?}");
    }
}
