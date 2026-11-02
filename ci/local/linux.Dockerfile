# Linux image for StreamCore's checks. The Rust version comes from
# rust-toolchain.toml (linux.sh passes it), so the image tag changes with the pin.
ARG RUST_VERSION
FROM rust:${RUST_VERSION}-slim-bookworm
RUN apt-get update \
    && apt-get install -y --no-install-recommends python3 ca-certificates \
    && rm -rf /var/lib/apt/lists/*
RUN rustup component add clippy rustfmt
