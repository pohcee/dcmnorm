# Releasing

## Releasing

One GitHub Actions workflow, [`.github/workflows/release.yml`](../.github/workflows/release.yml)
("Build and release"), builds, tests and publishes everything:

- **On every pull request and push to `main`**, it runs the test suite, checks the binding
  packages' metadata (`npm pack` contents, the Maven POMs, the wheel metadata, every version pin)
  and builds every CLI artifact for every platform (archives, `.deb` packages, CLI wheels, each
  smoke-tested), but publishes nothing. The bindings themselves (FFmpeg compiled from source) are
  built only for a tag or a manual run.
- **On a pushed `v*.*.*` tag**, it also builds the three bindings for the same five platforms and,
  after the tests and every build pass, publishes:
    - a GitHub release with, for each of `dcmnorm` and `dcmtalk` and each platform
      (`linux-x86_64`, `linux-aarch64`, `macos-x86_64`, `macos-aarch64`, `windows-x86_64`):
      `<name>-<tag>-<platform>.tar.gz` (`.zip` on Windows) plus a rolling `<name>-<platform>.tar.gz`
      "latest" alias, each with a `.sha256`. On both Linux platforms there's also
      `<name>-<tag>-<platform>.deb` (+ alias), built with [`cargo-deb`](https://github.com/kornelski/cargo-deb)
      from each exec crate's `[package.metadata.deb]` and installed for real by the workflow
      before upload.
    - [`dcmnorm`](https://pypi.org/project/dcmnorm/) on PyPI: per platform, a `cp39-abi3` wheel
      with the `import dcmnorm` module and both CLIs, and a `py3-none` wheel with the CLIs only
      (for Pythons the first doesn't fit). Only when the repository variable `PYPI_PUBLISH` is `true`.
    - [`dcmnorm`](https://www.npmjs.com/package/dcmnorm) and its five `@dcmnorm/<platform>` addon
      packages on npm (`NPM_PUBLISH`), and `com.pohcee:dcmnorm` with
      `com.pohcee:dcmnorm-natives` (one jar per platform classifier) on Maven Central
      (`MAVEN_PUBLISH`), plus relocation POMs from their pre-0.14 names `dcmnorm-java` and
      `dcmnorm-java-natives`. The GitHub release carries the addons, wheels and natives jars too.
      Setup for each registry: [packaging/README.md](../packaging/README.md).
    - the `claude-plugin` branch, rebuilt from this release's `plugin/` folder
      (`scripts/update-plugin-branch.sh`): the Claude plugin directory tracks that branch, since it
      downloads a whole ref and this repository is over its size limit.

The Linux CLIs are built in a manylinux_2_28 container and run on any glibc 2.28+ distribution;
the same binaries go into the archives, the `.deb` packages and the wheels.

To cut a release, run the local helper script:

```bash
./scripts/release-tag.sh patch          # bump types: patch, minor, major
./scripts/release-tag.sh minor --prerelease rc
./scripts/release-tag.sh patch --dry-run  # preview the computed next tag only
```

The script first fast-forwards onto `origin`. It then updates versions in `Cargo.toml`,
`exec/dcmnorm/Cargo.toml` and `exec/dcmtalk/Cargo.toml`, plus the Claude plugin's version, the
version its skill installs and every binding package version (`bindings/node/package.json` and
its loader, `bindings/python/pyproject.toml`, `bindings/java/pom.xml` and
`bindings/java/natives/pom.xml`; `scripts/check-versions.py --set`, and CI fails if any drifts), and refreshes only the workspace's
own entries in `Cargo.lock` (`cargo update --workspace`, so a release never silently upgrades
dependencies). Finally it creates a release commit and pushes both the commit and the version tag
to `origin`, which starts the workflow. The script is non-interactive, so release automation can
call it as its "release" step. If no `v*` tags exist yet, it uses the root `Cargo.toml`
`package.version` as the baseline. Prereleases must use `alpha`, `beta` or `rc` identifiers, the
ones PyPI versions allow (`v0.8.0-rc.1` is `0.8.0rc1` on PyPI).

The tag must be pushed with your own git credentials: GitHub doesn't start workflows for a tag
pushed with a workflow run's own token. If a tag ever ends up without a run, start **Build and
release** by hand from the Actions tab with the tag as the ref.
