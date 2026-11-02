#!/usr/bin/env bash
# Builds the input model's C ABI DLL (crates/streamcore-input-ffi) for PadWin: x64 and ARM64
# streamcore_input_ffi.dll (+ import library and PDB) under target/streamcore-input-windows/<arch>/,
# with the hand-written header crates/streamcore-input-ffi/include/streamcore_input.h. The DLLs link the C
# runtime statically, so they load without the Visual C++ Redistributable; the script checks it.
#
# Native build first: this commit's tracked files go over SSH to the Windows PC named by
# STREAMCORE_WINDOWS_HOST (an SSH host or alias; there is no default, and the script fails
# without it unless STREAMCORE_INPUT_WINDOWS_CROSS=1). The PC needs OpenSSH with key login,
# Git for Windows (Git Bash), rustup and the Visual Studio C++ tools; it runs the crates' tests and builds both DLLs in
# its own C:\streamcore-input-build (never a CI checkout); the ARM64 DLL needs the Visual
# Studio "MSVC ARM64 build tools" component there and is skipped without it. When the PC is unreachable, or
# with STREAMCORE_INPUT_WINDOWS_CROSS=1, it cross-compiles on this machine with cargo-xwin
# instead (installed if missing; it downloads the MSVC CRT and Windows SDK on first use,
# under Microsoft's license terms) and the tests do not run.
set -euo pipefail
CORE_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$CORE_ROOT"
OUT="$CORE_ROOT/target/streamcore-input-windows"
if [[ "${STREAMCORE_INPUT_WINDOWS_CROSS:-0}" != 1 && -z "${STREAMCORE_WINDOWS_HOST:-}" ]]; then
    echo "error: set STREAMCORE_WINDOWS_HOST to the Windows PC's SSH host, or STREAMCORE_INPUT_WINDOWS_CROSS=1 to cross-compile" >&2
    exit 1
fi
HOST="${STREAMCORE_WINDOWS_HOST:-}"
GIT_BASH='"C:\Program Files\Git\bin\bash.exe"'
SSH=(ssh -o BatchMode=yes -o ConnectTimeout=15 -o ServerAliveInterval=30 "$HOST")
rm -rf "$OUT" && mkdir -p "$OUT"
cp crates/streamcore-input-ffi/include/streamcore_input.h "$OUT/"

if [[ "${STREAMCORE_INPUT_WINDOWS_CROSS:-0}" != 1 ]] && "${SSH[@]}" exit 0 2>/dev/null; then
    git archive --format=tar HEAD \
        | "${SSH[@]}" "$GIT_BASH -c \"mkdir -p /c/streamcore-input-build && cat > /c/streamcore-input-build/incoming.tar\""
    "${SSH[@]}" "$GIT_BASH -s" < ci/local/streamcore-input-windows-remote.sh
    "${SSH[@]}" "$GIT_BASH -c \"cat /c/streamcore-input-build/out.tar\"" | tar -xf - -C "$OUT"
else
    echo "Windows PC $HOST not used; cross-compiling with cargo-xwin (tests do not run)" >&2
    CORE_VERSION="$(sed -n 's/^channel = "\([^"]*\)"/\1/p' rust-toolchain.toml)"
    export PATH="$(dirname "$(rustup which --toolchain "$CORE_VERSION" cargo)"):$HOME/.cargo/bin:$PATH"
    command -v cargo-xwin >/dev/null || cargo install --locked cargo-xwin
    for PAIR in x64:x86_64-pc-windows-msvc arm64:aarch64-pc-windows-msvc; do
        ARCH="${PAIR%%:*}" TARGET="${PAIR#*:}"
        rustup target list --installed --toolchain "$CORE_VERSION" | grep -qx "$TARGET" \
            || rustup target add --toolchain "$CORE_VERSION" "$TARGET"
        RUSTFLAGS="-C target-feature=+crt-static" \
            cargo xwin build --package streamcore-input-ffi --release --locked --target "$TARGET"
        mkdir -p "$OUT/$ARCH"
        cp "target/$TARGET/release/streamcore_input_ffi.dll" "target/$TARGET/release/streamcore_input_ffi.dll.lib" "$OUT/$ARCH/"
    done
fi
# PadWin ships the DLL alone, so it must not need the Visual C++ Redistributable.
for DLL in "$OUT"/*/streamcore_input_ffi.dll; do
    if LC_ALL=C grep -qai -e 'vcruntime140' -e 'api-ms-win-crt-' "$DLL"; then
        echo "error: $DLL imports the dynamic C runtime (VCRUNTIME140 / api-ms-win-crt); build with +crt-static" >&2
        exit 1
    fi
done
echo "Windows input model ready:"
find "$OUT" -type f | sed "s|$CORE_ROOT/||" | sort
