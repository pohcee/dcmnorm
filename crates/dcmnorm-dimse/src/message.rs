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
use std::io::{ErrorKind, Read};

use crate::error::{Error, Result};
use crate::pdu::{self, PDataValue, PDataValueType, Pdu, PDU_HEADER_SIZE};

/// What [`Receiver::receive_command`] got: a complete command, or a non-P-DATA PDU (A-RELEASE-RQ, A-ABORT, ...) that arrived
/// between messages.
#[derive(Debug)]
pub enum Incoming {
    Command { presentation_context_id: u8, data: Vec<u8> },
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
    Message { presentation_context_id: u8, data: Vec<u8> },
    Other(Pdu),
}

impl Receiver {
    /// The next PDU. Queued PDVs (left over from a PDU that also held an earlier message) come first, as one P-DATA-TF.
    pub(crate) fn receive_pdu<R: Read>(&mut self, stream: &mut R, max_pdu_length: u32) -> Result<Pdu> {
        if !self.pending.is_empty() {
            return Ok(Pdu::PData { data: self.pending.drain(..).collect() });
        }
        self.read_pdu(stream, max_pdu_length)
    }

    /// The next complete command, or the PDU that arrived instead of one between messages.
    pub(crate) fn receive_command<R: Read>(&mut self, stream: &mut R, max_pdu_length: u32) -> Result<Incoming> {
        match self.assemble(stream, max_pdu_length, PDataValueType::Command)? {
            Assembled::Message { presentation_context_id, data } => Ok(Incoming::Command { presentation_context_id, data }),
            Assembled::Other(pdu) => Ok(Incoming::Pdu(pdu)),
        }
    }

    /// The complete data set that follows the command just received.
    pub(crate) fn receive_data_set<R: Read>(&mut self, stream: &mut R, max_pdu_length: u32) -> Result<Vec<u8>> {
        match self.assemble(stream, max_pdu_length, PDataValueType::Data)? {
            Assembled::Message { data, .. } => Ok(data),
            Assembled::Other(pdu) => Err(Error::UnexpectedPdu(Box::new(pdu))),
        }
    }

