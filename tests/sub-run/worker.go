// tests/sub-run — a WORKER test: no client, no repo.
//
// `sub-run` dispatches a request and returns at once. Two claims:
//
//   - A worker may sub-run its OWN in-flight ArgTree. A blocking
//     implementation would wait on itself and deadlock; the server admits the
//     request under the current stack instead, where the recursive edge fails
//     on its own.
//   - Work dispatched that way keeps running AFTER ITS CALLER EXITS.
//
// HOW THE SECOND CLAIM IS OBSERVED, given there is deliberately no caller to
// ask: a HANDSHAKE ON TWO REFS, ordered by cause rather than by the clock.
//
//  1. The test runs a launcher, A, which sub-runs a detached job, B, and exits.
//  2. The test's next stage is A's `then`, so it runs only once A's container
//     is gone. Only then does it set the ref `go`.
//  3. B waits for `go` with `caos ref-then`, then sets the ref `done`.
//  4. The test waits for `done` with `caos ref-then`.
//
// B can only see `go` after A has exited, so B answering proves it was alive
// and running after its caller was gone. It also catches a `sub-run` that
// secretly blocked: A would wait on B, B on a ref that appears only after A
// exits, and the test would time out.
//
// An earlier version waited a fixed 8 seconds on an unrelated job and then
// looked once. On a busy suite B could still be queued when the 8 seconds were
// up, so the test failed with nothing wrong. Here the only clock is a failure
// deadline (`--timeout`), which a healthy run never reaches.
//
// NOTHING WAITS IN A CONTAINER. `ref-then` is a continuation: the worker that
// records it exits and the server holds the wait, so neither B nor the test
// occupies a runner slot while the other runs (design/refs.md). Polling with
// `ref-get` in a loop would be the hold-and-wait design/map-then.md rules out.
//
// B also sets `alive` before it waits, so a timeout can say whether B never
// ran or ran and never saw `go`. Every ref is under a run-unique prefix
// (tests/README.md) and is deleted at the end, pass or fail.
//
// One program plays every part, chosen by --role (none: the test itself), and
// the test is staged: no run can be waited on, so each check is the `then` of
// the dispatch it is about.
package main

import (
	"bytes"
	"context"
	"fmt"
	"os"
	"os/exec"
	"regexp"
	"strings"
	"time"

	"caos/w"
)

const absent = "absent"

// How long B waits for `go`, and the test for `done`. Failure deadlines only:
// on a healthy run both answer as soon as the other side writes.
const (
	detachedWait = "180"
	testWait     = "240"
)

func has(name string) bool { return w.Exists("/cas/args/" + name) }

func arg(name string) string { return strings.TrimSpace(w.Read("/cas/args/" + name)) }

// request forms a run of this same program, in the Go image this test runs
// in, playing role.
func request(role string, extra ...string) string {
	args := []string{"prepare-request", "--base:hash=" + w.Hash("/cas/args/base"),
		"--worker1:@=/cas/args/worker1", "--role=" + role}
	return w.Caos(append(args, extra...)...)
}

// next is the test curried for its next stage: the `then` of a dispatch.
func next(stage string, extra ...string) string {
	args := []string{"curry", "--base:@=/cas/args/base", "--worker1:@=/cas/args/worker1",
		"--stage=" + stage, "--test-salt:@=/cas/args/test-salt"}
	return w.Caos(append(args, extra...)...)
}

// The handshake's refs, under a prefix unique to this run.
func ref(nonce, name string) string { return "refs/heads/tests/sub-run/" + nonce + "/" + name }

// mint stores an empty-tree commit labelled for this run, and returns the CAS
// path it is recorded at: a ref under refs/heads/ names a commit.
func mint(nonce, label string) string {
	dir := "/tmp/empty"
	w.Must(os.MkdirAll(dir, 0o755))
	w.Put(dir, "/cas/empty")
	body := fmt.Sprintf("tree %s\nauthor sub-run <sub-run@caos> 0 +0000\n"+
		"committer sub-run <sub-run@caos> 0 +0000\n\nsub-run %s %s\n", w.Hash("/cas/empty"), nonce, label)
	w.Must(os.WriteFile("/tmp/commit", []byte(body), 0o644))
	path := "/cas/commit-" + label
	w.Caos("put-commit", "/tmp/commit", path)
	return path
}

// create sets a ref that must not exist yet, to a commit passed by its CAS
// path.
func create(nonce, name, label string) {
	w.Caos("ref-set", ref(nonce, name), "--new:@="+mint(nonce, label), "--expected="+absent)
}

// delivered is the commit a `ref-then` handed this stage as --result, checked
// to be one: a deletion would have come as --absent instead.
func delivered(name string) string {
	w.True(!has("absent"), "ref-then on %s delivered a deletion", name)
	kind := w.Caos("kind", "/cas/args/result")
	w.True(kind == "commit", "ref-then on %s delivered a %s, not a commit", name, kind)
	return w.Hash("/cas/args/result")
}

var requestReply = regexp.MustCompile(`^request [0-9a-f]{40}$`)

// dispatched is the request a `sub-run` reply names, checked for shape.
func dispatched(reply string) string {
	w.True(requestReply.MatchString(reply), "expected 'request <40-character hash>', got %q", reply)
	return strings.TrimPrefix(reply, "request ")
}

