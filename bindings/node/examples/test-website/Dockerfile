# Build context must be the dcmnorm repo root (this file needs both the node binding directory
# and the top-level test/files fixtures, which live outside bindings/node), e.g. from the repo
# root: `docker build -f bindings/node/examples/test-website/Dockerfile -t dcmnorm-test-website .`
#
# Build the addon first: bindings/node/build-in-docker.sh writes
# bindings/node/dcmnorm-node.linux-<arch>-gnu.node (glibc 2.31, so it runs on this bookworm-based
# image); no .node file is committed. This image needs no Rust toolchain since the addon is
# copied in prebuilt rather than compiled here.
FROM node:22-slim

WORKDIR /opt/dcmnorm-node

# Only the files the dcmnorm package needs at runtime (see its README's "Packaging" section) -
# not node_modules (napi-rs/cli - build-only), not target/ or src/ (Rust sources).
COPY bindings/node/package.json bindings/node/index.js bindings/node/index.d.ts ./
COPY bindings/node/*.node ./

WORKDIR /opt/dcmnorm-node/examples/test-website
COPY bindings/node/examples/test-website/package.json ./
RUN npm install --omit=dev

COPY bindings/node/examples/test-website/server.js ./
COPY bindings/node/examples/test-website/public ./public

# The hand-picked subset of the repo's own test fixtures listed in server.js's CURATED_SAMPLES -
# keep these two lists in sync. server.js prefers this directory over test/files (which isn't in
# this image) whenever it's present.
COPY test/files/ct.dcm test/files/mr.dcm test/files/wsi.dcm test/files/us.dcm test/files/dx2.dcm test/files/sr.dcm ./samples/

# Just the one file server.js needs to report the dcmnorm core library version being tested -
# not a full workspace checkout (see server.js's own comment on dcmnormCoreVersion).
COPY Cargo.toml ./dcmnorm-core-cargo.toml

ENV PORT=3000
EXPOSE 3000
CMD ["node", "server.js"]
