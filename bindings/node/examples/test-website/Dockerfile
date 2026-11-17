# Build context must be the dcmnorm repo root (this file needs the site and the top-level
# test/files fixtures), e.g. from the repo root:
#   docker build -f bindings/node/examples/test-website/Dockerfile \
#     --build-arg DCMNORM_VERSION=0.12.0 -t dcmnorm-test-website .
#
# The binding comes from npm: `dcmnorm` plus the @dcmnorm/<platform> package npm picks for this
# image (linux-x64-gnu on Cloud Run). No Rust toolchain and no locally built addon are needed.
FROM node:22-slim

ARG DCMNORM_VERSION
WORKDIR /opt/dcmnorm-test-website
COPY bindings/node/examples/test-website/package.json ./
RUN test -n "$DCMNORM_VERSION" || { echo "build with --build-arg DCMNORM_VERSION=<version>" >&2; exit 1; } \
 && npm pkg set "dependencies.dcmnorm=${DCMNORM_VERSION}" \
 && npm install --omit=dev --no-audit --no-fund \
 && node -e "require('dcmnorm')"

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
