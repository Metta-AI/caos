// The `caos-stack` tool's worker: send one message to the test stack for this
// tree (dev/test-stack) and return its reply. A ROUTER, like std/caos-test's: the
// stack logic is the daemon's, in dev/test-stack/worker.sh; this addresses the
// message.
//
// `affinity` AND `in` BOTH NAME THE TREE. The server routes on the first and the
// daemon materializes the second, and the daemon refuses a message where they
// disagree, so a hand-formed one cannot reach a stack built from another tree.
//
// TWO STAGES, selected by `stage` (SPEC, "Worker scripts"):
//
//	route   (default) form the message and tail-call it. For `start` with a
//	        `cloud-env`, the callback is the next stage.
//	env     the `then` of a `start` (an exact-request tail call, so --result and
//	        no --in): --result is the daemon's reply, which names
//	        the stack's ticket and the dev commit it published. Make a NEW cloud
//	        environment whose setup installs that client and talks to that
//	        stack, by tail-calling `drive`, with the reply as its `in`.
//	done    the `then` of that: put the reply and the new environment's name.
//
// THE ENVIRONMENT IS MADE HERE AND NOT IN THE DAEMON, and that is a boundary. It
// takes the claude-oauth-token, which only `drive` is granted (by its code
// identity); the daemon runs the code under test, and a secret it could reach
// would be a secret the tested code could. `drive` is reached through its own
// image, so the grant is its own wherever it is called from.
//
// A NEW ENVIRONMENT EACH TIME, named for the dev commit it carries. A cloud
// environment re-runs its setup only when the text changes, and every start
// changes it, so updating one would only add a way for two stacks to step on each
// other; a throwaway per stack is cheaper than that, and delete-able by name.
package main

import (
	"fmt"
	"os"
	"os/exec"
	"strings"
	"time"

	"caos/w"
)

// run runs a command and returns its trimmed stdout, failing with its stderr.
func run(name string, args ...string) string {
	cmd := exec.Command(name, args...)
	var stderr strings.Builder
	cmd.Stderr = &stderr
	out, err := cmd.Output()
	w.True(err == nil, "%s %s: %v: %s", name, strings.Join(args, " "), err, strings.TrimSpace(stderr.String()))
	return strings.TrimSpace(string(out))
}

func caos(args ...string) string { return run("caos", args...) }

func has(name string) bool {
	_, err := os.Lstat("/cas/args/" + name)
	return err == nil
}

// arg reads a blob argument, trimmed.
func arg(name string) string {
	caos("get", "/cas/args/"+name)
	return strings.TrimSpace(string(w.Check(os.ReadFile("/cas/args/" + name))))
}

func opt(name string) string {
	if has(name) {
		return arg(name)
	}
	return ""
}

// field reads `key=value` out of a reply of such lines.
func field(reply, key string) string {
	for _, line := range strings.Split(reply, "\n") {
		if v, ok := strings.CutPrefix(line, key+"="); ok {
			return strings.TrimSpace(v)
		}
	}
	return ""
}

// The args every stage hands on, because a `then` receives only --in and
// --result and `/cas/args/base` is the IMAGE, not this job's ArgTree.
func next(stage string, extra ...string) string {
	args := []string{"curry", "--base:@=/cas/args/base", "--worker1:@=/cas/args/worker1",
		"--stage=" + stage, "--daemon:@=/cas/args/daemon", "--drive:@=/cas/args/drive"}
	for _, name := range []string{"bootstrap-base", "secret-readers"} {
		if has(name) {
			args = append(args, "--"+name+":@=/cas/args/"+name)
		}
	}
	return caos(append(args, extra...)...)
}

func main() {
	w.Main(func() {
		switch stage := opt("stage"); stage {
		case "", "route":
			route()
		case "env":
			makeEnv()
		case "done":
			done()
		default:
			w.True(false, "unknown --stage: %s", stage)
		}
	})
}

