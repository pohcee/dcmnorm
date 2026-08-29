//! File Meta Information (group 0002) - the Part 10 preamble, "DICM" magic, and the group-0002
//! elements (transfer syntax, SOP class/instance UID, implementation identifiers, etc.) that
//! precede the main data set in every DICOM Part 10 file.

use std::io;
use std::io::{Read, Write};

use dcmnorm_core::header::{DataElement, Tag};
use dcmnorm_core::{PrimitiveValue, VR};
use dcmnorm_encoding::transfer_syntax::TransferSyntax;
use dcmnorm_encoding::TransferSyntaxIndex;
use dcmnorm_transcode::TransferSyntaxRegistry;

use crate::error::{ReadError, WithMetaError, WriteError};
use crate::mem::InMemElement;

const PREAMBLE_LEN: usize = 128;
const DICM_MAGIC: &[u8; 4] = b"DICM";

pub(crate) mod tags {
    use dcmnorm_core::header::Tag;

    pub const FILE_META_INFORMATION_GROUP_LENGTH: Tag = Tag(0x0002, 0x0000);
    pub const FILE_META_INFORMATION_VERSION: Tag = Tag(0x0002, 0x0001);
    pub const MEDIA_STORAGE_SOP_CLASS_UID: Tag = Tag(0x0002, 0x0002);
    pub const MEDIA_STORAGE_SOP_INSTANCE_UID: Tag = Tag(0x0002, 0x0003);
    pub const TRANSFER_SYNTAX_UID: Tag = Tag(0x0002, 0x0010);
    pub const IMPLEMENTATION_CLASS_UID: Tag = Tag(0x0002, 0x0012);
    pub const IMPLEMENTATION_VERSION_NAME: Tag = Tag(0x0002, 0x0013);
    pub const SOURCE_APPLICATION_ENTITY_TITLE: Tag = Tag(0x0002, 0x0016);
    pub const SENDING_APPLICATION_ENTITY_TITLE: Tag = Tag(0x0002, 0x0017);
    pub const RECEIVING_APPLICATION_ENTITY_TITLE: Tag = Tag(0x0002, 0x0018);
    pub const PRIVATE_INFORMATION_CREATOR_UID: Tag = Tag(0x0002, 0x0102);
    pub const PRIVATE_INFORMATION: Tag = Tag(0x0002, 0x0103);
}

/// The File Meta Information (group 0002) of a DICOM Part 10 file.
///
/// The three "mandatory" fields (`media_storage_sop_class_uid`,
/// `media_storage_sop_instance_uid`, `implementation_class_uid`) are plain `String`s with no
/// "absent" state, matching how `dcmnorm`'s `--remove`/`--set` CLI semantics for group-0002
/// attributes already distinguish "clear to empty string" from "was never set" - see
/// `dicom_edit.rs`. Everything else is `Option<String>` and uses `.take()` to represent removal.
///
/// Unlike `dicom-object`'s equivalent, `information_group_length` is not a field that can go
/// stale: it's always computed fresh in [`FileMetaTable::write_to`] from whatever meta elements
/// are actually about to be written, so there's nothing to "refresh" before a write.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct FileMetaTable {
    pub information_version: [u8; 2],
    pub media_storage_sop_class_uid: String,
    pub media_storage_sop_instance_uid: String,
    pub transfer_syntax: String,
    pub implementation_class_uid: String,
    pub implementation_version_name: Option<String>,
    pub source_application_entity_title: Option<String>,
    pub sending_application_entity_title: Option<String>,
    pub receiving_application_entity_title: Option<String>,
    pub private_information_creator_uid: Option<String>,
    pub private_information: Option<Vec<u8>>,
}

impl FileMetaTable {
    /// The transfer syntax UID declared in this meta table (trailing NUL/whitespace not
    /// trimmed here - callers that need a normalized comparison already trim, see
    /// `io.rs::normalize_transfer_syntax_uid`).
    pub fn transfer_syntax(&self) -> &str {
        &self.transfer_syntax
    }

