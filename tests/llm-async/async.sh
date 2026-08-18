#!/usr/bin/env bash
# An independently dispatched worker with a deterministic release barrier.
# The second llm-stub records this request, then blocks reading its response
# FIFO until the outer test has proved the primary conversation is idle.
set -euo pipefail

caos get /cas/args/gate-host
caos get /cas/args/gate-port
gate_host=$(</cas/args/gate-host)
gate_port=$(</cas/args/gate-port)

exec 3<>"/dev/tcp/$gate_host/$gate_port"
printf 'POST /v1/messages HTTP/1.1\r\nHost: %s\r\nContent-Type: application/json\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}' \
  "$gate_host" >&3
while IFS= read -r _line <&3; do
  :
done
exec 3>&-

printf 'independent work finished\n' >/tmp/async-result
caos put /tmp/async-result /cas/out
