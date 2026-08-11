#!/usr/bin/env bash
# Runs *inside* a bash worker (launched by this test's cli.sh). The test
# directory is at /cas/args/test, in a real /cas.
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

# The regression: workers stage a result by symlinking already-fetched /cas
# entries into a scratch tree and `caos put`ting it (this is how write/edit keep
# untouched siblings). When a staged sibling is itself a git symlink, `caos put`
# must reuse it AS a symlink — not follow it to its target and record a regular
# copy. Reproduce that staging exactly and confirm the link survives put+get.
echo "== a staged git symlink survives put + get ==" >&2
stage=/tmp/stage
rm -rf "$stage"; mkdir -p "$stage"
ln -s "$T/tree/file.txt" "$stage/file.txt"
ln -s "$T/tree/link.txt" "$stage/link.txt"   # staging link -> a git symlink node
caos put "$stage" /cas/staged
caos get -r /cas/staged
[ -L /cas/staged/link.txt ] \
  || fail "staged link.txt was flattened into a regular file"
staged_target=$(readlink /cas/staged/link.txt)
[ "$staged_target" = "file.txt" ] \
  || fail "staged link.txt target changed: $staged_target"
echo "  ok: staged link.txt is still a symlink -> $staged_target" >&2

echo "symlinks: ALL PASS" >&2