    // Method-call-syntax accessors alongside the public fields above, for call sites that
    // prefer `.media_storage_sop_class_uid()` over the raw field - both forms read/write the
    // same underlying field.
    pub fn media_storage_sop_class_uid(&self) -> &str {
        &self.media_storage_sop_class_uid
    }

    pub fn media_storage_sop_instance_uid(&self) -> &str {
        &self.media_storage_sop_instance_uid
    }

    pub fn implementation_class_uid(&self) -> &str {
        &self.implementation_class_uid
    }

    /// Set the declared transfer syntax to the given [`TransferSyntax`]'s UID.
    pub fn set_transfer_syntax(&mut self, ts: &TransferSyntax) {
        self.transfer_syntax = ts.uid().to_owned();
    }

    /// Resolve this table's declared transfer syntax against the registry.
    pub fn transfer_syntax_ts(&self) -> Option<&'static TransferSyntax> {
        TransferSyntaxRegistry.get(io_util::trim_uid(&self.transfer_syntax))
    }

    /// Iterate this meta table's elements in the same shape they'd be written to a file,
    /// including the group length element itself. Used for JSON export
    /// (`standard_json.rs`/`flat_json.rs`).
    pub fn to_element_iter(&self) -> impl Iterator<Item = InMemElement> + '_ {
        self.elements_for_write()
    }

    fn elements_for_write(&self) -> std::vec::IntoIter<InMemElement> {
        let mut elements = Vec::with_capacity(11);
        elements.push(primitive_element(
            tags::FILE_META_INFORMATION_GROUP_LENGTH,
            VR::UL,
            PrimitiveValue::from(self.encoded_len()),
        ));
        elements.extend(self.body_elements());
        elements.into_iter()
    }

    /// Every group-0002 element *except* the group-length element itself, in write order. The
    /// one place that decides which fields are present and how they're encoded - both
    /// [`Self::elements_for_write`] (prepends the group-length element) and [`Self::encoded_len`]
    /// (measures this same list's real encoded size) build on top of it, rather than each
    /// independently hand-duplicating "which fields exist and what VR they use." Adding a new
    /// meta field only ever needs a change here.
    fn body_elements(&self) -> Vec<InMemElement> {
        let mut elements = Vec::with_capacity(10);
        elements.push(primitive_element(
            tags::FILE_META_INFORMATION_VERSION,
            VR::OB,
            PrimitiveValue::from(self.information_version.to_vec()),
        ));
        elements.push(primitive_element(
            tags::MEDIA_STORAGE_SOP_CLASS_UID,
            VR::UI,
            PrimitiveValue::from(self.media_storage_sop_class_uid.clone()),
        ));
        elements.push(primitive_element(
            tags::MEDIA_STORAGE_SOP_INSTANCE_UID,
            VR::UI,
            PrimitiveValue::from(self.media_storage_sop_instance_uid.clone()),
        ));
        elements.push(primitive_element(
            tags::TRANSFER_SYNTAX_UID,
            VR::UI,
            PrimitiveValue::from(self.transfer_syntax.clone()),
        ));
        elements.push(primitive_element(
            tags::IMPLEMENTATION_CLASS_UID,
            VR::UI,
            PrimitiveValue::from(self.implementation_class_uid.clone()),
        ));
        if let Some(v) = &self.implementation_version_name {
            elements.push(primitive_element(
                tags::IMPLEMENTATION_VERSION_NAME,
                VR::SH,
                PrimitiveValue::from(v.clone()),
            ));
        }
        if let Some(v) = &self.source_application_entity_title {
            elements.push(primitive_element(
                tags::SOURCE_APPLICATION_ENTITY_TITLE,
                VR::AE,
                PrimitiveValue::from(v.clone()),
            ));
        }
        if let Some(v) = &self.sending_application_entity_title {
            elements.push(primitive_element(
                tags::SENDING_APPLICATION_ENTITY_TITLE,
                VR::AE,
                PrimitiveValue::from(v.clone()),
            ));
        }
        if let Some(v) = &self.receiving_application_entity_title {
            elements.push(primitive_element(
                tags::RECEIVING_APPLICATION_ENTITY_TITLE,
                VR::AE,
                PrimitiveValue::from(v.clone()),
            ));
        }
        if let Some(v) = &self.private_information_creator_uid {
            elements.push(primitive_element(
                tags::PRIVATE_INFORMATION_CREATOR_UID,
                VR::UI,
                PrimitiveValue::from(v.clone()),
            ));
        }
        if let Some(v) = &self.private_information {
            elements.push(primitive_element(
                tags::PRIVATE_INFORMATION,
                VR::OB,
                PrimitiveValue::from(v.clone()),
            ));
        }
        elements
    }

    /// The byte length of the meta group's elements *excluding* the group length element
    /// itself - i.e. the value that belongs in (0002,0000). Computed by actually encoding
    /// [`Self::body_elements`] (Explicit VR Little Endian, the same path [`Self::write_to`]
    /// uses) into a throwaway buffer and measuring it, rather than a hand-maintained per-field
    /// byte count - so it can never drift from what's actually written, by construction, not by
    /// convention.
    fn encoded_len(&self) -> u32 {
        let ts = TransferSyntaxRegistry
            .get(dcmnorm_dictionary::uids::EXPLICIT_VR_LITTLE_ENDIAN)
            .expect("Explicit VR Little Endian must be registered");
        let mut buf = Vec::new();
        {
            let mut writer = dcmnorm_parser::dataset::DataSetWriter::with_ts(&mut buf, ts)
                .expect("Explicit VR Little Endian writer construction cannot fail");
            for element in self.body_elements() {
                crate::mem::write_element_tokens(&mut writer, &element)
                    .expect("encoding a well-formed in-memory meta element cannot fail");
            }
        }
        buf.len() as u32
    }

    /// Write the 128-byte preamble, "DICM" magic, and this meta group (Explicit VR Little
    /// Endian, per PS3.10 §7.1) to `to`.
    pub fn write_to<W: Write>(&self, mut to: W) -> Result<(), WriteError> {
        to.write_all(&[0u8; PREAMBLE_LEN])
            .map_err(|source| WriteError::Io { source, context: "writing preamble" })?;
        to.write_all(DICM_MAGIC)
            .map_err(|source| WriteError::Io { source, context: "writing DICM magic" })?;

        let ts = TransferSyntaxRegistry
            .get(dcmnorm_dictionary::uids::EXPLICIT_VR_LITTLE_ENDIAN)
            .expect("Explicit VR Little Endian must be registered");

        let mut writer = dcmnorm_parser::dataset::DataSetWriter::with_ts(&mut to, ts)
            .map_err(|source| WriteError::Dataset { source })?;
        let elements = self.elements_for_write();
        for element in elements {
            crate::mem::write_element_tokens(&mut writer, &element)?;
        }
        Ok(())
    }

    /// Try to read the 128-byte preamble + "DICM" magic + group-0002 meta elements from
    /// `from`. Returns `Ok(None)` (not an error) if the preamble/magic aren't present, so
    /// callers can fall back to treating the stream as a bare (meta-less) data set - matching
    /// the permissive trial-parse behavior `dcmnorm` already relies on for raw dumps.
    pub fn read_from<R: Read>(mut from: R) -> Result<Option<Self>, ReadError> {
        let mut preamble = [0u8; PREAMBLE_LEN];
        if from.read_exact(&mut preamble).is_err() {
            return Ok(None);
        }
        let mut magic = [0u8; 4];
        if from.read_exact(&mut magic).is_err() || &magic != DICM_MAGIC {
            return Ok(None);
        }

        Self::read_meta_group(from).map(Some)
    }

    /// Read the group-0002 meta elements only, assuming the reader is already positioned right
    /// after the "DICM" magic (i.e. the preamble has already been consumed/verified).
    ///
    /// The meta group's own length - the value of its first element, (0002,0000) - is read
    /// first and used to read *exactly* that many further bytes into a bounded buffer, which
    /// is then parsed as a self-contained data set. This is deliberate: a token-stream reader
    /// has no way to "un-read" bytes once it has peeked at the next element's header, so
    /// detecting the end of group 0002 by peeking at the first element outside it (rather than
    /// by the declared length) would consume and discard the first several bytes of the main
    /// data set that follows - byte-desyncing everything read after it.
    pub fn read_meta_group<R: Read>(mut from: R) -> Result<Self, ReadError> {
        let group_length = read_group_length_element(&mut from)?;

        // `group_length` is a raw u32 taken directly from the file (up to ~4GiB) - read via
        // `Take` + `read_to_end` rather than pre-allocating `vec![0u8; group_length]` upfront.
        // Real meta groups are a few hundred bytes to a few KB; a corrupt/malicious file's first
        // 12 bytes declaring an implausible group length would otherwise force a multi-GB
        // allocation attempt before a single further byte is read, and a failing allocation of
        // that shape aborts the process rather than returning a catchable error. This way, growth
        // is bounded by bytes actually available on the stream, so a truncated/lying length
        // surfaces as a clean `ReadError::Io` as soon as the real data runs out.
        let mut meta_bytes = Vec::new();
        (&mut from)
            .take(group_length as u64)
            .read_to_end(&mut meta_bytes)
            .map_err(|source| ReadError::Io {
                source,
                context: "reading file meta group",
            })?;
        if meta_bytes.len() != group_length as usize {
            return Err(ReadError::Io {
                source: io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    format!(
                        "declared meta group length {group_length} exceeds available data ({} bytes read)",
                        meta_bytes.len()
                    ),
                ),
                context: "reading file meta group",
            });
        }

        let ts = TransferSyntaxRegistry
            .get(dcmnorm_dictionary::uids::EXPLICIT_VR_LITTLE_ENDIAN)
            .expect("Explicit VR Little Endian must be registered");

        let mut table = FileMetaTable::default();
        let reader =
            dcmnorm_parser::dataset::DataSetReader::new_with_ts(std::io::Cursor::new(meta_bytes), ts)
                .map_err(|source| ReadError::Dataset { source })?;

        // The meta group (beyond the group length element already consumed above) is always a
        // flat sequence of primitive elements (no sequences, no encapsulated pixel data) - fold
        // ElementHeader+PrimitiveValue token pairs directly. The buffer is exactly
        // `group_length` bytes, so the reader naturally stops when it's exhausted.
        let mut pending_tag: Option<Tag> = None;
        for token in reader {
            let token = token.map_err(|source| ReadError::Dataset { source })?;
            match token {
                dcmnorm_parser::dataset::DataToken::ElementHeader(header) => {
                    pending_tag = Some(header.tag);
                }
                dcmnorm_parser::dataset::DataToken::PrimitiveValue(value) => {
                    if let Some(tag) = pending_tag.take() {
                        table.apply(tag, value);
                    }
                }
                _ => {}
            }
        }

        Ok(table)
    }

    fn apply(&mut self, tag: Tag, value: PrimitiveValue) {
        let text = || value.to_str().trim_end_matches(['\0', ' ']).to_owned();
        match tag {
            tags::FILE_META_INFORMATION_GROUP_LENGTH => {}
            tags::FILE_META_INFORMATION_VERSION => {
                let bytes = value.to_bytes();
                if bytes.len() >= 2 {
                    self.information_version = [bytes[0], bytes[1]];
                }
            }
            tags::MEDIA_STORAGE_SOP_CLASS_UID => self.media_storage_sop_class_uid = text(),
            tags::MEDIA_STORAGE_SOP_INSTANCE_UID => self.media_storage_sop_instance_uid = text(),
            tags::TRANSFER_SYNTAX_UID => self.transfer_syntax = text(),
            tags::IMPLEMENTATION_CLASS_UID => self.implementation_class_uid = text(),
            tags::IMPLEMENTATION_VERSION_NAME => self.implementation_version_name = Some(text()),
            tags::SOURCE_APPLICATION_ENTITY_TITLE => {
                self.source_application_entity_title = Some(text())
            }
            tags::SENDING_APPLICATION_ENTITY_TITLE => {
                self.sending_application_entity_title = Some(text())
            }
            tags::RECEIVING_APPLICATION_ENTITY_TITLE => {
                self.receiving_application_entity_title = Some(text())
            }
            tags::PRIVATE_INFORMATION_CREATOR_UID => {
                self.private_information_creator_uid = Some(text())
            }
            tags::PRIVATE_INFORMATION => {
                self.private_information = Some(value.to_bytes().into_owned());
            }
            _ => {}
        }
    }
}

