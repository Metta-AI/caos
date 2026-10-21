// tests/std-files — a WORKER test of the file tools (std/read, ls, write, edit,
// copy, move, remove), run over HTTP the way an agent runs a tool.
//
// The tree they run over is built here: some files, and `feature/dirty`, a SOURCE
// TREE (a commit entry) holding a README and a directory. That is the case that
// matters, because the tools' whole reason for existing is to reach through a code
// reference: reading it, and writing it, which must leave a CHILD COMMIT of the
// source tree rather than a plain directory in its place.
//
// A reader answers with a blob, or a `report` that opens FAILED. A writer answers
// with a tree: `prop` (the new tree), `out`, a `message`, and `failed` on a
// mistake -- over an unchanged `prop`.
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

// result is what a tool answered: its kind, and for a tree its files by name
// (blobs only) plus the entries' hashes.
type result struct {
	kind  string
	hash  string
	text  string            // a blob's content
	files map[string]string // a tree's blob entries
	kinds map[string]string // a tree's entries' kinds
	oids  map[string]string // a tree's entries' hashes
}

// call runs a tool over a tree: POST /eval/run, as an agent's call is.
func call(tool, tree string, params ...string) result {
	seq++
	form := url.Values{}
	form.Set("base:hash", image(tool))
	form.Set("in:hash", tree)
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
	w.True(found, "%s answered %q", tool, body)
	return fetch(kind, hash)
}

func fetch(kind, hash string) result {
	r := result{kind: kind, hash: hash}
	dir := fmt.Sprintf("/cas/r-%d-%s", seq, hash[:8])
	run("caos", "get-hash", hash, dir)
	if kind == "blob" {
		run("caos", "get", dir)
		r.text = string(w.Check(os.ReadFile(dir)))
		return r
	}
	run("caos", "get", dir)
	r.files, r.kinds, r.oids = map[string]string{}, map[string]string{}, map[string]string{}
	for _, entry := range w.Check(os.ReadDir(dir)) {
		p := filepath.Join(dir, entry.Name())
		r.kinds[entry.Name()] = run("caos", "kind", p)
		r.oids[entry.Name()] = run("caos", "hash", p)
		if r.kinds[entry.Name()] == "blob" {
			run("caos", "get", p)
			r.files[entry.Name()] = string(w.Check(os.ReadFile(p)))
		}
	}
	return r
}

func failedReader(r result, want string, claim string) {
	w.True(r.kind == "tree", "%s: expected a report tree, got a %s", claim, r.kind)
	report := r.files["report"]
	w.True(strings.HasPrefix(report, "FAILED"), "%s: report does not open FAILED: %q", claim, report)
	w.True(strings.Contains(report, want), "%s: report lacks %q: %q", claim, want, report)
}

