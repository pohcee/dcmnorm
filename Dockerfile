FROM rust:1-bookworm AS builder

ENV DEBIAN_FRONTEND=noninteractive
ENV CARGO_HOME=/usr/local/cargo
ENV PATH=/usr/local/cargo/bin:${PATH}

RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        build-essential \
        ca-certificates \
        cmake \
        pkg-config \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /usr/src/dcmnorm

COPY . .

RUN cargo build -p dcmnorm-cli -p dcmtalk --release \
    && strip target/release/dcmnorm target/release/dcmtalk

FROM debian:bookworm-slim

# ffmpeg: the video transfer syntaxes (MPEG-2, MPEG-4 AVC/H.264, HEVC) and .mp4/.mov output.
ENV DEBIAN_FRONTEND=noninteractive

RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        ca-certificates \
        ffmpeg \
        libstdc++6 \
    && rm -rf /var/lib/apt/lists/*

COPY --from=builder /usr/src/dcmnorm/target/release/dcmnorm /usr/local/bin/dcmnorm
COPY --from=builder /usr/src/dcmnorm/target/release/dcmtalk /usr/local/bin/dcmtalk

# dcmnorm remains the default entrypoint for backward compatibility; run dcmtalk (echoscu/
# storescu/findscu/movescu/storescp) with `docker run --rm --entrypoint dcmtalk <image> ...`.
ENTRYPOINT ["dcmnorm"]