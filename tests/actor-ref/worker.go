// tests/actor-ref — SPIKE for design/actors.md, open question 6.
//
// Can a worker move a branch with a compare-and-swap WITHOUT any scratch
// repository and without fetching any history? A git push is a command line
// "<old> <new> <ref>" plus a pack, and the pack may be empty if the server
// already has the new object. `caos put-commit` puts the commit on the server
// without a push, so this speaks git-receive-pack directly with an empty pack.
//
// Proves, against the test stack:
//  1. creating a ref (old = zeros) at a commit made by `caos put-commit`;
//  2. updating it with the right <old> to a child commit;
//  3. a stale <old> is REJECTED and the ref does not move;
//  4. the result is a normal branch: git can fetch it and see both commits.
package main

import (
	"bytes"
	"crypto/sha1"
	"fmt"
	"io"
	"net/http"
	"os"
	"os/exec"
	"strings"

	"caos/w"
)

const zeros = "0000000000000000000000000000000000000000"

func run(name string, args ...string) string {
	cmd := exec.Command(name, args...)
	cmd.Env = append(os.Environ(), "GIT_TERMINAL_PROMPT=0")
	var stderr bytes.Buffer
	cmd.Stderr = &stderr
	out, err := cmd.Output()
	w.True(err == nil, "%s %s: %v: %s", name, strings.Join(args, " "), err, strings.TrimSpace(stderr.String()))
	return strings.TrimSpace(string(out))
}

func pkt(s string) string { return fmt.Sprintf("%04x%s", len(s)+4, s) }

// emptyPack is a valid pack holding no objects: "PACK", version 2, count 0,
// and the SHA-1 of those twelve bytes.
func emptyPack() []byte {
	header := []byte("PACK\x00\x00\x00\x02\x00\x00\x00\x00")
	sum := sha1.Sum(header)
	return append(header, sum[:]...)
}

// setRef asks git-receive-pack to move ref from old to new, sending no
// objects. It returns the server's status lines.
func setRef(url, ref, old, new string) string {
	var body bytes.Buffer
	body.WriteString(pkt(fmt.Sprintf("%s %s %s\x00 report-status agent=caos-spike\n", old, new, ref)))
	body.WriteString("0000")
	body.Write(emptyPack())
	req := w.Check(http.NewRequest("POST", url+"/git-receive-pack", &body))
	req.Header.Set("Content-Type", "application/x-git-receive-pack-request")
	req.Header.Set("Accept", "application/x-git-receive-pack-result")
	resp := w.Check(http.DefaultClient.Do(req))
	defer resp.Body.Close()
	text := string(w.Check(io.ReadAll(resp.Body)))
	w.True(resp.StatusCode == 200, "git-receive-pack answered %s: %s", resp.Status, text)
	return text
}

// commit mints a commit over a state subtree holding v=<value>, via
// `caos put` and `caos put-commit`, and returns its hash. Nothing is pushed.
func commit(tag, value, parent string) string {
	dir := "/tmp/root-" + tag
	w.Must(os.RemoveAll(dir))
	w.Must(os.MkdirAll(dir+"/state", 0o755))
	w.Must(os.WriteFile(dir+"/state/v", []byte(value+"\n"), 0o644))
	run("caos", "put", dir, "/cas/root-"+tag)
	tree := run("caos", "hash", "/cas/root-"+tag)
	text := "tree " + tree + "\n"
	if parent != "" {
		text += "parent " + parent + "\n"
	}
	text += "author actor <actor@caos> 0 +0000\ncommitter actor <actor@caos> 0 +0000\n\nactor state " + tag + "\n"
	file := "/tmp/commit-" + tag
	w.Must(os.WriteFile(file, []byte(text), 0o644))
	return run("caos", "put-commit", file, "/cas/commit-"+tag)
}

func remoteHead(url, ref string) string {
	out := run("git", "ls-remote", "--refs", url, ref)
	if out == "" {
		return ""
	}
	return strings.Fields(out)[0]
}

func main() {
	w.Main(func() {
		url := strings.TrimRight(os.Getenv("CAOS_SERVER_URL"), "/")
		w.True(url != "", "this test needs CAOS_SERVER_URL from the runner")
		run("caos", "get", "/cas/args/test-salt")
		salt := strings.TrimSpace(string(w.Check(os.ReadFile("/cas/args/test-salt"))))
		ref := fmt.Sprintf("refs/heads/actors/ref-%s-%d", salt, os.Getpid())

		w.Step("mint two commits with caos put-commit (no push)")
		c1 := commit("one", "1", "")
		c2 := commit("two", "2", c1)
		w.True(remoteHead(url, ref) == "", "fresh ref already exists")

		w.Step("create the ref with an empty pack")
		resp := setRef(url, ref, zeros, c1)
		fmt.Fprintf(os.Stderr, "create response: %q\n", resp)
		w.True(strings.Contains(resp, "ok "+ref), "create was not accepted: %q", resp)
		w.True(remoteHead(url, ref) == c1, "ref is %q, want %s", remoteHead(url, ref), c1)

		w.Step("a STALE <old> is rejected and the ref does not move")
		resp = setRef(url, ref, zeros, c2)
		fmt.Fprintf(os.Stderr, "stale response: %q\n", resp)
		w.True(strings.Contains(resp, "ng "+ref), "a stale <old> was not rejected: %q", resp)
		w.True(remoteHead(url, ref) == c1, "the ref moved on a stale update")

		w.Step("update with the right <old>")
		resp = setRef(url, ref, c1, c2)
		fmt.Fprintf(os.Stderr, "update response: %q\n", resp)
		w.True(strings.Contains(resp, "ok "+ref), "update was not accepted: %q", resp)
		w.True(remoteHead(url, ref) == c2, "ref is %q, want %s", remoteHead(url, ref), c2)

		w.Step("it is an ordinary branch")
		w.Must(os.RemoveAll("/tmp/check"))
		run("git", "init", "-q", "--bare", "/tmp/check")
		run("git", "-C", "/tmp/check", "fetch", "-q", url, ref)
		w.True(run("git", "-C", "/tmp/check", "rev-list", "--count", "FETCH_HEAD") == "2", "history is not two commits")
		w.True(run("git", "-C", "/tmp/check", "show", "FETCH_HEAD:state/v") == "2", "state/v is not 2")
		w.True(run("git", "-C", "/tmp/check", "show", "FETCH_HEAD~1:state/v") == "1", "parent state/v is not 1")

		w.Report("actor-ref: ALL PASS\n")
	})
}
