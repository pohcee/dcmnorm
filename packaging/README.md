# Packaging

How the release artifacts are built and published: the `dcmnorm` and `dcmtalk` CLIs (archives,
`.deb`, PyPI) and the three bindings (npm, PyPI, Maven Central). The
[Build and release workflow](../.github/workflows/release.yml) runs all of it.

| Event | What runs |
|---|---|
| pull request, push to `main` | tests, version pins, binding package metadata (`packages` job), every CLI build + smoke test. Nothing is published. |
| `v*.*.*` tag | all of the above plus the binding builds (FFmpeg from source, slow), then the GitHub release, PyPI, npm, Maven Central and the `claude-plugin` branch |
| manual run (`workflow_dispatch`) | everything a tag runs; publishing only when the ref is a tag |

Every publishing job needs `test`, `packages`, every `cli` build and every `build-node`,
`build-python` and `build-java` build to pass. Each registry is switched on by a repository
variable once its one-time setup below is done; until then a tag publishes everything else.

## CLI builds

| Platform | Runner | Build | Artifacts |
|---|---|---|---|
| `linux-x86_64` | `ubuntu-24.04` | `build-linux.sh` (manylinux_2_28 container) | `.tar.gz`, `.deb`, CLI wheel `manylinux_2_28_x86_64` |
| `linux-aarch64` | `ubuntu-24.04-arm` | `build-linux.sh` (manylinux_2_28 container) | `.tar.gz`, `.deb`, CLI wheel `manylinux_2_28_aarch64` |
| `macos-aarch64` | `macos-15` | `cargo build`, deployment target 11.0 | `.tar.gz`, CLI wheel `macosx_11_0_arm64` |
| `macos-x86_64` | `macos-15-intel` | `cargo build`, deployment target 10.15 | `.tar.gz`, CLI wheel `macosx_10_15_x86_64` |
| `windows-x86_64` | `windows-2025` | `cargo build` (MSVC) | `.zip`, CLI wheel `win_amd64` |

