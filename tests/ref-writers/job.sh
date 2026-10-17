# A job that tries to point `--ref` at a fresh commit, and reports whether the
# server let it. It proves its write with the run token the server injected,
# if it was granted one (design/ref-writers.md).
set -euo pipefail
caos get /cas/args/ref
ref=$(cat /cas/args/ref)
mkdir -p /tmp/job && cd /tmp/job
git init -q .
git remote add caos "$CAOS_SERVER_URL"
commit=$(git -c user.name=job -c user.email=job@caos commit-tree "$(git mktree </dev/null)" -m "from a job $ref")
options=()
if [ -r /secret/caos-write ]; then
  options=(-o "caos-auth=run:$(cat /secret/caos-write)")
fi
mkdir -p /tmp/out
if git push -q "${options[@]}" caos "$commit:$ref" 2>/tmp/out/stderr; then
  echo pushed > /tmp/out/verdict
else
  echo refused > /tmp/out/verdict
fi
caos put /tmp/out /cas/out