func failedWriter(r result, want string, unchanged string, claim string) {
	w.True(r.kind == "tree", "%s: expected a writer tree, got a %s", claim, r.kind)
	_, marked := r.files["failed"]
	w.True(marked, "%s: no `failed` marker (entries %v)", claim, r.kinds)
	w.True(strings.Contains(r.files["out"], want), "%s: out lacks %q: %q", claim, want, r.files["out"])
	w.True(r.oids["prop"] == unchanged, "%s: a failed call changed prop: %s vs %s", claim, r.oids["prop"], unchanged)
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

		w.Step("read: a file, a window of it, one inside a source tree")
		r := call("read", ws, "file-path", "notes.txt")
		w.True(r.kind == "blob" && r.text == "hello\nworld\n", "read notes.txt: %+v", r)
		r = call("read", ws, "file-path", "notes.txt", "offset", "2", "limit", "1")
		w.True(r.text == "world", "read window: %q", r.text)
		r = call("read", ws, "file-path", "feature/dirty/README.md")
		w.True(r.text == "one\ntwo\nthree\n", "read through a source tree: %q", r.text)

		w.Step("read: mistakes are values the model can act on")
		failedReader(call("read", ws, "file-path", "nope.txt"), "is not in this tree", "a missing file")
		failedReader(call("read", ws, "file-path", "feature"), "is a directory; use ls", "a directory")
		failedReader(call("read", ws, "file-path", "../x"), "`..` is not allowed", "a parent path")
		failedReader(call("read", ws, "file-path", "notes.txt", "offset", "9"), "past the end", "an offset past the end")

		w.Step("ls: the root, a directory, and the inside of a source tree")
		r = call("ls", ws)
		w.True(r.text == "feature/\nnotes.txt", "ls root: %q", r.text)
		r = call("ls", ws, "path", "feature")
		w.True(r.text == "dirty/", "a source tree lists as a directory: %q", r.text)
		r = call("ls", ws, "path", "feature/dirty")
		w.True(r.text == "README.md\nsrc/", "ls inside a source tree: %q", r.text)
		failedReader(call("ls", ws, "path", "notes.txt"), "is not a directory", "ls of a file")

		w.Step("write: a new file, and a file inside a source tree")
		r = call("write", ws, "file-path", "docs/new.txt", "content", "fresh\n")
		w.True(r.kind == "tree" && r.files["out"] == "wrote docs/new.txt (6 bytes)", "write: %+v", r.files)
		w.True(r.files["message"] == "write docs/new.txt", "message: %q", r.files["message"])
		_, marked := r.files["failed"]
		w.True(!marked, "a good write is marked failed")
		got := call("read", r.oids["prop"], "file-path", "docs/new.txt")
		w.True(got.text == "fresh\n", "the written file reads back: %q", got.text)
		got = call("read", r.oids["prop"], "file-path", "notes.txt")
		w.True(got.text == "hello\nworld\n", "an untouched file survives: %q", got.text)

		r = call("write", ws, "file-path", "feature/dirty/NEW.txt", "content", "inside\n")
		prop := r.oids["prop"]
		w.True(call("ls", prop, "path", "feature/dirty").text == "NEW.txt\nREADME.md\nsrc/",
			"the source tree has the new file beside the old ones")
		w.True(call("ls", prop, "path", "feature").text == "dirty/", "it is still a source tree")
		run("caos", "get-hash", prop, "/cas/prop-write")
		run("caos", "get", "/cas/prop-write")
		run("caos", "get", "/cas/prop-write/feature")
		w.True(run("caos", "kind", "/cas/prop-write/feature/dirty") == "commit",
			"writing inside a source tree leaves a COMMIT, not a plain directory")
		w.True(run("caos", "hash", "/cas/prop-write/feature/dirty") != commit,
			"and a NEW commit, not the original")

		failedWriter(call("write", ws, "file-path", ".caos/x", "content", "no"), "protocol metadata", ws, "writing .caos")
		failedWriter(call("write", ws, "file-path", "notes.txt"), "needs a string `content`", ws, "no content")

		w.Step("edit: one replacement, all of them, and the mistakes")
		r = call("edit", ws, "file-path", "notes.txt", "old-string", "world", "new-string", "there")
		w.True(r.files["out"] == "edited notes.txt (1 replacement)", "edit: %q", r.files["out"])
		w.True(call("read", r.oids["prop"], "file-path", "notes.txt").text == "hello\nthere\n", "the edit applied")
		failedWriter(call("edit", ws, "file-path", "notes.txt", "old-string", "absent", "new-string", "x"), "not found", ws, "no match")
		failedWriter(call("edit", ws, "file-path", "feature/dirty/README.md", "old-string", "e", "new-string", "E"), "appears", ws, "an ambiguous match")
		r = call("edit", ws, "file-path", "feature/dirty/README.md", "old-string", "e", "new-string", "E", "replace-all", "true")
		w.True(strings.Contains(r.files["out"], "replacements"), "replace-all: %q", r.files["out"])
		w.True(call("read", r.oids["prop"], "file-path", "feature/dirty/README.md").text == "onE\ntwo\nthrEE\n",
			"replace-all inside a source tree")

		w.Step("copy: a file, and a source tree stays a source tree")
		r = call("copy", ws, "from", "notes.txt", "to", "backup/notes.txt")
		w.True(call("read", r.oids["prop"], "file-path", "backup/notes.txt").text == "hello\nworld\n", "copied file")
		r = call("copy", ws, "from", "feature/dirty", "to", "feature/twin")
		run("caos", "get-hash", r.oids["prop"], "/cas/prop-copy")
		run("caos", "get", "/cas/prop-copy")
		run("caos", "get", "/cas/prop-copy/feature")
		w.True(run("caos", "kind", "/cas/prop-copy/feature/twin") == "commit", "the copy of a source tree is a source tree")
		w.True(run("caos", "hash", "/cas/prop-copy/feature/twin") == commit, "and the SAME commit: history intact")
		failedWriter(call("copy", ws, "from", "notes.txt", "to", "feature"), "already exists", ws, "copying onto something")
		failedWriter(call("copy", ws, "from", "missing", "to", "x"), "is not in this tree", ws, "copying what is not there")

		w.Step("move and remove")
		r = call("move", ws, "from", "notes.txt", "to", "docs/notes.txt")
		w.True(call("ls", r.oids["prop"]).text == "docs/\nfeature/", "moved away: %q", call("ls", r.oids["prop"]).text)
		w.True(call("read", r.oids["prop"], "file-path", "docs/notes.txt").text == "hello\nworld\n", "moved file")
		failedWriter(call("move", ws, "from", "feature", "to", "feature/inside"), "into itself", ws, "moving into itself")
		r = call("remove", ws, "file-path", "feature/dirty/src")
		w.True(call("ls", r.oids["prop"], "path", "feature/dirty").text == "README.md", "removed inside a source tree")
		r = call("remove", ws, "file-path", "feature")
		w.True(call("ls", r.oids["prop"]).text == "notes.txt", "removing a source tree removes it whole")
		failedWriter(call("remove", ws, "file-path", "nope"), "is not in this tree", ws, "removing what is not there")
	})
}
