# Benchmark image: dcmtk, dcm4che, and dcmnorm built/installed side by side in one
# environment so timing comparisons aren't skewed by different host machines, CPU
# governors, or filesystem types.
FROM debian:bookworm-slim

ENV DEBIAN_FRONTEND=noninteractive

# dcmtk: apt package. openjdk: dcm4che runtime. build deps: match dcmnorm's own
# bindings/node/build-in-docker.sh apt list (charls-sys, openjpeg-sys, lcms2-sys all compile
# native code), plus git + nasm because ffmpeg-next's "build" feature shallow-clones and
# compiles FFmpeg from source to link it statically.
RUN apt-get update -qq && apt-get install -y -qq \
    dcmtk \
    openjdk-17-jre-headless \
    curl unzip build-essential clang cmake pkg-config libclang-dev git nasm \
    hyperfine jq \
    > /dev/null && rm -rf /var/lib/apt/lists/*

# dcm4che 5.35.1 official binary distribution.
RUN curl -sL -o /tmp/dcm4che.zip \
      "https://sourceforge.net/projects/dcm4che/files/dcm4che3/5.35.1/dcm4che-5.35.1-bin.zip/download" \
    && unzip -q /tmp/dcm4che.zip -d /opt \
    && mv /opt/dcm4che-5.35.1 /opt/dcm4che \
    && rm /tmp/dcm4che.zip
ENV PATH="/opt/dcm4che/bin:${PATH}"

# Rust toolchain to build dcmnorm from source, matching the container's own glibc/libc
# rather than copying a host-built binary in.
RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y -q
ENV PATH="/root/.cargo/bin:${PATH}"

WORKDIR /repo
COPY . /repo

RUN cargo build --release -p dcmnorm-cli 2>&1 | tail -20
ENV PATH="/repo/target/release:${PATH}"

WORKDIR /repo/benchmarks
