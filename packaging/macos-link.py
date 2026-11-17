#!/usr/bin/env python3
"""Linker wrapper for the macOS builds of the bindings (FFmpeg linked in statically).

The release workflow sets it as the Rust linker for the Apple targets:

    CARGO_TARGET_AARCH64_APPLE_DARWIN_LINKER=<repo>/packaging/macos-link.py
    CARGO_TARGET_X86_64_APPLE_DARWIN_LINKER=<repo>/packaging/macos-link.py

ffmpeg-sys-next 8.1's build script emits link arguments written for other platforms:
  - `-Wl,--no-as-needed` (unconditionally), a GNU ld option Apple's linker rejects;
  - `-framework <F>` for a fixed list that includes frameworks current SDKs no longer ship
    (QTKit, for example), which fails the link with "framework not found".
This drops exactly those (a framework only when the SDK has no <F>.framework), also inside an
@response file when rustc passes one, and runs the system `cc` with the rest. Standard library
only. MACOS_LINK_SDK overrides the SDK path and MACOS_LINK_DRY_RUN=1 prints the command instead
(for testing on any machine).
"""

import os
import subprocess
import sys
from pathlib import Path

DROP = {"-Wl,--no-as-needed", "-Wl,--as-needed"}


def sdk_path() -> Path:
    if os.environ.get("MACOS_LINK_SDK"):
        return Path(os.environ["MACOS_LINK_SDK"])
    return Path(subprocess.run(["xcrun", "--show-sdk-path"], check=True, capture_output=True, text=True).stdout.strip())


def filter_args(args, sdk):
    kept, i = [], 0
    while i < len(args):
        arg = args[i]
        if arg == "-framework" and i + 1 < len(args):
            name = args[i + 1]
            if (sdk / "System" / "Library" / "Frameworks" / f"{name}.framework").is_dir():
                kept += [arg, name]
            else:
                print(f"macos-link.py: dropping -framework {name} (not in {sdk})", file=sys.stderr)
            i += 2
            continue
        if arg not in DROP:
            kept.append(arg)
        i += 1
    return kept


def main() -> None:
    sdk = sdk_path()
    args = []
    for arg in sys.argv[1:]:
        response = Path(arg[1:]) if arg.startswith("@") else None
        if response and response.is_file():
            # rustc writes one argument per line.
            lines = response.read_text(encoding="utf-8").splitlines()
            response.write_text("\n".join(filter_args(lines, sdk)) + "\n", encoding="utf-8")
        args.append(arg)
    command = ["cc", *filter_args(args, sdk)]
    if os.environ.get("MACOS_LINK_DRY_RUN") == "1":
        print(" ".join(command))
        return
    os.execvp(command[0], command)


if __name__ == "__main__":
    main()
