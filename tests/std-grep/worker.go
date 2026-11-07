// tests/std-grep — a WORKER test of std/grep under the root contract
// (design/tool-contract.md): the scope is the object bound as `root` (a
// directory, a source tree's commit, or a file), matches come back as
// `path:linenum:line` relative to it, and every answer is a tree: `stdout`, and
// `failed` on a mistake.
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

// at is the object at a path inside a tree (through a source tree's code).
func at(tree, path string) string {
	seq++
	dest := fmt.Sprintf("/cas/at-%d", seq)
	run("caos", "resolve", tree, path, dest)
	return run("caos", "hash", dest)
}

// grep runs the tool with `root` bound to an object (none for "") and returns
// its stdout and whether it failed.
func grep(root string, params ...string) (text string, failed bool) {
	seq++
	form := url.Values{}
	form.Set("base:hash", run("caos", "hash", "/cas/args/grep"))
	if root != "" {
		form.Set("root:hash", root)
	}
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
	w.True(found && kind == "tree", "grep answered %q, not a result tree", body)
	dir := fmt.Sprintf("/cas/r-%d", seq)
	run("caos", "get-hash", hash, dir)
	run("caos", "get", dir)
	run("caos", "get", dir+"/stdout")
	_, err = os.Lstat(dir + "/failed")
	return string(w.Check(os.ReadFile(dir + "/stdout"))), err == nil
}

func ok(text string, failed bool) string {
	w.True(!failed, "grep failed: %s", text)
	return text
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
		commit := run("caos", "hash", "/cas/commit")
		write("/tmp/ws/notes.txt", "hello\nworld\ntwo more\n")
		w.Must(os.MkdirAll("/tmp/ws/feature", 0o755))
		w.Must(os.Symlink("/cas/commit", "/tmp/ws/feature/dirty"))
		run("caos", "put", "/tmp/ws", "/cas/ws")
		ws := run("caos", "hash", "/cas/ws")

		w.Step("matches come back as path:line:text, through the source tree")
		text := ok(grep(ws, "pattern", "two"))
		want := "feature/dirty/README.md:2:two\nfeature/dirty/src/lib.txt:2:two libs\nnotes.txt:3:two more"
		w.True(text == want, "grep two:\n%s\nwant:\n%s", text, want)

		w.Step("the root narrows the search: a source tree, a directory, a file")
		text = ok(grep(commit, "pattern", "two"))
		w.True(text == "README.md:2:two\nsrc/lib.txt:2:two libs", "a source tree root: %q", text)
		text = ok(grep(at(ws, "feature/dirty/src"), "pattern", "two"))
		w.True(text == "lib.txt:2:two libs", "a directory root: %q", text)
		text = ok(grep(at(ws, "notes.txt"), "pattern", "world"))
		w.True(text == "2:world", "a file root: %q", text)

		w.Step("a pattern is a regular expression")
		text = ok(grep(commit, "pattern", "^(one|three)$"))
		w.True(text == "README.md:1:one\nREADME.md:3:three", "alternation: %q", text)

		w.Step("no matches is an answer, not an error")
		text = ok(grep(ws, "pattern", "zzz-not-there"))
		w.True(text == "no matches", "no matches: %q", text)

		w.Step("mistakes are values the model can act on")
		text, failed := grep(ws, "pattern", "(")
		w.True(failed && strings.Contains(text, "invalid pattern"), "a bad pattern: %v %q", failed, text)
		text, failed = grep("", "pattern", "x")
		w.True(failed && strings.Contains(text, "nothing is at the root path"), "no root: %v %q", failed, text)
		text, failed = grep(ws)
		w.True(failed && strings.Contains(text, "needs a string `pattern`"), "no pattern: %v %q", failed, text)
	})
}
