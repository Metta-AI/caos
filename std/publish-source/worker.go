// The `publish-source` tool's worker. Its docs live in the sibling
// `.caos-expr` here-string, not in this header (SPEC, "CaosTools").
//
// Publish a source tree's exact commit to a branch (design/agent-publish.md),
// as an ordinary worker (design/tool-contract.md): it holds no token, and
// pushes through its neighbour `DEEP-DEPS/github`, the one image the token is
// granted to. Nothing is recorded before the push: a call that does not
// finish is answered as unfinished and never run again behind the agent's
// back, so the lease is what keeps a push safe.
//
//   - Without force: read the branch's head, then push leased on it. Only a
//     create or a fast-forward can pass the server's ancestry check, so a
//     head that moves in between makes the push fail, never overwrite.
//   - With force: the caller passes `expected`, the head it looked at, and the
//     push is leased on exactly that.
//
// Stages, by --stage: none (check the call; read the head, or push), `push`
// (push leased on the head just read), `done` (render the receipt).
package main

import (
	"encoding/json"
	"fmt"
	"os"
	"path/filepath"
	"regexp"
	"strings"

	"caos/w"
)

type refusal struct{ msg string }

func refuse(format string, a ...any) { panic(refusal{fmt.Sprintf(format, a...)}) }

func arg(name string) string {
	path := "/cas/args/" + name
	if !w.Exists(path) {
		return ""
	}
	return strings.TrimSpace(w.Read(path))
}

func flag(name string) bool {
	switch strings.ToLower(arg(name)) {
	case "", "false", "0", "no":
		return false
	case "true", "1", "yes":
		return true
	}
	refuse("%s is true or false, not %q", name, arg(name))
	return false
}

// answer is the result under the root contract: `{stdout, failed?}`. A reader:
// publishing changes nothing in the session.
func answer(text string, failed bool) {
	const dir = "/tmp/publish-out"
	w.Must(os.MkdirAll(dir, 0o755))
	w.Must(os.WriteFile(filepath.Join(dir, "stdout"), []byte(text), 0o644))
	if failed {
		w.Must(os.WriteFile(filepath.Join(dir, "failed"), []byte("publish-source failed\n"), 0o644))
	}
	w.Put(dir, "/cas/out")
}

var hash = regexp.MustCompile(`^[0-9a-fA-F]{40}$`)

// run hands std/github a batch, and names the next stage of this worker as
// its `then`, carrying what that stage needs.
func run(stage string, ops []map[string]any, carry ...string) {
	batch := w.Check(json.Marshal(ops))
	id := arg("request-id")
	request := w.Caos("prepare-request", "--base:@=/cas/args/github", "--batch="+string(batch),
		"--request-id="+id+"-"+stage)
	args := []string{"curry", "--base:@=/cas/args/base", "--worker1:@=/cas/args/worker1",
		"--github:@=/cas/args/github", "--stage=" + stage, "--request-id=" + id}
	for _, name := range []string{"commit", "repository", "branch", "force", "allow-ignored"} {
		args = append(args, "--"+name+"="+arg(name))
	}
	args = append(args, carry...)
	w.Caos("run-request-then", request, "--then:hash="+w.Caos(args...))
}

func push(expected string) []map[string]any {
	return []map[string]any{{
		"op": "push", "repository": arg("repository"), "branch": arg("branch"),
		"commit": arg("commit"), "expected": expected,
		"force": flag("force"), "allow_ignored": flag("allow-ignored"),
	}}
}

// result reads std/github's one result, refusing an operation that could not
// be done at all.
func result() map[string]any {
	var results []map[string]any
	text := w.Read("/cas/args/result")
	if json.Unmarshal([]byte(text), &results) != nil || len(results) != 1 {
		refuse("GitHub's answer was not one result: %s", text)
	}
	if message, _ := results[0]["error"].(string); message != "" {
		refuse("%s", message)
	}
	return results[0]
}

func start() {
	const root = "/cas/args/root"
	if !w.Exists(root) {
		refuse("nothing is at the root path: root is the source tree to publish, such as sources/repo/01-layer")
	}
	if k := w.Caos("kind", root); k != "commit" {
		refuse("the root is a %s, not a source tree: publish-source pushes a source tree's commit", k)
	}
	commit := w.Caos("hash", root)
	if arg("repository") == "" || arg("branch") == "" {
		refuse("repository and branch are required")
	}
	if strings.HasPrefix(arg("branch"), "refs/") {
		refuse("branch must omit refs/heads/")
	}
	flag("allow-ignored")
	// The commit rides along under its own name from here on.
	if flag("force") {
		expected := arg("expected")
		if expected != "absent" && !hash.MatchString(expected) {
			refuse("a force push needs expected: the branch's head as you saw it (a full commit hash), or `absent` for a new branch. Read it with caos-std/github, op head.")
		}
		runWithCommit(commit, "done", push2(commit, strings.ToLower(expected)))
		return
	}
	runWithCommit(commit, "push", []map[string]any{{
		"op": "head", "repository": arg("repository"), "branch": arg("branch"),
	}})
}

// push2 is `push` for the first stage, where the commit is not yet an arg.
func push2(commit, expected string) []map[string]any {
	ops := push(expected)
	ops[0]["commit"] = commit
	return ops
}

// runWithCommit is `run` from the first stage, binding the commit it found.
func runWithCommit(commit, stage string, ops []map[string]any) {
	batch := w.Check(json.Marshal(ops))
	id := arg("request-id")
	request := w.Caos("prepare-request", "--base:@=/cas/args/github", "--batch="+string(batch),
		"--request-id="+id+"-"+stage)
	args := []string{"curry", "--base:@=/cas/args/base", "--worker1:@=/cas/args/worker1",
		"--github:@=/cas/args/github", "--stage=" + stage, "--request-id=" + id, "--commit=" + commit}
	for _, name := range []string{"repository", "branch", "force", "allow-ignored"} {
		args = append(args, "--"+name+"="+arg(name))
	}
	w.Caos("run-request-then", request, "--then:hash="+w.Caos(args...))
}

// receipt renders a push's result: what was pushed where, and how it ended.
func receipt(r map[string]any) {
	var out strings.Builder
	status, _ := r["status"].(string)
	fmt.Fprintf(&out, "%s: %s %s to %s %s\n", status, r["kind"], arg("commit"), arg("repository"), arg("branch"))
	fmt.Fprintf(&out, "leased on %v, observed %v\n", r["expected"], r["observed"])
	if d, _ := r["diagnostic"].(string); d != "" {
		out.WriteString(d + "\n")
	}
	if status == "uncertain" {
		out.WriteString("The push may still have landed: read the branch (caos-std/github, op head) before acting again.\n")
	}
	answer(out.String(), status != "complete")
}

func main() {
	w.Main(func() {
		defer func() {
			r := recover()
			if r == nil {
				return
			}
			no, ok := r.(refusal)
			if !ok {
				panic(r)
			}
			answer("publish-source: "+no.msg+"\n", true)
		}()
		switch arg("stage") {
		case "":
			start()
		case "push":
			head, _ := result()["head"].(string)
			expected := head
			if expected == "" {
				expected = "absent"
			}
			run("done", push(expected))
		case "done":
			receipt(result())
		default:
			refuse("unknown stage %q", arg("stage"))
		}
	})
}
