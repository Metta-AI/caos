// The `squash-layers` tool's worker. Its docs live in the sibling `.caos-expr`
// here-string, not in this header (SPEC, "CaosTools").
//
// A writer run on the conversation (SPEC, "Writers"). Args, under /cas/args:
//
//	in        the conversation tree it was run on
//	stack     the folder holding the layer gitlinks
//	onto      the commit the first layer is published onto
//	messages  a folder of message files, one per layer, named after its entry
//	into      the conversation path to write the squashed stack to
//
// The result is {prop, out, message, failed?}. `prop` is the conversation
// with `into` replaced by {<entry>: <squashed commit>}. It is built from
// links to the CAS entries it keeps, which `caos put` records by oid, so
// nothing in the conversation is checked out.
//
// A refusal proposes `in` unchanged and marks `failed`: the agent reads it and
// acts (merges, fixes a message). Only a broken environment is a job error.
package main

import (
	"bytes"
	"errors"
	"fmt"
	"os"
	"os/exec"
	"path/filepath"
	"strings"

	"caos/w"
)

const (
	in       = "/cas/args/in"
	stack    = "/cas/args/stack"
	messages = "/cas/args/messages"
	result   = "/tmp/squash-result"
	repo     = "/tmp/squash-repo"
)

// refusal is a problem with the input, answered as a value.
type refusal struct{ msg string }

func refuse(format string, a ...any) { panic(refusal{fmt.Sprintf(format, a...)}) }

// run runs a command and returns its trimmed stdout, failing the job on a
// non-zero exit with the command's stderr.
func run(name string, args ...string) string {
	out, code, stderr := try(name, args...)
	w.True(code == 0, "%s %s: exit %d: %s", name, strings.Join(args, " "), code, stderr)
	return out
}

// try runs a command and returns its trimmed stdout, exit code and stderr.
func try(name string, args ...string) (string, int, string) {
	cmd := exec.Command(name, args...)
	var stdout, stderr bytes.Buffer
	cmd.Stdout, cmd.Stderr = &stdout, &stderr
	err := cmd.Run()
	code := 0
	if err != nil {
		var exit *exec.ExitError
		w.True(errors.As(err, &exit), "running %s: %v", name, err)
		code = exit.ExitCode()
	}
	return strings.TrimRight(stdout.String(), "\n"), code, strings.TrimSpace(stderr.String())
}

func kind(path string) string { return run("caos", "kind", path) }
func hash(path string) string { return run("caos", "hash", path) }
func get(path string)         { run("caos", "get", path) }

func exists(path string) bool {
	_, err := os.Lstat(path)
	return err == nil
}

// entries lists a CAS folder's names in byte order, after fetching its listing.
func entries(dir string) []string {
	get(dir)
	var names []string
	for _, e := range w.Check(os.ReadDir(dir)) {
		names = append(names, e.Name())
	}
	return names
}

// intoPath checks `into` is a plain conversation-relative path and splits it.
func intoPath(raw string) []string {
	path := strings.TrimSuffix(raw, "/")
	if path == "" || strings.HasPrefix(path, "/") {
		refuse("into must be a conversation-relative path, not %q", raw)
	}
	parts := strings.Split(path, "/")
	for _, part := range parts {
		if part == "" || part == "." || part == ".." {
			refuse("into must be a conversation-relative path without . or .., not %q", raw)
		}
	}
	return parts
}

