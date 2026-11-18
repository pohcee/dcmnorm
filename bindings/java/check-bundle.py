#!/usr/bin/env python3
"""Checks a Central Portal bundle (central-publishing-maven-plugin's central-bundle.zip) the way
the portal validates it, before anything is uploaded.

    bindings/java/check-bundle.py <central-bundle.zip> [--unsigned]

Every file must sit in a <group path>/<artifactId>/<version>/ component directory and be named
<artifactId>-<version>[-<classifier>].<ext>. Every component needs its .pom; one whose packaging
is not pom also needs its main jar and the -sources and -javadoc jars. Every file needs .md5 and
.sha1 checksums and (unless --unsigned) a .asc signature (signatures need no checksums). Nothing else may be in the bundle: the
portal rejects a bundle with a file outside a component, such as the maven-metadata-local.xml
that central-publishing-maven-plugin 0.8.0 picked up from Maven 3.10's staging directory
("Bundle has content that does NOT have a .pom file").

Every POM must carry the metadata Central requires (name, description, url, a license, a
developer, scm url). Beyond the portal's rules, the bundle must hold exactly the release's four
components at one version (EXPECTED): com.pohcee:dcmnorm (the API jar), com.pohcee:dcmnorm-natives
(packaging pom, one jar per platform classifier) and the relocation POMs for their pre-0.14
names, com.pohcee:dcmnorm-java and com.pohcee:dcmnorm-java-natives, each a packaging-pom
<relocation> to the new artifact at the same version. Prints the components. Standard library
only.
"""

import re
import sys
import xml.etree.ElementTree as ET
import zipfile
from collections import defaultdict

CHECKSUMS = (".md5", ".sha1", ".sha256", ".sha512")
REQUIRED_CHECKSUMS = (".md5", ".sha1")
GROUP_DIR = "com/pohcee"
CLASSIFIERS = ("linux-x86_64", "linux-aarch_64", "osx-aarch_64", "osx-x86_64", "windows-x86_64")
# artifactId -> (packaging, the jars beside the .pom, the artifact a relocation POM points to)
EXPECTED = {
    "dcmnorm": ("jar", ("", "-sources", "-javadoc"), None),
    "dcmnorm-natives": ("pom", tuple(f"-{c}" for c in CLASSIFIERS), None),
    "dcmnorm-java": ("pom", (), "dcmnorm"),
    "dcmnorm-java-natives": ("pom", (), "dcmnorm-natives"),
}
POM_NS = {"m": "http://maven.apache.org/POM/4.0.0"}
# What the portal requires of every POM, as paths under <project>.
REQUIRED_METADATA = ("m:name", "m:description", "m:url", "m:licenses/m:license/m:name",
                     "m:developers/m:developer/m:name", "m:scm/m:url")


def check_pom(path: str, text: str, artifact: str, version: str, problems: list) -> None:
    root = ET.fromstring(text)
    for field in REQUIRED_METADATA:
        node = root.find(field, POM_NS)
        if node is None or not (node.text or "").strip():
            problems.append(f"{path}: no <{field.replace('m:', '')}> (Central requires it)")
    if artifact not in EXPECTED:
        return
    target = EXPECTED[artifact][2]
    relocation = root.find("m:distributionManagement/m:relocation", POM_NS)
    if target is None:
        if relocation is not None:
            problems.append(f"{path}: must not be a relocation POM")
        return
    if relocation is None:
        problems.append(f"{path}: no <distributionManagement><relocation> to {target}")
        return
    found = {f: (relocation.findtext(f"m:{f}", "", POM_NS) or "").strip()
             for f in ("groupId", "artifactId", "version")}
    wanted = {"groupId": "com.pohcee", "artifactId": target, "version": version}
    if found != wanted:
        problems.append(f"{path}: relocates to {found}, expected {wanted}")


def main() -> None:
    args = [a for a in sys.argv[1:] if a != "--unsigned"]
    signed = "--unsigned" not in sys.argv[1:]
    if len(args) != 1:
        sys.exit(__doc__)
    with zipfile.ZipFile(args[0]) as bundle:
        names = {n for n in bundle.namelist() if not n.endswith("/")}
        poms = {n: bundle.read(n).decode("utf-8") for n in names if n.endswith(".pom")}
    problems = []
    components = defaultdict(set)
    for name in sorted(names):
        parts = name.split("/")
        if len(parts) < 4:
            problems.append(f"{name}: not inside a <group>/<artifactId>/<version>/ directory")
            continue
        artifact, version, filename = parts[-3], parts[-2], parts[-1]
        if not filename.startswith(f"{artifact}-{version}"):
            problems.append(f"{name}: not a file of a <artifactId>/<version>/ component"
                            " (the portal reports this as content without a .pom)")
            continue
        components["/".join(parts[:-1])].add(filename)
    for directory, files in sorted(components.items()):
        artifact, version = directory.split("/")[-2:]
        stem = f"{artifact}-{version}"
        pom = f"{stem}.pom"
        if pom not in files:
            problems.append(f"{directory}: no {pom}")
            continue
        packaging = re.search(r"<packaging>\s*([^<\s]+)\s*</packaging>", poms[f"{directory}/{pom}"])
        packaging = packaging.group(1) if packaging else "jar"
        check_pom(f"{directory}/{pom}", poms[f"{directory}/{pom}"], artifact, version, problems)
        if artifact in EXPECTED:
            expected_packaging, jars, _ = EXPECTED[artifact]
            if packaging != expected_packaging:
                problems.append(f"{directory}: packaging {packaging}, expected {expected_packaging}")
            expected_primaries = {pom} | {f"{stem}{jar}.jar" for jar in jars}
            found_primaries = {f for f in files if not f.endswith(CHECKSUMS + (".asc",))}
            for missing in sorted(expected_primaries - found_primaries):
                problems.append(f"{directory}: no {missing}")
            for extra in sorted(found_primaries - expected_primaries):
                problems.append(f"{directory}: unexpected {extra}")
        if packaging != "pom":
            for needed in (f"{stem}.jar", f"{stem}-sources.jar", f"{stem}-javadoc.jar"):
                if needed not in files:
                    problems.append(f"{directory}: packaging {packaging} needs {needed}")
        primaries = sorted(f for f in files if not f.endswith(CHECKSUMS + (".asc",)))
        for primary in primaries:
            needed = list(REQUIRED_CHECKSUMS) + ([".asc"] if signed else [])
            for suffix in needed:
                if primary + suffix not in files:
                    problems.append(f"{directory}/{primary}: no {suffix}")
        print(f"{directory} (packaging {packaging}): {', '.join(primaries)}")
    expected_dirs = {f"{GROUP_DIR}/{artifact}" for artifact in EXPECTED}
    found_dirs = {d.rsplit("/", 1)[0] for d in components}
    for missing in sorted(expected_dirs - found_dirs):
        problems.append(f"{missing}: missing from the bundle")
    for extra in sorted(found_dirs - expected_dirs):
        problems.append(f"{extra}: not one of the release's artifacts ({', '.join(sorted(EXPECTED))})")
    versions = {d.rsplit("/", 1)[1] for d in components}
    if len(versions) > 1 or len(components) != len(found_dirs):
        problems.append(f"the components have different versions: {', '.join(sorted(versions))}")
    if problems:
        print("\n".join(problems), file=sys.stderr)
        sys.exit(f"{args[0]}: {len(problems)} problem(s); Central would reject this bundle,"
                 " or it is not the release's")
    print(f"ok: {len(components)} component(s), {len(names)} files")


if __name__ == "__main__":
    main()
