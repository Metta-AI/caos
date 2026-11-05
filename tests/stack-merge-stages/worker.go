// tests/stack-merge-stages — a WORKER test of std/stack-merge-to-master's
// later stages, given made-up GitHub answers: the CI verdict (a failing check
// refuses the whole merge and lists the checks; force does not refuse), the
// dry run's commit titles, and the end of an asynchronous merge. Each case
// runs one stage directly with a state and an answer, the way the stage
// before it would have left them; no stage that would call GitHub again is
// reached.
//
// Staged: each case is checked in the `then` of its own run.
package main

import (
	"encoding/json"
	"fmt"
	"os"
	"strconv"
	"strings"

	"caos/w"
)

const (
	c1 = "1111111111111111111111111111111111111111"
	c2 = "2222222222222222222222222222222222222222"
)

func targets() []map[string]any {
	return []map[string]any{
		{"name": "01-a", "branch": "a", "title": "Add a", "body": "Why a.", "commit": c1, "number": 7},
		{"name": "02-b", "branch": "b", "title": "Add b", "body": "", "commit": c2, "number": 8},
	}
}

func ci(runs1, runs2 string) string {
	r := func(runs string) string {
		return `{"op":"api","ok":true,"status":200,"body":{"check_runs":[` + runs + `]}},{"op":"api","ok":true,"status":200,"body":{"statuses":[]}}`
	}
	return "[" + r(runs1) + "," + r(runs2) + "]"
}

const (
	pass = `{"name":"build","status":"completed","conclusion":"success"}`
	fail = `{"name":"build","status":"completed","conclusion":"failure","html_url":"https://ci.example/1"}`
)

type testCase struct {
	name   string
	stage  string
	state  map[string]any
	answer string
	failed bool
	says   []string
}

func state(extra map[string]any) map[string]any {
	s := map[string]any{"repository": "https://github.com/o/r", "trunk": "main", "owner": "o", "repo": "r",
		"targets": targets(), "reading": []int{0, 1}, "deadline": 1}
	for k, v := range extra {
		s[k] = v
	}
	return s
}

var cases = []testCase{
	{
		name: "failing CI refuses everything", stage: "verdict", state: state(nil), answer: ci(pass, fail), failed: true,
		says: []string{"01-a  a  PR #7  checks: 1 passing", "02-b  b  PR #8  checks: FAILING: build",
			"refused; nothing was merged: CI is not passing on 02-b (PR #8): FAILING: build",
			"\n02-b  PR #8\n", "failure          build  https://ci.example/1"},
	},
	{
		name: "a dry run titles each squash", stage: "verdict", state: state(map[string]any{"dry_run": true}), answer: ci(pass, pass),
		says: []string{"dry run: nothing merged.", `would merge as "Add a (#7)"`, `would merge as "Add b (#8)"`},
	},
	{
		name: "force merges past failing CI", stage: "verdict", state: state(map[string]any{"dry_run": true, "force": true}), answer: ci(fail, pass),
		says: []string{"force=true: CI is not passing on 01-a (PR #7)"},
	},
	{
		name: "a failed merge stops the stack", stage: "merging", state: state(map[string]any{"k": 0, "until": 1}),
		answer: `[{"op":"api","ok":true,"status":200,"body":{"status":"failed","details":{"message":"Base branch was modified"}}}]`,
		failed: true,
		says:   []string{"01-a  a  PR #7", "NOT MERGED", "GitHub could not merge PR #7: Base branch was modified", "The layers above it were not merged."},
	},
	{
		name: "a merge queue is not a merge", stage: "merging", state: state(map[string]any{"k": 1, "until": 1, "merged": 1}),
		answer: `[{"op":"api","ok":true,"status":202,"body":{"status":"enqueued","details":{}}}]`,
		failed: true,
		says:   []string{"PR #8 was added to the merge queue", "Next: run caos-std/stack-sync with remove-merged=true"},
	},
}

func salt() string { return strings.TrimSpace(w.Read("/cas/args/test-salt")) }

func next(s int) string {
	return w.Caos("curry", "--base:@=/cas/args/base", "--worker1:@=/cas/args/worker1",
		"--stage="+strconv.Itoa(s), "--test-salt:@=/cas/args/test-salt",
		"--merge:@=/cas/args/merge")
}

func launch(i int) {
	c := cases[i]
	w.Step(fmt.Sprintf("case %d: %s", i, c.name))
	w.Must(os.WriteFile("/tmp/state", w.Check(json.Marshal(c.state)), 0o644))
	w.Put("/tmp/state", "/cas/state")
	w.Must(os.WriteFile("/tmp/answer", []byte(c.answer), 0o644))
	w.Put("/tmp/answer", "/cas/answer")
	request := w.Caos("prepare-request", "--base:@=/cas/args/merge", "--stage="+c.stage,
		"--state:@=/cas/state", "--result:@=/cas/answer",
		fmt.Sprintf("--request-id=stack-merge-stages-%s-%d", salt(), i))
	w.Caos("run-request-then", request, "--then:hash="+next(i))
}

func check(i int) {
	c := cases[i]
	const r = "/cas/args/result"
	w.Get(r)
	out := w.Read(r + "/stdout")
	failed := w.Exists(r + "/failed")
	w.True(failed == c.failed, "case %q: failed is %v:\n%s", c.name, failed, out)
	for _, s := range c.says {
		w.True(strings.Contains(out, s), "case %q does not say %q:\n%s", c.name, s, out)
	}
	fmt.Fprintf(os.Stderr, "  ok: %s\n", c.name)
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
		w.Report("stack-merge-stages: ALL PASS\n")
	})
}
