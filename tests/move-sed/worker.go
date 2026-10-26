// tests/move-sed — a WORKER test: no client, no repo.
//
// Runs std/move-sed over a fixture conversation holding a stack the way
// design/publish-stack.md lays one out: sources/repo with 00-base and its
// .source.json, two layers with their .pr.md descriptions, a 99-dirty, plus a
// source tree and a file at the root. A success is checked by the tree it
// proposes: it must equal the fixture rebuilt with exactly those entries
// renamed. A refusal must propose the fixture unchanged and mark `failed`.
//
// Every fixture message carries --test-salt, so a salted run is a cache miss.
//
// Staged: no run can be waited on, so each case is checked in the `then` of
// its own run, which then starts the next case.
package main

import (
	"fmt"
	"os"
	"path/filepath"
	"strconv"
	"strings"

	"caos/w"
)

var salt string

// mktree publishes a folder of files, each holding <content>\n, at /cas/<name>.
func mktree(name string, files ...string) string {
	dir := "/tmp/t"
	w.Must(os.RemoveAll(dir))
	w.Must(os.MkdirAll(dir, 0o755))
	for _, f := range files {
		path, content, _ := strings.Cut(f, "=")
		w.Must(os.WriteFile(filepath.Join(dir, path), []byte(content+"\n"), 0o644))
	}
	w.Put(dir, "/cas/"+name)
	return w.Hash("/cas/" + name)
}

func mint(name, tree string, ts int, msg string, parents ...string) string {
	var b strings.Builder
	fmt.Fprintf(&b, "tree %s\n", tree)
	for _, p := range parents {
		fmt.Fprintf(&b, "parent %s\n", p)
	}
	fmt.Fprintf(&b, "author dev <dev@caos> %d +0000\ncommitter dev <dev@caos> %d +0000\n\n%s (%s)\n", ts, ts, msg, salt)
	w.Must(os.WriteFile("/tmp/commit", []byte(b.String()), 0o644))
	return w.Caos("put-commit", "/tmp/commit", "/cas/"+name)
}

// A case: the arguments, and what must hold. A success names the renames
// (old=new, in the directory the case names); a refusal, the words its report
// must carry.
type testCase struct {
	name    string
	args    []string
	dir     string            // the fixture directory the renames apply to ("" = root)
	renames map[string]string // old entry name -> new; nil for a refusal
	dryRun  bool
	says    []string
}

var cases []testCase
var conv string

// link puts a symlink to a CAS object at path, as a gitlink or a folder.
func link(path, target string) {
	w.Must(os.MkdirAll(filepath.Dir(path), 0o755))
	w.Must(os.Symlink("/cas/"+target, path))
}

func file(path, content string) {
	w.Must(os.MkdirAll(filepath.Dir(path), 0o755))
	w.Must(os.WriteFile(path, []byte(content), 0o644))
}

// conversation stages the fixture at dir, with the entries of `where` renamed
// as `renames` says. The fixture's entries are written under their final
// names, so the expected tree for a success is built by the same function.
func conversation(dir, where string, renames map[string]string) string {
	w.Must(os.RemoveAll(dir))
	w.Must(os.MkdirAll(filepath.Join(dir, ".caos"), 0o755))
	name := func(in, entry string) string {
		if in == where {
			if to, ok := renames[entry]; ok {
				return to
			}
		}
		return entry
	}
	root := func(entry string) string { return filepath.Join(dir, name("", entry)) }
	repo := func(entry string) string { return filepath.Join(dir, "sources/repo", name("sources/repo", entry)) }
	file(filepath.Join(dir, ".caos/meta"), "protocol\n")
	file(root("notes.txt"), "conversation notes ("+salt+")\n")
	link(root("src"), "b")
	link(repo("00-base"), "b")
	file(repo("00-base.source.json"), `{"repository":"https://example.com/repo.git"}`+"\n")
	link(repo("01-feature"), "l1")
	file(repo("01-feature.pr.md"), "Add the feature\n\nWhy.\n")
	link(repo("02-tests"), "l2")
	file(repo("02-tests.pr.md"), "Test the feature\n")
	link(repo("99-dirty"), "l2")
	w.Put(dir, "/cas/"+filepath.Base(dir))
	return w.Hash("/cas/" + filepath.Base(dir))
}

