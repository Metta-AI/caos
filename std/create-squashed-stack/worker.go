// The `create-squashed-stack` tool's worker. Its docs live in the sibling `.caos-expr`
// here-string, not in this header (SPEC, "CaosTools").
//
// A writer run on the conversation (SPEC, "Writers"). Args, under /cas/args:
//
//	in    the conversation tree it was run on
//	plan  the conversation path of the plan file
//
// The plan names everything else: `onto`, `into`, and one block per layer of
// `layer=`, `commit=` and `message=` lines.
//
// The result is {prop, out, message, failed?}. `prop` is the conversation
// with `into` replaced by {<layer>: <squashed commit>}. It is built from links
// to the CAS entries it keeps, which `caos put` records by oid, so nothing in
// the conversation is checked out.
//
// A refusal proposes `in` unchanged and marks `failed`: the agent reads it and
// acts (merges, fixes the plan). Only a broken environment is a job error.
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
	in     = "/cas/args/in"
	result = "/tmp/squash-result"
	repo   = "/tmp/squash-repo"
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

func isOid(s string) bool {
	if len(s) != 40 && len(s) != 64 {
		return false
	}
	for _, c := range s {
		if !strings.ContainsRune("0123456789abcdef", c) {
			return false
		}
	}
	return true
}

// splitPath checks a conversation-relative path and splits it.
func splitPath(what, raw string) []string {
	path := strings.TrimSuffix(raw, "/")
	if path == "" || strings.HasPrefix(path, "/") {
		refuse("%s must be a conversation-relative path, not %q", what, raw)
	}
	parts := strings.Split(path, "/")
	for _, part := range parts {
		if part == "" || part == "." || part == ".." {
			refuse("%s must be a conversation-relative path without . or .., not %q", what, raw)
		}
	}
	return parts
}

// node finds a conversation path's entry without looking inside a source
// tree, and returns its CAS path, or "" when nothing is there.
func node(what, raw string) string {
	dir := in
	parts := splitPath(what, raw)
	for _, part := range parts[:len(parts)-1] {
		dir = filepath.Join(dir, part)
		if !exists(dir) {
			return ""
		}
		if k := kind(dir); k != "tree" {
			refuse("%s (%s) passes through %s, which is a %s, not a folder", what, raw, part, k)
		}
		get(dir)
	}
	target := filepath.Join(dir, parts[len(parts)-1])
	if !exists(target) {
		return ""
	}
	return target
}

// commitOf resolves a plan's commit reference: a full hash, or the
// conversation path of a gitlink. It returns the commit's oid.
func commitOf(what, ref string) string {
	if isOid(ref) {
		dst := "/cas/ref-" + ref
		if _, code, _ := try("caos", "get-hash", ref, dst); code != 0 {
			refuse("%s %s names no object; is it imported?", what, ref)
		}
		if k := kind(dst); k != "commit" {
			refuse("%s %s is a %s, not a commit", what, ref, k)
		}
		return ref
	}
	path := node(what, ref)
	if path == "" {
		refuse("%s: no such path: %s", what, ref)
	}
	if k := kind(path); k != "commit" {
		refuse("%s (%s) is a %s, not a gitlink", what, ref, k)
	}
	return hash(path)
}

// layer is one block of the plan, and the commit it resolves to.
type layer struct {
	name, ref, tip string
	message        []string
	line           int
}

type plan struct {
	onto, into string
	layers     []layer
}

// parse reads the plan: one key=value per line, `onto` and `into` once before
// the first `layer=`, then one block per layer.
func parse(text string) plan {
	var p plan
	seen := map[string]bool{}
	for i, raw := range strings.Split(strings.TrimSuffix(text, "\n"), "\n") {
		n := i + 1
		line := strings.TrimSuffix(raw, "\r")
		if line == "" {
			continue
		}
		key, value, ok := strings.Cut(line, "=")
		if !ok {
			refuse("plan line %d is not key=value: %q", n, line)
		}
		var cur *layer
		if len(p.layers) > 0 {
			cur = &p.layers[len(p.layers)-1]
		}
		switch key {
		case "onto", "into":
			if cur != nil {
				refuse("plan line %d: %s must come before the first layer=", n, key)
			}
			if seen[key] {
				refuse("plan line %d: %s is given twice", n, key)
			}
			seen[key] = true
			if key == "onto" {
				p.onto = value
			} else {
				p.into = value
			}
		case "layer":
			if value == "" || value == "." || value == ".." || strings.Contains(value, "/") {
				refuse("plan line %d: layer must be one path component, not %q", n, value)
			}
			if seen["layer "+value] {
				refuse("plan line %d: layer %s is given twice", n, value)
			}
			seen["layer "+value] = true
			p.layers = append(p.layers, layer{name: value, line: n})
		case "commit":
			if cur == nil {
				refuse("plan line %d: commit= must follow a layer=", n)
			}
			if cur.ref != "" {
				refuse("plan line %d: layer %s has two commit= lines", n, cur.name)
			}
			cur.ref = value
		case "message":
			if cur == nil {
				refuse("plan line %d: message= must follow a layer=", n)
			}
			cur.message = append(cur.message, value)
		default:
			refuse("plan line %d: unknown key %q; the keys are onto, into, layer, commit and message", n, key)
		}
	}
	if p.onto == "" {
		refuse("the plan has no onto=: the commit the first layer goes on, e.g. onto=mystack/00-base")
	}
	if p.into == "" {
		refuse("the plan has no into=: where to write the squashed stack, e.g. into=publish/mystack")
	}
	if len(p.layers) == 0 {
		refuse("the plan has no layer= blocks")
	}
	for _, l := range p.layers {
		if l.ref == "" {
			refuse("layer %s (plan line %d) has no commit= line", l.name, l.line)
		}
		if strings.TrimSpace(strings.Join(l.message, "")) == "" {
			refuse("layer %s (plan line %d) has no message= text", l.name, l.line)
		}
	}
	return p
}

