# dcmtalk CLI

## dcmtalk CLI Usage

`dcmtalk` is a DIMSE (DICOM network) client/server covering the same ground as [dcmtk](https://dcmtk.org/)'s
`echoscu`/`storescu`/`findscu`/`movescu`/`storescp`, built on this repository's own DICOM
Upper Layer implementation (no [dcmtk](https://dcmtk.org/) dependency).

Get the full option reference from either help form, for the tool itself or any subcommand:

```bash
dcmtalk -h
dcmtalk --help
dcmtalk echoscu --help
```

Command shape:

```text
dcmtalk <SUBCOMMAND> [OPTIONS] <ARGS>
```

Every SCU subcommand (`echoscu`/`storescu`/`findscu`/`movescu`) shares:

- `<DESTINATION>`: peer address as `HOST:PORT`
- `-a`, `--calling-aet <AE>`: our AE title (default `DCMTALK`)
- `-c`, `--called-aet <AE>`: the peer's AE title, if it requires one to match
- `--timeout <SECONDS>`: absolute timeout for the whole operation (connect through release)
- `-v`, `--verbose`: log association negotiation, presentation contexts, and each DIMSE command/response to stderr

### C-ECHO: verify connectivity

```bash
dcmtalk echoscu somepacs.example.com:11112
dcmtalk echoscu --verbose somepacs.example.com:11112
```

### C-STORE: send files

Sends one or more DICOM files; directories are scanned recursively. Files are sent under
their native transfer syntax when the peer accepts it, transcoded to Explicit/Implicit VR
Little Endian otherwise (unless `--never-transcode`):

```bash
dcmtalk storescu somepacs.example.com:11112 test/files/dx.dcm
dcmtalk storescu somepacs.example.com:11112 test/files/ --max-pdu 65536
```

### C-FIND: query studies

Query keys are DICOM keywords as `KEY=VALUE` (match) or bare `KEY` (return key, universal
match), repeatable. Matches print as one DICOM JSON line per study to stdout:

```bash
dcmtalk findscu somepacs.example.com:11112 -k PatientID=12345 -k StudyDate
```

### C-MOVE: retrieve a study

Asks the peer to push a study to another AE title it already knows how to reach:

```bash
dcmtalk movescu somepacs.example.com:11112 MY_STORE_AE 1.2.840.113619.2.55.3.604688119.971.1600000000.123
```

### storescp: receive files

Listens for inbound associations and writes C-STORE'd instances under `--cache-path` as
`S_<StudyInstanceUID>/<Modality>_<SOPInstanceUID>.dcm`. C-FIND/C-MOVE requests are answered
"unable to process" — this is a receive-only SCP, not a full PACS:

```bash
dcmtalk storescp 11112 --ae-title MY_STORE_AE --cache-path ./received
```

Use port `0` to bind an ephemeral port (useful for tests):

```bash
dcmtalk storescp 0 --verbose
```
