# The typed dataset API of #6: read / to_dicom_json / write / render_pixels / transcode_to,
# DicomDataset and its builder. Run by the extension module when `dcmnorm` is imported (src/lib.rs
# embeds this file), so everything here is plain `dcmnorm.<name>`; `_bind` hands it the native
# `_dataset_*` functions. A data set crosses from native code as bytes in the core's binary form
# (src/dicom_io/dataset_tree.rs), decoded below.
import os as _os
import struct as _struct
from collections import namedtuple as _namedtuple

__all__ = [
    "BulkDataRef",
    "DicomElement",
    "PersonName",
    "DicomDataset",
    "DicomDatasetBuilder",
    "RenderedPixels",
    "read",
    "to_dicom_json",
    "write",
    "render_pixels",
    "transcode_to",
]

_native = None
Tag = None


def _bind(module):
    global _native, Tag
    _native = module
    Tag = module.Tag


BulkDataRef = _namedtuple("BulkDataRef", "tag vr offset length")
BulkDataRef.__doc__ = """A value left in its source: absolute byte offset and length (-1: undefined,
for compressed pixel data a read stopped at)."""

DicomElement = _namedtuple("DicomElement", "tag vr type value")
DicomElement.__doc__ = """One attribute: ``type`` is how ``value`` is held - 'strings' (tuple of str),
'ints', 'floats', 'tags' (tuples), 'bytes', 'bulk_data' (BulkDataRef), 'fragments' ((offset table,
fragments)), 'sequence' (tuple of DicomDataset) or 'empty' (None)."""

_KINDS = ("empty", "strings", "ints", "floats", "tags", "bytes", "bulk_data", "fragments", "sequence")


# ---------------------------------------------------------------------------------------------
# The binary data set form
# ---------------------------------------------------------------------------------------------


def _decode(data, source):
    view = memoryview(data)
    pos = 5
    if bytes(view[:4]) != b"DCMT" or view[4] != 1:
        raise ValueError("not a binary data set")

    def u32():
        nonlocal pos
        (value,) = _struct.unpack_from(">I", view, pos)
        pos += 4
        return value

    def string():
        nonlocal pos
        n = u32()
        text = bytes(view[pos : pos + n]).decode("utf-8")
        pos += n
        return text

    def take(n):
        nonlocal pos
        chunk = bytes(view[pos : pos + n])
        pos += n
        return chunk

    def tree():
        nonlocal pos
        elements = {}
        for _ in range(u32()):
            tag = u32()
            vr = bytes(view[pos : pos + 2]).decode("ascii")
            kind = view[pos + 2]
            pos += 3
            if kind == 0:
                value = None
            elif kind == 1:
                value = tuple(string() for _ in range(u32()))
            elif kind == 2:
                n = u32()
                value = _struct.unpack_from(">%dq" % n, view, pos)
                pos += 8 * n
            elif kind == 3:
                n = u32()
                value = _struct.unpack_from(">%dd" % n, view, pos)
                pos += 8 * n
            elif kind == 4:
                n = u32()
                value = _struct.unpack_from(">%dI" % n, view, pos)
                pos += 4 * n
            elif kind == 5:
                value = take(u32())
            elif kind == 6:
                offset, length = _struct.unpack_from(">Qq", view, pos)
                pos += 16
                value = BulkDataRef(tag, vr, offset, length)
            elif kind == 7:
                n = u32()
                table = _struct.unpack_from(">%dI" % n, view, pos)
                pos += 4 * n
                value = (table, tuple(take(u32()) for _ in range(u32())))
            elif kind == 8:
                value = tuple(DicomDataset(tree()) for _ in range(u32()))
            else:
                raise ValueError("unknown value kind %d" % kind)
            elements[tag] = DicomElement(tag, vr, _KINDS[kind], value)
        return elements

    transfer_syntax_uid = string()
    meta = tree()
    dataset = tree()
    return DicomDataset(dataset, DicomDataset(meta), transfer_syntax_uid or None, source)


