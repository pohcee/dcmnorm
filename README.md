# dcmnorm plugin

Gives Claude the [`dcmnorm`](https://github.com/pohcee/dcmnorm) skill, for working with DICOM medical images through two
native command-line tools:

- `dcmnorm` reads, inspects, edits, anonymizes, validates, transcodes and renders DICOM files (DICOM to JSON and back,
  tag edits, transfer syntax conversion, PNG/JPEG frames, MP4 movies, multiplanar reformation).
- `dcmtalk` talks to PACS and other DICOM nodes: C-ECHO, C-STORE, C-FIND, C-MOVE and a storage SCP.

The plugin itself is just the skill. The first time a task needs the tools, Claude checks for them and, if they're
missing, installs the version the skill was written for from PyPI (`uv tool install dcmnorm==<version>`, or pipx /
pip). The package holds prebuilt binaries for Linux, macOS and Windows; nothing runs in Python. Video transfer
syntaxes and MP4 output also use the `ffmpeg` command when it's installed.

De-identification (`--set` edits) is only as complete as the edits applied, so verify the output before sharing images.

Install in Claude Code from this repository's marketplace:

```
/plugin marketplace add pohcee/dcmnorm
/plugin install dcmnorm@dcmnorm
```

## Privacy

The plugin and the tools collect nothing and run no service: there is no account, telemetry or server of ours that
receives data.

- `dcmnorm` works on files on your machine (or in Claude's sandbox) and writes only the output files you ask for.
- `dcmtalk` sends data over the network only to the DICOM nodes you name in the command: C-STORE sends the files you
  give it to that peer, C-FIND sends your query keys, C-MOVE asks the peer to send a study to an AE title it knows, and
  the storage SCP receives what peers send it. DICOM files carry patient data, so point these only at nodes you are
  allowed to send that data to.
- Installing the tools downloads them from PyPI; nothing about your data is sent.

What Claude itself does with the conversation and any files is covered by Anthropic's terms and privacy policy.

Licensed under Apache-2.0. Source, issues and the full CLI documentation: https://github.com/pohcee/dcmnorm
