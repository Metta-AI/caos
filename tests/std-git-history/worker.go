// tests/std-git-history — a WORKER test of the history tools: log, show and diff over
// a source tree's git history.
//
// The source tree is a commit entry (`feature/dirty`) whose history this test
// makes: a first commit with `a.txt`, and a second that changes it and adds
// `b.txt`. The tools take the tree's PATH and find the commit the entry stores.
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

// tool runs a history tool over a tree and returns what it answered: the text of a
// blob, or for a tree its `report`.
func tool(name, tree string, params ...string) (kind, text string) {
	seq++
	form := url.Values{}
	form.Set("base:hash", run("caos", "hash", "/cas/args/"+name))
	form.Set("in:hash", tree)
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
	w.True(found, "%s answered %q", name, body)
	dir := fmt.Sprintf("/cas/r-%d", seq)
	run("caos", "get-hash", hash, dir)
	run("caos", "get", dir)
	if kind == "blob" {
		return kind, string(w.Check(os.ReadFile(dir)))
	}
	run("caos", "get", dir+"/report")
	return kind, string(w.Check(os.ReadFile(dir + "/report")))
}

func lines(text string) []string { return strings.Split(strings.TrimSpace(text), "\n") }

func main() {
	w.Main(func() {
		server = strings.TrimRight(os.Getenv("CAOS_SERVER_URL"), "/")
		w.True(server != "", "this test needs CAOS_SERVER_URL from the runner")

		w.Step("a source tree with two commits, in a conversation tree")
		first := commit("one", "", "first", map[string]string{"a.txt": "one\n"})
		second := commit("two", first, "second", map[string]string{"a.txt": "two\n", "b.txt": "x\n"})
		write("/tmp/ws/notes.txt", "hello\n")
		w.Must(os.MkdirAll("/tmp/ws/feature", 0o755))
		w.Must(os.Symlink("/cas/commit-two", "/tmp/ws/feature/dirty"))
		commit("other", "", "other", map[string]string{"a.txt": "zero\n"})
		w.Must(os.Symlink("/cas/commit-other", "/tmp/ws/feature/base"))
		run("caos", "put", "/tmp/ws", "/cas/ws")
		ws := run("caos", "hash", "/cas/ws")

		w.Step("log: newest first, bounded, and by path")
		kind, text := tool("log", ws, "source-tree", "feature/dirty")
		w.True(kind == "blob", "log answers a blob, got a %s: %s", kind, text)
		got := lines(text)
		w.True(len(got) == 2, "two commits expected: %q", text)
		w.True(strings.HasSuffix(got[0], "second") && strings.HasSuffix(got[1], "first"), "newest first: %q", text)
		w.True(strings.Contains(got[0], "Tester") && strings.Contains(got[0], "2023-11-14"), "author and date: %q", got[0])
		_, text = tool("log", ws, "source-tree", "feature/dirty", "count", "1")
		w.True(len(lines(text)) == 1 && strings.HasSuffix(strings.TrimSpace(text), "second"), "count=1: %q", text)
		_, text = tool("log", ws, "source-tree", "feature/dirty", "path", "b.txt")
		w.True(len(lines(text)) == 1 && strings.HasSuffix(strings.TrimSpace(text), "second"), "only the commit that added b.txt: %q", text)
		_, text = tool("log", ws, "source-tree", "feature/dirty", "rev", "HEAD~1")
		w.True(len(lines(text)) == 1 && strings.HasSuffix(strings.TrimSpace(text), "first"), "from HEAD~1: %q", text)

		w.Step("show: a commit with its message and the diff it introduced")
		_, text = tool("show", ws, "source-tree", "feature/dirty")
		for _, want := range []string{second, "second", "diff --git a/a.txt", "-one", "+two", "b.txt"} {
			w.True(strings.Contains(text, want), "show lacks %q:\n%s", want, text)
		}
		_, text = tool("show", ws, "source-tree", "feature/dirty", "rev", first)
		w.True(strings.Contains(text, "first") && strings.Contains(text, "+one"), "show of the root commit by hash: %s", text)
		_, text = tool("show", ws, "source-tree", "feature/dirty", "path", "b.txt")
		w.True(strings.Contains(text, "b.txt") && !strings.Contains(text, "a/a.txt"), "show scoped to a path: %s", text)

		w.Step("diff: the latest step by default, or any two revisions")
		_, text = tool("diff", ws, "source-tree", "feature/dirty")
		w.True(strings.Contains(text, "-one") && strings.Contains(text, "+two") && strings.Contains(text, "b.txt"), "default diff: %s", text)
		_, text = tool("diff", ws, "source-tree", "feature/dirty", "from", first, "to", second, "path", "b.txt")
		w.True(strings.Contains(text, "+x") && !strings.Contains(text, "a.txt"), "diff scoped to a path: %s", text)
		_, text = tool("diff", ws, "source-tree", "feature/dirty", "to", first)
		w.True(strings.Contains(text, "no earlier revision"), "the root commit has no earlier revision: %s", text)

		w.Step("diff: two source trees compared by path, even on unrelated histories")
		_, text = tool("diff", ws, "source-tree", "feature/dirty", "from", "feature/base", "to", "feature/dirty")
		w.True(strings.Contains(text, "-zero") && strings.Contains(text, "+two") && strings.Contains(text, "b.txt"), "path to path: %s", text)
		_, text = tool("diff", ws, "source-tree", "feature/dirty", "from", "feature/base")
		w.True(strings.Contains(text, "-zero") && strings.Contains(text, "+two"), "path against HEAD: %s", text)

		w.Step("mistakes are values the model can act on")
		for _, c := range []struct {
			name   string
			params []string
			want   string
		}{
			{"log", nil, "needs `source-tree`"},
			{"log", []string{"source-tree", "notes.txt"}, "is not a source tree"},
			{"log", []string{"source-tree", "nope"}, "is not in this tree"},
			{"log", []string{"source-tree", "feature/dirty", "rev", "main"}, "not a revision this tool can resolve"},
			{"log", []string{"source-tree", "feature/dirty", "count", "x"}, "nonnegative integer"},
			{"show", []string{"source-tree", "../x"}, "names no source tree"},
		} {
			kind, text := tool(c.name, ws, c.params...)
			w.True(kind == "tree" && strings.HasPrefix(text, "FAILED") && strings.Contains(text, c.want),
				"%s %v: %s %q (want FAILED ... %q)", c.name, c.params, kind, text, c.want)
		}
	})
}
