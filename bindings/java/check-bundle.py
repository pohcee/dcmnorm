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
("Bundle has content that does NOT have a .pom file"). Prints the components. Standard library
only.
"""

import re
import sys
import zipfile
from collections import defaultdict

CHECKSUMS = (".md5", ".sha1", ".sha256", ".sha512")
REQUIRED_CHECKSUMS = (".md5", ".sha1")


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
    if problems:
        print("\n".join(problems), file=sys.stderr)
        sys.exit(f"{args[0]}: {len(problems)} problem(s); Central would reject this bundle")
    print(f"ok: {len(components)} component(s), {len(names)} files")


if __name__ == "__main__":
    main()
