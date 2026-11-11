#!/usr/bin/env bash
# Builds crates/streamcore-video-windows on a Windows test machine over SSH and runs its bench.
#
#   STREAMCORE_WINDOWS_HOST=<ssh host> scripts/run-video-windows-bench.sh sync
#   STREAMCORE_WINDOWS_HOST=<ssh host> scripts/run-video-windows-bench.sh build
#   STREAMCORE_WINDOWS_HOST=<ssh host> scripts/run-video-windows-bench.sh run NAME COMMAND [args...]
#   STREAMCORE_WINDOWS_HOST=<ssh host> scripts/run-video-windows-bench.sh ssh-run NAME COMMAND [args...]
#
# `sync` copies the tracked and new files of this checkout to $DIR\src. `build` runs
# `cargo build --release --features bench` there with CARGO_HOME and the target directory inside
# $DIR, so nothing is written outside it. `run` runs the bench in the session of the user logged
# on at the console, where Windows Graphics Capture works (an SSH session cannot capture): it
# writes $DIR\run-NAME.cmd, registers a one-shot scheduled task for that user (`/IT`, no stored
# password), starts it, waits for it, deletes the task (also on exit or interrupt) and copies
# $DIR\out\NAME.txt and NAME.json to target/bench-out/. `ssh-run` runs a command that needs no
# desktop (`report`, `probe`) directly over SSH.
#
# The machine needs Rust (rustup) with the toolchain in rust-toolchain.toml, the MSVC build
# tools, OpenSSH server with cmd as the default shell, and, for `run`, a user logged on at the
# console. The script installs nothing and changes no display, power or driver settings.
set -euo pipefail

HOST=${STREAMCORE_WINDOWS_HOST:?set STREAMCORE_WINDOWS_HOST to the ssh host of the Windows machine}
DIR=${STREAMCORE_WINDOWS_DIR:-C:\\streamcore-p5}
CARGO=${STREAMCORE_WINDOWS_CARGO:-cargo}
TASK=streamcore-p5-bench
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
OUT=${STREAMCORE_BENCH_OUT:-$ROOT/target/bench-out}
EXE="$DIR\\target\\release\\streamcore-video-windows-bench.exe"
SCP_DIR=${DIR//\\//}

remote() { ssh "$HOST" "$@"; }

sync_tree() {
  local tar
  tar=$(mktemp)
  (cd "$ROOT" && git ls-files -co --exclude-standard -z | COPYFILE_DISABLE=1 xargs -0 tar -cf "$tar")
  remote "if exist $DIR\\src rmdir /s /q $DIR\\src" >/dev/null
  remote "mkdir $DIR\\src"
  scp -q "$tar" "$HOST:$SCP_DIR/src.tar"
  rm -f "$tar"
  remote "cd /d $DIR\\src && tar -xf ..\\src.tar && del ..\\src.tar"
}

build() {
  remote "cd /d $DIR\\src && set CARGO_HOME=$DIR\\cargo-home&& set CARGO_TARGET_DIR=$DIR\\target&& $CARGO build --release --features bench -p streamcore-video-windows --locked"
}

fetch() {
  local name=$1
  mkdir -p "$OUT"
  scp -q "$HOST:$SCP_DIR/out/$name.txt" "$OUT/$name.txt"
  scp -q "$HOST:$SCP_DIR/out/$name.json" "$OUT/$name.json" 2>/dev/null || true
  cat "$OUT/$name.txt"
}

ssh_run() {
  local name=$1
  shift
  remote "cd /d $DIR && (if not exist out mkdir out) && \"$EXE\" $* --json out\\$name.json > out\\$name.txt 2>&1" || true
  fetch "$name"
}

cleanup_task() {
  remote "schtasks /Delete /TN $TASK /F" >/dev/null 2>&1 || true
}

run_task() {
  local name=$1
  shift
  local user cmdfile
  user=$(remote whoami | tr -d '\r')
  cmdfile=$(mktemp)
  # Written locally and copied, so no shell mangles the backslashes.
  {
    printf '@echo off\r\n'
    printf 'cd /d %s\r\n' "$DIR"
    printf 'if not exist out mkdir out\r\n'
    printf 'del out\\%s.done 2>nul\r\n' "$name"
    printf '"%s" %s --json out\\%s.json > out\\%s.txt 2>&1\r\n' "$EXE" "$*" "$name" "$name"
    printf 'echo %%ERRORLEVEL%% > out\\%s.done\r\n' "$name"
  } >"$cmdfile"
  scp -q "$cmdfile" "$HOST:$SCP_DIR/run-$name.cmd"
  rm -f "$cmdfile"
  trap cleanup_task EXIT INT TERM
  remote "schtasks /Create /TN $TASK /SC ONCE /ST 23:59 /IT /RU $user /TR \"$DIR\\run-$name.cmd\" /F" >/dev/null
  remote "schtasks /Run /TN $TASK" >/dev/null
  local waited=0
  until remote "if exist $DIR\\out\\$name.done (exit 0) else (exit 1)"; do
    sleep 2
    waited=$((waited + 2))
    if [ "$waited" -gt 600 ]; then
      echo "timeout waiting for $name" >&2
      exit 1
    fi
  done
  cleanup_task
  trap - EXIT INT TERM
  remote "del $DIR\\run-$name.cmd $DIR\\out\\$name.done" >/dev/null
  fetch "$name"
}

case "${1:-}" in
  sync) sync_tree ;;
  build) build ;;
  run | ssh-run)
    mode=$1
    shift
    [ $# -ge 2 ] || { echo "usage: $0 $mode NAME COMMAND [args...]" >&2; exit 2; }
    if [ "$mode" = run ]; then run_task "$@"; else ssh_run "$@"; fi
    ;;
  *)
    echo "usage: $0 sync | build | run NAME COMMAND [args...] | ssh-run NAME COMMAND [args...]" >&2
    exit 2
    ;;
esac