/// Read the (0002,0000) File Meta Information Group Length element directly, by its known
/// fixed wire format (Explicit VR Little Endian, VR UL, short-form 2-byte length, 4-byte
/// value - PS3.5 §7.1) rather than through the general-purpose data set reader, since its
/// value is needed *before* we know how many further bytes belong to the meta group at all.
fn read_group_length_element<R: Read>(from: &mut R) -> Result<u32, ReadError> {
    let mut header = [0u8; 8];
    from.read_exact(&mut header).map_err(|source| ReadError::Io {
        source,
        context: "reading file meta group length element header",
    })?;

    let tag = Tag(
        u16::from_le_bytes([header[0], header[1]]),
        u16::from_le_bytes([header[2], header[3]]),
    );
    if tag != tags::FILE_META_INFORMATION_GROUP_LENGTH || &header[4..6] != b"UL" {
        return Err(ReadError::UnexpectedToken {
            context: "expected (0002,0000) UL as the first file meta element",
        });
    }
    let value_len = u16::from_le_bytes([header[6], header[7]]);
    if value_len != 4 {
        return Err(ReadError::UnexpectedToken {
            context: "file meta group length element has an unexpected value length",
        });
    }

    let mut value = [0u8; 4];
    from.read_exact(&mut value).map_err(|source| ReadError::Io {
        source,
        context: "reading file meta group length value",
    })?;
    Ok(u32::from_le_bytes(value))
}

