#!/usr/bin/env bash
# A DIAGNOSTIC test, not a behavioural one: it asserts nothing and always
# passes. It prints what the harness delivered AND what the inner stack knows
# about it, because the 7 rustc-tool failures are a client-side
# `git push … fatal: bad tree object <runner layer00>` and the wrapper demonstrably
# CONTAINS that object.
#
# It exists because every earlier attempt RECONSTRUCTED the chain from the HOST's
# std, while a wrapper is built from the TREE UNDER TEST's std — different builds,
# different runner deltas, so the reconstruction only ever agreed with itself.
set -euo pipefail

W=/cas/args/in/std
RUNNER=$W/rgrep/DEEP-DEPS/rustc/DEEP-DEPS/runner

echo "== the delivered std subset ==" >&2
find "$W" -maxdepth 3 2>/dev/null | sort >&2 || echo "  (no $W)" >&2

echo "== recorded hashes down rgrep's mount chain ==" >&2
for p in "$W" "$W/rgrep" "$W/rgrep/DEEP-DEPS/rustc" "$RUNNER" "$RUNNER/layer00"; do
  if [ -e "$p" ]; then printf '  %-44s %s\n' "$(caos hash "$p" 2>&1 | tail -1)" "${p#$W/}" >&2
  else printf '  %-44s %s\n' "!! ABSENT" "${p#$W/}" >&2; fi
done

L=$(caos hash "$RUNNER/layer00" 2>/dev/null | tail -1 || true)
echo "== does the INNER SERVER hold that layer00 ($L)? ==" >&2
echo "-- refs the inner server advertises:" >&2
git ls-remote "$CAOS_SERVER_URL" 2>&1 | head -20 >&2 || true
echo "-- direct fetch of $L into a scratch repo:" >&2
rm -rf /tmp/probe && git init -q --bare /tmp/probe
if git -c fetch.negotiationAlgorithm=noop -C /tmp/probe fetch -q --no-write-fetch-head \
     "$CAOS_SERVER_URL" "$L" 2>&1 | head -5 >&2; then
  echo "   fetch OK -> server HAS it: $(git -C /tmp/probe cat-file -t "$L" 2>&1)" >&2
else
  echo "   fetch FAILED -> server does NOT have it" >&2
fi

echo "== does the CLIENT repo have it? ==" >&2
git -C /tmp/client cat-file -t "$L" 2>&1 | head -2 >&2 || true

echo "deps-dump: ALL PASS" >&2