// checkInto refuses an `into` that would overwrite something other than a
// previous result: every folder on the way must be a plain folder that is not
// the stack or the messages, and an existing `into` must hold only gitlinks
// and not be the stack itself.
func checkInto(parts []string, into string) {
	stackOid, messagesOid := hash(stack), hash(messages)
	dir := in
	for _, part := range parts[:len(parts)-1] {
		dir = filepath.Join(dir, part)
		if !exists(dir) {
			return
		}
		if k := kind(dir); k != "tree" {
			refuse("into (%s) passes through %s, which is a %s, not a folder", into, part, k)
		}
		if oid := hash(dir); oid == stackOid || oid == messagesOid {
			refuse("into (%s) lies inside the stack or the messages; choose a path outside them", into)
		}
	}
	target := filepath.Join(dir, parts[len(parts)-1])
	if !exists(target) {
		return
	}
	if k := kind(target); k != "tree" {
		refuse("into (%s) is a %s; it must be a new path or a folder this tool wrote", into, k)
	}
	if hash(target) == stackOid {
		refuse("into (%s) is the stack itself; choose a separate path, such as publish/<stack>", into)
	}
	for _, name := range entries(target) {
		if kind(filepath.Join(target, name)) != "commit" {
			refuse("into (%s) holds %s, which is not a gitlink; it must be a new path or a folder this tool wrote", into, name)
		}
	}
}

// layer is one layer to publish: its entry name, tip commit and message file.
type layer struct{ name, tip, message string }

// layers reads the message files in byte order and pairs each with its entry.
func layers() []layer {
	get(stack)
	var out []layer
	for _, name := range entries(messages) {
		file := filepath.Join(messages, name)
		if kind(file) != "blob" {
			refuse("messages/%s is not a file; each layer's message is one file named after its stack entry", name)
		}
		get(file)
		if w.Check(os.Stat(file)).Size() == 0 {
			refuse("messages/%s is empty; write layer %s's commit message into it", name, name)
		}
		entry := filepath.Join(stack, name)
		if !exists(entry) {
			refuse("the stack has no entry %s, which messages/%s names", name, name)
		}
		if kind(entry) != "commit" {
			refuse("stack entry %s is not a source gitlink", name)
		}
		out = append(out, layer{name, hash(entry), file})
	}
	if len(out) == 0 {
		refuse("messages has no files; write one per layer to publish, named after its stack entry")
	}
	return out
}

// fetchGraph fetches the commit graph only (--filter=tree:0), as std/merge
// does: ancestry and the tips' own headers are all this reads, and a layer's
// tree is reused by oid rather than fetched.
func fetchGraph(onto string, ls []layer) {
	w.Must(os.RemoveAll(repo))
	w.Must(os.MkdirAll(repo, 0o755))
	git := func(args ...string) { run("git", append([]string{"-C", repo}, args...)...) }
	git("init", "-q")
	git("remote", "add", "origin", os.Getenv("CAOS_SERVER_URL"))
	git("config", "extensions.partialClone", "origin")
	git("config", "remote.origin.promisor", "true")
	git("config", "remote.origin.partialclonefilter", "tree:0")
	args := []string{"fetch", "-q", "--filter=tree:0", "origin", onto}
	for _, l := range ls {
		args = append(args, l.tip)
	}
	git(args...)
}

// checkAncestry refuses a layer that does not contain the one below it, or
// `onto` for the first, before anything is minted.
func checkAncestry(onto string, ls []layer) {
	below, belowTip := "the base "+onto, onto
	for _, l := range ls {
		_, code, stderr := try("git", "-C", repo, "merge-base", "--is-ancestor", belowTip, l.tip)
		switch code {
		case 0:
		case 1:
			refuse("layer %s (%s) does not contain %s (%s). Merge %s into %s first.",
				l.name, l.tip, below, belowTip, below, l.name)
		default:
			w.True(false, "git merge-base: exit %d: %s", code, stderr)
		}
		below, belowTip = "layer "+l.name, l.tip
	}
}

// mint makes each layer's squashed commit: the tip's tree on the commit below,
// with the tip's author and committer copied verbatim, never taken from the
// clock, so the same input mints the same commits and an unchanged republish
// pushes nothing new.
func mint(onto string, ls []layer) []string {
	var commits []string
	parent := onto
	for i, l := range ls {
		raw := run("git", "-C", repo, "cat-file", "commit", l.tip)
		var tree, author, committer string
		for _, line := range strings.Split(raw, "\n") {
			if line == "" {
				break
			}
			switch {
			case strings.HasPrefix(line, "tree "):
				tree = strings.TrimPrefix(line, "tree ")
			case strings.HasPrefix(line, "author "):
				author = line
			case strings.HasPrefix(line, "committer "):
				committer = line
			}
		}
		message := w.Check(os.ReadFile(l.message))
		if message[len(message)-1] != '\n' {
			message = append(message, '\n')
		}
		var body bytes.Buffer
		fmt.Fprintf(&body, "tree %s\nparent %s\n%s\n%s\n\n", tree, parent, author, committer)
		body.Write(message)
		file := fmt.Sprintf("/tmp/squash-commit-%d", i)
		w.Must(os.WriteFile(file, body.Bytes(), 0o644))
		parent = run("caos", "put-commit", file, fmt.Sprintf("/cas/c%d", i))
		commits = append(commits, parent)
	}
	return commits
}

