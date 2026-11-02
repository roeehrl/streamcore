#!/usr/bin/env bash
# StreamCore's Linux checks: scripts/ci.sh in a linux/arm64 container with the
# pinned Rust. Cargo's registry and the target directory live in named volumes, so
# repeat runs are incremental. Needs Docker (on a Mac, Colima works; Colima only
# mounts the home directory, so run this from a checkout under it).
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$ROOT"

if ! docker info >/dev/null 2>&1; then
    command -v colima >/dev/null || { echo 'error: Docker is not running and Colima is not installed' >&2; exit 1; }
    colima status >/dev/null 2>&1 || colima start
fi
if [[ -z "${DOCKER_HOST:-}" && -S "$HOME/.colima/default/docker.sock" ]] && ! docker info >/dev/null 2>&1; then
    export DOCKER_HOST="unix://$HOME/.colima/default/docker.sock"
fi

RUST_VERSION="$(sed -n 's/^channel = "\([^"]*\)"/\1/p' rust-toolchain.toml)"
IMAGE="streamcore-ci-linux:$RUST_VERSION-$(shasum -a 256 ci/local/linux.Dockerfile | cut -c1-12)"
if ! docker image inspect "$IMAGE" >/dev/null 2>&1; then
    docker build --build-arg "RUST_VERSION=$RUST_VERSION" -t "$IMAGE" -f ci/local/linux.Dockerfile ci/local
fi

docker run --rm \
    -v "$ROOT:/src" \
    -v streamcore-ci-cargo-registry:/usr/local/cargo/registry \
    -v streamcore-ci-linux-target:/src/target \
    -w /src "$IMAGE" bash scripts/ci.sh