The CLI build needs only a C/C++ toolchain and CMake: FFmpeg is not built or linked, and the
video transfer syntaxes use the `ffmpeg` command at run time (see the top-level README, "Video
transfer syntaxes"). Release builds are stripped (`CARGO_PROFILE_RELEASE_STRIP=symbols`).

## Binding builds

Each binding is built for the same five platforms, with FFmpeg linked in statically and the same
codec set everywhere (the crates' default features; on Windows `ffmpeg-codec-linked` with vcpkg's
static FFmpeg in place of the FFmpeg source build). A codec that fails to build fails the release.

| Platform | Node (`build-node`) | Python (`build-python`) | Java (`build-java`) |
|---|---|---|---|
| Linux x86_64 / aarch64 (`ubuntu-24.04` / `-arm`) | `bindings/node/build-in-docker.sh`, node:22-bullseye-slim, glibc 2.31 | `bindings/python/build-in-docker.sh`, manylinux_2_28 | `bindings/java/build-in-docker.sh`, manylinux_2_28 |
| macOS arm64 / x86_64 (`macos-15` / `-intel`) | `napi build` | `maturin build` | `cargo build -p dcmnorm-java` |
| Windows x64 (`windows-latest`) | `napi build`, vcpkg FFmpeg | `maturin build`, vcpkg FFmpeg | `cargo build`, vcpkg FFmpeg |

Each build checks that no FFmpeg shared library is linked (`objdump`/manylinux policy, `otool -L`,
`check-windows-deps.ps1`) and loads the result: Node `load-check.js` (and `test/smoke.js` on
Linux), Python `smoke_test.py`, Java `check-natives.sh` (`LoadCheck`, plus `Smoke` on Linux). Each
does a parse and an MPEG-2 round trip, which fails without FFmpeg. Nothing native is committed to
the repository; the builds' outputs are `.gitignore`d.

## Files

- `build-linux.sh [x86_64|aarch64]` builds both CLIs in `quay.io/pypa/manylinux_2_28_<arch>`
  into `target/manylinux_2_28-<arch>/release/`, then fails if either binary needs a glibc newer
  than 2.28 or links a shared library outside the manylinux policy. Needs Docker that runs the
  image natively. `manylinux.cmake` is the CMake toolchain file it uses so CMake installs
  CharLS into `lib/`, where charls-sys looks, instead of RHEL's `lib64/`.
- `build-binding-linux.sh <python|java> [x86_64|aarch64]` builds the Python wheel or the Java
  library in the same image, with FFmpeg from source (shared target dir
  `target/manylinux_2_28-<arch>-bindings`), and applies the same glibc/policy checks. The
  bindings' `build-in-docker.sh` scripts call it.
- `package.py --platform <platform> --bin-dir <dir> --out-dir dist` writes the release archives
  (versioned and "latest" names, each with a `.sha256`) and the CLI-only wheel. The version comes
  from `exec/dcmnorm/Cargo.toml` and must match the tag when run for one.
- `stage_cli.py <CLI wheel>` copies the CLI wheel's binaries into `bindings/python/data/scripts/`
  (and the LICENSE next to `pyproject.toml`) for the abi3 wheel.
- `smoke_test.py <wheel> [--same-cli-as <CLI wheel>]` installs a wheel into a fresh virtual
  environment and runs both CLIs from it (version, DICOM to JSON, DICOM to PNG); for the abi3
  wheel also `import dcmnorm` with an in-process MPEG-2 round trip, and, with `--same-cli-as`,
  checks its CLI binaries are byte-identical to the CLI wheel's.
- `macos-link.py` is the linker for the macOS binding builds: it drops the GNU-only
  `-Wl,--no-as-needed` and the `-framework` arguments for frameworks the SDK no longer has that
  ffmpeg-sys-next 8.1 emits, then runs `cc`.
- `portable-cc/` holds compiler wrappers (`gcc`, `cc`, `clang`, ...) that drop `-march=native`.
  The Linux and macOS binding builds put it first on `PATH`: when not cross-compiling,
  ffmpeg-sys-next configures FFmpeg with `-march=native -mtune=native`, which tunes FFmpeg's C
  code for the CI runner's CPU and can crash (SIGILL) on older CPUs. FFmpeg's assembly picks its
  SIMD level at run time either way.
- `check-windows-deps.ps1 <file>` fails if a Windows binary imports an FFmpeg DLL.
- `pypi-README.md` is the PyPI project description (both wheels).
- `make-brand-images.py` draws the dcmnorm icon and everything made from it: the plugin's listing icon
  (`plugin/.claude-plugin/icon.png`), the same icon for the README and PyPI (`docs/images/dcmnorm-icon.png`),
  the Node test website's favicon and
  `docs/images/social-preview.png`, which is uploaded by hand as the repository's GitHub social preview
  (Settings > General > Social preview). Edit the drawing there, not the PNGs.
- `../.github/actions/ffmpeg-vcpkg` installs the pinned, static, LGPL-only vcpkg FFmpeg for the
  Windows binding builds (cached).

## What each registry gets

All packages carry the release version (`scripts/release-tag.sh` sets it through
`scripts/check-versions.py --set`; CI fails if one drifts). Sizes are from the v0.11.2 Linux
x86_64 build.

### PyPI: `dcmnorm`

Two wheels per platform, same name and version:

| Wheel | Contents | Size (linux-x86_64) |
|---|---|---|
| `dcmnorm-<v>-cp39-abi3-<plat>.whl` (maturin, `bindings/python`) | `dcmnorm/` extension module + `dcmnorm-<v>.data/scripts/{dcmnorm,dcmtalk}` | 21.7 MB (module 31.7 MB unpacked) |
| `dcmnorm-<v>-py3-none-<plat>.whl` (`package.py`) | `dcmnorm-<v>.data/scripts/{dcmnorm,dcmtalk}` | 7.7 MB |

`<plat>`: `manylinux_2_28_x86_64`, `manylinux_2_28_aarch64`, `macosx_11_0_arm64`,
`macosx_10_15_x86_64`, `win_amd64`. pip, uv and pipx install the abi3 wheel on CPython 3.9+
(except free-threaded builds) and the CLI-only wheel everywhere else (CPython 3.8, 3.13t/3.14t,
other implementations), so `uv tool install dcmnorm`, `pipx install dcmnorm` and
`pip install dcmnorm` put both CLIs on `PATH` on every Python, and `import dcmnorm` works on
CPython 3.9+. The binaries in both wheels are the same files (the CLI build's). Both declare
`Requires-Python >=3.8`. No sdist. Prerelease tags map to PyPI versions as `v0.8.0-rc.1` to
`0.8.0rc1` (only `alpha`, `beta` and `rc` identifiers are allowed). The old separate
`dcmnorm-python` distribution is retired (it was never published).

### npm: `dcmnorm` + `@dcmnorm/<platform>`

| Package | Contents | Size (packed) |
|---|---|---|
| `dcmnorm` | `index.js` (loader), `index.d.ts`, README, LICENSE; `optionalDependencies` on the five below | 27 kB |
| `@dcmnorm/linux-x64-gnu` (`os` linux, `cpu` x64, `libc` glibc) | `dcmnorm-node.linux-x64-gnu.node`, README, LICENSE | 12 MB (31 MB unpacked) |
| `@dcmnorm/linux-arm64-gnu` (linux, arm64, glibc) | `dcmnorm-node.linux-arm64-gnu.node`, ... | |
| `@dcmnorm/darwin-arm64` (darwin, arm64) | `dcmnorm-node.darwin-arm64.node`, ... | |
| `@dcmnorm/darwin-x64` (darwin, x64) | `dcmnorm-node.darwin-x64.node`, ... | |
| `@dcmnorm/win32-x64-msvc` (win32, x64) | `dcmnorm-node.win32-x64-msvc.node`, ... | |

`npm i dcmnorm` downloads only the platform package that matches. The platform packages are
published first, then `dcmnorm`; a version already on npm is skipped (so a re-run finishes a
partial publish); prerelease versions get the `next` dist-tag. `bindings/node/prepare-npm.js`
generates and checks the layout (see `bindings/node/README.md`, "Packaging").

### Maven Central: `com.pohcee:dcmnorm-java` + `com.pohcee:dcmnorm-java-natives`

| Artifact | Contents | Size |
|---|---|---|
| `dcmnorm-java-<v>.jar` | the API classes (no native code), plus `-sources.jar` and `-javadoc.jar` | 43 kB |
| `dcmnorm-java-natives-<v>.pom` (packaging `pom`) | metadata only | |
| `dcmnorm-java-natives-<v>-<classifier>.jar` | `native/<classifier>/<library>` + `.sha256` | 13 MB for `linux-x86_64` |

Classifiers: `linux-x86_64`, `linux-aarch_64`, `osx-aarch_64`, `osx-x86_64`, `windows-x86_64`. A
consumer depends on the API and the one classifier it runs on (`bindings/java/README.md`,
"Install"). Every file is GPG-signed. `dcmnorm-java-natives` is published first, then
`dcmnorm-java`, each as its own Central Portal deployment that the job waits on until it is
published.

### GitHub release

The CLI archives and `.deb` packages (each with a `.sha256`), the Gemini CLI extension archives,
and the binding artifacts: every `.node` addon, every abi3 wheel and every natives jar.

## Local builds

The Linux x86_64 versions of everything, as the workflow builds them (Docker needed):

```bash
# CLIs and the CLI wheel
packaging/build-linux.sh x86_64
python3 packaging/package.py --platform linux-x86_64 --bin-dir target/manylinux_2_28-x86_64/release --out-dir dist
python3 packaging/smoke_test.py dist/dcmnorm-*-py3-none-manylinux_2_28_x86_64.whl

# The abi3 wheel (module + the same CLIs)
python3 packaging/stage_cli.py dist/dcmnorm-*-py3-none-manylinux_2_28_x86_64.whl
bindings/python/build-in-docker.sh
python3 packaging/smoke_test.py bindings/python/dist/*.whl --same-cli-as dist/dcmnorm-*-py3-none-manylinux_2_28_x86_64.whl

# The Node addon and the npm packages (other platforms as empty placeholders)
bindings/node/build-in-docker.sh
(cd bindings/node && node prepare-npm.js --placeholder && cp dcmnorm-node.linux-x64-gnu.node npm/linux-x64-gnu/ \
  && npm pack --dry-run && cd npm/linux-x64-gnu && npm pack --dry-run)

# The Java library, the API jar and the linux-x86_64 natives jar (JDK 11+ and Maven)
bindings/java/build-in-docker.sh
bindings/java/check-natives.sh linux-x86_64 "$PWD/target/manylinux_2_28-x86_64/release/dcmtalk"
```

## Publishing: one-time setup

Do each registry's setup, then set its repository variable to `true` (Settings > Secrets and
variables > Actions > Variables). The next tag publishes to it. A tag that already ran without it
can be published by re-running the workflow for that tag from the Actions tab (PyPI and npm skip
what is already there; Maven Central rejects a version it already has).

### PyPI (`PYPI_PUBLISH`)

Publishing uses [Trusted Publishing](https://docs.pypi.org/trusted-publishers/): PyPI accepts
uploads from this repository's workflow by OIDC, so no API token is stored anywhere.

1. On PyPI, add a trusted publisher for project `dcmnorm` (or a pending one, before the project
   exists): owner `pohcee`, repository `dcmnorm`, workflow `release.yml`, environment `pypi`.
2. On GitHub, create the `pypi` environment (Settings > Environments). Optionally limit it to
   `v*` tags under "Deployment branches and tags".
3. Set `PYPI_PUBLISH` to `true`.

Nothing changes in this setup with the abi3 wheels: they are more files of the same project.

### npm (`NPM_PUBLISH`)

npm [trusted publishing](https://docs.npmjs.com/trusted-publishers) (OIDC, npm 11.5.1+, with
provenance) is configured per package, and only for a package that already exists, so the names
are claimed once by hand first:

1. Create the free npm organization `dcmnorm` (npmjs.com > Add organization), which owns the
   `@dcmnorm` scope. Use an npm account with two-factor authentication.
2. Claim the six names by publishing a placeholder version of each from your machine, e.g.
   `0.0.0` of a package.json with only `name`, `version`, `description`, `license` and
   `repository` (`npm publish --access public`; mark it `npm deprecate <name>@0.0.0
   "placeholder"` afterwards if you like): `dcmnorm`, `@dcmnorm/linux-x64-gnu`,
   `@dcmnorm/linux-arm64-gnu`, `@dcmnorm/darwin-arm64`, `@dcmnorm/darwin-x64`,
   `@dcmnorm/win32-x64-msvc`.
3. For each of the six, on npmjs.com > the package > Settings > Trusted publishing, add GitHub
   Actions: organization or user `pohcee`, repository `dcmnorm`, workflow `release.yml`,
   environment `npm`. Then, under Publishing access, choose "Require two-factor authentication
   and disallow tokens" so only the workflow can publish.
4. On GitHub, create the `npm` environment (optionally limited to `v*` tags).
5. Set `NPM_PUBLISH` to `true`.

The workflow passes an `NPM_TOKEN` secret to npm when one exists; it is not needed with trusted
publishing and only exists as a fallback for a first publish without step 3. Leave it unset.

### Maven Central (`MAVEN_PUBLISH`)

Publishing goes through the [Sonatype Central Portal](https://central.sonatype.com) with the
`central-publishing-maven-plugin`, signed with GPG.

1. Sign in to the Central Portal and add the namespace `com.pohcee` (Namespaces > Add
   Namespace). Verify it with the DNS TXT record the portal shows: add it to `pohcee.com`, wait
   for it to resolve, click Verify. The namespace must be verified before the first publish.
2. Generate a user token (View Account > Generate User Token) and store its two halves as the
   repository secrets `MAVEN_CENTRAL_USERNAME` and `MAVEN_CENTRAL_PASSWORD`.
3. Create a GPG signing key for releases (`gpg --full-generate-key`, RSA 4096 or ed25519, with a
   passphrase), publish its public key to a keyserver Central checks
   (`gpg --keyserver keyserver.ubuntu.com --send-keys <key id>`), and store the private key and
   its passphrase as `MAVEN_GPG_PRIVATE_KEY` (`gpg --armor --export-secret-keys <key id>`) and
   `MAVEN_GPG_PASSPHRASE`.
4. On GitHub, create the `maven-central` environment (optionally limited to `v*` tags) and put
   the four secrets there or at the repository level.
5. Set `MAVEN_PUBLISH` to `true`.