func put(text string) {
	w.Must(os.WriteFile("/tmp/out", []byte(text), 0o644))
	caos("put", "/tmp/out", "/cas/out")
}

func route() {
	// List the tree one level — enough to look for the codebase's own files.
	caos("get", "/cas/args/in")
	_ = exec.Command("caos", "get", "/cas/args/in/dev").Run()
	if _, err := os.Stat("/cas/args/in/flake.nix"); err != nil {
		put("caos-stack runs a caos dev stack built from the tree. This source tree is\n" +
			"not the caos codebase (no flake.nix / dev/stack-up), so there is no stack\n" +
			"for it to start or inspect.\ncaos-stack is specific to the caos codebase; run it there.\n")
		return
	}
	if _, err := os.Stat("/cas/args/in/dev/stack-up"); err != nil {
		put("caos-stack runs a caos dev stack built from the tree. This source tree is\n" +
			"not the caos codebase (no flake.nix / dev/stack-up), so there is no stack\n" +
			"for it to start or inspect.\ncaos-stack is specific to the caos codebase; run it there.\n")
		return
	}

	op := arg("op")
	switch op {
	case "start", "status", "logs", "harvest", "stop":
	default:
		put(fmt.Sprintf("unknown op %s: use one of start, status, logs, harvest, stop\n", op))
		return
	}

	tree := caos("hash", "/cas/args/in")
	args := []string{"prepare-request", "--base:@=/cas/args/daemon", "--affinity=" + tree,
		"--op=" + op, "--in:@=/cas/args/in"}
	for _, name := range []string{"request-id", "relay", "advertise", "log", "cursor", "harvest-refs"} {
		if has(name) {
			args = append(args, "--"+name+":@=/cas/args/"+name)
		}
	}
	request := caos(args...)

	if op == "start" && has("cloud-env") {
		caos("run-request-then", request, "--then:hash="+next("env"))
		return
	}
	caos("run-request-then", request)
}

// setupScript is a cloud environment's setup for a session that installs the
// client this stack published and talks to this stack. The first stage is the
// bootstrap from `bootstrap-base`, which then re-runs the dev tree's own setup
// (design/cloud-setup.md).
func setupScript(base, devCommit, ticket, readers string) string {
	line := "go run /tmp/caos-bootstrap.go --base=\"$B\" --enable-bash --dev-commit=" + devCommit
	if readers != "" {
		line += " \\\n  --secret-readers=" + readers
	}
	line += " \\\n  --server=" + ticket
	return "B=" + base + "\n" +
		"curl -fsSL \"$B/integrations/claude-code/cloud/bootstrap.go\" -o /tmp/caos-bootstrap.go\n" +
		line + "\n"
}

func makeEnv() {
	reply := arg("result")
	ticket, devCommit := field(reply, "ticket"), field(reply, "dev_commit")
	w.True(ticket != "" && devCommit != "", "the stack's reply names no ticket and dev_commit:\n%s", reply)
	base := opt("bootstrap-base")
	if base == "" {
		base = "https://raw.githubusercontent.com/Metta-AI/caos/main"
	}
	name := "z Caos Dev " + devCommit[:12]
	script := setupScript(base, devCommit, ticket, opt("secret-readers"))

	// `at` is the time, because a worker's result is memoized on its ArgTree and
	// creating an environment is not a pure function of its arguments.
	driveCall := caos("curry", "--base:@=/cas/args/drive", "--verb=env-create", "--env="+name,
		"--init-script="+script, fmt.Sprintf("--at=%d", time.Now().Unix()))
	caos("run-then", "/cas/args/result", "--run:hash="+driveCall, "--then:hash="+next("done"))
}

func done() {
	reply := arg("in")
	name := "z Caos Dev " + field(reply, "dev_commit")[:12]
	caos("get", "/cas/args/result")
	created := strings.TrimSpace(string(w.Check(os.ReadFile("/cas/args/result"))))
	put(reply + "\ncloud_env=" + name + "\n" + created + "\n")
}
