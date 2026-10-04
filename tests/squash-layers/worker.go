// tests/squash-layers — a WORKER test: no client, no repo.
//
// Runs std/squash-layers over a conversation tree holding the stack shape
// design/stacks.md describes: a base B at 00-base, layer 1 on B, layer 2
// copied from layer 1, then layer 1 changed and merged up into layer 2, so
// layer 2's tip is a merge commit; a third layer sits on layer 2. Squashing
// must give one single-parent commit per layer that has a message file,
// B <- C1 <- C2 <- C3, each with its layer tip's tree, author and committer
// and its file's message byte for byte.
//
// The tool is a writer, so a success is checked by the tree it proposes: it
// must equal the conversation with only `into` replaced, built here. A refusal
// must propose the conversation unchanged and mark `failed`.
//
// Every fixture message carries --test-salt, so a salted run mints new
// commits; the server's salt does not reach a sub-run's key (CLAUDE.md).
//
// Staged: no run can be waited on, so each case is checked in the `then` of
// its own run, which then starts the next case.
package main

import (
	"bytes"
	"errors"
	"fmt"
	"os"
	"os/exec"
	"path/filepath"
	"strconv"
	"strings"

	"caos/w"
)

func run(name string, args ...string) string {
	cmd := exec.Command(name, args...)
	var stdout, stderr bytes.Buffer
	cmd.Stdout, cmd.Stderr = &stdout, &stderr
	err := cmd.Run()
	if err != nil {
		var exit *exec.ExitError
		w.True(errors.As(err, &exit), "running %s: %v", name, err)
	}
	w.True(err == nil, "%s %s: %s", name, strings.Join(args, " "), strings.TrimSpace(stderr.String()))
	return strings.TrimRight(stdout.String(), "\n")
}

func caos(args ...string) string { return run("caos", args...) }

func read(path string) string {
	caos("get", path)
	return string(w.Check(os.ReadFile(path)))
}

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
	caos("put", dir, "/cas/"+name)
	return caos("hash", "/cas/"+name)
}

// header is a commit's headers. Every commit has its own time, and the
// committer's differs from the author's, so copying them is a real check.
func header(tree string, ts int, parents ...string) string {
	var b strings.Builder
	fmt.Fprintf(&b, "tree %s\n", tree)
	for _, p := range parents {
		fmt.Fprintf(&b, "parent %s\n", p)
	}
	fmt.Fprintf(&b, "author dev <dev@caos> %d +0000\ncommitter dev <dev@caos> %d +0000\n\n", ts, ts+50)
	return b.String()
}

func putCommit(name, body string) string {
	w.Must(os.WriteFile("/tmp/commit", []byte(body), 0o644))
	return caos("put-commit", "/tmp/commit", "/cas/"+name)
}

func mint(name, tree string, ts int, msg string, parents ...string) string {
	return putCommit(name, header(tree, ts, parents...)+fmt.Sprintf("%s (%s)\n", msg, salt))
}

// folder publishes a folder whose entries link to /cas objects: a commit makes
// a gitlink, a tree a plain folder.
func folder(name string, links ...string) {
	dir := "/tmp/s"
	w.Must(os.RemoveAll(dir))
	w.Must(os.MkdirAll(dir, 0o755))
	for _, l := range links {
		entry, target, _ := strings.Cut(l, "=")
		w.Must(os.Symlink("/cas/"+target, filepath.Join(dir, entry)))
	}
	caos("put", dir, "/cas/"+name)
}

// messages publishes a messages folder: each <entry>=<text> one file holding
// exactly <text>, and <entry>/ a subfolder.
func messages(name string, files ...string) {
	dir := "/tmp/m"
	w.Must(os.RemoveAll(dir))
	w.Must(os.MkdirAll(dir, 0o755))
	for _, f := range files {
		if strings.HasSuffix(f, "/") {
			w.Must(os.MkdirAll(filepath.Join(dir, f), 0o755))
			w.Must(os.WriteFile(filepath.Join(dir, f, "x"), []byte("x\n"), 0o644))
			continue
		}
		entry, text, _ := strings.Cut(f, "=")
		w.Must(os.WriteFile(filepath.Join(dir, entry), []byte(text), 0o644))
	}
	caos("put", dir, "/cas/"+name)
}

