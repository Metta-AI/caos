# Clean code

- Write simple, clean code
- Do not write defensive code just because you aren't sure about how things work
- Die when there's an unexpected error
- Don't truncate/tail logs. Leave the full thing. We can shorten later, closer to usage if we like

# Shell, under `set -euo pipefail`

Every script here runs with it, and two constructs quietly break under it.

- **`[ cond ] && action` exits the script when the condition is false**, if that
  list is the last command in its scope (a function, a loop body, a `{ }` block,
  the script). Write `if`. This is not hypothetical: it is the single largest
  source of bugs in this tree's shell — `build-builtins.sh` still carries three
  latent instances, one of which makes `./build-builtins.sh <name>` exit 1
  before doing anything.
- **`pipefail` makes a pipeline fail on its LEFTMOST failure**, not its last
  command. `curl -f ... | awk` returns curl's 22 on a 404, so a lookup whose
  "absent" answer is a 404 dies instead of returning empty. Distinguish absent
  from broken explicitly — a swallowed error here silently hides an unreachable
  service.

# Before committing

- Build and deploy with `nix build && caosd up` (the deploy publishes the
  binaries as `refs/caos/bins`), then test with `caos-cli run-tool test` —
  binaries are never rebuilt inside caos
- Testing against a stack you did NOT just deploy to (a client-only change,
  say): run `caosd std-check` first. It takes seconds, and it turns a wiped
  registry into one clear error instead of every test failing inside the
  fan-out. `caosd up` needs no such check — it republishes, which repairs.
- **Never read a gate's exit status through a pipe.** `caosd up 2>&1 | tail`
  reports `tail`'s status, so a failed deploy looks like a pass — that happened,
  and the next step ran against a stack that was not up. Use
  `cmd 2>&1 | tail; echo "EXIT=${PIPESTATUS[0]}"`, or don't pipe.
- If this doesn't catch everything, we need to add it to the above step