def _encode(dataset):
    parts = [b"DCMT\x01"]

    def u32(value):
        parts.append(_struct.pack(">I", value & 0xFFFFFFFF))

    def string(text):
        data = text.encode("utf-8")
        u32(len(data))
        parts.append(data)

    def tree(ds):
        u32(len(ds))
        for e in ds:
            u32(e.tag)
            parts.append(e.vr.encode("ascii") + bytes([_KINDS.index(e.type)]))
            v = e.value
            if e.type == "empty":
                pass
            elif e.type == "strings":
                u32(len(v))
                for text in v:
                    string(text)
            elif e.type == "ints":
                u32(len(v))
                parts.append(_struct.pack(">%dq" % len(v), *v))
            elif e.type == "floats":
                u32(len(v))
                parts.append(_struct.pack(">%dd" % len(v), *v))
            elif e.type == "tags":
                u32(len(v))
                parts.append(_struct.pack(">%dI" % len(v), *v))
            elif e.type == "bytes":
                u32(len(v))
                parts.append(bytes(v))
            elif e.type == "bulk_data":
                parts.append(_struct.pack(">Qq", v.offset, v.length))
            elif e.type == "fragments":
                table, fragments = v
                u32(len(table))
                parts.append(_struct.pack(">%dI" % len(table), *table))
                u32(len(fragments))
                for fragment in fragments:
                    u32(len(fragment))
                    parts.append(bytes(fragment))
            elif e.type == "sequence":
                u32(len(v))
                for item in v:
                    tree(item)
            else:
                raise ValueError("unknown value type %r" % e.type)

    string(dataset.transfer_syntax_uid or "")
    tree(dataset.file_meta)
    tree(dataset)
    return b"".join(parts)


# ---------------------------------------------------------------------------------------------
# DicomDataset
# ---------------------------------------------------------------------------------------------


class PersonName:
    """A PN value split into its groups (alphabetic, ideographic, phonetic) and components."""

    __slots__ = ("value", "alphabetic", "ideographic", "phonetic")

    Group = _namedtuple("Group", "family_name given_name middle_name name_prefix name_suffix")

    def __init__(self, value):
        self.value = value or ""
        groups = self.value.split("=")

        def group(text):
            c = text.split("^")
            c += [""] * (5 - len(c))
            return PersonName.Group(c[0], c[1], c[2], c[3], "^".join(c[4:]))

        self.alphabetic = group(groups[0] if groups else "")
        self.ideographic = group(groups[1] if len(groups) > 1 else "")
        self.phonetic = group(groups[2] if len(groups) > 2 else "")

    @property
    def family_name(self):
        return self.alphabetic.family_name

    @property
    def given_name(self):
        return self.alphabetic.given_name

    def __str__(self):
        return self.value

    def __repr__(self):
        return "PersonName(%r)" % self.value


def _number(text):
    text = text.strip()
    try:
        return int(text)
    except ValueError:
        return float(text)


def _format(value):
    if isinstance(value, float) and value.is_integer() and abs(value) < 1e15:
        return str(int(value))
    return str(value)


