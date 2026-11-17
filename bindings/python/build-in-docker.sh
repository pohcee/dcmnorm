#!/bin/bash
# Builds the release wheel (dcmnorm-<v>-cp39-abi3-manylinux_2_28_<arch>.whl: the extension module
# plus the dcmnorm/dcmtalk CLIs) for this machine's architecture in a manylinux_2_28 container,
# into dist/. Stage the CLI binaries first, from the CLI-only wheel packaging/package.py writes:
#
#   packaging/build-linux.sh
#   python3 packaging/package.py --platform linux-x86_64 --bin-dir target/manylinux_2_28-x86_64/release --out-dir dist
#   python3 packaging/stage_cli.py dist/dcmnorm-*-py3-none-manylinux_2_28_x86_64.whl
#   bindings/python/build-in-docker.sh
#   python3 packaging/smoke_test.py bindings/python/dist/*.whl --same-cli-as dist/dcmnorm-*-py3-none-manylinux_2_28_x86_64.whl
#
# See packaging/build-binding-linux.sh.
set -euo pipefail
exec "$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)/packaging/build-binding-linux.sh" python "$@"
