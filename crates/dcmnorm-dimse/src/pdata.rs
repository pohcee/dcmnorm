//! [`PDataWriter`]: a `std::io::Write` adapter that transparently splits a byte stream across as many P-DATA-TF PDUs as
//! needed, so callers can stream a large command/data set without manually chunking it to the negotiated max PDU length.
//! Ported from `dicom-ul`'s `association::pdata`. Receiving is message-level, in `message.rs`.

use std::io::{self, Write};

use crate::conn;
use crate::pdu::{PDataValueType, Pdu, PDU_HEADER_SIZE, PDV_HEADER_SIZE};

/// Local ceiling on outbound PDV fragment size so peers advertising unlimited max PDU (`0` ->
/// `u32::MAX`) still receive bounded streaming PDUs instead of buffering multi-gigabyte payloads.
/// Even so it always satisfies PS3.8 Annex E's even-length fragment rule.
pub const MAX_OUTBOUND_PDV_DATA: usize = 1024 * 1024;

/// Writes bytes as one or more P-DATA-TF PDUs on the given presentation context, splitting at
/// the negotiated max PDU length (clamped to [`MAX_OUTBOUND_PDV_DATA`] and rounded down to an even
/// byte count per PS3.8 Annex E). Call [`Self::finish`] to send the final `is_last = true` PDV;
/// dropping the writer without calling `finish` (or calling [`Self::abort`]) abandons the stream
/// without emitting a false end-of-dataset marker.
pub struct PDataWriter<W: Write> {
    stream: W,
    presentation_context_id: u8,
    value_type: PDataValueType,
    max_data_len: usize,
    buffer: Vec<u8>,
    finished: bool,
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
        // message-control-header) + the fragment bytes themselves. PS3.8 Annex E requires each
        // fragment to carry an even number of bytes; cap at MAX_OUTBOUND_PDV_DATA so an unlimited
        // peer max PDU (0 -> u32::MAX) still streams with bounded memory.
        let raw_max = max_pdu_length.saturating_sub(PDU_HEADER_SIZE + PDV_HEADER_SIZE) as usize;
        let max_data_len = (raw_max.min(MAX_OUTBOUND_PDV_DATA) & !1).max(2);
        PDataWriter {
            stream,
            presentation_context_id,
            value_type,
            max_data_len,
            buffer: Vec::with_capacity(max_data_len.min(crate::pdu::LARGE_PDU_SIZE as usize)),
            finished: false,
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

    /// Flush any buffered bytes as the final `is_last = true` PDV of this stream.
    pub fn finish(mut self) -> io::Result<()> {
        if self.finished {
            return Ok(());
        }
        self.finished = true;
        // Always send a final PDU, even an empty one - an empty write (e.g. a zero-length
        // command) must still produce at least one is_last PDV for the peer to see.
        self.dispatch(true)
    }

    /// Explicitly abandon an in-progress stream without sending a final `is_last = true` PDV.
    pub fn abort(mut self) {
        self.finished = true;
        self.buffer.clear();
    }
}

impl<W: Write> Write for PDataWriter<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let mut written = 0;
        while written < buf.len() {
            // Flush a full buffer only when more bytes arrive, so a payload whose length is an
            // exact multiple of max_data_len ends on one final `is_last = true` PDU instead of
            // emitting a trailing empty PDV.
            if self.buffer.len() == self.max_data_len {
                self.dispatch(false)?;
            }
            let room = self.max_data_len - self.buffer.len();
            let take = room.min(buf.len() - written);
            self.buffer.extend_from_slice(&buf[written..written + take]);
            written += take;
        }
        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn finish_sends_single_final_pdv_and_drop_does_not_duplicate() {
        let mut wire = Vec::new();
        {
            let mut w = PDataWriter::new(&mut wire, 1, PDataValueType::Data, 64);
            // 64 - 12 = 52 bytes per PDV; write exactly 52 bytes.
            w.write_all(&[0xAB; 52]).unwrap();
            w.finish().unwrap();
        }
        let mut cursor = Cursor::new(wire);
        let pdu = conn::receive_pdu(&mut cursor, 1024).unwrap();
        match pdu {
            Pdu::PData { data } => {
                assert_eq!(data.len(), 1);
                assert!(data[0].is_last);
                assert_eq!(data[0].data.len(), 52);
            }
            other => panic!("expected PData, got {other:?}"),
        }
        assert_eq!(cursor.position() as usize, cursor.into_inner().len());
    }

