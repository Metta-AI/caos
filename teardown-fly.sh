#!/usr/bin/env bash
# Tear down a caos stack on fly.io — every app named `<stack>-caos-*` (server,
# registry, redis, and all on-demand `<stack>-caos-worker-<hash16>` apps).
# Destroying an app removes its machines, volumes, and IPs too, so this wipes the
# whole stack, INCLUDING caosd's /git data. Irreversible.
#
# Usage: ./teardown-fly.sh <stack> [-y|--yes]
#   <stack>   the stack name passed to deploy-fly.sh
#   -y|--yes  skip the confirmation prompt
#
# Requires: .caos-dev/fly.env with CAOS_FLY_TOKEN; flyctl on PATH.
set -euo pipefail
cd "$(dirname "$0")"
PROJECT=$PWD

STACK=${1:-}
[ -n "$STACK" ] || { echo "usage: $0 <stack> [-y|--yes]" >&2; exit 2; }
[[ "$STACK" =~ ^[a-z0-9][a-z0-9-]*$ ]] || {
  echo "stack must be lowercase [a-z0-9-] (got: $STACK)" >&2; exit 2; }
ASSUME_YES=0
[ "${2:-}" = -y ] || [ "${2:-}" = --yes ] && ASSUME_YES=1

set -a; . "$PROJECT/.caos-dev/fly.env"; set +a
: "${CAOS_FLY_TOKEN:?set CAOS_FLY_TOKEN in .caos-dev/fly.env}"
export FLY_API_TOKEN=$CAOS_FLY_TOKEN

# All apps whose name leads with this stack. The `-caos-` separator means stack
# `foo` never matches `foobar-caos-*` (it'd need `foo-caos-`).
mapfile -t apps < <(flyctl apps list 2>/dev/null \
  | grep -oE "${STACK}-caos-[a-z0-9-]*" | sort -u)

if [ "${#apps[@]}" -eq 0 ]; then
  echo "no apps found for stack '$STACK' (looked for ${STACK}-caos-*)" >&2
  exit 0
fi

echo "Stack '$STACK' — these ${#apps[@]} app(s) and ALL their data will be destroyed:" >&2
printf '  %s\n' "${apps[@]}" >&2

if [ "$ASSUME_YES" -ne 1 ]; then
  printf 'Proceed? [y/N] ' >&2
  read -r ans
  [[ "$ans" =~ ^[Yy] ]] || { echo "aborted" >&2; exit 1; }
fi

for app in "${apps[@]}"; do
  echo "==> destroying $app" >&2
  flyctl apps destroy "$app" --yes
done
echo "==> stack '$STACK' torn down." >&2