fn primitive_element(tag: Tag, vr: VR, value: PrimitiveValue) -> InMemElement {
    DataElement::new(tag, vr, value)
}

/// Builder for [`FileMetaTable`], mirroring `dicom-object`'s `FileMetaTableBuilder` fluent API.
#[derive(Debug, Clone, Default)]
pub struct FileMetaTableBuilder {
    table: FileMetaTable,
}

impl FileMetaTableBuilder {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn information_version(mut self, v: [u8; 2]) -> Self {
        self.table.information_version = v;
        self
    }

    pub fn media_storage_sop_class_uid(mut self, v: impl Into<String>) -> Self {
        self.table.media_storage_sop_class_uid = v.into();
        self
    }

    pub fn media_storage_sop_instance_uid(mut self, v: impl Into<String>) -> Self {
        self.table.media_storage_sop_instance_uid = v.into();
        self
    }

    pub fn transfer_syntax(mut self, v: impl Into<String>) -> Self {
        self.table.transfer_syntax = v.into();
        self
    }

    pub fn implementation_class_uid(mut self, v: impl Into<String>) -> Self {
        self.table.implementation_class_uid = v.into();
        self
    }

    pub fn implementation_version_name(mut self, v: impl Into<String>) -> Self {
        self.table.implementation_version_name = Some(v.into());
        self
    }

