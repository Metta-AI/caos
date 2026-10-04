// tests/squash-stack — a WORKER test: no client, no repo.
//
// Runs std/squash-stack over a conversation tree holding the stack shape
// design/stacks.md describes: a base B at 00-base, layer 1 on B, layer 2
// copied from layer 1, then layer 1 changed and merged up into layer 2, so
// layer 2's tip is a merge commit; a third layer sits on layer 2. A plan must
// give one single-parent commit per layer, B <- C1 <- C2 <- C3, each with its
// commit's tree, author and committer and its plan message.
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

// A case: a plan, and what must hold. A success names where the stack lands;
// a refusal, the words its report must carry.
type testCase struct {
	name, plan, into string
	says             []string
}

var b, other, l2b, conv string
var cases []testCase

// plan is a plan's text from its lines.
func plan(lines ...string) string { return strings.Join(lines, "\n") + "\n" }

// conversation stages the fixture conversation at dir, with each
// <path>=<cas-name> in extra put in place of what was there.
func conversation(dir string, extra ...string) {
	w.Must(os.RemoveAll(dir))
	for _, d := range []string{"publish/mystack", "publish/other", ".caos", "plans"} {
		w.Must(os.MkdirAll(filepath.Join(dir, d), 0o755))
	}
	for _, l := range []string{"mystack=merged", "unmerged=unmerged", "plain=plain", "src=b",
		"publish/mystack/01-core=b"} {
		entry, target, _ := strings.Cut(l, "=")
		w.Must(os.Symlink("/cas/"+target, filepath.Join(dir, entry)))
	}
	w.Must(os.WriteFile(filepath.Join(dir, "notes.txt"), []byte("conversation notes\n"), 0o644))
	w.Must(os.WriteFile(filepath.Join(dir, ".caos/meta"), []byte("protocol\n"), 0o644))
	w.Must(os.WriteFile(filepath.Join(dir, "publish/other/f.txt"), []byte("another stack\n"), 0o644))
	for _, c := range cases {
		w.Must(os.WriteFile(filepath.Join(dir, "plans", c.name), []byte(c.plan), 0o644))
	}
	for _, l := range extra {
		path, target, _ := strings.Cut(l, "=")
		w.Must(os.RemoveAll(filepath.Join(dir, path)))
		w.Must(os.MkdirAll(filepath.Dir(filepath.Join(dir, path)), 0o755))
		w.Must(os.Symlink("/cas/"+target, filepath.Join(dir, path)))
	}
}

var squashed [3]string

// build mints every fixture. It is deterministic, so each stage mints the same
// oids again rather than carrying them.
func build() {
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
	l2b = mint("l2b", l2bT, 1700000400, "merge layer 1", l2a, l1b)
	l3T := mktree("l3-t", "f.txt=base", "a.txt=one, revised", "b.txt=two", "c.txt=docs")
	mint("l3", l3T, 1700000500, "layer 3", l2b)
	folder("merged", "00-base=b", "01-core=l1b", "02-tests=l2b", "02_docs=l3")
	folder("unmerged", "00-base=b", "01-core=l1b", "02-tests=l2a")
	folder("plain", "00-base=b", "01-core=l1b", "notes=b-t")

	// The messages: a body after a blank line, and a paragraph break in it.
	m1 := []string{"Add the core (" + salt + ")", "", "The parser core,", "over two lines."}
	m2 := []string{"Test the core (" + salt + ")"}
	m3 := []string{"Document the core (" + salt + ")", "", "First paragraph.", "", "Second paragraph."}
	block := func(name, commit string, message []string) []string {
		lines := []string{"layer=" + name, "commit=" + commit}
		for _, m := range message {
			lines = append(lines, "message="+m)
		}
		return lines
	}
	stack := func(onto, into string, layers ...[]string) string {
		lines := []string{"onto=" + onto, "into=" + into}
		for _, l := range layers {
			lines = append(lines, l...)
		}
		return plan(lines...)
	}
	// Layer 2 is named by hash, and layer 3 is published under a name of its
	// own: the plan, not the stack folder, decides both.
	good := func(onto, into string) string {
		return stack(onto, into, block("01-core", "mystack/01-core", m1),
			block("02-tests", l2b, m2), block("03-docs", "mystack/02_docs", m3))
	}
	cases = []testCase{
		{"ok", good("mystack/00-base", "publish/mystack"), "publish/mystack", nil},
		{"ok-new-path", good(b, "out/deep/mystack"), "out/deep/mystack", nil},
		{"unmerged", stack("mystack/00-base", "publish/mystack", block("01-core", "unmerged/01-core", m1),
			block("02-tests", "unmerged/02-tests", m2)), "", []string{"layer 02-tests", "does not contain layer 01-core", "Merge"}},
		{"stale-onto", stack(other, "publish/mystack", block("01-core", "mystack/01-core", m1)),
			"", []string{"layer 01-core", "does not contain onto (" + other + ")"}},
		{"out-of-order", stack("mystack/00-base", "publish/mystack", block("02-tests", "mystack/02-tests", m2),
			block("01-core", "mystack/01-core", m1)), "", []string{"layer 01-core", "does not contain layer 02-tests"}},
		{"unknown-key", plan("onto=mystack/00-base", "into=publish/mystack", "branch=x"), "", []string{"plan line 3: unknown key \"branch\""}},
		{"late-onto", plan("into=publish/mystack", "layer=01-core", "onto=mystack/00-base"), "", []string{"onto must come before the first layer="}},
		{"twice", stack("mystack/00-base", "publish/mystack", block("01-core", "mystack/01-core", m1),
			block("01-core", "mystack/02-tests", m2)), "", []string{"layer 01-core is given twice"}},
		{"no-commit", plan("onto=mystack/00-base", "into=publish/mystack", "layer=01-core", "message=x"), "", []string{"layer 01-core (plan line 3) has no commit= line"}},
		{"no-message", plan("onto=mystack/00-base", "into=publish/mystack", "layer=01-core", "commit=mystack/01-core", "message="), "", []string{"layer 01-core (plan line 3) has no message= text"}},
		{"no-onto", plan("into=publish/mystack", "layer=01-core", "commit=mystack/01-core", "message=x"), "", []string{"the plan has no onto="}},
		{"tree-commit", stack("mystack/00-base", "publish/mystack", block("notes", "plain/notes", m1)), "", []string{"(plain/notes) is a tree, not a gitlink"}},
		{"missing-commit", stack("mystack/00-base", "publish/mystack", block("01-core", "mystack/nope", m1)), "", []string{"no such path: mystack/nope"}},
		{"into-stack", good("mystack/00-base", "mystack"), "", []string{"into (mystack) holds mystack/00-base, which the plan reads"}},
		{"into-source-tree", good("mystack/00-base", "src/publish"), "", []string{"passes through src, which is a commit"}},
		{"into-folder", good("mystack/00-base", "publish"), "", []string{"into (publish) holds mystack, which is not a gitlink"}},
	}
	conversation("/tmp/conv")
	caos("put", "/tmp/conv", "/cas/conv")
	conv = caos("hash", "/cas/conv")

	// The commits the tool must mint, minted here. Equal oids check tree,
	// parent, author, committer and message at once.
	msg := func(lines []string) string { return strings.Join(lines, "\n") + "\n" }
	squashed[0] = putCommit("c1", header(l1bT, 1700000300, b)+msg(m1))
	squashed[1] = putCommit("c2", header(l2bT, 1700000400, squashed[0])+msg(m2))
	squashed[2] = putCommit("c3", header(l3T, 1700000500, squashed[1])+msg(m3))
	folder("squashed", "01-core=c1", "02-tests=c2", "03-docs=c3")
}

