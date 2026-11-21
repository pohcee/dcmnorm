//! Message-level receive for an established association: [`Receiver`] turns the incoming PDU stream into whole DIMSE
//! commands and data sets (PS3.7 6.3.1 / PS3.8 9.3.5, Annex E).
//!
//! A message is a command, optionally followed by a data set, each sent as one or more PDV fragments with the last one
//! flagged. PDU boundaries carry no meaning: one P-DATA-TF may hold a command PDV *and* the first fragment of its data set,
//! or the tail of one message and the start of the next (dcm4che packs PDVs up to the max PDU length; it sends a pending
//! C-FIND-RSP, its identifier and the final C-FIND-RSP in one PDU). Reading "one PDU = one command" or "the second PDV is the
//! whole data set" loses data against such peers, so every PDV is queued and consumed in order.
//!
//! All state lives here, not on the stack of a read call, so a read that fails with a timeout (`WouldBlock`/`TimedOut`,
//! which `dcmnorm`'s `poll_bounded` uses as its cancellation tick) can simply be retried: the bytes of a half-read PDU,
//! the queued PDVs and the fragments assembled so far are all kept.

use std::collections::VecDeque;
use std::io::{ErrorKind, Read, Write};

use crate::error::{Error, Result};
use crate::pdu::{self, PDataValue, PDataValueType, Pdu, PDU_HEADER_SIZE};

/// What [`Receiver::receive_command`] got: a complete command, or a non-P-DATA PDU (A-RELEASE-RQ, A-ABORT, ...) that arrived
/// between messages.
#[derive(Debug)]
pub enum Incoming {
    Command {
        presentation_context_id: u8,
        data: Vec<u8>,
    },
    Pdu(Pdu),
}

#[derive(Debug, Default)]
pub(crate) struct Receiver {
    /// The PDU being read: header + body, `filled` bytes valid. Sized to the PDU, so a stream is never read past the end
    /// of the current PDU.
    raw: Vec<u8>,
    filled: usize,
    /// PDVs received but not consumed yet (the rest of a PDU that held more than the current message).
    pending: VecDeque<PDataValue>,
    /// Fragments of the command or data set being assembled.
    partial: Vec<u8>,
    partial_context: Option<u8>,
}

enum Assembled {
    Message {
        presentation_context_id: u8,
        data: Vec<u8>,
    },
    Other(Pdu),
}

impl Receiver {
    /// The next PDU. Queued PDVs (left over from a PDU that also held an earlier message) come first, as one P-DATA-TF.
    pub(crate) fn receive_pdu<R: Read>(
        &mut self,
        stream: &mut R,
        max_pdu_length: u32,
    ) -> Result<Pdu> {
        if !self.pending.is_empty() {
            return Ok(Pdu::PData {
                data: self.pending.drain(..).collect(),
            });
        }
        self.read_pdu(stream, max_pdu_length)
    }

    /// The next complete command, or the PDU that arrived instead of one between messages.
    pub(crate) fn receive_command<R: Read>(
        &mut self,
        stream: &mut R,
        max_pdu_length: u32,
    ) -> Result<Incoming> {
        match self.assemble(stream, max_pdu_length, PDataValueType::Command)? {
            Assembled::Message {
                presentation_context_id,
                data,
            } => Ok(Incoming::Command {
                presentation_context_id,
                data,
            }),
            Assembled::Other(pdu) => Ok(Incoming::Pdu(pdu)),
        }
    }

    /// The complete data set that follows the command just received.
    pub(crate) fn receive_data_set<R: Read>(
        &mut self,
        stream: &mut R,
        max_pdu_length: u32,
    ) -> Result<Vec<u8>> {
        match self.assemble(stream, max_pdu_length, PDataValueType::Data)? {
            Assembled::Message { data, .. } => Ok(data),
            Assembled::Other(pdu) => Err(Error::UnexpectedPdu(Box::new(pdu))),
        }
    }

