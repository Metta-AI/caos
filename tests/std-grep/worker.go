// tests/std-grep — a WORKER test of std/grep, which runs the rgrep fold over a
// scope and renders its sparse result as `path:linenum:line`.
//
// The tree has a SOURCE TREE in it (a commit entry, `feature/dirty`), because a
// search that stops at a code reference is not the tool: matches inside one must
// come back under their conversation path.
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

// grep runs the tool over a tree and returns what it answered: the text of a
// blob, or for a tree its `report`.
func grep(tree string, params ...string) (kind, text string) {
	seq++
	form := url.Values{}
	form.Set("base:hash", run("caos", "hash", "/cas/args/grep"))
	form.Set("in:hash", tree)
	form.Set("salt", fmt.Sprintf("std-grep-%d-%d", os.Getpid(), seq))
	for i := 0; i < len(params); i += 2 {
		form.Set(params[i], params[i+1])
	}
	resp, err := client.PostForm(server+"/eval/run", form)
	w.True(err == nil, "POST did not complete: %v", err)
	defer resp.Body.Close()
	body := strings.TrimSpace(string(w.Check(io.ReadAll(resp.Body))))
	w.True(resp.StatusCode == 200, "grep: HTTP %d: %s", resp.StatusCode, body)
	kind, hash, found := strings.Cut(body, " ")
	w.True(found, "grep answered %q", body)
	dir := fmt.Sprintf("/cas/r-%d", seq)
	run("caos", "get-hash", hash, dir)
	run("caos", "get", dir)
	if kind == "blob" {
		return kind, string(w.Check(os.ReadFile(dir)))
	}
	run("caos", "get", dir+"/report")
	return kind, string(w.Check(os.ReadFile(dir + "/report")))
}

func main() {
	w.Main(func() {
		server = strings.TrimRight(os.Getenv("CAOS_SERVER_URL"), "/")
		w.True(server != "", "this test needs CAOS_SERVER_URL from the runner")

		w.Step("a tree with a source tree in it")
		write("/tmp/code/README.md", "one\ntwo\nthree\n")
		write("/tmp/code/src/lib.txt", "lib\ntwo libs\n")
		run("caos", "put", "/tmp/code", "/cas/code")
		tree := run("caos", "hash", "/cas/code")
		write("/tmp/commit", "tree "+tree+"\nauthor t <t@caos> 1 +0000\ncommitter t <t@caos> 1 +0000\n\nthe source tree\n")
		run("caos", "put-commit", "/tmp/commit", "/cas/commit")
		write("/tmp/ws/notes.txt", "hello\nworld\ntwo more\n")
		w.Must(os.MkdirAll("/tmp/ws/feature", 0o755))
		w.Must(os.Symlink("/cas/commit", "/tmp/ws/feature/dirty"))
		run("caos", "put", "/tmp/ws", "/cas/ws")
		ws := run("caos", "hash", "/cas/ws")

		w.Step("matches come back as path:line:text, through the source tree")
		kind, text := grep(ws, "pattern", "two")
		w.True(kind == "blob", "a search answers a blob, got a %s: %s", kind, text)
		want := "feature/dirty/README.md:2:two\nfeature/dirty/src/lib.txt:2:two libs\nnotes.txt:3:two more"
		w.True(text == want, "grep two:\n%s\nwant:\n%s", text, want)

		w.Step("a scope narrows the search, to a directory or a file")
		_, text = grep(ws, "pattern", "two", "path", "feature/dirty/src")
		w.True(text == "feature/dirty/src/lib.txt:2:two libs", "scoped to a directory: %q", text)
		_, text = grep(ws, "pattern", "world", "path", "notes.txt")
		w.True(text == "notes.txt:2:world", "scoped to a file: %q", text)

		w.Step("a pattern is a regular expression")
		_, text = grep(ws, "pattern", "^(one|three)$", "path", "feature")
		w.True(text == "feature/dirty/README.md:1:one\nfeature/dirty/README.md:3:three", "alternation: %q", text)

		w.Step("no matches is an answer, not an error")
		kind, text = grep(ws, "pattern", "zzz-not-there")
		w.True(kind == "blob" && text == "no matches", "no matches: %s %q", kind, text)

		w.Step("mistakes are values the model can act on")
		kind, text = grep(ws, "pattern", "(")
		w.True(kind == "tree" && strings.HasPrefix(text, "FAILED: invalid pattern"), "a bad pattern: %s %q", kind, text)
		kind, text = grep(ws, "pattern", "x", "path", "no/such/dir")
		w.True(kind == "tree" && strings.HasPrefix(text, "FAILED: no/such/dir"), "a missing path: %s %q", kind, text)
		kind, text = grep(ws, "pattern", "x", "path", "../up")
		w.True(kind == "tree" && strings.Contains(text, "`..` is not allowed"), "a parent path: %s %q", kind, text)
		kind, text = grep(ws)
		w.True(kind == "tree" && strings.Contains(text, "needs a string `pattern`"), "no pattern: %s %q", kind, text)
	})
}
