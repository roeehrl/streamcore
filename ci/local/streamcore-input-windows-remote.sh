# Runs in Git Bash on the Windows PC; scripts/build-streamcore-input-windows.sh sends it
# after the sources. Uses its own directory, never a CI checkout.
# rustc finds the Visual Studio linker and libraries for each target itself.
set -euo pipefail
ROOT=/c/streamcore-input-build
mkdir -p "$ROOT/src"
cd "$ROOT/src"
find . -mindepth 1 -maxdepth 1 ! -name target -exec rm -rf {} +
# An older commit must not reuse a newer branch's library just because archive
# mtimes predate the retained Cargo target cache.
tar -xmf "$ROOT/incoming.tar"
rm -f "$ROOT/incoming.tar" "$ROOT/out.tar"
export PATH="$(cygpath "$USERPROFILE")/.cargo/bin:$PATH"
echo "==> $(uname -s) $(rustc --version)"
cargo test --package streamcore-input --package streamcore-input-ffi --locked
STAGE="$ROOT/stage"
rm -rf "$STAGE"
# ARM64 needs Visual Studio's "MSVC ARM64 build tools" component (VC\Tools\MSVC\*\lib\arm64).
PAIRS=(x64:x86_64-pc-windows-msvc)
if compgen -G "/c/Program Files*/Microsoft Visual Studio/*/*/VC/Tools/MSVC/*/lib/arm64" >/dev/null; then
    PAIRS+=(arm64:aarch64-pc-windows-msvc)
else
    echo "warning: no MSVC ARM64 libraries on this PC; skipping the ARM64 DLL" >&2
fi
for PAIR in "${PAIRS[@]}"; do
    ARCH="${PAIR%%:*}" TARGET="${PAIR#*:}"
    rustup target list --installed | grep -qx "$TARGET" || rustup target add "$TARGET"
    # Static C runtime: the DLL must load on a Windows PC without the Visual C++ Redistributable.
    RUSTFLAGS="-C target-feature=+crt-static" \
        cargo build --package streamcore-input-ffi --release --locked --target "$TARGET"
    mkdir -p "$STAGE/$ARCH"
    cp "target/$TARGET/release/streamcore_input_ffi.dll" "target/$TARGET/release/streamcore_input_ffi.dll.lib" \
        "target/$TARGET/release/streamcore_input_ffi.pdb" "$STAGE/$ARCH/"
done
tar -cf "$ROOT/out.tar" -C "$STAGE" .
