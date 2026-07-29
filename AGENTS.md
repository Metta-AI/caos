# Clean code

- Write simple, clean code
- Do not write defensive code just because you aren't sure about how things work
- Die when there's an unexpected error
- Don't truncate/tail logs. Leave the full thing. We can shorten later, closer to usage if we like

# Before committing

- Build and deploy with `nix build && caosd up` (the deploy publishes the
  binaries as `refs/caos/bins`), then test with `caos-cli run-tool test` —
  binaries are never rebuilt inside caos
- Testing against a stack you did NOT just deploy to (a client-only change,
  say): run `caosd std-check` first. It takes seconds, and it turns a wiped
  registry into one clear error instead of every test failing inside the
  fan-out. `caosd up` needs no such check — it republishes, which repairs.
- If this doesn't catch everything, we need to add it to the above step