func main() {
	w.Main(func() {
		switch {
		case !has("role"):
			test()
		case arg("role") == "self":
			self()
		case arg("role") == "launch":
			launch()
		case arg("role") == "detached":
			detached()
		default:
			w.True(false, "unknown --role %q", arg("role"))
		}
	})
}

// self sub-runs the very request it is running as.
func self() {
	q := w.Hash("/cas/args")
	// A blocking implementation would wait on this worker's own result. Bound
	// the regression so a broken one fails promptly instead of hanging.
	ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
	defer cancel()
	cmd := exec.CommandContext(ctx, "caos", "sub-run", q)
	var stderr bytes.Buffer
	cmd.Stderr = &stderr
	out, err := cmd.Output()
	w.True(err == nil, "sub-run of its own request %s: %v: %s", q, err, strings.TrimSpace(stderr.String()))
	reply := strings.TrimSpace(string(out))
	w.True(reply == "request "+q, "sub-run returned %q, expected %q", reply, "request "+q)
	w.Report(reply + "\n")
}

// launch is A: dispatch B and exit, leaving nobody waiting for it.
func launch() {
	b := request("detached", "--nonce="+arg("nonce"))
	reply := w.Caos("sub-run", b)
	w.True(reply == "request "+b, "sub-run returned %q, expected %q", reply, "request "+b)
	w.Report(reply + "\n")
}

// detached is B. First: say it is alive, then wait for `go` without holding a
// slot. Then, as that wait's `then`: answer on `done`.
func detached() {
	nonce := arg("nonce")
	if !has("result") {
		create(nonce, "alive", "alive")
		then := w.Caos("curry", "--base:@=/cas/args/base", "--worker1:@=/cas/args/worker1",
			"--role=detached", "--nonce="+nonce)
		w.Caos("ref-then", ref(nonce, "go"), "--expected="+absent, "--timeout="+detachedWait,
			"--then:hash="+then)
		return
	}
	seen := arg("result")
	w.True(seen != absent, "ref-then on %s delivered %q", ref(nonce, "go"), seen)
	create(nonce, "done", "done")
	w.Report("detached: saw go at " + seen + ", answered on done\n")
}

// cleanup deletes every ref this run may have made, without failing: it runs
// on the way out of a failure too.
func cleanup(nonce string) {
	for _, name := range []string{"go", "alive", "done"} {
		value, err := exec.Command("caos", "ref-get", ref(nonce, name)).Output()
		v := strings.TrimSpace(string(value))
		if err != nil || v == absent {
			continue
		}
		_ = exec.Command("caos", "ref-set", ref(nonce, name), absent, "--expected="+v).Run()
	}
}

// nonceFor is a prefix unique to this run and legal in a ref name. The salt
// alone is not enough: an unsalted run has none, and suite runs can share a
// stack (tests/README.md).
func nonceFor(salt string) string {
	raw := fmt.Sprintf("%s-%d-%d", salt, time.Now().UnixNano(), os.Getpid())
	return regexp.MustCompile(`[^A-Za-z0-9-]+`).ReplaceAllString(raw, "-")
}

func test() {
	stage := "start"
	if has("stage") {
		stage = arg("stage")
	}
	nonce := ""
	if has("nonce") {
		nonce = arg("nonce")
	}
	switch stage {
	case "start":
		nonce = nonceFor(arg("test-salt"))
		w.Step("sub-run dispatches an in-flight request without waiting")
		// `run-request-then` runs the ArgTree unchanged, so the hash `self`
		// computes is the request it is already running as.
		w.Caos("run-request-then", request("self", "--nonce="+nonce),
			"--then:hash="+next("selfed", "--nonce="+nonce))

	case "selfed":
		q := dispatched(arg("result"))
		fmt.Fprintf(os.Stderr, "  ok: the worker continued after dispatching its own request (%s)\n", q)

		w.Step("dispatched work keeps running after its caller exits")
		w.Caos("run-request-then", request("launch", "--nonce="+nonce),
			"--then:hash="+next("launched", "--nonce="+nonce))

	case "launched":
		// This stage is the launcher's `then`: its container has exited.
		b := dispatched(arg("result"))
		fmt.Fprintf(os.Stderr, "  launcher dispatched %s and exited\n", b)
		create(nonce, "go", "go")
		w.Caos("ref-then", ref(nonce, "done"), "--expected="+absent, "--timeout="+testWait, "--catch",
			"--then:hash="+next("answered", "--nonce="+nonce, "--detached="+b))

	case "answered":
		b := arg("detached")
		if has("error") {
			why := arg("error")
			alive := w.Caos("ref-get", ref(nonce, "alive"))
			cleanup(nonce)
			w.True(alive != absent, "the detached request %s never ran: it never set %s (%s)",
				b, ref(nonce, "alive"), why)
			w.True(false, "the detached request %s ran but never answered on %s (%s)",
				b, ref(nonce, "done"), why)
		}
		done := arg("result")
		cleanup(nonce)
		w.True(done != absent, "ref-then on %s delivered %q", ref(nonce, "done"), done)
		fmt.Fprintf(os.Stderr, "  ok: %s saw go only after its caller exited, and answered (%s)\n", b, done)
		w.Report("sub-run: ALL PASS\n")

	default:
		w.True(false, "unknown --stage %q", stage)
	}
}
