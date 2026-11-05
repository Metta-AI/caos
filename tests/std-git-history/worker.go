// tests/std-git-history — a WORKER test of the history tools: log, show and diff over
// a source tree's git history, under the root contract (design/tool-contract.md).
//
// The source tree is a commit whose history this test makes: a first commit
// with `a.txt`, and a second that changes it and adds `b.txt`. The harness
// binds a source tree's commit as `root`; here the test binds it directly.
// Every answer is a tree: `stdout`, and `failed` on a mistake.
package main

import (
	"fmt"
	"io"
	"net/http"
	"net/url"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
	"time"

	"caos/w"
)

var (
	server string
	client = &http.Client{Timeout: 10 * time.Minute}
	seq    int
)

func run(name string, args ...string) string {
	out, err := exec.Command(name, args...).CombinedOutput()
	w.True(err == nil, "%s %s: %v: %s", name, strings.Join(args, " "), err, out)
	return strings.TrimSpace(string(out))
}

func write(path, text string) {
	w.Must(os.MkdirAll(filepath.Dir(path), 0o755))
	w.Must(os.WriteFile(path, []byte(text), 0o644))
}

// commit puts a tree holding `files` and a commit over it, and returns the
// commit's hash.
func commit(name, parent, message string, files map[string]string) string {
	dir := "/tmp/tree-" + name
	w.Must(os.RemoveAll(dir))
	for file, text := range files {
		write(filepath.Join(dir, file), text)
	}
	run("caos", "put", dir, "/cas/tree-"+name)
	text := "tree " + run("caos", "hash", "/cas/tree-"+name) + "\n"
	if parent != "" {
		text += "parent " + parent + "\n"
	}
	stamp := "1700000000"
	if parent != "" {
		stamp = "1700000100"
	}
	text += "author Tester <t@caos> " + stamp + " +0000\ncommitter Tester <t@caos> " + stamp + " +0000\n\n" + message + "\n"
	write("/tmp/commit-"+name, text)
	run("caos", "put-commit", "/tmp/commit-"+name, "/cas/commit-"+name)
	return run("caos", "hash", "/cas/commit-"+name)
}

// tool runs a history tool with `root` bound to an object (none for "") and
// returns its stdout and whether it failed. A param named with a `:hash`
// suffix binds an object, as the harness binds a `{commit}` parameter.
func tool(name, root string, params ...string) (text string, failed bool) {
	seq++
	form := url.Values{}
	form.Set("base:hash", run("caos", "hash", "/cas/args/"+name))
	if root != "" {
		form.Set("root:hash", root)
	}
	form.Set("salt", fmt.Sprintf("std-git-history-%d-%d", os.Getpid(), seq))
	for i := 0; i < len(params); i += 2 {
		form.Set(params[i], params[i+1])
	}
	resp, err := client.PostForm(server+"/eval/run", form)
	w.True(err == nil, "POST did not complete: %v", err)
	defer resp.Body.Close()
	body := strings.TrimSpace(string(w.Check(io.ReadAll(resp.Body))))
	w.True(resp.StatusCode == 200, "%s: HTTP %d: %s", name, resp.StatusCode, body)
	kind, hash, found := strings.Cut(body, " ")
	w.True(found && kind == "tree", "%s answered %q, not a result tree", name, body)
	dir := fmt.Sprintf("/cas/r-%d", seq)
	run("caos", "get-hash", hash, dir)
	run("caos", "get", dir)
	run("caos", "get", dir+"/stdout")
	_, err = os.Lstat(dir + "/failed")
	return string(w.Check(os.ReadFile(dir + "/stdout"))), err == nil
}

func ok(text string, failed bool) string {
	w.True(!failed, "the call failed: %s", text)
	return text
}

func lines(text string) []string { return strings.Split(strings.TrimSpace(text), "\n") }