// rebuild stages `src` (a CAS folder, or "" for one that does not exist yet)
// at `dst`, with the path `parts` below it replaced by `node`. Each folder on
// the way is rebuilt from links to its other entries; nothing else is listed.
func rebuild(src, dst string, parts []string, node string) {
	w.Must(os.MkdirAll(dst, 0o755))
	head := parts[0]
	if src != "" {
		for _, name := range entries(src) {
			if name != head {
				w.Must(os.Symlink(filepath.Join(src, name), filepath.Join(dst, name)))
			}
		}
	}
	if len(parts) == 1 {
		w.Must(os.Symlink(node, filepath.Join(dst, head)))
		return
	}
	child := ""
	if src != "" && exists(filepath.Join(src, head)) {
		child = filepath.Join(src, head)
	}
	rebuild(child, filepath.Join(dst, head), parts[1:], node)
}

// finish writes the writer result and stores it at /cas/out.
func finish(prop, out, message string, failed bool) {
	w.Must(os.WriteFile(filepath.Join(result, "out"), []byte(out), 0o644))
	if message != "" {
		w.Must(os.WriteFile(filepath.Join(result, "message"), []byte(message), 0o644))
	}
	if failed {
		w.Must(os.WriteFile(filepath.Join(result, "failed"), []byte(out), 0o644))
	}
	w.Must(os.Symlink(prop, filepath.Join(result, "prop")))
	run("caos", "put", result, "/cas/out")
}

func main() {
	w.Main(func() {
		w.Must(os.RemoveAll(result))
		w.Must(os.MkdirAll(result, 0o755))
		defer func() {
			r := recover()
			if r == nil {
				return
			}
			no, ok := r.(refusal)
			if !ok {
				panic(r)
			}
			w.Must(os.RemoveAll(result))
			w.Must(os.MkdirAll(result, 0o755))
			finish(in, "FAILED: "+no.msg+"\n", "", true)
		}()

		get(in)
		get("/cas/args/into")
		into := strings.TrimSpace(string(w.Check(os.ReadFile("/cas/args/into"))))
		parts := intoPath(into)
		into = strings.Join(parts, "/")
		onto := hash("/cas/args/onto")
		checkInto(parts, into)
		ls := layers()
		fetchGraph(onto, ls)
		checkAncestry(onto, ls)
		commits := mint(onto, ls)

		const squashed = "/tmp/squash-layers"
		w.Must(os.RemoveAll(squashed))
		w.Must(os.MkdirAll(squashed, 0o755))
		var report strings.Builder
		fmt.Fprintf(&report, "squashed onto %s into %s:\n", onto, into)
		for i, l := range ls {
			w.Must(os.Symlink(fmt.Sprintf("/cas/c%d", i), filepath.Join(squashed, l.name)))
			fmt.Fprintf(&report, "  %s/%s  %s\n", into, l.name, commits[i])
		}
		report.WriteString("Next: publish_source each, bottom to top, with rewrite=true.\n")
		run("caos", "put", squashed, "/cas/squashed")

		w.Must(os.RemoveAll("/tmp/squash-prop"))
		rebuild(in, "/tmp/squash-prop", parts, "/cas/squashed")
		run("caos", "put", "/tmp/squash-prop", "/cas/prop")
		finish("/cas/prop", report.String(), "squash-layers: "+into+"\n", false)
	})
}