class DicomDataset:
    """A read-only DICOM data set as plain Python values, from :func:`read`. Holds nothing native.

    Accessors follow dcm4che's naming in Python style: ``get_string(Tag.StudyInstanceUID)``,
    ``get_int(Tag.NumberOfFrames, 1)``, ``get_sequence(Tag.ReferencedSeriesSequence)``.
    Single-value getters return the default (``None`` unless given) when the attribute is absent
    or that value is empty; list getters return ``None`` when absent. Iterating yields
    :class:`DicomElement` s in tag order; ``ds[tag]`` is the element, ``tag in ds`` works.
    Large bulk values (PixelData, binary values over 32 bytes) are :class:`BulkDataRef` s into the
    source unless read with ``load_bulk_data=True``. Edit with :meth:`to_builder`.
    """

    __slots__ = ("_elements", "_file_meta", "transfer_syntax_uid", "_source")

    def __init__(self, elements=None, file_meta=None, transfer_syntax_uid=None, source=None):
        self._elements = dict(sorted((elements or {}).items()))
        self._file_meta = file_meta
        self.transfer_syntax_uid = transfer_syntax_uid
        self._source = source

    @staticmethod
    def empty():
        return _EMPTY

    def __len__(self):
        return len(self._elements)

    def __iter__(self):
        return iter(self._elements.values())

    def __contains__(self, tag):
        return tag in self._elements

    def __getitem__(self, tag):
        return self._elements[tag]

    def __eq__(self, other):
        return isinstance(other, DicomDataset) and self._elements == other._elements

    def __repr__(self):
        return "DicomDataset(%d elements%s)" % (
            len(self._elements),
            ", " + self.transfer_syntax_uid if self.transfer_syntax_uid else "",
        )

    def get(self, tag):
        return self._elements.get(tag)

    def tags(self):
        return list(self._elements)

    def vr(self, tag):
        e = self._elements.get(tag)
        return e.vr if e else None

    @property
    def file_meta(self):
        """The File Meta Information read with the data set (``write`` creates it afresh)."""
        return self._file_meta if self._file_meta is not None else _EMPTY

    @property
    def transfer_syntax(self):
        """:attr:`transfer_syntax_uid` as a ``TransferSyntax`` member, or None."""
        return _native.TransferSyntax.from_uid(self.transfer_syntax_uid) if self.transfer_syntax_uid else None

    def get_string(self, tag, default=None, index=0):
        e = self._elements.get(tag)
        if e is None or not isinstance(e.value, tuple) or index >= len(e.value):
            return default
        v = e.value[index]
        if e.type == "strings":
            return v if v != "" else default
        if e.type in ("ints", "floats"):
            return _format(v)
        if e.type == "tags":
            return "%08X" % v
        return default

    def get_strings(self, tag):
        e = self._elements.get(tag)
        if e is None:
            return None
        if e.type == "strings":
            return list(e.value)
        if e.type in ("ints", "floats"):
            return [_format(v) for v in e.value]
        if e.type == "tags":
            return ["%08X" % v for v in e.value]
        return []

    def get_number(self, tag, default=None, index=0):
        """Value ``index`` as an int or float (IS/DS parsed), or ``default``."""
        e = self._elements.get(tag)
        if e is None or not isinstance(e.value, tuple) or index >= len(e.value):
            return default
        if e.type == "strings":
            try:
                return _number(e.value[index])
            except ValueError:
                return default
        return e.value[index] if e.type in ("ints", "floats", "tags") else default

    def get_int(self, tag, default=None, index=0):
        v = self.get_number(tag, None, index)
        return default if v is None else int(v)

    def get_float(self, tag, default=None, index=0):
        v = self.get_number(tag, None, index)
        return default if v is None else float(v)

    def get_numbers(self, tag):
        """Every value as a number; None if absent (ValueError for one that isn't a number)."""
        e = self._elements.get(tag)
        if e is None:
            return None
        if e.type == "strings":
            return [_number(v) for v in e.value]
        return list(e.value) if e.type in ("ints", "floats", "tags") else []

    def get_ints(self, tag):
        v = self.get_numbers(tag)
        return None if v is None else [int(x) for x in v]

    def get_floats(self, tag):
        v = self.get_numbers(tag)
        return None if v is None else [float(x) for x in v]

    def get_bytes(self, tag):
        """A binary value that was read, or None (absent, or left in the source)."""
        e = self._elements.get(tag)
        if e is None:
            return None
        if e.type == "bytes":
            return e.value
        return b"" if e.type == "empty" else None

    def get_bulk_data_ref(self, tag):
        e = self._elements.get(tag)
        return e.value if e is not None and e.type == "bulk_data" else None

    def get_sequence(self, tag):
        e = self._elements.get(tag)
        return list(e.value) if e is not None and e.type == "sequence" else None

    def get_nested_dataset(self, tag, index=0):
        items = self.get_sequence(tag)
        return items[index] if items is not None and index < len(items) else None

    def get_person_name(self, tag, index=0):
        v = self.get_string(tag, None, index)
        return None if v is None else PersonName(v)

    @property
    def number_of_frames(self):
        return max(1, self.get_int(Tag.NumberOfFrames, 1))

    @property
    def frame_time_ms(self):
        """FrameTime (0018,1063), or None."""
        return self.get_float(Tag.FrameTime)

    @property
    def frame_time_vector(self):
        return self.get_floats(Tag.FrameTimeVector) if Tag.FrameTimeVector in self else None

    @property
    def pixel_aspect_ratio(self):
        """Vertical / horizontal pixel size: PixelAspectRatio, else the PixelSpacing ratio, else 1."""
        for tag in (Tag.PixelAspectRatio, Tag.PixelSpacing):
            v = self.get_floats(tag) if tag in self else None
            if v and len(v) == 2 and v[0] > 0 and v[1] > 0:
                return v[0] / v[1]
        return 1.0

    def to_builder(self):
        return DicomDatasetBuilder(self)


_EMPTY = DicomDataset()


