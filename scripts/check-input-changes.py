#!/usr/bin/env python3
"""Fails when the shared input model changes without an entry in its change log.

Pad's apps build streamcore-input from a pinned StreamCore commit and move only when
they re-pin. crates/streamcore-input/CHANGES.md is how
they find out what changed: every change to the paths below adds an entry there.

The range is this commit against main: the merge base for a branch, or the first
parent for a commit already on main. Set STREAMCORE_INPUT_BASE to compare against
another commit.
"""

import os
import subprocess
import sys

INPUT_PATHS = (
    "crates/streamcore-input/",
    "crates/streamcore-input-ffi/",
    "protocol/input/",
    "StreamCoreInput/",
    "scripts/build-streamcore-input-apple.sh",
    "scripts/build-streamcore-input-android.sh",
    "scripts/build-streamcore-input-windows.sh",
    "ci/local/streamcore-input-windows-remote.sh",
)
LOG = "crates/streamcore-input/CHANGES.md"
MAIN_REFS = ("mirror/heads/main", "origin/main", "main")


def git(*args, check=True):
    result = subprocess.run(["git", *args], capture_output=True, text=True)
    if check and result.returncode:
        sys.exit("error: git %s: %s" % (" ".join(args), result.stderr.strip()))
    return result.stdout.strip() if result.returncode == 0 else None


def base():
    if os.environ.get("STREAMCORE_INPUT_BASE"):
        return git("rev-parse", os.environ["STREAMCORE_INPUT_BASE"])
    head = git("rev-parse", "HEAD")
    for ref in MAIN_REFS:
        if git("rev-parse", "--verify", "--quiet", ref + "^{commit}", check=False):
            fork = git("merge-base", "HEAD", ref)
            if fork != head:
                return fork
            # Already on main: check what this commit brought in.
            return git("rev-parse", "--verify", "--quiet", "HEAD^1", check=False)
    sys.exit("error: no main branch to compare with; set STREAMCORE_INPUT_BASE")


def main():
    start = base()
    if not start:
        print("input change log: root commit, nothing to compare")
        return
    changed = git("diff", "--name-only", start, "HEAD").splitlines()
    touched = sorted(path for path in changed if path != LOG and path.startswith(INPUT_PATHS))
    if not touched:
        print("input change log: no input changes since %s" % start[:7])
        return
    if LOG not in changed:
        print("error: these input files changed since %s without an entry in %s:" % (start[:7], LOG), file=sys.stderr)
        for path in touched:
            print("  " + path, file=sys.stderr)
        print("Add an entry saying what changed for the apps that pin the input model.", file=sys.stderr)
        sys.exit(1)
    added = git("diff", "-U0", start, "HEAD", "--", LOG)
    if not any(line.startswith("+## ") for line in added.splitlines()):
        sys.exit("error: %s changed but gained no new '## ' entry" % LOG)
    print("input change log: %d input files changed, with a new entry in %s" % (len(touched), LOG))


if __name__ == "__main__":
    main()
