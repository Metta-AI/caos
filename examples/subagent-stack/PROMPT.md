Add an --uppercase option to this tiny greeting command and organize the work
as two reviewable changes using subagents and workspaces.

Start by confirming that the selected workspace is named main. Then:

1. Spawn two subagents from workspace main before waiting for either:
   - Code agent: edit only greet.sh. Support an optional leading --uppercase
     flag, followed by an optional name. Keep "Hello, world!" as the default.
     "bash greet.sh --uppercase Ada" must print "HELLO, ADA!".
     "--uppercase" without a name must print "HELLO, WORLD!".
     Reject unknown options and extra arguments with a nonzero exit status.
     Use Bash only and preserve executable permissions.
   - Documentation agent: edit only README.md. Document the same interface,
     with examples for the default, a named greeting, and uppercase mode.
     Do not change implementation files or add tests.

2. Wait for both agents. Review their results and use harvest_agent to apply
   each result to workspace main. Resolve any conflicts, check the documented
   examples, and keep code plus documentation together in this workspace.
   Do not promote either of these first two children.

3. Spawn a third subagent from the updated main workspace:
   - Test agent: add only test.sh, using Bash with no dependencies. Test the
     default greeting, a named greeting, uppercase with and without a name,
     unknown-option rejection, and extra-argument rejection. Run the tests.
     Do not change greet.sh or README.md; report a defect if tests expose one.

4. Wait for the test agent and review its result. Promote its main workspace
   into a parent-conversation workspace named checks using the workspaces tool
   (action=promote, name=checks, child=<test agent ID>, source=main).
   Do not harvest this child's changes into main. Promotion should make checks
   depend on main at the commit from which that child started.

5. Verify the final organization:
   - main contains the feature and its documentation, with no test.sh.
   - checks contains that same feature plus test.sh, and its upstream is main.
   - Both workspaces retain the original GitHub repository as their destination.
   Run the relevant checks in each workspace. Do not modify files merely to
   create additional commits.

Stop and summarize the two proposed PRs and validation results.
Do not publish or merge anything; I will open the publication preview myself.
