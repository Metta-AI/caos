// The actor wrapper (README.md): run an inner `(state, message) ->
// (state', reply)` request against state kept on a Git branch, and publish the
// new state with a compare-and-swap.
//
// `Q = actor { state-ref, inner, request-id, message }` has two positions:
//
//   - start reads the branch head (`caos ref-get`, then a depth-1 `tree:0`
//     fetch of that one commit), takes the `state/` subtree oid from the head,
//     builds the inner request and tail-calls it with Q (plus the observed head
//     and the input state) as the callback;
//   - finish receives the inner's `{state, reply}`. An unchanged state returns
//     the reply without touching Git; otherwise it mints `{state: <new oid>}`
//     as a commit on the observed head with `caos put-commit` and moves the
//     branch with a compare-and-swap. A lost race fails the request, which is
//     never cached, so the caller retries.
//
// Neither position checks the state out: it travels as a tree oid.
//
// THE BRANCH IS READ AND MOVED WITH `caos ref-get` AND `caos ref-set`, not with
// git. `ref-set --expected=<observed head>` is a compare-and-swap the server
// checks under the ref's lock, and the new commit is already on the server
// (`caos put-commit` put it there), so nothing is pushed: no scratch
// repository, no fetch of the parent, no history. `git push` cannot do this:
// it resolves the new commit in a local repository and walks its ancestry to
// build a pack, which needs every ancestor commit (README.md, open question
// 6). The wrapper used to speak receive-pack directly to the same effect;
// tests/actor-ref still proves that route works for git clients.
package main

import (
	"bytes"
	"os"
	"os/exec"
	"path/filepath"
	"strings"

	"caos/w"
)

const (
	stateEntry = "state"
	noHead     = "none"
	absent     = "absent"
	gitDir     = "/tmp/actor-git"
)

// run runs a command and returns its trimmed stdout, failing with its stderr.
func run(name string, args ...string) string {
	cmd := exec.Command(name, args...)
	cmd.Env = append(os.Environ(), "GIT_TERMINAL_PROMPT=0")
	var stderr bytes.Buffer
	cmd.Stderr = &stderr
	out, err := cmd.Output()
	w.True(err == nil, "%s %s: %v: %s", name, strings.Join(args, " "), err, strings.TrimSpace(stderr.String()))
	return strings.TrimSpace(string(out))
}

func caos(args ...string) string { return run("caos", args...) }

func exists(path string) bool {
	_, err := os.Lstat(path)
	return err == nil
}

// readArg fetches a blob argument and returns it trimmed.
func readArg(name string) string {
	path := "/cas/args/" + name
	caos("get", path)
	return strings.TrimSpace(string(w.Check(os.ReadFile(path))))
}

func serverURL() string {
	url := strings.TrimRight(os.Getenv("CAOS_SERVER_URL"), "/")
	w.True(url != "", "CAOS_SERVER_URL not set")
	return url
}

// readRef is the branch's head on the server, or "" if the branch is absent.
// `caos ref-get` records the head at the CAS path `at`, a fresh one per read.
func readRef(ref, at string) string {
	if w.RefGet(ref, at) {
		return caos("hash", at)
	}
	return ""
}

// stateOf is the `state/` subtree oid of head, reading only the commit and its
// root tree: a depth-1 `tree:0` fetch into a throwaway partial-clone repository.
// Start only reads, so cutting the history off is fine here.
func stateOf(head string) string {
	w.Must(os.RemoveAll(gitDir))
	run("git", "init", "-q", "--bare", gitDir)
	git := func(args ...string) string { return run("git", append([]string{"-C", gitDir}, args...)...) }
	git("config", "core.repositoryformatversion", "1")
	git("config", "extensions.partialClone", "origin")
	git("config", "remote.origin.url", serverURL())
	git("config", "remote.origin.promisor", "true")
	git("config", "remote.origin.partialclonefilter", "tree:0")
	git("fetch", "--quiet", "--no-tags", "--no-write-fetch-head", "--depth=1", "--filter=tree:0", "origin", head)
	for _, line := range strings.Split(git("ls-tree", head), "\n") {
		// "040000 tree <oid>\t<name>"
		meta, name, ok := strings.Cut(line, "\t")
		if ok && name == stateEntry {
			return strings.Fields(meta)[2]
		}
	}
	return ""
}

