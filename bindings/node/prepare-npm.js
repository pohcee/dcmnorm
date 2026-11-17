#!/usr/bin/env node
// Lays out the npm packages for a release and checks what each one would publish.
//
//   node prepare-npm.js --artifacts <dir>   the release: <dir> holds every platform's .node
//   node prepare-npm.js --placeholder       the pull-request check: empty stand-in .node files
//   node prepare-npm.js --fix-loader        only rewrite index.js's package names (after a build)
//
// Layout (the esbuild / napi-rs per-platform scheme): the main package `dcmnorm` holds only the
// JS loader (index.js) and the types (index.d.ts), and lists one optionalDependency per platform
// package `@dcmnorm/<platform>`, each of which holds only that platform's .node and declares
// os/cpu/libc, so npm installs just the one that fits the machine. The platform package dirs are
// generated under npm/ (not committed) and the repository LICENSE is copied into every package.
//
// napi generates index.js with platform packages named `<package name>-<platform>`
// (`dcmnorm-linux-x64-gnu`) and has no setting for a scope of their own, so this rewrites those
// requires to `@dcmnorm/<platform>`. `npm run build` does that after every build, and the
// pull-request check (`git diff --exit-code index.js` after this script) catches a committed
// loader that missed it.
//
// Then `npm pack --dry-run --json` must list exactly the expected files for every package, the
// loader must require exactly these platform packages, and the main package's
// optionalDependencies must name exactly them at the package's own version: a missing .node, a
// stray file or a drifted version fails here rather than on npm. Prints the directories to
// publish, platform packages first, then ".".
"use strict";

const assert = require("assert");
const fs = require("fs");
const path = require("path");
const { execFileSync } = require("child_process");

const ROOT = __dirname;
const NPM_DIR = path.join(ROOT, "npm");
const SCOPE = "@dcmnorm";
const pkg = require("./package.json");

// napi target triple -> platform name (as in the .node file name) and the package's os/cpu/libc.
const PLATFORMS = {
  "x86_64-unknown-linux-gnu": { name: "linux-x64-gnu", os: "linux", cpu: "x64", libc: "glibc" },
  "aarch64-unknown-linux-gnu": { name: "linux-arm64-gnu", os: "linux", cpu: "arm64", libc: "glibc" },
  "x86_64-pc-windows-msvc": { name: "win32-x64-msvc", os: "win32", cpu: "x64" },
  "aarch64-apple-darwin": { name: "darwin-arm64", os: "darwin", cpu: "arm64" },
  "x86_64-apple-darwin": { name: "darwin-x64", os: "darwin", cpu: "x64" },
};

function run(cmd, args, cwd = ROOT) {
  return execFileSync(cmd, args, { cwd, encoding: "utf8", stdio: ["ignore", "pipe", "inherit"], shell: process.platform === "win32" });
}

function packFiles(dir) {
  const [result] = JSON.parse(run("npm", ["pack", "--dry-run", "--json", "--ignore-scripts"], dir));
  return { files: result.files.map((f) => f.path).sort(), size: result.size };
}

function fixLoader() {
  const file = path.join(ROOT, "index.js");
  const before = fs.readFileSync(file, "utf8");
  const prefix = `${pkg.name}-`.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
  const after = before.replace(new RegExp(`require\\((['"])${prefix}([a-z0-9-]+)`, "g"), `require($1${SCOPE}/$2`);
  if (after !== before) fs.writeFileSync(file, after);
  return after;
}

function main() {
  const args = process.argv.slice(2);
  const loader = fixLoader();
  if (args.includes("--fix-loader")) return;
  const placeholder = args.includes("--placeholder");
  const i = args.indexOf("--artifacts");
  const artifacts = i >= 0 ? path.resolve(args[i + 1]) : null;
  if (placeholder === Boolean(artifacts)) {
    console.error("usage: node prepare-npm.js --artifacts <dir> | --placeholder | --fix-loader");
    process.exit(2);
  }

  const platforms = pkg.napi.targets.map((triple) => {
    assert.ok(PLATFORMS[triple], `prepare-npm.js has no npm platform package for the napi target ${triple}`);
    return PLATFORMS[triple];
  });
  const binary = (p) => `${pkg.napi.binaryName}.${p.name}.node`;
  const license = path.join(ROOT, "..", "..", "LICENSE");

  fs.rmSync(NPM_DIR, { recursive: true, force: true });
  for (const p of platforms) {
    const dir = path.join(NPM_DIR, p.name);
    fs.mkdirSync(dir, { recursive: true });
    const sub = {
      name: `${SCOPE}/${p.name}`,
      version: pkg.version,
      description: `The ${p.name} native addon of the dcmnorm npm package (installed by it; not used directly)`,
      license: pkg.license,
      author: pkg.author,
      homepage: pkg.homepage,
      repository: pkg.repository,
      bugs: pkg.bugs,
      main: binary(p),
      files: [binary(p)],
      os: [p.os],
      cpu: [p.cpu],
      ...(p.libc ? { libc: [p.libc] } : {}),
      engines: pkg.engines,
      publishConfig: { access: "public" },
    };
    fs.writeFileSync(path.join(dir, "package.json"), JSON.stringify(sub, null, 2) + "\n");
    fs.writeFileSync(
      path.join(dir, "README.md"),
      `# \`${sub.name}\`\n\nThe ${p.name} native addon of [\`${pkg.name}\`](https://www.npmjs.com/package/${pkg.name}). ` +
        `Install \`${pkg.name}\`, which depends on this package on matching platforms.\n`,
    );
    fs.copyFileSync(license, path.join(dir, "LICENSE"));
    const target = path.join(dir, binary(p));
    if (placeholder) fs.writeFileSync(target, "");
    else fs.copyFileSync(path.join(artifacts, binary(p)), target);
  }
  fs.copyFileSync(license, path.join(ROOT, "LICENSE"));

  const expectedDeps = Object.fromEntries(platforms.map((p) => [`${SCOPE}/${p.name}`, pkg.version]));
  assert.deepStrictEqual(
    pkg.optionalDependencies,
    expectedDeps,
    "package.json optionalDependencies must list every napi target's package at the package version (scripts/check-versions.py --set fixes the versions)",
  );
  const required = new Set([...loader.matchAll(/require\('(@[^/']+\/[a-z0-9-]+)'\)/g)].map((m) => m[1]));
  for (const name of Object.keys(expectedDeps)) assert.ok(required.has(name), `index.js does not require ${name}`);

  const main = packFiles(ROOT);
  assert.deepStrictEqual(main.files, ["LICENSE", "README.md", "index.d.ts", "index.js", "package.json"], `${pkg.name} contents`);
  console.error(`${pkg.name}@${pkg.version}: ${main.files.join(", ")} (${main.size} bytes packed)`);
  for (const p of platforms) {
    const dir = path.join(NPM_DIR, p.name);
    const sub = JSON.parse(fs.readFileSync(path.join(dir, "package.json"), "utf8"));
    const packed = packFiles(dir);
    assert.deepStrictEqual(packed.files, ["LICENSE", "README.md", binary(p), "package.json"], `${sub.name} contents`);
    if (!placeholder) assert.ok(fs.statSync(path.join(dir, binary(p))).size > 0, `${sub.name}: empty .node`);
    console.error(`${sub.name}@${sub.version} [${sub.os}/${sub.cpu}${sub.libc ? "/" + sub.libc : ""}]: ${packed.files.join(", ")} (${packed.size} bytes packed)`);
  }
  for (const p of platforms) console.log(path.join("npm", p.name));
  console.log(".");
}

main();
