// tests/std-eval-path — a WORKER test of std/eval-path: what a path evaluates to.
//
// The tree has directories whose `.caos-expr` makes something (a `curry`, which
// evaluates to an ArgTree without running anything), directories with none (which
// evaluate to themselves), and a SOURCE TREE whose own root carries an expression:
// a path that begins inside it must be evaluated against that root.
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

// evalPath runs the tool and returns what it answered: the text of a blob, or for
// a tree its `report`.
func evalPath(tree string, params ...string) (kind, text string) {
	seq++
	form := url.Values{}
	form.Set("base:hash", run("caos", "hash", "/cas/args/eval-path"))
	form.Set("in:hash", tree)
	form.Set("salt", fmt.Sprintf("std-eval-path-%d-%d", os.Getpid(), seq))
	for i := 0; i < len(params); i += 2 {
		form.Set(params[i], params[i+1])
	}
	resp, err := client.PostForm(server+"/eval/run", form)
	w.True(err == nil, "POST did not complete: %v", err)
	defer resp.Body.Close()
	body := strings.TrimSpace(string(w.Check(io.ReadAll(resp.Body))))
	w.True(resp.StatusCode == 200, "eval-path: HTTP %d: %s", resp.StatusCode, body)
	kind, hash, found := strings.Cut(body, " ")
	w.True(found, "eval-path answered %q", body)
	dir := fmt.Sprintf("/cas/r-%d", seq)
	run("caos", "get-hash", hash, dir)
	run("caos", "get", dir)
	if kind == "blob" {
		return kind, string(w.Check(os.ReadFile(dir)))
	}
	run("caos", "get", dir+"/report")
	return kind, string(w.Check(os.ReadFile(dir + "/report")))
}

// the answer is `<kind> <hash>` on its first line.
func answer(text string) (kind, hash string) {
	first, _, _ := strings.Cut(text, "\n")
	kind, hash, _ = strings.Cut(first, " ")
	return kind, hash
}

func main() {
	w.Main(func() {
		server = strings.TrimRight(os.Getenv("CAOS_SERVER_URL"), "/")
		w.True(server != "", "this test needs CAOS_SERVER_URL from the runner")

		w.Step("a tree with expressions, plain directories, and a source tree")
		image := run("caos", "hash", "/cas/args/base")
		// A curry evaluates to the curried ArgTree and runs nothing.
		write("/tmp/ws/made/.caos-expr", "curry --base:hash="+image+" --greeting=hi\n")
		write("/tmp/ws/plain/hello.txt", "just a file\n")
		// The source tree's own root carries an expression, so `tool` means
		// something only against that root.
		write("/tmp/code/tool/.caos-expr", "curry --base:hash="+image+" --from=the-source-tree\n")
		write("/tmp/code/README.md", "code\n")
		run("caos", "put", "/tmp/code", "/cas/code")
		write("/tmp/commit", "tree "+run("caos", "hash", "/cas/code")+"\nauthor t <t@caos> 1 +0000\ncommitter t <t@caos> 1 +0000\n\nthe source tree\n")
		run("caos", "put-commit", "/tmp/commit", "/cas/commit")
		w.Must(os.MkdirAll("/tmp/ws/feature", 0o755))
		w.Must(os.Symlink("/cas/commit", "/tmp/ws/feature/dirty"))
		run("caos", "put", "/tmp/ws", "/cas/ws")
		ws := run("caos", "hash", "/cas/ws")

		w.Step("an expression evaluates to what it makes")
		kind, text := evalPath(ws, "path", "made")
		k, made := answer(text)
		w.True(kind == "blob" && k == "tree" && len(made) == 40, "made: %s %q", kind, text)
		w.True(strings.Contains(text, "as `in`"), "it says how to look inside: %q", text)

		w.Step("a directory with no expression evaluates to itself")
		_, text = evalPath(ws, "path", "plain")
		k, plain := answer(text)
		w.True(k == "tree" && plain != made, "plain: %q", text)
		_, text = evalPath(ws)
		k, root := answer(text)
		w.True(k == "tree" && root == ws, "the root evaluates to itself: %q (want %s)", text, ws)

		w.Step("the result is a tree to look inside: pass its hash as `in`")
		_, again := evalPath(made, "path", "")
		_, againHash := answer(again)
		w.True(againHash == made, "evaluating a made tree's root is the tree: %q", again)

		w.Step("a blob is shown as text")
		_, text = evalPath(ws, "path", "plain/hello.txt")
		k, _ = answer(text)
		w.True(k == "blob" && strings.Contains(text, "just a file"), "blob: %q", text)

		w.Step("a path inside a source tree is evaluated against that tree's root")
		_, text = evalPath(ws, "path", "feature/dirty/tool")
		k, tool := answer(text)
		w.True(k == "tree" && len(tool) == 40 && tool != made, "tool inside the source tree: %q", text)
		_, text = evalPath(ws, "path", "feature/dirty")
		k, srcRoot := answer(text)
		w.True(k == "tree", "the source tree's own root: %q", text)
		w.True(srcRoot != root, "it is the source tree's code tree, not the conversation: %q", text)

		w.Step("a path that does not evaluate is reported, not raised")
		kind, text = evalPath(ws, "path", "no/such/path")
		w.True(kind == "tree" && strings.HasPrefix(text, "FAILED"), "missing path: %s %q", kind, text)
		kind, text = evalPath(ws, "path", "../up")
		w.True(kind == "tree" && strings.Contains(text, "`..` is not allowed"), "parent path: %s %q", kind, text)
	})
}
