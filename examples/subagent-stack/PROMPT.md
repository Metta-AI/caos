Add an --uppercase option to this greeting command and organize the work as
two reviewable changes using subagents.

Start with feature/dirty. Confirm feature/.base-url names the destination and
feature/00-base records its incorporated base. Code references are paths in
the conversation tree, not registered workspace objects.

1. Spawn two subagents from feature/dirty before waiting for either:
   - Code agent: edit only greet.sh. Support an optional leading --uppercase
     flag followed by an optional name. The default is "Hello, world!".
     Uppercase Ada prints "HELLO, ADA!"; uppercase without a name prints
     "HELLO, WORLD!". Reject unknown options and extra arguments.
     Use Bash only and preserve executable permissions.
   - Documentation agent: edit only README.md. Document the same interface
     with examples. Do not change implementation files or add tests.

2. Wait for both agents. Review and harvest both results into feature/dirty.
   Resolve conflicts and check the documented examples. With the workspaces
   tool, move feature/dirty to feature/01-uppercase. This names a PR boundary
   without changing or squashing its code commit.

3. Create feature/dirty by copying feature/01-uppercase. Spawn a test subagent
   from feature/dirty: add only test.sh using Bash with no dependencies. Cover
   the default and named greeting, uppercase with and without a name, and
   rejection of unknown options and extra arguments. Run the tests. Report
   defects rather than changing greet.sh or README.md.

4. Review and harvest the test result into feature/dirty, then move that
   reference to feature/02-checks. The second boundary must descend from the
   first in actual Git history; naming it does not perform that integration.

5. Verify feature/01-uppercase contains code and docs without test.sh, and
   feature/02-checks contains both plus passing tests. Keep .base-url and
   00-base unchanged. Do not create unnecessary commits.

Stop and summarize the two proposed PRs and validation. Do not publish or merge;
I will open Ctrl+P to review feature/01-uppercase -> the external base and
feature/02-checks -> feature/01-uppercase.
