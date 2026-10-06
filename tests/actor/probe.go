// An IMPURE inner, for tests only: it does what kv.go does, after a side effect
// on the server that the test then observes. Real inners must be pure.
//
//	--race-ref=R   if R does not exist yet, push a competing commit to it, so the
//	               wrapper's leased push (which observed no head) loses the race
//	--count-ref=C  push one new commit to C per execution, so the number of
//	               commits on C is the number of times this inner actually ran
//	--twin-ref=T   if T does not exist yet, push to it the commit a DIFFERENT
//	               request with the same outcome would make: what `put k v`
//	               produces from no state, minted exactly as the wrapper mints
//	               it. The wrapper's own push is then refused for a commit that
//	               is not its own
package main

import (
	"bytes"
	"fmt"
	"math/rand"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
	"time"

	"caos/w"
)

func exists(path string) bool {
	_, err := os.Lstat(path)
	return err == nil
}

func optArg(name string) string {
	path := "/cas/args/" + name
	if !exists(path) {
		return ""
	}
	cmd := exec.Command("caos", "get", path)
	w.True(cmd.Run() == nil, "reading --%s", name)
	return strings.TrimSpace(string(w.Check(os.ReadFile(path))))
}

// git runs git in the scratch repository with stdin and returns trimmed stdout.
func git(stdin string, args ...string) string {
	cmd := exec.Command("git", append([]string{"-C", "/tmp/probe"}, args...)...)
	cmd.Env = append(os.Environ(), "GIT_TERMINAL_PROMPT=0")
	cmd.Stdin = strings.NewReader(stdin)
	var stderr bytes.Buffer
	cmd.Stderr = &stderr
	out, err := cmd.Output()
	w.True(err == nil, "git %s: %v: %s", strings.Join(args, " "), err, strings.TrimSpace(stderr.String()))
	return strings.TrimSpace(string(out))
}

func headOf(ref string) string {
	out := git("", "ls-remote", "--refs", "caos", ref)
	if out == "" {
		return ""
	}
	return strings.Fields(out)[0]
}

func main() {
	w.Main(func() {
		url := strings.TrimRight(os.Getenv("CAOS_SERVER_URL"), "/")
		w.True(url != "", "needs CAOS_SERVER_URL from the runner")
		raceRef, countRef, twinRef := optArg("race-ref"), optArg("count-ref"), optArg("twin-ref")

		w.Must(os.RemoveAll("/tmp/probe"))
		w.Must(os.MkdirAll("/tmp/probe", 0o755))
		git("", "init", "-q", ".")
		git("", "config", "user.email", "probe@caos")
		git("", "config", "user.name", "probe")
		git("", "config", "gc.auto", "0")
		git("", "remote", "add", "caos", url)

		if raceRef != "" && headOf(raceRef) == "" {
			blob := git("0\n", "hash-object", "-w", "--stdin")
			sub := git(fmt.Sprintf("100644 blob %s\tx\n", blob), "mktree")
			root := git(fmt.Sprintf("040000 tree %s\tstate\n", sub), "mktree")
			winner := git("", "commit-tree", root, "-m", "competing writer")
			git("", "push", "-q", "--force-with-lease="+raceRef+":", "caos", winner+":"+raceRef)
		}

		if twinRef != "" && headOf(twinRef) == "" {
			w.Must(execCmd("", "caos", "get", "/cas/args/message"))
			fields := strings.Fields(string(w.Check(os.ReadFile("/cas/args/message"))))
			w.True(len(fields) == 3 && fields[0] == "put", "--twin-ref needs a put message")
			blob := git(fields[2]+"\n", "hash-object", "-w", "--stdin")
			sub := git(fmt.Sprintf("100644 blob %s\t%s\n", blob, fields[1]), "mktree")
			root := git(fmt.Sprintf("040000 tree %s\tstate\n", sub), "mktree")
			twin := git("tree "+root+"\nauthor actor <actor@caos> 0 +0000\ncommitter actor <actor@caos> 0 +0000\n\nactor state\n",
				"hash-object", "-t", "commit", "-w", "--stdin")
			git("", "push", "-q", "--force-with-lease="+twinRef+":", "caos", twin+":"+twinRef)
		}

		if countRef != "" {
			empty := git("", "mktree")
			prior := headOf(countRef)
			msg := fmt.Sprintf("ran %d-%d", time.Now().UnixNano(), rand.Intn(32768))
			var run string
			if prior != "" {
				git("", "fetch", "-q", "caos", prior)
				run = git("", "commit-tree", empty, "-p", prior, "-m", msg)
			} else {
				run = git("", "commit-tree", empty, "-m", msg)
			}
			git("", "push", "-q", "--force-with-lease="+countRef+":"+prior, "caos", run+":"+countRef)
		}

		// Then behave as kv: build the --kv program as a command inside the
		// prelude module (this worker runs there, in /tmp/run) and run it.
		w.Must(execCmd("", "caos", "get", "/cas/args/kv"))
		dir := "/tmp/run/kvcmd"
		w.Must(os.RemoveAll(dir))
		w.Must(os.MkdirAll(dir, 0o755))
		w.Must(os.WriteFile(filepath.Join(dir, "main.go"), w.Check(os.ReadFile("/cas/args/kv")), 0o644))
		w.Must(execCmd("/tmp/run", "go", "run", "./kvcmd"))
	})
}

// execCmd runs a command with this worker's stdio, in dir ("" for the current one).
func execCmd(dir, name string, args ...string) error {
	cmd := exec.Command(name, args...)
	cmd.Dir = dir
	cmd.Stdout, cmd.Stderr = os.Stdout, os.Stderr
	return cmd.Run()
}