// conversation stages the fixture conversation at dir, with each
// <path>=<cas-name> in extra put in place of what was there.
func conversation(dir string, extra ...string) {
	w.Must(os.RemoveAll(dir))
	for _, d := range []string{"publish/mystack", "publish/other", ".caos"} {
		w.Must(os.MkdirAll(filepath.Join(dir, d), 0o755))
	}
	for _, l := range []string{"mystack=merged", "unmerged=unmerged", "plain=plain", "src=b",
		"msgs=msgs", "publish/mystack/01-core=b"} {
		entry, target, _ := strings.Cut(l, "=")
		w.Must(os.Symlink("/cas/"+target, filepath.Join(dir, entry)))
	}
	w.Must(os.WriteFile(filepath.Join(dir, "notes.txt"), []byte("conversation notes\n"), 0o644))
	w.Must(os.WriteFile(filepath.Join(dir, ".caos/meta"), []byte("protocol\n"), 0o644))
	w.Must(os.WriteFile(filepath.Join(dir, "publish/other/f.txt"), []byte("another stack\n"), 0o644))
	for _, l := range extra {
		path, target, _ := strings.Cut(l, "=")
		w.Must(os.RemoveAll(filepath.Join(dir, path)))
		w.Must(os.MkdirAll(filepath.Dir(filepath.Join(dir, path)), 0o755))
		w.Must(os.Symlink("/cas/"+target, filepath.Join(dir, path)))
	}
}

var b, other, conv string
var squashed [3]string

// build mints every fixture. It is deterministic, so each stage mints the same
// oids again rather than carrying them.
func build() {
	m1 := "Add the core (" + salt + ")\n\nThe parser core,\nover two lines.\n"
	// No final newline: the one byte the tool supplies.
	m2 := "Test the core (" + salt + ")\n\nOne body line, and no final newline."
	m3 := "Document the core (" + salt + ")\n\nFirst paragraph.\n\nSecond paragraph.\n"

	bT := mktree("b-t", "f.txt=base")
	b = mint("b", bT, 1700000000, "base")
	other = mint("other", bT, 1700000050, "unrelated base")
	l1aT := mktree("l1a-t", "f.txt=base", "a.txt=one")
	l1a := mint("l1a", l1aT, 1700000100, "layer 1", b)
	l2aT := mktree("l2a-t", "f.txt=base", "a.txt=one", "b.txt=two")
	l2a := mint("l2a", l2aT, 1700000200, "layer 2", l1a)
	l1bT := mktree("l1b-t", "f.txt=base", "a.txt=one, revised")
	l1b := mint("l1b", l1bT, 1700000300, "revise layer 1", l1a)
	// What std/merge makes merging l1b up into l2a: [ours, theirs].
	l2bT := mktree("l2b-t", "f.txt=base", "a.txt=one, revised", "b.txt=two")
	l2b := mint("l2b", l2bT, 1700000400, "merge layer 1", l2a, l1b)
	l3T := mktree("l3-t", "f.txt=base", "a.txt=one, revised", "b.txt=two", "c.txt=docs")
	mint("l3", l3T, 1700000500, "layer 3", l2b)

	// `02_docs` sorts after `02-tests` in byte order ('-' is 0x2d, '_' 0x5f)
	// and before it in a collation that ignores punctuation; the tool takes
	// byte order, so this stack is in order.
	folder("merged", "00-base=b", "01-core=l1b", "02-tests=l2b", "02_docs=l3")
	folder("unmerged", "00-base=b", "01-core=l1b", "02-tests=l2a")
	folder("plain", "00-base=b", "01-core=l1b", "notes=b-t")
	messages("msgs", "01-core="+m1, "02-tests="+m2, "02_docs="+m3)
	messages("msgs-2", "01-core="+m1, "02-tests="+m2)
	messages("msgs-extra", "01-core="+m1, "03-docs="+m3)
	messages("msgs-plain", "01-core="+m1, "notes="+m3)
	messages("msgs-empty-file", "01-core="+m1, "02-tests=")
	messages("msgs-dir", "01-core="+m1, "02-tests/")
	messages("msgs-none")
	conversation("/tmp/conv")
	caos("put", "/tmp/conv", "/cas/conv")
	conv = caos("hash", "/cas/conv")

	// The commits the tool must mint, minted here. Equal oids check tree,
	// parent, author, committer and message at once.
	squashed[0] = putCommit("c1", header(l1bT, 1700000300, b)+m1)
	squashed[1] = putCommit("c2", header(l2bT, 1700000400, squashed[0])+m2+"\n")
	squashed[2] = putCommit("c3", header(l3T, 1700000500, squashed[1])+m3)
	folder("squashed", "01-core=c1", "02-tests=c2", "02_docs=c3")
}

// A case: the args, and what must hold. "ok <into>" means the stack lands at
// <into>; otherwise the refusal must say each of `says`.
type testCase struct {
	stack, messages, onto, into string
	ok                          bool
	says                        []string
}