// emptyState is the empty tree, as a CAS path (so it can be bound by path).
func emptyState() string {
	dir := "/tmp/actor-empty-state"
	w.Must(os.RemoveAll(dir))
	w.Must(os.MkdirAll(dir, 0o755))
	caos("put", dir, "/cas/empty-state")
	return "/cas/empty-state"
}

func main() {
	w.Main(func() {
		stateRef := readArg("state-ref")
		w.True(strings.HasPrefix(stateRef, "refs/heads/actors/") && !strings.Contains(stateRef, ".."),
			"state-ref %q must be under refs/heads/actors/", stateRef)
		if exists("/cas/args/result") {
			finish(stateRef)
		} else {
			start(stateRef)
		}
	})
}

func start(stateRef string) {
	head := readRef(stateRef)
	statePath, stateOid := "", ""
	if head != "" {
		stateOid = stateOf(head)
	}
	if stateOid != "" {
		caos("get-hash", stateOid, "/cas/state")
		statePath = "/cas/state"
	} else {
		statePath = emptyState()
		stateOid = caos("hash", statePath)
	}

	request := caos("prepare-request", "--base:@=/cas/args/inner",
		"--state:@="+statePath, "--message:@=/cas/args/message")

	// The callback is this same Q, carrying what finish needs to publish.
	q := caos("hash", "/cas/args")
	headText := head
	if headText == "" {
		headText = noHead
	}
	callback := caos("curry", "--base:hash="+q, "--head="+headText, "--old-state="+stateOid)
	caos("run-request-then", request, "--then:hash="+callback)
}

func finish(stateRef string) {
	result := "/cas/args/result"
	// List the result's children as hash-tagged entries; nothing is downloaded.
	caos("get", result)
	newState := caos("hash", filepath.Join(result, stateEntry))
	if newState != readArg("old-state") {
		head := readArg("head")
		if head == noHead {
			head = ""
		}
		publish(stateRef, head, newState)
	}
	caos("forward", filepath.Join(result, "reply"), "/cas/out")
}

// publish mints the commit {state: newState} on head and moves the branch.
func publish(stateRef, head, newState string) {
	// The root tree {state: <newState>}: a symlink to the already-fetched
	// result entry, which `caos put` resolves to its recorded hash.
	root := "/tmp/actor-root"
	w.Must(os.RemoveAll(root))
	w.Must(os.MkdirAll(root, 0o755))
	w.Must(os.Symlink("/cas/args/result/"+stateEntry, filepath.Join(root, stateEntry)))
	caos("put", root, "/cas/new-root")
	tree := caos("hash", "/cas/new-root")

	text := "tree " + tree + "\n"
	if head != "" {
		text += "parent " + head + "\n"
	}
	text += "author actor <actor@caos> 0 +0000\ncommitter actor <actor@caos> 0 +0000\n\nactor state\n"
	w.Must(os.WriteFile("/tmp/actor-commit", []byte(text), 0o644))
	candidate := caos("put-commit", "/tmp/actor-commit", "/cas/new-commit")

	old := head
	if old == "" {
		old = absent
	}
	complaint, err := setRef(stateRef, old, candidate)
	if err == nil {
		return
	}
	// Ambiguous or refused: re-read the ref to learn what actually happened.
	switch observed := readRef(stateRef); {
	case observed == candidate:
		return
	case observed == head:
		w.True(false, "moving %s: %v: %s", stateRef, err, complaint)
	default:
		w.True(false, "lost the race for %s: %v: %s", stateRef, err, complaint)
	}
}

// setRef moves ref from old to new with a compare-and-swap, returning the
// server's complaint rather than failing: a refusal (a stale old value) and an
// ambiguous outcome (the reply lost) are told apart by re-reading the ref.
func setRef(ref, old, new string) (string, error) {
	cmd := exec.Command("caos", "ref-set", ref, new, "--expected="+old)
	var stderr bytes.Buffer
	cmd.Stderr = &stderr
	err := cmd.Run()
	return strings.TrimSpace(stderr.String()), err
}
