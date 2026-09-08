# Scripted LLM test support

`worker-common.sh` contains fixture plumbing shared by the independent v3
conversation integration tests. This source-only std entry has no
`.caos-expr`; deepening mounts the files themselves rather than producing a
runnable image.

Keep behavioral assertions in the consuming test. Sharing the stub setup,
temporary conversation naming, admission, leased ref updates, and record
readers lets those tests fan out without copying a second test framework into
every directory.
