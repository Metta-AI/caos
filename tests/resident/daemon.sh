#!/usr/bin/env bash
# The resident worker under test: handles one message per job, in ONE process,
# and says in its reply which process and which job it was.
#
#   <anything>  reply `host=<container> token=<this process> n=<jobs so far>
#               kept=<yes if /cas/kept, left by an earlier job, was still there>`
#   stop        reply `stopped …` and exit normally: an explicit stop op
#   crash       die by SIGKILL in the middle of the job
#
# `host` tells containers apart (every container's pids start from the same
# number) and `token` tells processes apart.
set -euo pipefail

# The runner is PID 1 of its container, so when it started names the container:
# /etc/hostname does not, because workers run with --network=host and share it.
read -ra stat < /proc/1/stat
host=${stat[21]}
token="$$-$RANDOM-$RANDOM-$(date +%s%N)"
n=0

while true; do
  caos get /cas/args/in
  msg=$(cat /cas/args/in)
  n=$((n + 1))

  # The narrow reset removes the PREVIOUS job's result with its args. A
  # leftover here would be answered for this job.
  if [ -e /cas/out ]; then echo "stale /cas/out at the start of job $n" >&2; exit 1; fi

  # Content fetched outside /cas/args survives the reset: that is what staying
  # resident is FOR.
  if [ -e /cas/kept ]; then
    kept=yes
  else
    kept=no
    echo kept > /tmp/kept
    caos put /tmp/kept /cas/kept
  fi

  case "$msg" in
    crash) kill -9 $$ ;;
    stop)
      printf 'stopped host=%s token=%s n=%s' "$host" "$token" "$n" > /tmp/out
      caos put /tmp/out /cas/out
      exit 0
      ;;
  esac

  printf 'host=%s token=%s n=%s kept=%s in=%s' "$host" "$token" "$n" "$kept" "$msg" > /tmp/out
  caos put /tmp/out /cas/out

  # `caos next` returns 0 with the next job's args at /cas/args, or 10 to say
  # leave. Anything else is an error, and dying on it is right.
  if caos next; then continue; fi
  rc=$?
  if [ "$rc" -eq 10 ]; then exit 0; fi
  exit "$rc"
done
