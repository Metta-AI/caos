#!/usr/bin/env bash
# Runs cwd'd into a client repo with this test tree at ./test and $CAOS_CLI
# set, INSIDE the dev stack — the suite's per-test job
# (dev/cli-test stages the repo, then runs this).
#
# The unit tests of std/llm-step. tests/unit-test runs `cargo test` over rust/,
# and llm-step is not in that workspace: rustc builds it, with its path
# dependencies spliced in beside its source. So nothing else runs the
# #[cfg(test)] modules in std/llm-step/src. This does, by laying the tree out
# the way rustc does and handing it to the cargo worker, which builds
# `--offline` on the baked crates.io deps (std/cargo, rust/crates/bake-anchor).
set -euo pipefail

fail() { echo "FAIL: $*" >&2; exit 1; }
commit() { git add -A && git -c user.email=test@caos -c user.name=caos commit -qm "$1"; }

# Target musl: the one target the deps bake carries (see tests/cargo-self).
tgt="$(uname -m)-unknown-linux-musl"

# The layout is the one in std/llm-step/Cargo.toml: llm-step's own Cargo.toml and
# src/ at the root, each path dependency in a directory of its own name beside
# them. conversation-protocol reaches git-locator as `../git-locator`, which is
# the same directory from here. Copied out of the mounts (DEEP-DEPS is staged
# from read-only content), and only what cargo reads: a deepened mount also
# carries DEEP-DEPS/ and the caos metadata, which cargo has no use for.
mkdir ws
cp DEEP-DEPS/llm-step/Cargo.toml ws/
cp -rL DEEP-DEPS/llm-step/src ws/src
for crate in llm-client conversation-protocol git-locator worker-common; do
  mkdir "ws/$crate"
  cp DEEP-DEPS/"$crate"/Cargo.toml "ws/$crate/"
  cp -rL DEEP-DEPS/"$crate"/src "ws/$crate/src"
done
chmod -R u+w ws
commit "llm-step with its path dependencies"

echo "== cargo test of std/llm-step ==" >&2
"$CAOS_CLI" run result --base:@=DEEP-DEPS/cargo \
  --tree:@=ws --cmd=test "--target=$tgt"

# A cargo job REPORTS rather than fails: a broken build or a failing test comes
# back as a value with a nonzero `exit`, so the run succeeding says nothing.
if [ "$(cat result/exit)" != "0" ]; then
  echo "---- stderr ----" >&2; tail -c 4000 result/stderr >&2 || true
  echo "---- stdout ----" >&2; tail -c 4000 result/stdout >&2 || true
  fail "cargo test of std/llm-step exited $(cat result/exit)"
fi

# An exit of 0 with no tests would pass too (a wrong layout can leave cargo with
# nothing to run), so check that tests RAN, and that some of the ones that
# motivated this test are among them.
grep -Eq 'test result: ok\. [1-9][0-9]* passed' result/stdout \
  || fail "no passing tests in the output: $(tail -c 2000 result/stdout)"
for name in \
  tools::tests::copy_and_move_refuse_what_cannot_work_before_touching_the_tree \
  tools::tests::remove_refuses_the_protocol_directory_and_bad_paths \
  tools::tests::the_direct_tools_are_declared_with_the_arguments_the_worker_reads; do
  grep -qF "test $name ... ok" result/stdout || fail "$name did not run and pass"
done
echo "  ok: $(grep -Eo 'test result: ok\. [0-9]+ passed' result/stdout | head -1)" >&2
