# The repo's own entry points (format `<path> <name>`, path relative to this
# directory). These are what a CLIENT reaches for — `run-tool` runs every tool
# script on `bash`, and `talk`/`chat` drive a turn with `llm-step` — declared the
# same way a worker's dependencies are, and expanded by the root `.caos-expr`
# into `DEEP-DEPS/<name>`.
#
# A consumer repo writes the same three lines pointing at wherever it mounted
# caos (`./flake-inputs/caos/std/bash bash`), so the client code is identical in
# both: descend `DEEP-DEPS/<name>` and evaluate what is there. Nothing looks a
# builtin up by an ambient name.
./std/bash bash
./std/llm-step llm-step
./std/llm-call llm-call