func main() {
	w.Main(func() {
		server = strings.TrimRight(os.Getenv("CAOS_SERVER_URL"), "/")
		w.True(server != "", "this test needs CAOS_SERVER_URL from the runner")

		w.Step("a source tree with two commits, and an unrelated one")
		first := commit("one", "", "first", map[string]string{"a.txt": "one\n"})
		second := commit("two", first, "second", map[string]string{"a.txt": "two\n", "b.txt": "x\n"})
		other := commit("other", "", "other", map[string]string{"a.txt": "zero\n"})
		write("/tmp/notes.txt", "hello\n")
		run("caos", "put", "/tmp/notes.txt", "/cas/notes")
		notes := run("caos", "hash", "/cas/notes")

		w.Step("log: newest first, bounded, and by path")
		text := ok(tool("log", second))
		got := lines(text)
		w.True(len(got) == 2, "two commits expected: %q", text)
		w.True(strings.HasSuffix(got[0], "second") && strings.HasSuffix(got[1], "first"), "newest first: %q", text)
		w.True(strings.Contains(got[0], "Tester") && strings.Contains(got[0], "2023-11-14"), "author and date: %q", got[0])
		text = ok(tool("log", second, "count", "1"))
		w.True(len(lines(text)) == 1 && strings.HasSuffix(strings.TrimSpace(text), "second"), "count=1: %q", text)
		text = ok(tool("log", second, "path", "b.txt"))
		w.True(len(lines(text)) == 1 && strings.HasSuffix(strings.TrimSpace(text), "second"), "only the commit that added b.txt: %q", text)
		text = ok(tool("log", second, "rev", "HEAD~1"))
		w.True(len(lines(text)) == 1 && strings.HasSuffix(strings.TrimSpace(text), "first"), "from HEAD~1: %q", text)

		w.Step("show: a commit with its message and the diff it introduced")
		text = ok(tool("show", second))
		for _, want := range []string{second, "second", "diff --git a/a.txt", "-one", "+two", "b.txt"} {
			w.True(strings.Contains(text, want), "show lacks %q:\n%s", want, text)
		}
		text = ok(tool("show", second, "rev", first))
		w.True(strings.Contains(text, "first") && strings.Contains(text, "+one"), "show of the root commit by hash: %s", text)
		text = ok(tool("show", second, "path", "b.txt"))
		w.True(strings.Contains(text, "b.txt") && !strings.Contains(text, "a/a.txt"), "show scoped to a path: %s", text)

		w.Step("diff: the latest step by default, or any two revisions")
		text = ok(tool("diff", second))
		w.True(strings.Contains(text, "-one") && strings.Contains(text, "+two") && strings.Contains(text, "b.txt"), "default diff: %s", text)
		text = ok(tool("diff", second, "from", first, "to", second, "path", "b.txt"))
		w.True(strings.Contains(text, "+x") && !strings.Contains(text, "a.txt"), "diff scoped to a path: %s", text)
		text = ok(tool("diff", second, "to", first))
		w.True(strings.Contains(text, "no earlier revision"), "the root commit has no earlier revision: %s", text)

		w.Step("diff: against another source tree, even on an unrelated history")
		text = ok(tool("diff", second, "against:hash", other))
		w.True(strings.Contains(text, "-zero") && strings.Contains(text, "+two") && strings.Contains(text, "b.txt"), "against: %s", text)

		w.Step("mistakes are values the model can act on")
		for _, c := range []struct {
			name   string
			root   string
			params []string
			want   string
		}{
			{"log", "", nil, "nothing is at the root path"},
			{"log", notes, nil, "is not a source tree"},
			{"log", second, []string{"rev", "main"}, "not a revision this tool can resolve"},
			{"log", second, []string{"count", "x"}, "nonnegative integer"},
			{"diff", second, []string{"from", "sources/repo/00-base"}, "name another source tree to compare with as `against`"},
		} {
			text, failed := tool(c.name, c.root, c.params...)
			w.True(failed && strings.Contains(text, c.want),
				"%s %v: failed=%v %q (want %q)", c.name, c.params, failed, text, c.want)
		}
	})
}