    pub fn source_application_entity_title(mut self, v: impl Into<String>) -> Self {
        self.table.source_application_entity_title = Some(v.into());
        self
    }

    pub fn sending_application_entity_title(mut self, v: impl Into<String>) -> Self {
        self.table.sending_application_entity_title = Some(v.into());
        self
    }

    pub fn receiving_application_entity_title(mut self, v: impl Into<String>) -> Self {
        self.table.receiving_application_entity_title = Some(v.into());
        self
    }

    pub fn private_information_creator_uid(mut self, v: impl Into<String>) -> Self {
        self.table.private_information_creator_uid = Some(v.into());
        self
    }

    pub fn private_information(mut self, v: impl Into<Vec<u8>>) -> Self {
        self.table.private_information = Some(v.into());
        self
    }

    /// Build the table. Unset mandatory fields (`media_storage_sop_class_uid`,
    /// `media_storage_sop_instance_uid`, `implementation_class_uid`) are left as empty
    /// strings rather than erroring - matching dcmnorm's existing tolerance for building
    /// meta tables incrementally (e.g. the raw-dataset trial-parse fallback path, which only
    /// knows the transfer syntax at first).
    pub fn build(self) -> Result<FileMetaTable, WithMetaError> {
        if self.table.implementation_class_uid.is_empty() {
            let mut table = self.table;
            table.implementation_class_uid = crate::IMPLEMENTATION_CLASS_UID.to_owned();
            if table.implementation_version_name.is_none() {
                table.implementation_version_name = Some(crate::IMPLEMENTATION_VERSION_NAME.to_owned());
            }
            return Ok(table);
        }
        Ok(self.table)
    }
}