// next is this test curried for stage s, the `then` of a case's run.
func next(s int) string {
	return caos("curry", "--base:@=/cas/args/base", "--worker1:@=/cas/args/worker1",
		"--stage="+strconv.Itoa(s), "--test-salt:@=/cas/args/test-salt", "--squash:@=/cas/args/squash")
}

// launch runs case i. The last case names a plan that does not exist.
func launch(i int) {
	path := "plans/missing"
	if i < len(cases) {
		path = "plans/" + cases[i].name
	}
	w.Step(fmt.Sprintf("case %d: %s", i, path))
	request := caos("prepare-request", "--base:hash="+caos("hash", "/cas/args/squash"),
		"--in:@=/cas/conv", "--plan="+path)
	caos("run-request-then", request, "--then:hash="+next(i))
}

func check(i int) {
	c := testCase{name: "missing", says: []string{"plan: no such path: plans/missing"}}
	if i < len(cases) {
		c = cases[i]
	}
	const r = "/cas/args/result"
	caos("get", r)
	for _, part := range []string{"prop", "out"} {
		_, err := os.Lstat(filepath.Join(r, part))
		w.True(err == nil, "case %s: the result has no %s", c.name, part)
	}
	out := read(filepath.Join(r, "out"))
	_, failedErr := os.Lstat(filepath.Join(r, "failed"))
	failed := failedErr == nil
	prop := caos("hash", filepath.Join(r, "prop"))
	if c.into != "" {
		w.True(!failed, "case %s was refused: %s", c.name, out)
		conversation("/tmp/expect", c.into+"=squashed")
		caos("put", "/tmp/expect", "/cas/expect")
		w.True(prop == caos("hash", "/cas/expect"),
			"case %s: the proposal is not the conversation with %s replaced by {01-core: C1, 02-tests: C2, 03-docs: C3}; out: %s", c.name, c.into, out)
		for _, commit := range squashed {
			w.True(strings.Contains(out, commit), "case %s: out does not name %s: %s", c.name, commit, out)
		}
		_, err := os.Lstat(filepath.Join(r, "message"))
		w.True(err == nil, "case %s: no commit message for the conversation", c.name)
		fmt.Fprintf(os.Stderr, "  ok: %s = {01-core: C1, 02-tests: C2, 03-docs: C3}, nothing else changed\n", c.into)
		return
	}
	w.True(failed, "case %s was not refused: %s", c.name, out)
	w.True(strings.HasPrefix(out, "FAILED: "), "case %s has no FAILED banner: %s", c.name, out)
	w.True(prop == conv, "case %s: a refusal changed the conversation", c.name)
	for _, s := range c.says {
		w.True(strings.Contains(out, s), "case %s does not say %q: %s", c.name, s, out)
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
		if i < len(cases) {
			launch(i + 1)
			return
		}
		w.Report("squash-stack: ALL PASS\n")
	})
}
