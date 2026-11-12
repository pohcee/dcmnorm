#!/usr/bin/env python3
"""Keeps the Claude plugin and skill version pins in step with the CLI version.

    scripts/check-versions.py            check: exit 1 if any pin differs from exec/dcmnorm/Cargo.toml
    scripts/check-versions.py --set X    write X into every pin (scripts/release-tag.sh does this)

The pins: the plugin's version (plugin/.claude-plugin/plugin.json and its marketplace entry in
.claude-plugin/marketplace.json) and the version the skill installs and names in its Setup
section (plugin/skills/dcmnorm/SKILL.md). The skill installs `dcmnorm==<pin>` from PyPI, so it
must name a version that is (or, at release time, is about to be) published.
"""

import argparse
import json
import re
import sys
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parent.parent
CARGO_TOML = REPO_ROOT / "exec" / "dcmnorm" / "Cargo.toml"
PLUGIN_JSON = REPO_ROOT / "plugin" / ".claude-plugin" / "plugin.json"
MARKETPLACE_JSON = REPO_ROOT / ".claude-plugin" / "marketplace.json"
SKILL_MD = REPO_ROOT / "plugin" / "skills" / "dcmnorm" / "SKILL.md"

VERSION = r"\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?"  # SemVer, as in Cargo.toml and plugin.json
PYPI_VERSION = r"\d+\.\d+\.\d+(?:(?:a|b|rc)\d+)?"  # the PyPI form the skill installs
# Every place the skill names its version: "**dcmnorm X**", "version X or newer", "dcmnorm==X".
SKILL_PATTERNS = [
    re.compile(rf"(\*\*dcmnorm )({PYPI_VERSION})(\*\*)"),
    re.compile(rf"(version )({PYPI_VERSION})( or newer)"),
    re.compile(rf"(dcmnorm==)({PYPI_VERSION})()"),
]


def cargo_version() -> str:
    in_package = False
    for line in CARGO_TOML.read_text(encoding="utf-8").splitlines():
        stripped = line.strip()
        if stripped.startswith("["):
            in_package = stripped == "[package]"
            continue
        match = re.match(r'version\s*=\s*"([^"]+)"', stripped)
        if in_package and match:
            return match.group(1)
    sys.exit(f"no [package] version in {CARGO_TOML}")


def pypi_version(semver: str) -> str:
    """The skill pins the PyPI form: 0.7.3-rc.1 -> 0.7.3rc1 (see packaging/package.py)."""
    sys.path.insert(0, str(REPO_ROOT / "packaging"))
    import package  # noqa: E402

    return package.pep440_version(semver)


def json_pins() -> list:
    plugin = json.loads(PLUGIN_JSON.read_text(encoding="utf-8"))
    marketplace = json.loads(MARKETPLACE_JSON.read_text(encoding="utf-8"))
    entries = [p for p in marketplace["plugins"] if p["name"] == plugin["name"]]
    if len(entries) != 1:
        sys.exit(f"{MARKETPLACE_JSON} must list the {plugin['name']} plugin exactly once")
    return [(PLUGIN_JSON, plugin["version"]), (MARKETPLACE_JSON, entries[0]["version"])]


def skill_pins(text: str) -> list:
    found = []
    for pattern in SKILL_PATTERNS:
        found.extend(match.group(2) for match in pattern.finditer(text))
    if not found:
        sys.exit(f"no version pins found in {SKILL_MD}")
    return found


def set_versions(version: str) -> None:
    for path in (PLUGIN_JSON, MARKETPLACE_JSON):
        text = path.read_text(encoding="utf-8")
        # Only the "version" keys; a textual edit keeps the files' formatting.
        text = re.sub(rf'("version":\s*")({VERSION})(")', rf"\g<1>{version}\g<3>", text)
        path.write_text(text, encoding="utf-8")
    pinned = pypi_version(version)
    text = SKILL_MD.read_text(encoding="utf-8")
    for pattern in SKILL_PATTERNS:
        text = pattern.sub(rf"\g<1>{pinned}\g<3>", text)
    SKILL_MD.write_text(text, encoding="utf-8")


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--set", metavar="VERSION", help="write VERSION into every pin")
    args = parser.parse_args()

    if args.set:
        set_versions(args.set)

    version = cargo_version()
    expected_skill = pypi_version(version)
    problems = []
    for path, found in json_pins():
        if found != version:
            problems.append(f"{path.relative_to(REPO_ROOT)}: version {found}, expected {version}")
    for found in skill_pins(SKILL_MD.read_text(encoding="utf-8")):
        if found != expected_skill:
            problems.append(f"{SKILL_MD.relative_to(REPO_ROOT)}: pins {found}, expected {expected_skill}")
    if problems:
        print("\n".join(problems), file=sys.stderr)
        sys.exit("version pins are out of step with exec/dcmnorm/Cargo.toml"
                 " (scripts/check-versions.py --set <version> fixes them)")
    print(f"version pins match {version}")


if __name__ == "__main__":
    main()