class DicomDatasetBuilder:
    """Builds a modified copy of a DicomDataset; bulk data references stay references, read from
    the source when written."""

    def __init__(self, source):
        self._elements = dict(source._elements)
        self._file_meta = source._file_meta
        self._transfer_syntax_uid = source.transfer_syntax_uid
        self._source = source._source

    def _put(self, tag, vr, type_, value):
        if tag >> 16 == 0x0002:
            raise ValueError("File Meta Information is created by write(), not set in the data set")
        self._elements[tag] = DicomElement(tag, str(vr), type_, value)
        return self

    def set(self, tag, vr, *values):
        """Text values for any VR (a binary VR parses them when written): ``set(tag, VR.PN,
        "Doe^Jane")``, or ``set(tag, ["value"])`` with the dictionary VR."""
        if isinstance(vr, (list, tuple)):
            values, vr = tuple(vr), _native.vr_of(tag)
            if vr is None:
                raise ValueError("%08X is not in the dictionary: give its VR" % tag)
        vr = str(getattr(vr, "value", vr))
        if vr == "SQ":
            raise ValueError("use set_sequence for SQ")
        if not values:
            return self._put(tag, vr, "empty", None)
        return self._put(tag, vr, "strings", tuple("" if v is None else str(v) for v in values))

    def set_int(self, tag, vr, *values):
        return self._put(tag, getattr(vr, "value", vr), "ints" if values else "empty", tuple(int(v) for v in values) or None)

    def set_float(self, tag, vr, *values):
        return self._put(tag, getattr(vr, "value", vr), "floats" if values else "empty", tuple(float(v) for v in values) or None)

    def set_tags(self, tag, *tags):
        return self._put(tag, "AT", "tags" if tags else "empty", tuple(tags) or None)

    def set_bytes(self, tag, vr, data):
        return self._put(tag, getattr(vr, "value", vr), "bytes" if data else "empty", bytes(data) or None)

    def set_null(self, tag, vr):
        return self._put(tag, getattr(vr, "value", vr), "empty", None)

    def set_sequence(self, tag, items):
        return self._put(tag, "SQ", "sequence", tuple(items))

    def add_sequence_item(self, tag, item):
        e = self._elements.get(tag)
        existing = e.value if e is not None and e.type == "sequence" else ()
        return self.set_sequence(tag, existing + (item,))

    def edit_sequence_item(self, tag, index, edit):
        """``edit(builder)`` item ``index`` of a sequence (nest for deeper edits)."""
        e = self._elements.get(tag)
        if e is None or e.type != "sequence" or not 0 <= index < len(e.value):
            raise IndexError("%08X has no item %d" % (tag, index))
        builder = e.value[index].to_builder()
        edit(builder)
        items = list(e.value)
        items[index] = builder.build()
        return self.set_sequence(tag, items)

    def remove_sequence_item(self, tag, index):
        e = self._elements.get(tag)
        if e is None or e.type != "sequence" or not 0 <= index < len(e.value):
            raise IndexError("%08X has no item %d" % (tag, index))
        return self.set_sequence(tag, e.value[:index] + e.value[index + 1 :])

    def remove(self, tag):
        self._elements.pop(tag, None)
        return self

    def remove_private_tags(self):
        for tag in [t for t in self._elements if (t >> 16) & 1]:
            del self._elements[tag]
        return self

    def build(self):
        return DicomDataset(self._elements, self._file_meta, self._transfer_syntax_uid, self._source)


class RenderedPixels:
    """One rendered frame as 8-bit pixels (no image encoding): ``data`` is ``bytes`` of
    ``height * width * channels``, rows top to bottom, 1 channel (gray) or 3 (RGB interleaved).
    ``data`` is the only copy (made once from the native buffer); :meth:`memoryview` and
    :meth:`to_numpy` view it without copying."""

    __slots__ = ("width", "height", "channels", "data", "overlays", "selected_overlay_index")

    def __init__(self, width, height, channels, data, overlays, selected_overlay_index):
        self.width, self.height, self.channels = width, height, channels
        self.data, self.overlays, self.selected_overlay_index = data, overlays, selected_overlay_index

    @property
    def shape(self):
        """``(height, width)`` for gray, ``(height, width, 3)`` for color."""
        return (self.height, self.width) if self.channels == 1 else (self.height, self.width, self.channels)

    def memoryview(self):
        """A read-only memoryview over :attr:`data` with :attr:`shape`."""
        return memoryview(self.data).cast("B", self.shape)

    def to_numpy(self):
        """A read-only ``numpy.ndarray`` (uint8, :attr:`shape`) over :attr:`data` - needs numpy,
        which dcmnorm doesn't depend on."""
        import numpy

        return numpy.frombuffer(self.data, dtype=numpy.uint8).reshape(self.shape)

    def __repr__(self):
        return "RenderedPixels(%dx%d, %d channel%s)" % (self.width, self.height, self.channels, "" if self.channels == 1 else "s")