    /// Streams the data set following the command just received directly into `sink`, writing one
    /// PDV at a time without accumulating the entire dataset in memory. Preserves any trailing
    /// PDVs packed into the final P-DATA-TF PDU (e.g. the next DIMSE command) in `self.pending`.
    pub(crate) fn receive_data_set_to<R: Read, W: Write>(
        &mut self,
        stream: &mut R,
        max_pdu_length: u32,
        expected_pc_id: Option<u8>,
        sink: &mut W,
    ) -> Result<u64> {
        let mut total = 0u64;
        loop {
            if let Some(pdv) = self.pending.pop_front() {
                if pdv.value_type != PDataValueType::Data {
                    return Err(Error::UnexpectedPdv {
                        expected: PDataValueType::Data,
                        got: pdv.value_type,
                    });
                }
                if let Some(expected) = expected_pc_id {
                    if pdv.presentation_context_id != expected {
                        return Err(Error::UnexpectedPresentationContext {
                            expected,
                            got: pdv.presentation_context_id,
                        });
                    }
                }
                if !pdv.data.is_empty() {
                    sink.write_all(&pdv.data).map_err(Error::Io)?;
                    total += pdv.data.len() as u64;
                }
                if pdv.is_last {
                    return Ok(total);
                }
                continue;
            }
            match self.read_pdu(stream, max_pdu_length)? {
                Pdu::PData { data } => self.pending.extend(data),
                other => return Err(Error::UnexpectedPdu(Box::new(other))),
            }
        }
    }

    /// Returns a streaming [`std::io::Read`] over the Data PDVs of the data set following the
    /// command just received. Yields EOF once the PDV with `is_last == true` is exhausted,
    /// leaving any subsequent PDVs in `self.pending`.
    pub(crate) fn data_set_reader<'a, R: Read>(
        &'a mut self,
        stream: &'a mut R,
        max_pdu_length: u32,
        expected_pc_id: Option<u8>,
    ) -> PDataReader<'a, R> {
        PDataReader {
            receiver: self,
            stream,
            max_pdu_length,
            expected_pc_id,
            current: Vec::new(),
            pos: 0,
            done: false,
            wire_error: None,
        }
    }

    fn assemble<R: Read>(
        &mut self,
        stream: &mut R,
        max_pdu_length: u32,
        want: PDataValueType,
    ) -> Result<Assembled> {
        loop {
            if let Some(pdv) = self.pending.pop_front() {
                if pdv.value_type != want {
                    let got = pdv.value_type;
                    self.partial.clear();
                    self.partial_context = None;
                    return Err(Error::UnexpectedPdv {
                        expected: want,
                        got,
                    });
                }
                self.partial_context
                    .get_or_insert(pdv.presentation_context_id);
                if self.partial.is_empty() {
                    self.partial = pdv.data; // the common single-fragment case: no copy
                } else {
                    self.partial.extend_from_slice(&pdv.data);
                }
                if pdv.is_last {
                    let presentation_context_id = self
                        .partial_context
                        .take()
                        .unwrap_or(pdv.presentation_context_id);
                    return Ok(Assembled::Message {
                        presentation_context_id,
                        data: std::mem::take(&mut self.partial),
                    });
                }
                continue;
            }
            match self.read_pdu(stream, max_pdu_length)? {
                Pdu::PData { data } => self.pending.extend(data),
                // A release/abort is only valid between messages; in the middle of one the message is lost either way.
                other if self.partial.is_empty() && self.partial_context.is_none() => {
                    return Ok(Assembled::Other(other))
                }
                other => {
                    self.partial.clear();
                    self.partial_context = None;
                    return Err(Error::UnexpectedPdu(Box::new(other)));
                }
            }
        }
    }

    /// Reads one PDU, resumably: a read error (a timeout tick) keeps what was read so far for the next call.
    /// The declared length is checked against `max_pdu_length` before the body is allocated.
    fn read_pdu<R: Read>(&mut self, stream: &mut R, max_pdu_length: u32) -> Result<Pdu> {
        let header = PDU_HEADER_SIZE as usize;
        loop {
            let need = if self.filled < header {
                header
            } else {
                let length =
                    u32::from_be_bytes([self.raw[2], self.raw[3], self.raw[4], self.raw[5]]);
                if length > max_pdu_length {
                    self.raw.clear();
                    self.filled = 0;
                    return Err(Error::Pdu(pdu::Error::PduTooLarge {
                        pdu_length: length,
                        max_pdu_length,
                    }));
                }
                header + length as usize
            };
            if self.filled >= header && self.filled == need {
                let parsed = pdu::parse_pdu_body(self.raw[0], &self.raw[header..need]);
                self.raw.clear();
                self.filled = 0;
                return Ok(parsed?);
            }
            if self.raw.len() < need {
                self.raw.resize(need, 0);
            }
            match stream.read(&mut self.raw[self.filled..need]) {
                Ok(0) => return Err(Error::Io(std::io::Error::from(ErrorKind::UnexpectedEof))),
                Ok(n) => self.filled += n,
                Err(e) if e.kind() == ErrorKind::Interrupted => {}
                Err(e) => return Err(Error::Io(e)),
            }
        }
    }
}