    fn assemble<R: Read>(&mut self, stream: &mut R, max_pdu_length: u32, want: PDataValueType) -> Result<Assembled> {
        loop {
            if let Some(pdv) = self.pending.pop_front() {
                if pdv.value_type != want {
                    let got = pdv.value_type;
                    self.partial.clear();
                    self.partial_context = None;
                    return Err(Error::UnexpectedPdv { expected: want, got });
                }
                self.partial_context.get_or_insert(pdv.presentation_context_id);
                if self.partial.is_empty() {
                    self.partial = pdv.data; // the common single-fragment case: no copy
                } else {
                    self.partial.extend_from_slice(&pdv.data);
                }
                if pdv.is_last {
                    let presentation_context_id = self.partial_context.take().unwrap_or(pdv.presentation_context_id);
                    return Ok(Assembled::Message { presentation_context_id, data: std::mem::take(&mut self.partial) });
                }
                continue;
            }
            match self.read_pdu(stream, max_pdu_length)? {
                Pdu::PData { data } => self.pending.extend(data),
                // A release/abort is only valid between messages; in the middle of one the message is lost either way.
                other if self.partial.is_empty() && self.partial_context.is_none() => return Ok(Assembled::Other(other)),
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
                let length = u32::from_be_bytes([self.raw[2], self.raw[3], self.raw[4], self.raw[5]]);
                if length > max_pdu_length {
                    self.raw.clear();
                    self.filled = 0;
                    return Err(Error::Pdu(pdu::Error::PduTooLarge { pdu_length: length, max_pdu_length }));
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{self, Cursor};

    fn pdv(value_type: PDataValueType, is_last: bool, data: &[u8]) -> PDataValue {
        PDataValue { presentation_context_id: 1, value_type, is_last, data: data.to_vec() }
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
        let mut wire = pdu_bytes(&Pdu::PData { data: vec![pdv(Command, true, b"cmd"), pdv(Data, false, b"part1-")] });
        wire.extend(pdu_bytes(&Pdu::PData { data: vec![pdv(Data, false, b"part2-")] }));
        wire.extend(pdu_bytes(&Pdu::PData { data: vec![pdv(Data, true, b"part3")] }));
        let mut stream = Cursor::new(wire);
        let mut r = Receiver::default();
        match r.receive_command(&mut stream, MAX).unwrap() {
            Incoming::Command { presentation_context_id, data } => assert_eq!((presentation_context_id, data.as_slice()), (1, &b"cmd"[..])),
            other => panic!("expected a command, got {other:?}"),
        }
        assert_eq!(r.receive_data_set(&mut stream, MAX).unwrap(), b"part1-part2-part3");
    }

    /// dcm4che's C-FIND: pending response, its identifier and the final response in ONE PDU.
    #[test]
    fn several_messages_in_one_pdu_are_all_delivered_in_order() {
        let wire = pdu_bytes(&Pdu::PData {
            data: vec![pdv(Command, true, b"pending"), pdv(Data, true, b"match"), pdv(Command, true, b"final")],
        });
        let mut stream = Cursor::new(wire);
        let mut r = Receiver::default();
        let command = |r: &mut Receiver, s: &mut Cursor<Vec<u8>>| match r.receive_command(s, MAX).unwrap() {
            Incoming::Command { data, .. } => data,
            other => panic!("expected a command, got {other:?}"),
        };
        assert_eq!(command(&mut r, &mut stream), b"pending");
        assert_eq!(r.receive_data_set(&mut stream, MAX).unwrap(), b"match");
        assert_eq!(command(&mut r, &mut stream), b"final");
    }

    #[test]
    fn a_command_split_into_fragments_is_reassembled() {
        let mut wire = pdu_bytes(&Pdu::PData { data: vec![pdv(Command, false, b"com")] });
        wire.extend(pdu_bytes(&Pdu::PData { data: vec![pdv(Command, true, b"mand")] }));
        let mut r = Receiver::default();
        match r.receive_command(&mut Cursor::new(wire), MAX).unwrap() {
            Incoming::Command { data, .. } => assert_eq!(data, b"command"),
            other => panic!("expected a command, got {other:?}"),
        }
    }

    #[test]
    fn release_between_messages_is_returned_as_a_pdu() {
        let mut r = Receiver::default();
        match r.receive_command(&mut Cursor::new(pdu_bytes(&Pdu::ReleaseRQ)), MAX).unwrap() {
            Incoming::Pdu(Pdu::ReleaseRQ) => {}
            other => panic!("expected A-RELEASE-RQ, got {other:?}"),
        }
    }

    #[test]
    fn data_where_a_command_is_expected_is_a_protocol_error() {
        let wire = pdu_bytes(&Pdu::PData { data: vec![pdv(Data, true, b"stray")] });
        let mut r = Receiver::default();
        assert!(matches!(
            r.receive_command(&mut Cursor::new(wire), MAX),
            Err(Error::UnexpectedPdv { expected: Command, got: Data })
        ));
    }

    #[test]
    fn release_in_the_middle_of_a_data_set_is_an_error() {
        let mut wire = pdu_bytes(&Pdu::PData { data: vec![pdv(Data, false, b"half")] });
        wire.extend(pdu_bytes(&Pdu::ReleaseRQ));
        let mut r = Receiver::default();
        assert!(matches!(r.receive_data_set(&mut Cursor::new(wire), MAX), Err(Error::UnexpectedPdu(_))));
    }

    #[test]
    fn leftover_pdvs_come_first_from_receive_pdu() {
        let wire = pdu_bytes(&Pdu::PData { data: vec![pdv(Command, true, b"rsp"), pdv(Data, true, b"failed-list")] });
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
        let mut wire = pdu_bytes(&Pdu::PData { data: vec![pdv(Command, true, b"cmd"), pdv(Data, false, &[7u8; 300])] });
        wire.extend(pdu_bytes(&Pdu::PData { data: vec![pdv(Data, true, &[8u8; 300])] }));
        let mut stream = Trickle { bytes: wire, pos: 0, chunk: 7, block_next: true };
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
        assert!(timeouts > 50, "the test stream should have timed out many times, got {timeouts}");
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
        assert!(matches!(r.receive_pdu(&mut Cursor::new(vec![0x04, 0x00, 0x00]), MAX), Err(Error::Io(_))));
    }
}
