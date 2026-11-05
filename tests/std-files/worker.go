// tests/std-files — a WORKER test of the file tools (std/read, ls, write, edit,
// copy, move, remove), run over HTTP the way the harness runs a tool, under the
// root contract (design/tool-contract.md, "`root`").
//
// Each call is handed one object as `root` (a file, a directory, or a source
// tree's commit) and nothing else, and answers with a tree: `stdout`, `failed`
// on a mistake, and for a writer the new object as `root`, or `deleted`.
// Putting that back into a session, and the child commits above it, is the
// harness's (std/llm-step/src/root.rs, and tests/root-tools end to end).
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

// image is the built tool's identity, from the arg that names it.
func image(tool string) string { return run("caos", "hash", "/cas/args/"+tool) }

// result is a tool's answer tree: its blob entries by name, and every entry's
// kind and hash.
type result struct {
	hash  string
	files map[string]string
	kinds map[string]string
	oids  map[string]string
}

func (r result) failed() bool {
	_, marked := r.files["failed"]
	return marked
}

// call runs a tool with `root` bound to an object (none when root is ""):
// POST /eval/run, as the harness's call is.
func call(tool, root string, params ...string) result {
	seq++
	form := url.Values{}
	form.Set("base:hash", image(tool))
	if root != "" {
		form.Set("root:hash", root)
	}
	form.Set("salt", fmt.Sprintf("std-files-%d-%d", os.Getpid(), seq))
	w.True(len(params)%2 == 0, "params are name, value pairs")
	for i := 0; i < len(params); i += 2 {
		form.Set(params[i], params[i+1])
	}
	resp, err := client.PostForm(server+"/eval/run", form)
	w.True(err == nil, "%s: POST did not complete: %v", tool, err)
	defer resp.Body.Close()
	body := strings.TrimSpace(string(w.Check(io.ReadAll(resp.Body))))
	w.True(resp.StatusCode == 200, "%s: HTTP %d: %s", tool, resp.StatusCode, body)
	kind, hash, found := strings.Cut(body, " ")
	w.True(found && kind == "tree", "%s answered %q, not a result tree", tool, body)
	return fetch(hash)
}

func fetch(hash string) result {
	r := result{hash: hash, files: map[string]string{}, kinds: map[string]string{}, oids: map[string]string{}}
	dir := fmt.Sprintf("/cas/r-%d-%s", seq, hash[:8])
	run("caos", "get-hash", hash, dir)
	run("caos", "get", dir)
	for _, entry := range w.Check(os.ReadDir(dir)) {
		p := filepath.Join(dir, entry.Name())
		r.kinds[entry.Name()] = run("caos", "kind", p)
		r.oids[entry.Name()] = run("caos", "hash", p)
		if r.kinds[entry.Name()] == "blob" {
			run("caos", "get", p)
			r.files[entry.Name()] = string(w.Check(os.ReadFile(p)))
		}
	}
	_, hasOut := r.files["stdout"]
	w.True(hasOut, "result %s has no stdout: %v", hash, r.kinds)
	return r
}

// ok checks a call that must succeed and returns it.
func ok(r result, claim string) result {
	w.True(!r.failed(), "%s failed: %s", claim, r.files["stdout"])
	return r
}

func refused(r result, want, claim string) {
	w.True(r.failed(), "%s: no `failed` marker (stdout %q)", claim, r.files["stdout"])
	w.True(strings.Contains(r.files["stdout"], want), "%s: stdout lacks %q: %q", claim, want, r.files["stdout"])
	_, root := r.kinds["root"]
	_, deleted := r.kinds["deleted"]
	w.True(!root && !deleted, "%s: a failed call proposed a change: %v", claim, r.kinds)
}

// at is the object at a path inside a tree (through a source tree's code).
func at(tree, path string) string {
	seq++
	dest := fmt.Sprintf("/cas/at-%d", seq)
	run("caos", "resolve", tree, path, dest)
	return run("caos", "hash", dest)
}

// entry is the kind and hash of a direct entry of a tree, as stored: a source
// tree comes back as its commit.
func entry(tree, dir, name string) (string, string) {
	seq++
	dest := fmt.Sprintf("/cas/entry-%d", seq)
	run("caos", "get-hash", tree, dest)
	run("caos", "get", dest)
	parent := dest
	if dir != "" {
		parent = filepath.Join(dest, dir)
		run("caos", "get", parent)
	}
	p := filepath.Join(parent, name)
	return run("caos", "kind", p), run("caos", "hash", p)
}