/// Streaming [`std::io::Read`] over one DIMSE data set's incoming P-DATA-TF Data PDVs.
///
/// Buffers at most one PDV (bounded by `max_pdu_length`) at a time and yields EOF as soon as the
/// final PDV (`is_last == true`) has been consumed, leaving any trailing PDVs from the same PDU
/// untouched in the association's receive queue.
pub struct PDataReader<'a, R> {
    receiver: &'a mut Receiver,
    stream: &'a mut R,
    max_pdu_length: u32,
    expected_pc_id: Option<u8>,
    current: Vec<u8>,
    pos: usize,
    done: bool,
    wire_error: Option<String>,
}

impl<R: Read> PDataReader<'_, R> {
    /// Returns the latched DIMSE protocol or transport error, if any occurred while reading PDVs.
    /// Callers can use this to distinguish fatal wire/framing failures from recoverable
    /// dataset-processing errors even after intermediate code converts `std::io::Error` to strings.
    pub fn wire_error(&self) -> Option<&str> {
        self.wire_error.as_deref()
    }

    fn latch_error(&mut self, err: Error) -> Error {
        if self.wire_error.is_none() {
            self.wire_error = Some(err.to_string());
        }
        err
    }

    fn fetch_next_pdv(&mut self) -> Result<bool> {
        if let Some(ref msg) = self.wire_error {
            return Err(Error::Io(std::io::Error::other(msg.clone())));
        }
        loop {
            if self.done {
                return Ok(false);
            }
            if let Some(pdv) = self.receiver.pending.pop_front() {
                if pdv.value_type != PDataValueType::Data {
                    return Err(self.latch_error(Error::UnexpectedPdv {
                        expected: PDataValueType::Data,
                        got: pdv.value_type,
                    }));
                }
                if let Some(expected) = self.expected_pc_id {
                    if pdv.presentation_context_id != expected {
                        return Err(self.latch_error(Error::UnexpectedPresentationContext {
                            expected,
                            got: pdv.presentation_context_id,
                        }));
                    }
                }
                self.current = pdv.data;
                self.pos = 0;
                self.done = pdv.is_last;
                if !self.current.is_empty() {
                    return Ok(true);
                }
                continue;
            }
            match self.receiver.read_pdu(self.stream, self.max_pdu_length) {
                Ok(Pdu::PData { data }) => self.receiver.pending.extend(data),
                Ok(other) => return Err(self.latch_error(Error::UnexpectedPdu(Box::new(other)))),
                Err(err) => return Err(self.latch_error(err)),
            }
        }
    }

    /// Consume and discard all remaining bytes of this data set through its `is_last == true`
    /// PDV, so the association remains aligned for subsequent messages even after a mid-stream
    /// processing error. Returns `Err` immediately if a wire or protocol error was already latched.
    pub fn drain_to_end(&mut self) -> Result<u64> {
        if let Some(ref msg) = self.wire_error {
            return Err(Error::Io(std::io::Error::other(msg.clone())));
        }
        let mut drained = (self.current.len() - self.pos) as u64;
        self.current.clear();
        self.pos = 0;
        while !self.done {
            if self.fetch_next_pdv()? {
                drained += self.current.len() as u64;
                self.current.clear();
                self.pos = 0;
            }
        }
        Ok(drained)
    }
}

