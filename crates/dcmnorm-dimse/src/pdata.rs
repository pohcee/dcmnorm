//! [`PDataWriter`]/[`PDataReader`]: `std::io::Write`/`Read` adapters that transparently split (or
//! reassemble) a byte stream across as many P-DATA-TF PDUs as needed, so callers can stream a
//! large command/data set without manually chunking it to the negotiated max PDU length. Ported
//! from `dicom-ul`'s `association::pdata`, simplified to plain blocking reads (see `conn.rs`'s
//! doc comment for why the incremental-buffer-fill dance isn't needed for a sync-only stream).

use std::collections::VecDeque;
use std::io::{self, Read, Write};

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

/// Reads bytes reassembled from one or more incoming P-DATA-TF PDUs on a single presentation
/// context, until the peer marks its final PDV `is_last`.
pub struct PDataReader<R> {
    stream: R,
    max_pdu_length: u32,
    buffer: VecDeque<u8>,
    presentation_context_id: Option<u8>,
    done: bool,
}

impl<R: Read> PDataReader<R> {
    pub(crate) fn new(stream: R, max_pdu_length: u32) -> Self {
        PDataReader {
            stream,
            max_pdu_length,
            buffer: VecDeque::new(),
            presentation_context_id: None,
            done: false,
        }
    }
}

impl<R: Read> Read for PDataReader<R> {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if self.buffer.is_empty() {
            if self.done {
                return Ok(0);
            }
            let pdu = conn::receive_pdu(&mut self.stream, self.max_pdu_length)
                .map_err(|e| io::Error::other(e.to_string()))?;
            match pdu {
                Pdu::PData { data } => {
                    for pdv in data {
                        self.presentation_context_id.get_or_insert(pdv.presentation_context_id);
                        self.buffer.extend(pdv.data);
                        if pdv.is_last {
                            self.done = true;
                        }
                    }
                }
                other => {
                    return Err(io::Error::other(format!(
                        "expected P-DATA-TF while streaming, got {other:?}"
                    )));
                }
            }
        }

        // Bulk-copy from the deque's (up to two, ring-buffer-wrapped) contiguous slices instead
        // of popping one byte at a time - this is the streaming path for a whole received
        // C-STORE data set/image, so a per-byte loop is a meaningful cost on multi-MB payloads.
        let n = out.len().min(self.buffer.len());
        let (front, back) = self.buffer.as_slices();
        if n <= front.len() {
            out[..n].copy_from_slice(&front[..n]);
        } else {
            out[..front.len()].copy_from_slice(front);
            out[front.len()..n].copy_from_slice(&back[..n - front.len()]);
        }
        self.buffer.drain(..n);
        Ok(n)
    }
}