func main() {
	w.Main(func() {
		server = strings.TrimRight(os.Getenv("CAOS_SERVER_URL"), "/")
		w.True(server != "", "this test needs CAOS_SERVER_URL from the runner")

		w.Step("a tree with a source tree in it")
		write("/tmp/code/README.md", "one\ntwo\nthree\n")
		write("/tmp/code/src/lib.txt", "lib\n")
		run("caos", "put", "/tmp/code", "/cas/code")
		codeTree := run("caos", "hash", "/cas/code")
		write("/tmp/commit", "tree "+codeTree+"\nauthor t <t@caos> 1 +0000\ncommitter t <t@caos> 1 +0000\n\nthe source tree\n")
		run("caos", "put-commit", "/tmp/commit", "/cas/commit")
		commit := run("caos", "hash", "/cas/commit")
		write("/tmp/ws/notes.txt", "hello\nworld\n")
		w.Must(os.MkdirAll("/tmp/ws/feature", 0o755))
		w.Must(os.Symlink("/cas/commit", "/tmp/ws/feature/dirty"))
		run("caos", "put", "/tmp/ws", "/cas/ws")
		ws := run("caos", "hash", "/cas/ws")
		notes := at(ws, "notes.txt")
		readme := at(ws, "feature/dirty/README.md")

		w.Step("read: a file, a window of it, and the mistakes")
		r := ok(call("read", notes), "read notes.txt")
		w.True(r.files["stdout"] == "hello\nworld\n", "read: %q", r.files["stdout"])
		r = ok(call("read", notes, "offset", "2", "limit", "1"), "read a window")
		w.True(r.files["stdout"] == "world", "read window: %q", r.files["stdout"])
		r = ok(call("read", readme), "read a file from a source tree")
		w.True(r.files["stdout"] == "one\ntwo\nthree\n", "read README: %q", r.files["stdout"])
		refused(call("read", ""), "nothing is at the root path", "reading nothing")
		refused(call("read", ws), "the root is a directory", "reading a directory")
		refused(call("read", notes, "offset", "9"), "past the end", "an offset past the end")

		w.Step("ls: a directory, and a source tree's code")
		r = ok(call("ls", ws), "ls the tree")
		w.True(r.files["stdout"] == "feature/\nnotes.txt", "ls: %q", r.files["stdout"])
		r = ok(call("ls", commit), "ls a source tree")
		w.True(r.files["stdout"] == "README.md\nsrc/", "ls a source tree: %q", r.files["stdout"])
		refused(call("ls", notes), "the root is a file", "ls of a file")

		w.Step("write: a new file, a replaced one, and the mistakes")
		r = ok(call("write", "", "content", "fresh\n"), "write a new file")
		w.True(r.kinds["root"] == "blob" && r.files["root"] == "fresh\n", "write: %v %q", r.kinds, r.files["root"])
		w.True(r.files["stdout"] == "wrote 6 bytes", "write stdout: %q", r.files["stdout"])
		r = ok(call("write", notes, "content", "replaced\n"), "replace a file")
		w.True(r.files["root"] == "replaced\n", "replaced: %q", r.files["root"])
		refused(call("write", ws, "content", "x"), "the root is a directory", "writing over a directory")
		refused(call("write", notes), "needs a string `content`", "no content")

		w.Step("edit: one replacement, all of them, and the mistakes")
		r = ok(call("edit", notes, "old-string", "world", "new-string", "there"), "edit")
		w.True(r.files["root"] == "hello\nthere\n", "edit: %q", r.files["root"])
		w.True(r.files["stdout"] == "edited (1 replacement)", "edit stdout: %q", r.files["stdout"])
		refused(call("edit", notes, "old-string", "absent", "new-string", "x"), "not found", "no match")
		refused(call("edit", readme, "old-string", "e", "new-string", "E"), "appears", "an ambiguous match")
		r = ok(call("edit", readme, "old-string", "e", "new-string", "E", "replace-all", "true"), "replace-all")
		w.True(r.files["root"] == "onE\ntwo\nthrEE\n", "replace-all: %q", r.files["root"])
		refused(call("edit", ws, "old-string", "a", "new-string", "b"), "the root is a directory", "editing a directory")

		w.Step("copy: a file, a source tree stays a source tree, inside a source tree")
		r = ok(call("copy", ws, "from", "notes.txt", "to", "backup/notes.txt"), "copy a file")
		w.True(r.kinds["root"] == "tree", "copy proposes a tree: %v", r.kinds)
		w.True(at(r.oids["root"], "backup/notes.txt") == notes, "the copy is the same file")
		r = ok(call("copy", ws, "from", "feature/dirty", "to", "feature/twin"), "copy a source tree")
		kind, hash := entry(r.oids["root"], "feature", "twin")
		w.True(kind == "commit" && hash == commit, "the copy of a source tree is the SAME commit: %s %s", kind, hash)
		r = ok(call("copy", commit, "from", "README.md", "to", "COPY.md"), "copy inside a source tree")
		w.True(r.kinds["root"] == "tree", "a source tree root proposes its new code tree: %v", r.kinds)
		w.True(at(r.oids["root"], "COPY.md") == readme, "the copy inside the source tree")
		refused(call("copy", ws, "from", "notes.txt", "to", "feature"), "already exists", "copying onto something")
		refused(call("copy", ws, "from", "missing", "to", "x"), "is not in this tree", "copying what is not there")
		refused(call("copy", notes, "from", "a", "to", "b"), "the root is a file", "copying with a file as root")

		w.Step("move and remove")
		r = ok(call("move", ws, "from", "notes.txt", "to", "docs/notes.txt"), "move")
		moved := ok(call("ls", r.oids["root"]), "ls after the move")
		w.True(moved.files["stdout"] == "docs/\nfeature/", "moved away: %q", moved.files["stdout"])
		w.True(at(r.oids["root"], "docs/notes.txt") == notes, "the moved file")
		refused(call("move", ws, "from", "feature", "to", "feature/inside"), "into itself", "moving into itself")
		r = ok(call("remove", notes), "remove")
		_, deleted := r.kinds["deleted"]
		_, proposed := r.kinds["root"]
		w.True(deleted && !proposed, "remove answers `deleted` and no root: %v", r.kinds)
		refused(call("remove", ""), "nothing is at the root path", "removing nothing")
	})
}