impl<R: Read> Read for PDataReader<'_, R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        if self.pos >= self.current.len() {
            match self.fetch_next_pdv() {
                Ok(true) => {}
                Ok(false) => return Ok(0),
                Err(Error::Io(io_err)) => return Err(io_err),
                Err(other) => return Err(std::io::Error::other(other.to_string())),
            }
        }
        let avail = self.current.len() - self.pos;
        let n = avail.min(buf.len());
        buf[..n].copy_from_slice(&self.current[self.pos..self.pos + n]);
        self.pos += n;
        Ok(n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{self, Cursor};

    fn pdv(value_type: PDataValueType, is_last: bool, data: &[u8]) -> PDataValue {
        PDataValue {
            presentation_context_id: 1,
            value_type,
            is_last,
            data: data.to_vec(),
        }
    }
    fn pdu_bytes(pdu: &Pdu) -> Vec<u8> {
        let mut out = Vec::new();
        pdu::write_pdu(&mut out, pdu).unwrap();
        out
    }
    const MAX: u32 = 1 << 20;
    use PDataValueType::{Command, Data};

    /// dcm4che's C-STORE: the command and the FIRST data fragment share a PDU, the rest follows in later PDUs.
    #[test]
    fn data_set_split_across_the_command_pdu_and_later_pdus() {
        let mut wire = pdu_bytes(&Pdu::PData {
            data: vec![pdv(Command, true, b"cmd"), pdv(Data, false, b"part1-")],
        });
        wire.extend(pdu_bytes(&Pdu::PData {
            data: vec![pdv(Data, false, b"part2-")],
        }));
        wire.extend(pdu_bytes(&Pdu::PData {
            data: vec![pdv(Data, true, b"part3")],
        }));
        let mut stream = Cursor::new(wire);
        let mut r = Receiver::default();
        match r.receive_command(&mut stream, MAX).unwrap() {
            Incoming::Command {
                presentation_context_id,
                data,
            } => assert_eq!((presentation_context_id, data.as_slice()), (1, &b"cmd"[..])),
            other => panic!("expected a command, got {other:?}"),
        }
        assert_eq!(
            r.receive_data_set(&mut stream, MAX).unwrap(),
            b"part1-part2-part3"
        );
    }

    /// dcm4che's C-FIND: pending response, its identifier and the final response in ONE PDU.
    #[test]
    fn several_messages_in_one_pdu_are_all_delivered_in_order() {
        let wire = pdu_bytes(&Pdu::PData {
            data: vec![
                pdv(Command, true, b"pending"),
                pdv(Data, true, b"match"),
                pdv(Command, true, b"final"),
            ],
        });
        let mut stream = Cursor::new(wire);
        let mut r = Receiver::default();
        let command =
            |r: &mut Receiver, s: &mut Cursor<Vec<u8>>| match r.receive_command(s, MAX).unwrap() {
                Incoming::Command { data, .. } => data,
                other => panic!("expected a command, got {other:?}"),
            };
        assert_eq!(command(&mut r, &mut stream), b"pending");
        assert_eq!(r.receive_data_set(&mut stream, MAX).unwrap(), b"match");
        assert_eq!(command(&mut r, &mut stream), b"final");
    }

    #[test]
    fn a_command_split_into_fragments_is_reassembled() {
        let mut wire = pdu_bytes(&Pdu::PData {
            data: vec![pdv(Command, false, b"com")],
        });
        wire.extend(pdu_bytes(&Pdu::PData {
            data: vec![pdv(Command, true, b"mand")],
        }));
        let mut r = Receiver::default();
        match r.receive_command(&mut Cursor::new(wire), MAX).unwrap() {
            Incoming::Command { data, .. } => assert_eq!(data, b"command"),
            other => panic!("expected a command, got {other:?}"),
        }
    }

    #[test]
    fn release_between_messages_is_returned_as_a_pdu() {
        let mut r = Receiver::default();
        match r
            .receive_command(&mut Cursor::new(pdu_bytes(&Pdu::ReleaseRQ)), MAX)
            .unwrap()
        {
            Incoming::Pdu(Pdu::ReleaseRQ) => {}
            other => panic!("expected A-RELEASE-RQ, got {other:?}"),
        }
    }

    #[test]
    fn data_where_a_command_is_expected_is_a_protocol_error() {
        let wire = pdu_bytes(&Pdu::PData {
            data: vec![pdv(Data, true, b"stray")],
        });
        let mut r = Receiver::default();
        assert!(matches!(
            r.receive_command(&mut Cursor::new(wire), MAX),
            Err(Error::UnexpectedPdv {
                expected: Command,
                got: Data
            })
        ));
    }

    #[test]
    fn release_in_the_middle_of_a_data_set_is_an_error() {
        let mut wire = pdu_bytes(&Pdu::PData {
            data: vec![pdv(Data, false, b"half")],
        });
        wire.extend(pdu_bytes(&Pdu::ReleaseRQ));
        let mut r = Receiver::default();
        assert!(matches!(
            r.receive_data_set(&mut Cursor::new(wire), MAX),
            Err(Error::UnexpectedPdu(_))
        ));
    }

    #[test]
    fn leftover_pdvs_come_first_from_receive_pdu() {
        let wire = pdu_bytes(&Pdu::PData {
            data: vec![pdv(Command, true, b"rsp"), pdv(Data, true, b"failed-list")],
        });
        let mut stream = Cursor::new(wire);
        let mut r = Receiver::default();
        r.receive_command(&mut stream, MAX).unwrap();
        match r.receive_pdu(&mut stream, MAX).unwrap() {
            Pdu::PData { data } => assert_eq!(data[0].data, b"failed-list"),
            other => panic!("expected the queued PDV, got {other:?}"),
        }
    }

    /// A stream that delivers its bytes in small chunks and fails with `WouldBlock` (a read-timeout tick) between them.
    struct Trickle {
        bytes: Vec<u8>,
        pos: usize,
        chunk: usize,
        block_next: bool,
    }
    impl Read for Trickle {
        fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
            if self.block_next {
                self.block_next = false;
                return Err(io::Error::from(ErrorKind::WouldBlock));
            }
            self.block_next = true;
            let n = self.chunk.min(out.len()).min(self.bytes.len() - self.pos);
            out[..n].copy_from_slice(&self.bytes[self.pos..self.pos + n]);
            self.pos += n;
            Ok(n)
        }
    }

    /// The timeout tick must not lose the bytes of a half-read PDU (or a half-assembled message): retrying continues.
    #[test]
    fn timeouts_in_the_middle_of_a_pdu_and_of_a_message_are_resumable() {
        let mut wire = pdu_bytes(&Pdu::PData {
            data: vec![pdv(Command, true, b"cmd"), pdv(Data, false, &[7u8; 300])],
        });
        wire.extend(pdu_bytes(&Pdu::PData {
            data: vec![pdv(Data, true, &[8u8; 300])],
        }));
        let mut stream = Trickle {
            bytes: wire,
            pos: 0,
            chunk: 7,
            block_next: true,
        };
        let mut r = Receiver::default();
        let mut timeouts = 0;
        let command = loop {
            match r.receive_command(&mut stream, MAX) {
                Ok(Incoming::Command { data, .. }) => break data,
                Ok(other) => panic!("expected a command, got {other:?}"),
                Err(Error::Io(e)) if e.kind() == ErrorKind::WouldBlock => timeouts += 1,
                Err(e) => panic!("unexpected error {e}"),
            }
        };
        assert_eq!(command, b"cmd");
        let data = loop {
            match r.receive_data_set(&mut stream, MAX) {
                Ok(data) => break data,
                Err(Error::Io(e)) if e.kind() == ErrorKind::WouldBlock => timeouts += 1,
                Err(e) => panic!("unexpected error {e}"),
            }
        };
        assert!(
            timeouts > 50,
            "the test stream should have timed out many times, got {timeouts}"
        );
        assert_eq!(data.len(), 600);
        assert!(data[..300].iter().all(|&b| b == 7) && data[300..].iter().all(|&b| b == 8));
    }

    #[test]
    fn declared_length_above_the_maximum_is_rejected_before_allocating() {
        let mut header = vec![0x04, 0x00];
        header.extend_from_slice(&(MAX + 1).to_be_bytes());
        let mut r = Receiver::default();
        assert!(matches!(
            r.receive_pdu(&mut Cursor::new(header), MAX),
            Err(Error::Pdu(pdu::Error::PduTooLarge { .. }))
        ));
    }

    #[test]
    fn eof_is_an_io_error() {
        let mut r = Receiver::default();
        assert!(matches!(
            r.receive_pdu(&mut Cursor::new(vec![0x04, 0x00, 0x00]), MAX),
            Err(Error::Io(_))
        ));
    }

    #[test]
    fn pdata_reader_streams_dataset_and_preserves_packed_next_command() {
        let mut wire = pdu_bytes(&Pdu::PData {
            data: vec![pdv(Command, true, b"cmd1"), pdv(Data, false, b"hello-")],
        });
        wire.extend(pdu_bytes(&Pdu::PData {
            data: vec![pdv(Data, true, b"world"), pdv(Command, true, b"cmd2")],
        }));
        let mut stream = Cursor::new(wire);
        let mut r = Receiver::default();
        match r.receive_command(&mut stream, MAX).unwrap() {
            Incoming::Command { data, .. } => assert_eq!(data, b"cmd1"),
            other => panic!("expected cmd1, got {other:?}"),
        }
        let mut out = Vec::new();
        {
            let mut reader = r.data_set_reader(&mut stream, MAX, Some(1));
            reader.read_to_end(&mut out).unwrap();
        }
        assert_eq!(out, b"hello-world");
        match r.receive_command(&mut stream, MAX).unwrap() {
            Incoming::Command { data, .. } => assert_eq!(data, b"cmd2"),
            other => panic!("expected cmd2, got {other:?}"),
        }
    }

    #[test]
    fn pdata_reader_drain_to_end_keeps_association_aligned() {
        let mut wire = pdu_bytes(&Pdu::PData {
            data: vec![pdv(Command, true, b"cmd1"), pdv(Data, false, b"prefix-")],
        });
        wire.extend(pdu_bytes(&Pdu::PData {
            data: vec![pdv(Data, false, b"middle-")],
        }));
        wire.extend(pdu_bytes(&Pdu::PData {
            data: vec![pdv(Data, true, b"suffix"), pdv(Command, true, b"cmd2")],
        }));
        let mut stream = Cursor::new(wire);
        let mut r = Receiver::default();
        r.receive_command(&mut stream, MAX).unwrap();
        {
            let mut reader = r.data_set_reader(&mut stream, MAX, Some(1));
            let mut head = [0u8; 4];
            reader.read_exact(&mut head).unwrap();
            assert_eq!(&head, b"pref");
            let drained = reader.drain_to_end().unwrap();
            assert_eq!(drained, 16); // "ix-middle-suffix"
        }
        match r.receive_command(&mut stream, MAX).unwrap() {
            Incoming::Command { data, .. } => assert_eq!(data, b"cmd2"),
            other => panic!("expected cmd2, got {other:?}"),
        }
    }

    #[test]
    fn receive_data_set_to_streams_exact_bytes_and_keeps_next_command() {
        let mut wire = pdu_bytes(&Pdu::PData {
            data: vec![pdv(Command, true, b"cmd1"), pdv(Data, false, b"abc")],
        });
        wire.extend(pdu_bytes(&Pdu::PData {
            data: vec![pdv(Data, true, b"def"), pdv(Command, true, b"cmd2")],
        }));
        let mut stream = Cursor::new(wire);
        let mut r = Receiver::default();
        r.receive_command(&mut stream, MAX).unwrap();
        let mut sink = Vec::new();
        let written = r
            .receive_data_set_to(&mut stream, MAX, Some(1), &mut sink)
            .unwrap();
        assert_eq!(written, 6);
        assert_eq!(sink, b"abcdef");
        match r.receive_command(&mut stream, MAX).unwrap() {
            Incoming::Command { data, .. } => assert_eq!(data, b"cmd2"),
            other => panic!("expected cmd2, got {other:?}"),
        }
    }

    #[test]
    fn pdata_reader_rejects_mismatched_presentation_context_id() {
        let wire = pdu_bytes(&Pdu::PData {
            data: vec![
                pdv(Command, true, b"cmd1"),
                PDataValue {
                    presentation_context_id: 3,
                    value_type: Data,
                    is_last: false,
                    data: b"wrong-pc".to_vec(),
                },
                pdv(Data, true, b"trailing-valid-pc"),
            ],
        });
        let mut stream = Cursor::new(wire);
        let mut r = Receiver::default();
        r.receive_command(&mut stream, MAX).unwrap();
        let mut reader = r.data_set_reader(&mut stream, MAX, Some(1));
        let mut out = Vec::new();
        let err = reader.read_to_end(&mut out).unwrap_err();
        assert!(
            err.to_string().contains("presentation context ID"),
            "unexpected error: {err}"
        );
        // The wire error must remain latched so drain_to_end() cannot mask a framing violation.
        assert!(reader.wire_error().is_some());
        assert!(reader.drain_to_end().is_err());
    }
}