# ---------------------------------------------------------------------------------------------
# The API
# ---------------------------------------------------------------------------------------------


def _source_ref(source):
    """What write() can later read bulk data from: a path or the bytes; None for a file object."""
    if isinstance(source, (bytes, bytearray, memoryview)):
        return ("bytes", bytes(source))
    if isinstance(source, (str, _os.PathLike)):
        return ("path", _os.path.abspath(_os.fspath(source)))
    return None


def read(source, *, until_pixel_data=False, load_bulk_data=False):
    """Read a DICOM object into a :class:`DicomDataset`.

    ``source``: a path, bytes-like, or a binary file object (read front to back, not closed; with
    ``until_pixel_data`` nothing past the PixelData header is read). ``until_pixel_data``: stop at
    the PixelData header (PixelData is then a BulkDataRef). ``load_bulk_data``: read bulk values
    instead of leaving them in the source.
    """
    reference = _source_ref(source)
    native_source = reference[1] if reference and reference[0] == "path" else source
    data = _native._dataset_read(native_source, until_pixel_data, load_bulk_data)
    return _decode(data, reference)


def to_dicom_json(
    source,
    *,
    include_tags=None,
    exclude_tags=None,
    bulk_data="uri",
    bulk_data_uri_base=None,
    bulk_data_uri_builder=None,
    include_file_meta=False,
    keyword_and_vm=False,
):
    """PS3.18 Annex F DICOM JSON (a str) of a source or a :class:`DicomDataset`.

    ``bulk_data='uri'`` (default) writes PixelData and binary values over 32 bytes as
    ``BulkDataURI`` without reading them; ``'inline'`` base64-embeds them.
    ``bulk_data_uri_builder(ref: BulkDataRef) -> str`` builds each URI yourself.
    """
    options = {
        "include_tags": list(include_tags) if include_tags else None,
        "exclude_tags": list(exclude_tags) if exclude_tags else None,
        "bulk_data": bulk_data,
        "bulk_data_uri_base": bulk_data_uri_base,
        "include_file_meta": include_file_meta,
        "keyword_and_vm": keyword_and_vm,
    }
    if isinstance(source, DicomDataset):
        kind, value = source._source or (None, None)
        return _native._dataset_document_to_json(
            _encode(source),
            value if kind == "path" else None,
            value if kind == "bytes" else None,
            options,
            bulk_data_uri_builder,
            BulkDataRef,
        )
    return _native._dataset_to_json(source, options, bulk_data_uri_builder, BulkDataRef)


def write(dataset, output=None, *, transfer_syntax=None, bulk_data_source=None):
    """Write ``dataset`` as a DICOM file: to ``output`` (a path, or a binary file object), or
    return its bytes when ``output`` is None. File Meta Information is created for it; bulk data
    references are read natively from the source it was read from (or ``bulk_data_source``, a
    path or bytes). ``transfer_syntax`` transcodes."""
    reference = dataset._source
    if bulk_data_source is not None:
        reference = _source_ref(bulk_data_source)
    kind, value = reference or (None, None)
    path = None
    if output is not None and not hasattr(output, "write"):
        path = _os.fspath(output)
    data = _native._dataset_write(
        _encode(dataset),
        value if kind == "path" else None,
        value if kind == "bytes" else None,
        path,
        str(getattr(transfer_syntax, "value", transfer_syntax)) if transfer_syntax is not None else None,
    )
    if output is None:
        return data
    if path is None:
        output.write(data)
    return None


def render_pixels(source, frame_index=0, **params):
    """Render frame ``frame_index`` to 8-bit pixels (:class:`RenderedPixels`), no image encoding.

    Params: ``window_center``/``window_width``, ``voi_lut``, ``modality_lut``, ``icc_profile``,
    ``output_width``, ``output_height``, ``show_overlays``, ``overlay_index``, ``overlay_color``.
    """
    width, height, channels, data, overlays, selected = _native._dataset_render(source, frame_index, params)
    return RenderedPixels(width, height, channels, data, overlays, selected)


def transcode_to(source, transfer_syntax_uid, output=None):
    """Transcode a source: to ``output`` (a path), or return the bytes."""
    uid = str(getattr(transfer_syntax_uid, "value", transfer_syntax_uid))
    return _native._dataset_transcode(source, _os.fspath(output) if output is not None else None, uid)
