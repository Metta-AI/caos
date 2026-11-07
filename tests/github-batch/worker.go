// tests/github-batch — a WORKER test of std/github's batch, with no client.
//
// Each case runs std/github with one batch and checks the JSON array it
// answers with: one result per operation, a stop at the first operation that
// is not ok, and the later ones marked skipped. Nothing reaches GitHub or any
// other remote: the repositories are `.invalid` hosts, which fail to resolve,
// and the pushed commit is one the caos server does not hold, which its
// /git/push refuses before anything is sent.
//
// Every request carries --test-salt in its request-id, so a salted run really
// runs std/github again; the server's salt does not reach a sub-run's key
// (CLAUDE.md).
//
// Staged: no run can be waited on, so each case is checked in the `then` of
// its own run, which then starts the next case.
package main

import (
	"bytes"
	"encoding/json"
	"errors"
	"fmt"
	"os"
	"os/exec"
	"strconv"
	"strings"

	"caos/w"
)

const invalid = "https://caos-github-batch.invalid/repo.git"

// Not stored on the server, so /git/push refuses it as a validation error.
const unknown = "0123456789abcdef0123456789abcdef01234567"

func run(name string, args ...string) string {
	cmd := exec.Command(name, args...)
	var stdout, stderr bytes.Buffer
	cmd.Stdout, cmd.Stderr = &stdout, &stderr
	err := cmd.Run()
	if err != nil {
		var exit *exec.ExitError
		w.True(errors.As(err, &exit), "running %s: %v", name, err)
	}
	w.True(err == nil, "%s %s: %s", name, strings.Join(args, " "), strings.TrimSpace(stderr.String()))
	return strings.TrimRight(stdout.String(), "\n")
}

func caos(args ...string) string { return run("caos", args...) }

func read(path string) string {
	caos("get", path)
	return string(w.Check(os.ReadFile(path)))
}

// testCase is one batch and what its results must say, operation by
// operation: whether it is ok, and text its error must contain.
type testCase struct {
	name   string
	batch  string
	expect []expectation
}

type expectation struct {
	ok      bool
	skipped bool
	error   string
	fields  map[string]string
}

var cases = []testCase{
	{
		name:   "an unreachable remote stops the batch",
		batch:  `[{"op":"head","repository":"` + invalid + `","branch":"main"},{"op":"api","method":"GET","path":"/zen"}]`,
		expect: []expectation{
			{error: "reading the refs of " + invalid + " failed"},
			{skipped: true},
		},
	},
	{
		name:   "malformed operations are refused before anything is sent",
		batch:  `[{"op":"push","repository":"` + invalid + `","commit":"abc","branch":"topic","expected":"absent"}]`,
		expect: []expectation{{error: "full commit hash"}},
	},
	{
		name:   "a push needs the lease",
		batch:  `[{"op":"push","repository":"` + invalid + `","commit":"` + unknown + `","branch":"topic"}]`,
		expect: []expectation{{error: "push needs expected"}},
	},
	{
		name:   "a remote with credentials in it is refused",
		batch:  `[{"op":"head","repository":"https://user:pw@caos-github-batch.invalid/repo.git","branch":"main"}]`,
		expect: []expectation{{error: "HTTPS URL without credentials"}},
	},
	{
		name:   "an unknown op",
		batch:  `[{"op":"merge"}]`,
		expect: []expectation{{error: "op must be api, head, push or import"}},
	},
	{
		name:   "an api call that is not well-formed",
		batch:  `[{"op":"api","method":"GET","path":"/repos/o/r/pulls","body":{"x":1}}]`,
		expect: []expectation{{error: "a GET takes no body"}},
	},
	{
		name:   "the server refuses a commit it does not hold, and that is final",
		batch:  `[{"op":"push","repository":"` + invalid + `","commit":"` + unknown + `","branch":"topic","expected":"absent"},{"op":"head","repository":"` + invalid + `","branch":"main"}]`,
		expect: []expectation{
			{fields: map[string]string{"status": "conflict", "kind": "validation-rejected", "commit": unknown, "expected": "absent"}},
			{skipped: true},
		},
	},
	{
		name:   "an import from an unreachable remote",
		batch:  `[{"op":"import","repository":"` + invalid + `","revision":"` + unknown + `"}]`,
		expect: []expectation{{error: "importing " + invalid + " at " + unknown + " failed"}},
	},
}

// next is this test curried for stage s, the `then` of a case's run.
func next(s int) string {
	return caos("curry", "--base:@=/cas/args/base", "--worker1:@=/cas/args/worker1",
		"--stage="+strconv.Itoa(s), "--test-salt:@=/cas/args/test-salt", "--github:@=/cas/args/github")
}

func launch(i int, salt string) {
	c := cases[i]
	w.Step(fmt.Sprintf("case %d: %s", i, c.name))
	request := caos("prepare-request", "--base:hash="+caos("hash", "/cas/args/github"),
		"--batch="+c.batch, fmt.Sprintf("--request-id=github-batch-%d-%s", i, salt))
	caos("run-request-then", request, "--then:hash="+next(i))
}

func check(i int) {
	c := cases[i]
	text := read("/cas/args/result")
	var results []map[string]any
	w.True(json.Unmarshal([]byte(text), &results) == nil, "case %q: the result is not a JSON array: %s", c.name, text)
	w.True(len(results) == len(c.expect), "case %q: %d results for %d expected: %s", c.name, len(results), len(c.expect), text)
	for j, want := range c.expect {
		got := results[j]
		skipped, _ := got["skipped"].(bool)
		ok, _ := got["ok"].(bool)
		message, _ := got["error"].(string)
		w.True(skipped == want.skipped, "case %q, operation %d: skipped is %v: %s", c.name, j, skipped, text)
		w.True(ok == want.ok, "case %q, operation %d: ok is %v: %s", c.name, j, ok, text)
		if want.error != "" {
			w.True(strings.Contains(message, want.error), "case %q, operation %d: error does not say %q: %s", c.name, j, want.error, text)
		}
		for key, value := range want.fields {
			w.True(fmt.Sprint(got[key]) == value, "case %q, operation %d: %s is %v, not %s: %s", c.name, j, key, got[key], value, text)
		}
	}
	fmt.Fprintf(os.Stderr, "  ok: %s\n", strings.Join(strings.Fields(text), " "))
}

func main() {
	w.Main(func() {
		salt := strings.TrimSpace(read("/cas/args/test-salt"))
		if _, err := os.Lstat("/cas/args/stage"); err != nil {
			launch(0, salt)
			return
		}
		i := w.Check(strconv.Atoi(strings.TrimSpace(read("/cas/args/stage"))))
		check(i)
		if i+1 < len(cases) {
			launch(i+1, salt)
			return
		}
		w.Report("github-batch: ALL PASS\n")
	})
}
