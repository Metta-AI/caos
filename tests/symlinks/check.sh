#!/usr/bin/env bash
# Runs *inside* a bash worker (launched by this test's cli.sh). The test directory is at
# /cas/args/test and builtins are at /cas/std/<name>, all in a real /cas.
#
# Proves a git symlink survives the round trip into a worker: the fixture tree/
# holds a real file and a symlink to it. caos ingests the directory (reusing git's
# recorded objects, where the link is a mode-120000 blob), and `caos get -r`
# materializes it back into the worker's /cas. The worker must then see the link
# as a genuine symlink — not a regular file holding the target's path, and not a
# dereferenced copy of the file's contents.
set -euo pipefail
T=/cas/args/test
caos get -r "$T"   # materialize the fixture so it's readable in this worker

fail() { echo "FAIL: $*" >&2; exit 1; }

file="$T/tree/file.txt"
link="$T/tree/link.txt"

echo "== the link is a real symlink ==" >&2
[ -L "$link" ] || fail "$link is not a symlink"
echo "  ok: link.txt is a symlink" >&2

echo "== it points at the right target ==" >&2
target=$(readlink "$link")
[ "$target" = "file.txt" ] || fail "expected target file.txt, got: $target"
echo "  ok: link.txt -> $target" >&2

echo "== the file itself is a regular file ==" >&2
[ -f "$file" ] && [ ! -L "$file" ] || fail "$file is not a regular file"
echo "  ok: file.txt is a regular file" >&2

echo "== reading through the link yields the file's contents ==" >&2
[ "$(cat "$link")" = "$(cat "$file")" ] \
  || fail "content via the link differs from the file"
echo "  ok: cat link.txt == cat file.txt" >&2

echo "symlinks: ALL PASS" >&2
