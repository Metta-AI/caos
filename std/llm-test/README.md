# Scripted LLM test support

`common.sh` contains fixture plumbing shared by the independent conversation
integration tests. This source-only std entry has no `.caos-expr`; deepening
mounts the files themselves rather than producing a runnable image.

Keep behavioral assertions in the consuming test. Sharing the stub setup,
temporary conversation naming, and git/ref helpers lets those tests be split
for fan-out without copying a second test framework into every directory.
