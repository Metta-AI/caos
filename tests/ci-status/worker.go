// tests/ci-status — a WORKER test of std/ci-status's `check` stage: given
// what std/github answered for a reference (its check runs and its combined
// status), the verdict line, the order of the checks, and the refusal of an
// answer that is an error. Each case runs the stage directly with a made-up
// answer and a deadline already past, so it answers rather than reading again.
//
// Staged: no run can be waited on, so each case is checked in the `then` of
// its own run, which then starts the next case.
package main

import (
	"fmt"
	"os"
	"strconv"
	"strings"

	"caos/w"
)

type testCase struct {
	name   string
	answer string
	failed bool
	says   []string
}

func results(runs, statuses string) string {
	return `[{"op":"api","ok":true,"status":200,"body":{"total_count":1,"check_runs":[` + runs + `]}},` +
		`{"op":"api","ok":true,"status":200,"body":{"sha":"0123456789abcdef0123","statuses":[` + statuses + `]}}]`
}

var cases = []testCase{
	{
		name:   "passing",
		answer: results(`{"name":"build","status":"completed","conclusion":"success"},{"name":"lint","status":"completed","conclusion":"skipped"}`, `{"context":"ci/x","state":"success"}`),
		says:   []string{"checks: 3 passing", "head 0123456789ab", "branch feature"},
	},
	{
		name:   "failing first, then pending",
		answer: results(`{"name":"build","status":"completed","conclusion":"success"},{"name":"test","status":"in_progress"}`, `{"context":"deploy","state":"failure"}`),
		says:   []string{"checks: FAILING: deploy; pending: test\n", "\nfailure          deploy\nin_progress      test\nsuccess          build\n"},
	},
	{
		name:   "pending past the deadline says so",
		answer: results(`{"name":"test","status":"queued"}`, ``),
		says:   []string{"checks: pending: test (0 passing)", "still pending after waiting 1 min"},
	},
	{
		name:   "no checks",
		answer: results(``, ``),
		says:   []string{"checks: no checks"},
	},
	{
		name:   "GitHub refused",
		answer: `[{"op":"api","ok":false,"status":404,"path":"/repos/o/r/commits/feature/check-runs","body":{"message":"Not Found"}},{"op":"api","ok":false,"skipped":true}]`,
		failed: true,
		says:   []string{"GitHub answered 404"},
	},
}

// next is this test curried for stage s, the `then` of a case's run.
func next(s int) string {
	return w.Caos("curry", "--base:@=/cas/args/base", "--worker1:@=/cas/args/worker1",
		"--stage="+strconv.Itoa(s), "--test-salt:@=/cas/args/test-salt", "--ci-status:@=/cas/args/ci-status")
}

func launch(i int) {
	c := cases[i]
	w.Step(fmt.Sprintf("case %d: %s", i, c.name))
	w.Must(os.WriteFile("/tmp/answer", []byte(c.answer), 0o644))
	w.Put("/tmp/answer", "/cas/answer")
	request := w.Caos("prepare-request", "--base:@=/cas/args/ci-status", "--stage=check",
		"--result:@=/cas/answer", "--repository=https://github.com/o/r", "--reference=feature",
		"--deadline=1", "--read=0", "--wait-min=1",
		"--request-id=ci-status-"+strings.TrimSpace(w.Read("/cas/args/test-salt"))+"-"+strconv.Itoa(i))
	w.Caos("run-request-then", request, "--then:hash="+next(i))
}

func check(i int) {
	c := cases[i]
	const r = "/cas/args/result"
	w.Get(r)
	out := w.Read(r + "/stdout")
	failed := w.Exists(r + "/failed")
	w.True(failed == c.failed, "case %q: failed is %v: %s", c.name, failed, out)
	for _, s := range c.says {
		w.True(strings.Contains(out, s), "case %q does not say %q:\n%s", c.name, s, out)
	}
	fmt.Fprintf(os.Stderr, "  ok: %s\n", strings.SplitN(out, "\n", 2)[0])
}

func main() {
	w.Main(func() {
		if !w.Exists("/cas/args/stage") {
			launch(0)
			return
		}
		i := w.Check(strconv.Atoi(strings.TrimSpace(w.Read("/cas/args/stage"))))
		check(i)
		if i+1 < len(cases) {
			launch(i + 1)
			return
		}
		w.Report("ci-status: ALL PASS\n")
	})
}