// build mints every fixture. It is deterministic, so each stage mints the same
// oids again rather than carrying them.
func build() {
	bT := mktree("b-t", "f.txt=base")
	b := mint("b", bT, 1700000000, "base")
	l1 := mint("l1", mktree("l1-t", "f.txt=base", "a.txt=one"), 1700000100, "layer 1", b)
	mint("l2", mktree("l2-t", "f.txt=base", "a.txt=one", "b.txt=two"), 1700000200, "layer 2", l1)
	conv = conversation("/tmp/conv", "", nil)

	args := func(dir, pattern, replacement string, extra ...string) []string {
		a := []string{"--pattern=" + pattern, "--replacement=" + replacement}
		if dir != "" {
			a = append(a, "--dir="+dir)
		}
		return append(a, extra...)
	}
	cases = []testCase{
		// A layer and its description, renamed together: the split in
		// design/stacks.md, where 01-feature becomes 02-y.
		{name: "split", args: args("sources/repo", `^01-feature(\..+)?$`, "02-y$1"), dir: "sources/repo",
			renames: map[string]string{"01-feature": "02-y", "01-feature.pr.md": "02-y.pr.md"}},
		// A shift up by one, and a named group.
		{name: "shift", args: args("sources/repo", `^02-(?P<rest>tests.*)$`, "03-${rest}"), dir: "sources/repo",
			renames: map[string]string{"02-tests": "03-tests", "02-tests.pr.md": "03-tests.pr.md"}},
		// The root, named as `.`, with .caos left alone because the pattern is anchored.
		{name: "root", args: args(".", `^notes\.txt$`, "NOTES.txt"), dir: "",
			renames: map[string]string{"notes.txt": "NOTES.txt"}},
		// A dry run lists the renames and proposes the conversation unchanged.
		{name: "dry-run", args: args("sources/repo", `^01-feature(\..+)?$`, "02-y$1", "--dry-run=true"),
			dryRun: true, says: []string{"dry run", "01-feature -> 02-y", "01-feature.pr.md -> 02-y.pr.md"}},
		// Refusals, each before anything changes.
		{name: "collide", args: args("sources/repo", `^01-feature$`, "02-tests"),
			says: []string{"01-feature would become 02-tests, which exists and is not being renamed"}},
		{name: "duplicate", args: args("sources/repo", `^0[12]-(feature|tests)$`, "same"),
			says: []string{"both 01-feature and 02-tests would become same"}},
		{name: "no-match", args: args("sources/repo", `^zzz`, "x"),
			says: []string{"no entry matches", "01-feature.pr.md"}},
		{name: "no-op", args: args("sources/repo", `^01-feature$`, "01-feature"),
			says: []string{"keeps its name"}},
		{name: "sed-group", args: args("sources/repo", `^01-(.*)$`, `02-\1`),
			says: []string{`uses sed's \1`, "$1"}},
		{name: "bad-regex", args: args("sources/repo", `(`, "x"),
			says: []string{"not a valid Go regular expression"}},
		{name: "slash", args: args("sources/repo", `^01-feature$`, "a/b"),
			says: []string{`would become "a/b", which is not one directory entry name`}},
		{name: "in-source-tree", args: args("src/sub", `.`, "x"),
			says: []string{"src is a source tree", "use move"}},
		{name: "missing-dir", args: args("sources/nowhere", `.`, "x"),
			says: []string{"no such path: sources/nowhere"}},
		{name: "a-file", args: args("notes.txt", `.`, "x"),
			says: []string{"notes.txt is a blob, not a directory"}},
		{name: "caos", args: args(".", `^\.caos$`, "x"),
			says: []string{".caos is protocol metadata"}},
		{name: "dotdot", args: args("sources/../repo", `.`, "x"),
			says: []string{"without . or .."}},
		{name: "no-pattern", args: []string{"--dir=sources/repo", "--replacement=x"},
			says: []string{"pattern is required"}},
	}
}

// next is this test curried for stage s, the `then` of a case's run.
func next(s int) string {
	return w.Caos("curry", "--base:@=/cas/args/base", "--worker1:@=/cas/args/worker1",
		"--stage="+strconv.Itoa(s), "--test-salt:@=/cas/args/test-salt", "--tool:@=/cas/args/tool")
}

// launch runs case i.
func launch(i int) {
	c := cases[i]
	w.Step(fmt.Sprintf("case %d: %s", i, c.name))
	args := append([]string{"prepare-request", "--base:hash=" + w.Hash("/cas/args/tool"), "--in:@=/cas/conv"}, c.args...)
	request := w.Caos(args...)
	w.Caos("run-request-then", request, "--then:hash="+next(i))
}

func check(i int) {
	c := cases[i]
	const r = "/cas/args/result"
	w.Get(r)
	for _, part := range []string{"prop", "out"} {
		w.True(w.Exists(filepath.Join(r, part)), "case %s: the result has no %s", c.name, part)
	}
	out := w.Read(filepath.Join(r, "out"))
	failed := w.Exists(filepath.Join(r, "failed"))
	prop := w.Hash(filepath.Join(r, "prop"))
	for _, s := range c.says {
		w.True(strings.Contains(out, s), "case %s does not say %q: %s", c.name, s, out)
	}
	switch {
	case c.renames != nil:
		w.True(!failed, "case %s was refused: %s", c.name, out)
		expect := conversation("/tmp/expect", c.dir, c.renames)
		w.True(prop == expect, "case %s: the proposal is not the fixture with %v renamed in %q; out: %s", c.name, c.renames, c.dir, out)
		for from, to := range c.renames {
			w.True(strings.Contains(out, from+" -> "+to), "case %s: out does not list %s -> %s: %s", c.name, from, to, out)
		}
		w.True(w.Exists(filepath.Join(r, "message")), "case %s: no commit message for the conversation", c.name)
		fmt.Fprintf(os.Stderr, "  ok: %s", out)
	case c.dryRun:
		w.True(!failed, "case %s was refused: %s", c.name, out)
		w.True(prop == conv, "case %s: a dry run changed the conversation", c.name)
		fmt.Fprintf(os.Stderr, "  ok: %s", out)
	default:
		w.True(failed, "case %s was not refused: %s", c.name, out)
		w.True(strings.HasPrefix(out, "FAILED: "), "case %s has no FAILED banner: %s", c.name, out)
		w.True(prop == conv, "case %s: a refusal changed the conversation", c.name)
		fmt.Fprintf(os.Stderr, "  ok: %s", out)
	}
}

func main() {
	w.Main(func() {
		salt = strings.TrimSpace(w.Read("/cas/args/test-salt"))
		build()
		if !w.Exists("/cas/args/stage") {
			launch(0)
			return
		}
		i := w.Check(strconv.Atoi(strings.TrimSpace(w.Read("/cas/args/stage"))))
		check(i)
		if i+1 < len(cases) {
			launch(i + 1)
			return
		}
		w.Report("move-sed: ALL PASS\n")
	})
}
