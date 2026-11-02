#!/usr/bin/env bash
# StreamCore's Rust checks: formatting, Clippy with warnings denied, the workspace's
# tests, and the scheduling probe's loopback self-test and CLI check. The Linux lane
# (ci/local/linux.sh) runs this in a container; on a Mac, local CI runs the same
# steps as separate jobs (ci/local/jobs.json).
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

# Homebrew rustup may provide the toolchain without Cargo/Rust proxies on PATH.
# Reuse the installed toolchain only when its compiler matches the exact pin.
if ! command -v cargo >/dev/null || ! command -v rustc >/dev/null; then
    RUST_VERSION="$(sed -n 's/^channel = "\([^"]*\)"/\1/p' rust-toolchain.toml)"
    CARGO="$(rustup which --toolchain "$RUST_VERSION" cargo 2>/dev/null || true)"
    [[ -n "$CARGO" ]] || { echo "error: install Rust $RUST_VERSION with rustup" >&2; exit 1; }
    export PATH="$(dirname "$CARGO"):$PATH"
fi

cargo fmt --all --check
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo test --workspace --all-features --locked
cargo run --package streamcore-transport --example scheduling_probe --locked -- self-test
python3 ci/scheduling/test-probe.py
