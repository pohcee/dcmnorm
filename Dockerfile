FROM rust:1-bookworm AS builder

ENV DEBIAN_FRONTEND=noninteractive
ENV CARGO_HOME=/usr/local/cargo
ENV PATH=/usr/local/cargo/bin:${PATH}

RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        build-essential \
        ca-certificates \
        clang \
        cmake \
        libc6-dev \
        libavcodec-dev \
        libavformat-dev \
        libavutil-dev \
        libclang-dev \
        libswresample-dev \
        libswscale-dev \
        pkg-config \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /usr/src/dcmnorm

COPY . .

RUN cargo build -p dcmnorm-cli --release \
    && strip target/release/dcmnorm

FROM debian:bookworm-slim

ENV DEBIAN_FRONTEND=noninteractive

RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        ca-certificates \
        ffmpeg \
        libstdc++6 \
    && rm -rf /var/lib/apt/lists/*

COPY --from=builder /usr/src/dcmnorm/target/release/dcmnorm /usr/local/bin/dcmnorm

ENTRYPOINT ["dcmnorm"]