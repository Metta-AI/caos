// The `import-source` tool's worker. Its docs live in the sibling
// `.caos-expr` here-string, not in this header (SPEC, "CaosTools").
//
// Import an HTTPS Git repository at one revision as a new source tree
// (design/agent-github.md, "Importing"), as an ordinary writer
// (design/tool-contract.md): it holds no token, and imports through its
// neighbour `DEEP-DEPS/github`, the one image the token is granted to, which
// resolves the revision on the remote and fetches that exact commit into caos.
// Nothing is recorded first: a call that does not finish is answered as
// unfinished, and the commits already fetched are simply unused.
//
// `root` is the directory to import into (a stack directory such as
// sources/repo, absent for a new one); `into` names the new entry in it. The
// answer is that directory with two entries added: `into`, a gitlink to the
// commit, and `into.source.json`, where it came from.
//
// Stages, by --stage: none (check the call, import), `done` (add the entries).
package main

import (
	"encoding/json"
	"fmt"
	"os"
	"path/filepath"
	"strings"
	"time"

	"caos/w"
)

const root = "/cas/args/root"

type refusal struct{ msg string }

func refuse(format string, a ...any) { panic(refusal{fmt.Sprintf(format, a...)}) }

func arg(name string) string {
	path := "/cas/args/" + name
	if !w.Exists(path) {
		return ""
	}
	return strings.TrimSpace(w.Read(path))
}

// finish writes the result: the new directory as `root`, or `failed`.
func finish(newRoot, out string, failed bool) {
	const dir = "/tmp/import-out"
	w.Must(os.MkdirAll(dir, 0o755))
	w.Must(os.WriteFile(filepath.Join(dir, "stdout"), []byte(out), 0o644))
	if failed {
		w.Must(os.WriteFile(filepath.Join(dir, "failed"), []byte("import-source failed\n"), 0o644))
	} else {
		w.Must(os.Symlink(newRoot, filepath.Join(dir, "root")))
		w.Must(os.WriteFile(filepath.Join(dir, "commit-message"), []byte("import-source "+arg("source")+" into "+arg("into")+"\n"), 0o644))
	}
	w.Put(dir, "/cas/out")
}

// names is the root's entries, after checking it is a directory; none when
// nothing is there yet.
func names() []string {
	if !w.Exists(root) {
		return nil
	}
	if k := w.Caos("kind", root); k != "tree" {
		refuse("the root is a %s: import-source adds a source tree to a directory, such as sources/repo", k)
	}
	w.Get(root)
	var list []string
	for _, e := range w.Check(os.ReadDir(root)) {
		list = append(list, e.Name())
	}
	return list
}

// into is the new entry's name, which must be one free name in the root.
func into() string {
	name := arg("into")
	if name == "" || strings.ContainsAny(name, "/\\") || name == "." || name == ".." || name == ".caos" {
		refuse("into is the name of the new source tree inside root, such as 00-base, not %q", name)
	}
	for _, existing := range names() {
		if existing == name || existing == name+".source.json" {
			refuse("%s is occupied in root; choose an unused name, such as %s-2", existing, name)
		}
	}
	return name
}

func start() {
	into()
	source := arg("source")
	if source == "" {
		refuse("source is required: the HTTPS repository URL")
	}
	op := map[string]any{"op": "import", "repository": source}
	if revision := arg("revision"); revision != "" {
		op["revision"] = revision
	}
	batch := w.Check(json.Marshal([]map[string]any{op}))
	id := arg("request-id")
	request := w.Caos("prepare-request", "--base:@=/cas/args/github", "--batch="+string(batch),
		"--request-id="+id+"-import")
	args := []string{"curry", "--base:@=/cas/args/base", "--worker1:@=/cas/args/worker1",
		"--github:@=/cas/args/github", "--stage=done", "--request-id=" + id,
		"--source=" + source, "--into=" + arg("into"), "--revision=" + arg("revision")}
	if w.Exists(root) {
		args = append(args, "--root:@="+root)
	}
	w.Caos("run-request-then", request, "--then:hash="+w.Caos(args...))
}

func done() {
	var results []map[string]any
	text := w.Read("/cas/args/result")
	if json.Unmarshal([]byte(text), &results) != nil || len(results) != 1 {
		refuse("GitHub's answer was not one result: %s", text)
	}
	r := results[0]
	if message, _ := r["error"].(string); message != "" {
		refuse("%s", message)
	}
	commit, _ := r["commit"].(string)
	if len(commit) != 40 {
		refuse("the import answered no commit: %s", text)
	}
	name := into()

	// The directory as it was, every entry linked, plus the two new ones.
	const staged = "/tmp/import-dir"
	w.Must(os.MkdirAll(staged, 0o755))
	for _, existing := range names() {
		w.Must(os.Symlink(filepath.Join(root, existing), filepath.Join(staged, existing)))
	}
	w.Caos("get-hash", commit, "/cas/imported")
	w.Must(os.Symlink("/cas/imported", filepath.Join(staged, name)))
	provenance := map[string]any{
		"commit":             commit,
		"default_branch":     r["default_branch"],
		"observed_at":        time.Now().Unix(),
		"repository":         arg("source"),
		"requested_revision": nil,
	}
	if revision := arg("revision"); revision != "" {
		provenance["requested_revision"] = revision
	}
	w.Must(os.WriteFile(filepath.Join(staged, name+".source.json"), w.Check(json.Marshal(provenance)), 0o644))
	w.Put(staged, "/cas/new-root")
	finish("/cas/new-root", fmt.Sprintf("Imported %s at %s\n", commit, name), false)
}

func main() {
	w.Main(func() {
		defer func() {
			r := recover()
			if r == nil {
				return
			}
			no, ok := r.(refusal)
			if !ok {
				panic(r)
			}
			finish("", "import-source: "+no.msg+"\n", true)
		}()
		if arg("stage") == "done" {
			done()
			return
		}
		start()
	})
}
