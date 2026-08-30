# dcmnorm-object

dcmnorm's own DICOM in-memory object model and Part 10 file I/O
(`InMemDicomObject`, `FileDicomObject`/`DefaultDicomObject`, `FileMetaTable`/
`FileMetaTableBuilder`, `OpenFileOptions`), replacing `dicom-object` +
`dicom-parser`'s tree-building layer.

Unlike the other `dcmnorm-*` crates (`core`, `dictionary`, `encoding`,
`transcode`, `parser`), this one is **not** a mechanical port — it's
new, purpose-built code, scoped to exactly what `dcmnorm` itself needs rather
than `dicom-object`'s full general-purpose, dictionary-generic API. See the
phased dicom-rs removal plan (Phase 4) for why: nothing else in the
dependency graph (`dicom-ul` doesn't touch it) forces this crate to keep the
`dicom-object` name or its full surface, unlike the other forks.

Design choices, and why:

- **Elements are stored as a `Vec<InMemElement>` sorted by tag, not a
  `BTreeMap`/`HashMap`.** DICOM datasets are always encoded in ascending tag
  order on disk (PS3.5 §7.1), and dcmnorm's access pattern is
  parse-once-query-many. A sorted `Vec` with binary-search lookup avoids a
  per-element tree-node allocation, keeps elements contiguous in memory
  (cache-friendly iteration, which `standard_json.rs`/`flat_json.rs`/render
  do a lot of), and preserves on-disk order for free when writing back out -
  no separate order-tracking needed. Insertion is O(n) instead of a map's
  O(log n), but objects are built once via a single ordered parse pass
  (`InMemDicomObject::build_object`, which appends in the token stream's
  order and only needs to search when handling out-of-order `.put()` calls
  from CLI edits) rather than element-by-element random insertion.
- **The wire-format value codec (per-VR byte encode/decode, tag/VR/length
  header framing) is not reimplemented here.** It's inherited from
  `dcmnorm-parser`'s `DataSetReader`/`DataSetWriter` token stream - a
  mechanical, byte-identical fork of `dicom-parser`, itself the same proven
  engine `dicom-object` always built on. This crate's own code is the tree
  assembly/orchestration layer that folds that token stream into
  `InMemDicomObject` and back, plus `FileMetaTable`/Part 10 preamble
  handling - the part where dcmnorm's specific needs and behavior fixes
  actually live.
- **The meta group length (0002,0000) is computed at serialization time,
  always, not cached and periodically refreshed.** `dicom-object`'s
  equivalent could go stale relative to what it actually serialized;
  dcmnorm used to work around this by calling `refresh_meta_group_length()`
  before every write call site. Here there's no cached length to go stale -
  `FileMetaTable::write_to` always computes it from the meta elements it's
  about to write, so the bug class is structurally impossible.
- **Errors are simple, dcmnorm-owned enums, not a full port of
  `dicom-object`'s multi-variant `snafu` error types.** Confirmed by grep
  that no dcmnorm code ever matches on a specific `ReadError`/`WriteError`/
  `WithMetaError` variant - they're only ever propagated via `?`/`Display` -
  so there was no reason to replicate that surface.
