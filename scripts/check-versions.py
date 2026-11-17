#!/usr/bin/env python3
"""Keeps the plugin, skill and binding package versions in step with the CLI version.

    scripts/check-versions.py            check: exit 1 if any pin differs from exec/dcmnorm/Cargo.toml
    scripts/check-versions.py --set X    write X into every pin (scripts/release-tag.sh does this)

The pins: the plugin's version in each of its manifests (Claude: plugin/.claude-plugin/plugin.json
and its marketplace entry in .claude-plugin/marketplace.json; Gemini CLI:
plugin/gemini-extension.json; Antigravity: plugin/.antigravity-plugin/plugin.json) and the version
the skill installs and names in its Setup section (plugin/skills/dcmnorm/SKILL.md). The skill installs `dcmnorm==<pin>` from PyPI, so it
must name a version that is (or, at release time, is about to be) published.

The binding packages publish with the release version too (packaging/README.md): the npm packages
(bindings/node/package.json: its version and every optionalDependencies platform package, and the
version the napi-generated bindings/node/index.js expects of them), the PyPI wheel
(bindings/python/pyproject.toml, in its PEP 440 form) and the Maven artifacts
(bindings/java/pom.xml and bindings/java/natives/pom.xml).
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
# A copy of the skill at the repository root, where skill indexers (gh skill, skills.sh, crawlers) look
# for skills/<name>/SKILL.md. It must stay byte-identical to the plugin's copy.
ROOT_SKILL_MD = REPO_ROOT / "skills" / "dcmnorm" / "SKILL.md"
# The same plugin's manifests for other agents; each holds a top-level "version".
OTHER_MANIFESTS = [
    REPO_ROOT / "plugin" / "gemini-extension.json",
    REPO_ROOT / "plugin" / ".antigravity-plugin" / "plugin.json",
]

VERSION = r"\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?"  # SemVer, as in Cargo.toml and plugin.json
PYPI_VERSION = r"\d+\.\d+\.\d+(?:(?:a|b|rc)\d+)?"  # the PyPI form the skill installs
NODE_PACKAGE_JSON = REPO_ROOT / "bindings" / "node" / "package.json"
NODE_INDEX_JS = REPO_ROOT / "bindings" / "node" / "index.js"
PYPROJECT = REPO_ROOT / "bindings" / "python" / "pyproject.toml"
POMS = [REPO_ROOT / "bindings" / "java" / "pom.xml", REPO_ROOT / "bindings" / "java" / "natives" / "pom.xml"]
# napi's loader checks each platform package's version against this (when
# NAPI_RS_ENFORCE_VERSION_CHECK is set): `bindingPackageVersion !== '0.11.2'` and its message.
INDEX_JS_PATTERNS = [
    re.compile(rf"(bindingPackageVersion !== ')({VERSION})(')"),
    re.compile(rf"(expected )({VERSION})( but got)"),
]
PYPROJECT_VERSION = re.compile(rf'(?m)^(version = ")({PYPI_VERSION})(")')
# The project's own <version>: the first one after </artifactId> at the top level.
POM_VERSION = re.compile(rf"(<artifactId>dcmnorm-java(?:-natives)?</artifactId>\s*<version>)({VERSION})(</version>)")
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
    pins = [(PLUGIN_JSON, plugin["version"]), (MARKETPLACE_JSON, entries[0]["version"])]
    for path in OTHER_MANIFESTS:
        pins.append((path, json.loads(path.read_text(encoding="utf-8"))["version"]))
    return pins


def binding_pins() -> list:
    """(path, found, expected kind) for every binding version; kind "semver" or "pypi"."""
    pins = []
    node = json.loads(NODE_PACKAGE_JSON.read_text(encoding="utf-8"))
    pins.append((NODE_PACKAGE_JSON, node["version"], "semver"))
    for name, found in sorted(node.get("optionalDependencies", {}).items()):
        pins.append((NODE_PACKAGE_JSON, f"{found} ({name})", "semver"))
    index = NODE_INDEX_JS.read_text(encoding="utf-8")
    found_index = {m.group(2) for pattern in INDEX_JS_PATTERNS for m in pattern.finditer(index)}
    if not found_index:
        sys.exit(f"no platform package version check found in {NODE_INDEX_JS}")
    pins.extend((NODE_INDEX_JS, found, "semver") for found in sorted(found_index))
    match = PYPROJECT_VERSION.search(PYPROJECT.read_text(encoding="utf-8"))
    if not match:
        sys.exit(f"no [project] version in {PYPROJECT}")
    pins.append((PYPROJECT, match.group(2), "pypi"))
    for pom in POMS:
        match = POM_VERSION.search(pom.read_text(encoding="utf-8"))
        if not match:
            sys.exit(f"no project version in {pom}")
        pins.append((pom, match.group(2), "semver"))
    return pins


def set_binding_versions(version: str) -> None:
    node = json.loads(NODE_PACKAGE_JSON.read_text(encoding="utf-8"))
    node["version"] = version
    for name in node.get("optionalDependencies", {}):
        node["optionalDependencies"][name] = version
    NODE_PACKAGE_JSON.write_text(json.dumps(node, indent=2, ensure_ascii=False) + "\n", encoding="utf-8")
    text = NODE_INDEX_JS.read_text(encoding="utf-8")
    for pattern in INDEX_JS_PATTERNS:
        text = pattern.sub(rf"\g<1>{version}\g<3>", text)
    NODE_INDEX_JS.write_text(text, encoding="utf-8")
    text = PYPROJECT.read_text(encoding="utf-8")
    PYPROJECT.write_text(PYPROJECT_VERSION.sub(rf"\g<1>{pypi_version(version)}\g<3>", text, count=1), encoding="utf-8")
    for pom in POMS:
        text = pom.read_text(encoding="utf-8")
        pom.write_text(POM_VERSION.sub(rf"\g<1>{version}\g<3>", text, count=1), encoding="utf-8")


def skill_pins(text: str) -> list:
    found = []
    for pattern in SKILL_PATTERNS:
        found.extend(match.group(2) for match in pattern.finditer(text))
    if not found:
        sys.exit(f"no version pins found in {SKILL_MD}")
    return found


def set_versions(version: str) -> None:
    for path in (PLUGIN_JSON, MARKETPLACE_JSON, *OTHER_MANIFESTS):
        text = path.read_text(encoding="utf-8")
        # Only the "version" keys; a textual edit keeps the files' formatting.
        text = re.sub(rf'("version":\s*")({VERSION})(")', rf"\g<1>{version}\g<3>", text)
        path.write_text(text, encoding="utf-8")
    pinned = pypi_version(version)
    text = SKILL_MD.read_text(encoding="utf-8")
    for pattern in SKILL_PATTERNS:
        text = pattern.sub(rf"\g<1>{pinned}\g<3>", text)
    SKILL_MD.write_text(text, encoding="utf-8")
    ROOT_SKILL_MD.parent.mkdir(parents=True, exist_ok=True)
    ROOT_SKILL_MD.write_text(text, encoding="utf-8")
    set_binding_versions(version)


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
    for path, found, kind in binding_pins():
        expected = expected_skill if kind == "pypi" else version
        if found.split(" ")[0] != expected:
            problems.append(f"{path.relative_to(REPO_ROOT)}: version {found}, expected {expected}")
    if not ROOT_SKILL_MD.is_file() or ROOT_SKILL_MD.read_bytes() != SKILL_MD.read_bytes():
        problems.append(
            f"{ROOT_SKILL_MD.relative_to(REPO_ROOT)} differs from {SKILL_MD.relative_to(REPO_ROOT)}"
            " (the skill is edited in plugin/; --set copies it)"
        )
    if problems:
        print("\n".join(problems), file=sys.stderr)
        sys.exit("version pins are out of step with exec/dcmnorm/Cargo.toml"
                 " (scripts/check-versions.py --set <version> fixes them)")
    print(f"version pins match {version}")


if __name__ == "__main__":
    main()