    #[test]
    fn drop_without_finish_does_not_send_final_pdv() {
        let mut wire = Vec::new();
        {
            let mut w = PDataWriter::new(&mut wire, 1, PDataValueType::Data, 64);
            // 52 bytes fills buffer; writing 60 dispatches one non-final PDV and buffers 8 bytes.
            w.write_all(&[0xCD; 60]).unwrap();
            // Simulate mid-stream error dropping `w` without calling `finish()`.
        }
        let mut cursor = Cursor::new(wire);
        let pdu = conn::receive_pdu(&mut cursor, 1024).unwrap();
        match pdu {
            Pdu::PData { data } => {
                assert_eq!(data.len(), 1);
                assert!(
                    !data[0].is_last,
                    "dropped writer must not emit is_last=true"
                );
                assert_eq!(data[0].data.len(), 52);
            }
            other => panic!("expected PData, got {other:?}"),
        }
        assert_eq!(
            cursor.position() as usize,
            cursor.into_inner().len(),
            "no trailing final PDV should be emitted on drop"
        );
    }

    #[test]
    fn odd_and_unlimited_max_pdu_lengths_produce_even_bounded_fragments() {
        // Odd peer max PDU (65 -> 53 raw -> rounded down to even 52).
        let mut wire = Vec::new();
        {
            let mut w = PDataWriter::new(&mut wire, 1, PDataValueType::Data, 65);
            w.write_all(&[0x11; 60]).unwrap();
            w.finish().unwrap();
        }
        let mut cursor = Cursor::new(wire);
        let pdu1 = conn::receive_pdu(&mut cursor, 1024).unwrap();
        let pdu2 = conn::receive_pdu(&mut cursor, 1024).unwrap();
        match (pdu1, pdu2) {
            (Pdu::PData { data: d1 }, Pdu::PData { data: d2 }) => {
                assert_eq!(d1[0].data.len(), 52);
                assert_eq!(d1[0].data.len() % 2, 0);
                assert!(!d1[0].is_last);
                assert_eq!(d2[0].data.len(), 8);
                assert!(d2[0].is_last);
            }
            other => panic!("expected two PData PDUs, got {other:?}"),
        }

        // Unlimited peer max PDU (u32::MAX) is capped at MAX_OUTBOUND_PDV_DATA (1 MiB).
        let mut big_wire = Vec::new();
        {
            let mut w = PDataWriter::new(&mut big_wire, 1, PDataValueType::Data, u32::MAX);
            assert_eq!(w.max_data_len, MAX_OUTBOUND_PDV_DATA);
            w.write_all(&vec![0x22; MAX_OUTBOUND_PDV_DATA + 16])
                .unwrap();
            w.finish().unwrap();
        }
        let mut big_cursor = Cursor::new(big_wire);
        let p1 = conn::receive_pdu(&mut big_cursor, (MAX_OUTBOUND_PDV_DATA as u32) + 64).unwrap();
        let p2 = conn::receive_pdu(&mut big_cursor, (MAX_OUTBOUND_PDV_DATA as u32) + 64).unwrap();
        match (p1, p2) {
            (Pdu::PData { data: d1 }, Pdu::PData { data: d2 }) => {
                assert_eq!(d1[0].data.len(), MAX_OUTBOUND_PDV_DATA);
                assert!(!d1[0].is_last);
                assert_eq!(d2[0].data.len(), 16);
                assert!(d2[0].is_last);
            }
            other => panic!("expected two bounded PData PDUs, got {other:?}"),
        }
    }
}
