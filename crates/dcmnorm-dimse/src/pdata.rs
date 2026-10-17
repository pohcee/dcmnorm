//! [`PDataWriter`]: a `std::io::Write` adapter that transparently splits a byte stream across as many P-DATA-TF PDUs as
//! needed, so callers can stream a large command/data set without manually chunking it to the negotiated max PDU length.
//! Ported from `dicom-ul`'s `association::pdata`. Receiving is message-level, in `message.rs`.

use std::io::{self, Write};

use crate::conn;
use crate::pdu::{PDataValueType, Pdu, PDU_HEADER_SIZE, PDV_HEADER_SIZE};

/// Writes bytes as one or more P-DATA-TF PDUs on the given presentation context, splitting at
/// the negotiated max PDU length. The final (possibly partial) PDU is flushed on [`Self::finish`]
/// or [`Drop`].
pub struct PDataWriter<W: Write> {
    stream: W,
    presentation_context_id: u8,
    value_type: PDataValueType,
    max_data_len: usize,
    buffer: Vec<u8>,
}

impl<W: Write> PDataWriter<W> {
    pub(crate) fn new(
        stream: W,
        presentation_context_id: u8,
        value_type: PDataValueType,
        max_pdu_length: u32,
    ) -> Self {
        // Every PDV must fit within one P-DATA-TF PDU: PDU header (6) + PDV item header (6,
        // counted once more since PDV_HEADER_SIZE already covers item-length + context-id +
        // message-control-header) + the fragment bytes themselves.
        let max_data_len =
            max_pdu_length.saturating_sub(PDU_HEADER_SIZE + PDV_HEADER_SIZE).max(1) as usize;
        PDataWriter {
            stream,
            presentation_context_id,
            value_type,
            max_data_len,
            buffer: Vec::with_capacity(max_data_len.min(crate::pdu::LARGE_PDU_SIZE as usize)),
        }
    }

    fn dispatch(&mut self, is_last: bool) -> io::Result<()> {
        let pdv = crate::pdu::PDataValue {
            presentation_context_id: self.presentation_context_id,
            value_type: self.value_type,
            is_last,
            data: std::mem::take(&mut self.buffer),
        };
        conn::send_pdu(&mut self.stream, &Pdu::PData { data: vec![pdv] })
            .map_err(|e| io::Error::other(e.to_string()))
    }

    /// Flush any buffered bytes as the final PDV of this stream.
    pub fn finish(mut self) -> io::Result<()> {
        self.finish_impl()
    }

    fn finish_impl(&mut self) -> io::Result<()> {
        // Always send a final PDU, even an empty one - an empty write (e.g. a zero-length
        // command) must still produce at least one is_last PDV for the peer to see.
        self.dispatch(true)
    }
}

impl<W: Write> Write for PDataWriter<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let mut written = 0;
        while written < buf.len() {
            let room = self.max_data_len - self.buffer.len();
            let take = room.min(buf.len() - written);
            self.buffer.extend_from_slice(&buf[written..written + take]);
            written += take;
            if self.buffer.len() == self.max_data_len {
                self.dispatch(false)?;
            }
        }
        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl<W: Write> Drop for PDataWriter<W> {
    fn drop(&mut self) {
        let _ = self.finish_impl();
    }
}