// within reports whether path is at or below dir, both conversation paths.
func within(path, dir string) bool {
	return path == dir || strings.HasPrefix(path, dir+"/")
}

// checkInto refuses an `into` that would overwrite anything but a previous
// result: it must not hold the plan or anything the plan reads, and an
// existing `into` must be a folder of gitlinks only.
func checkInto(p plan, planPath string) string {
	into := strings.Join(splitPath("into", p.into), "/")
	reads := []string{planPath}
	if !isOid(p.onto) {
		reads = append(reads, p.onto)
	}
	for _, l := range p.layers {
		if !isOid(l.ref) {
			reads = append(reads, l.ref)
		}
	}
	for _, r := range reads {
		if within(strings.TrimSuffix(r, "/"), into) {
			refuse("into (%s) holds %s, which the plan reads; choose a separate path, such as publish/<stack>", into, r)
		}
	}
	target := node("into", into)
	if target == "" {
		return into
	}
	if k := kind(target); k != "tree" {
		refuse("into (%s) is a %s; it must be a new path or a folder this tool wrote", into, k)
	}
	for _, name := range entries(target) {
		if kind(filepath.Join(target, name)) != "commit" {
			refuse("into (%s) holds %s, which is not a gitlink; it must be a new path or a folder this tool wrote", into, name)
		}
	}
	return into
}

// fetchGraph fetches the commit graph only (--filter=tree:0), as std/merge
// does: ancestry and the commits' own headers are all this reads, and a
// layer's tree is reused by oid rather than fetched.
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

// checkAncestry refuses a layer whose commit does not contain the one before
// it, or `onto` for the first, before anything is minted.
func checkAncestry(onto string, ls []layer) {
	below, belowTip := "onto", onto
	for _, l := range ls {
		_, code, stderr := try("git", "-C", repo, "merge-base", "--is-ancestor", belowTip, l.tip)
		switch code {
		case 0:
		case 1:
			refuse("layer %s (%s) does not contain %s (%s). Merge %s into it first.",
				l.name, l.tip, below, belowTip, below)
		default:
			w.True(false, "git merge-base: exit %d: %s", code, stderr)
		}
		below, belowTip = "layer "+l.name, l.tip
	}
}

// mint makes each layer's squashed commit: the commit's tree on the one
// below, with its author and committer copied verbatim, never taken from the
// clock, so the same plan mints the same commits and an unchanged republish
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
		body := fmt.Sprintf("tree %s\nparent %s\n%s\n%s\n\n%s\n",
			tree, parent, author, committer, strings.Join(l.message, "\n"))
		file := fmt.Sprintf("/tmp/squash-commit-%d", i)
		w.Must(os.WriteFile(file, []byte(body), 0o644))
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
		get("/cas/args/plan")
		planPath := strings.Join(splitPath("plan", strings.TrimSpace(string(w.Check(os.ReadFile("/cas/args/plan"))))), "/")
		file := node("plan", planPath)
		if file == "" {
			refuse("plan: no such path: %s", planPath)
		}
		if k := kind(file); k != "blob" {
			refuse("plan (%s) is a %s, not a file", planPath, k)
		}
		get(file)
		p := parse(string(w.Check(os.ReadFile(file))))
		into := checkInto(p, planPath)
		onto := commitOf("onto", p.onto)
		for i := range p.layers {
			p.layers[i].tip = commitOf("layer "+p.layers[i].name+"'s commit", p.layers[i].ref)
		}
		fetchGraph(onto, p.layers)
		checkAncestry(onto, p.layers)
		commits := mint(onto, p.layers)

		const squashed = "/tmp/create-squashed-stack"
		w.Must(os.RemoveAll(squashed))
		w.Must(os.MkdirAll(squashed, 0o755))
		var report strings.Builder
		fmt.Fprintf(&report, "squashed onto %s into %s:\n", onto, into)
		for i, l := range p.layers {
			w.Must(os.Symlink(fmt.Sprintf("/cas/c%d", i), filepath.Join(squashed, l.name)))
			fmt.Fprintf(&report, "  %s/%s  %s\n", into, l.name, commits[i])
		}
		report.WriteString("Next: publish_source each, bottom to top, with force=true.\n")
		run("caos", "put", squashed, "/cas/squashed")

		w.Must(os.RemoveAll("/tmp/squash-prop"))
		rebuild(in, "/tmp/squash-prop", strings.Split(into, "/"), "/cas/squashed")
		run("caos", "put", "/tmp/squash-prop", "/cas/prop")
		finish("/cas/prop", report.String(), "create-squashed-stack: "+into+"\n", false)
	})
}