/// Read/write helpers shared with `mem.rs`.
pub(crate) mod io_util {
    pub fn trim_uid(uid: &str) -> &str {
        uid.trim_end_matches(|c: char| c.is_whitespace() || c == '\0')
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A corrupt/malicious file can declare an implausible (0002,0000) group length (up to
    /// ~4GiB) while actually containing only a handful of bytes. Before this was hardened,
    /// `read_meta_group` allocated `vec![0u8; group_length]` upfront - a failing allocation of
    /// that shape aborts the process rather than returning a catchable error. This proves the
    /// fix: the declared length is far larger than the (tiny) actual stream, so this must return
    /// a clean `Err` quickly, not hang or attempt a multi-GB allocation.
    #[test]
    fn read_meta_group_rejects_group_length_exceeding_available_data() {
        let mut bytes = Vec::new();
        // (0002,0000) UL, value length 4, declared group length far beyond what follows.
        bytes.extend_from_slice(&[0x02, 0x00, 0x00, 0x00]); // tag
        bytes.extend_from_slice(b"UL"); // VR
        bytes.extend_from_slice(&4u16.to_le_bytes()); // value length
        bytes.extend_from_slice(&0xFFFF_FFF0u32.to_le_bytes()); // group length: ~4GiB
        bytes.extend_from_slice(b"only a few more bytes"); // far short of the declared length

        let result = FileMetaTable::read_meta_group(std::io::Cursor::new(bytes));
        assert!(
            matches!(result, Err(ReadError::Io { .. })),
            "expected a clean I/O error for a group length exceeding available data, got {result:?}"
        );
    }

    /// A `group_length` of exactly 0 is a degenerate but well-formed case (an empty meta group)
    /// and must not be confused with the "exceeds available data" error path above.
    #[test]
    fn read_meta_group_accepts_zero_group_length_as_empty() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&[0x02, 0x00, 0x00, 0x00]);
        bytes.extend_from_slice(b"UL");
        bytes.extend_from_slice(&4u16.to_le_bytes());
        bytes.extend_from_slice(&0u32.to_le_bytes()); // group length: 0

        let result = FileMetaTable::read_meta_group(std::io::Cursor::new(bytes));
        assert!(result.is_ok(), "expected a zero-length meta group to parse as empty, got {result:?}");
    }
}
