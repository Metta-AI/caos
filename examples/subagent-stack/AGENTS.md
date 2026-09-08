This is a small Bash project. There is no package manager or build step.
Use Bash and coreutils already available in the worker; do not install tools.

Keep changes confined to the delegated files. Preserve executable permissions.
Shell scripts use set -euo pipefail. Use if statements for conditional actions
and for commands expected to fail.

Validate code with bash -n greet.sh and exercise the documented examples.
If test.sh exists, also run bash test.sh. Do not add infrastructure, dependencies,
or extra documentation files. Do not create PRs or publish branches from a worker.