func cases() []testCase {
	return []testCase{
		{"merged", "msgs", "b", "publish/mystack", true, nil},
		{"merged", "msgs", "b", "out/deep/mystack", true, nil},
		{"unmerged", "msgs-2", "b", "publish/mystack", false, []string{"does not contain", "02-tests", "01-core", "Merge"}},
		{"merged", "msgs-2", "other", "publish/mystack", false, []string{"does not contain", "01-core", "the base " + other}},
		{"merged", "msgs-extra", "b", "publish/mystack", false, []string{"the stack has no entry 03-docs"}},
		{"plain", "msgs-plain", "b", "publish/mystack", false, []string{"stack entry notes is not a source gitlink"}},
		{"merged", "msgs-none", "b", "publish/mystack", false, []string{"messages has no files"}},
		{"merged", "msgs-empty-file", "b", "publish/mystack", false, []string{"messages/02-tests is empty"}},
		{"merged", "msgs-dir", "b", "publish/mystack", false, []string{"messages/02-tests is not a file"}},
		{"merged", "msgs", "b", "mystack", false, []string{"is the stack itself"}},
		{"merged", "msgs", "b", "mystack/out", false, []string{"lies inside the stack"}},
		{"merged", "msgs", "b", "src/publish", false, []string{"passes through src, which is a commit"}},
		{"merged", "msgs", "b", "notes.txt", false, []string{"is a blob"}},
		{"merged", "msgs", "b", "publish", false, []string{"holds mystack, which is not a gitlink"}},
		{"merged", "msgs", "b", "../x", false, []string{"without . or .."}},
	}
}

// next is this test curried for stage s, the `then` of a case's run.
func next(s int) string {
	return caos("curry", "--base:@=/cas/args/base", "--worker1:@=/cas/args/worker1",
		"--stage="+strconv.Itoa(s), "--test-salt:@=/cas/args/test-salt", "--squash:@=/cas/args/squash")
}

func launch(i int) {
	c := cases()[i]
	w.Step(fmt.Sprintf("case %d: stack=%s messages=%s onto=%s into=%s", i, c.stack, c.messages, c.onto, c.into))
	request := caos("prepare-request", "--base:hash="+caos("hash", "/cas/args/squash"),
		"--in:@=/cas/conv", "--stack:@=/cas/"+c.stack, "--messages:@=/cas/"+c.messages,
		"--onto:@=/cas/"+c.onto, "--into="+c.into)
	caos("run-request-then", request, "--then:hash="+next(i))
}

func check(i int) {
	c := cases()[i]
	const r = "/cas/args/result"
	caos("get", r)
	for _, part := range []string{"prop", "out"} {
		_, err := os.Lstat(filepath.Join(r, part))
		w.True(err == nil, "case %d: the result has no %s", i, part)
	}
	out := read(filepath.Join(r, "out"))
	_, failedErr := os.Lstat(filepath.Join(r, "failed"))
	failed := failedErr == nil
	prop := caos("hash", filepath.Join(r, "prop"))
	if c.ok {
		w.True(!failed, "case %d was refused: %s", i, out)
		conversation("/tmp/expect", c.into+"=squashed")
		caos("put", "/tmp/expect", "/cas/expect")
		w.True(prop == caos("hash", "/cas/expect"),
			"case %d: the proposal is not the conversation with %s replaced by {01-core: C1, 02-tests: C2, 02_docs: C3}; out: %s", i, c.into, out)
		for _, commit := range squashed {
			w.True(strings.Contains(out, commit), "case %d: out does not name %s: %s", i, commit, out)
		}
		_, err := os.Lstat(filepath.Join(r, "message"))
		w.True(err == nil, "case %d: no commit message for the conversation", i)
		fmt.Fprintf(os.Stderr, "  ok: %s = {01-core: C1, 02-tests: C2, 02_docs: C3}, nothing else changed\n", c.into)
		return
	}
	w.True(failed, "case %d was not refused: %s", i, out)
	w.True(strings.HasPrefix(out, "FAILED: "), "case %d has no FAILED banner: %s", i, out)
	w.True(prop == conv, "case %d: a refusal changed the conversation", i)
	for _, s := range c.says {
		w.True(strings.Contains(out, s), "case %d does not say %q: %s", i, s, out)
	}
	fmt.Fprintf(os.Stderr, "  ok: %s", out)
}

func main() {
	w.Main(func() {
		salt = strings.TrimSpace(read("/cas/args/test-salt"))
		build()
		if _, err := os.Lstat("/cas/args/stage"); err != nil {
			launch(0)
			return
		}
		i := w.Check(strconv.Atoi(strings.TrimSpace(read("/cas/args/stage"))))
		check(i)
		if i+1 < len(cases()) {
			launch(i + 1)
			return
		}
		w.Report("squash-layers: ALL PASS\n")
	})
}
