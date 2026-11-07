// tests/stack-sync-merge — a WORKER test of std/stack-sync's `sync` stage, the
// part that does git's work: given what the remote said (in the state, as the
// earlier stages leave it), a stack of real commits is brought up to date and
// returned as a new directory. No remote is read: each case runs the stage
// directly.
//
//   - restack: the trunk moved, so 00-base moves to it; the published 01 is in
//     sync with its branch and is restacked onto the new base by a merge;
//     the unpublished 02 is restacked onto the new 01.
//   - conflict: 01 changed the line the trunk changed, so the sync stops at
//     01, naming the merge to run, and returns 00-base moved and 01 as it was.
//
// Staged: each case is checked in the `then` of its own run.
package main

import (
	"encoding/json"
	"fmt"
	"os"
	"os/exec"
	"path/filepath"
	"sort"
	"strconv"
	"strings"

	"caos/w"
)

var made int

// commit puts a commit of `files` with `parents` into caos and returns its
// hash. The salt keeps each run's commits its own.
func commit(files map[string]string, message string, parents ...string) string {
	made++
	dir := fmt.Sprintf("/tmp/tree-%d", made)
	w.Must(os.MkdirAll(dir, 0o755))
	for name, content := range files {
		w.Must(os.WriteFile(filepath.Join(dir, name), []byte(content), 0o644))
	}
	w.Put(dir, fmt.Sprintf("/cas/tree-%d", made))
	var body strings.Builder
	fmt.Fprintf(&body, "tree %s\n", w.Caos("hash", fmt.Sprintf("/cas/tree-%d", made)))
	for _, p := range parents {
		fmt.Fprintf(&body, "parent %s\n", p)
	}
	fmt.Fprintf(&body, "author t <t@t> 1700000000 +0000\ncommitter t <t@t> 1700000000 +0000\n\n%s (%s)\n",
		message, salt())
	file := fmt.Sprintf("/tmp/commit-%d", made)
	w.Must(os.WriteFile(file, []byte(body.String()), 0o644))
	return w.Caos("put-commit", file, fmt.Sprintf("/cas/commit-%d", made))
}

func salt() string { return strings.TrimSpace(w.Read("/cas/args/test-salt")) }

type layer struct {
	Name   string `json:"name"`
	Branch string `json:"branch"`
	Commit string `json:"commit"`
	Remote string `json:"remote"`
}

// stack puts a stack directory: 00-base, the layers, and the files beside.
func stack(base string, layers []layer, files map[string]string) {
	const dir = "/tmp/stack"
	w.Must(os.RemoveAll(dir))
	w.Must(os.MkdirAll(dir, 0o755))
	link := func(name, hash string) {
		at := "/cas/link-" + name
		w.Caos("get-hash", hash, at)
		w.Must(os.Symlink(at, filepath.Join(dir, name)))
	}
	link("00-base", base)
	for _, l := range layers {
		link(l.Name, l.Commit)
	}
	for name, content := range files {
		w.Must(os.WriteFile(filepath.Join(dir, name), []byte(content), 0o644))
	}
	w.Put(dir, "/cas/stack")
}

// files lists a commit's files with their contents, through git.
func files(hash string) map[string]string {
	const repo = "/tmp/check"
	if _, err := os.Stat(repo); err != nil {
		// `git -C` needs the directory to exist, even for `init`.
		w.Must(os.MkdirAll(repo, 0o755))
		for _, args := range [][]string{{"init", "-q"}, {"remote", "add", "origin", os.Getenv("CAOS_SERVER_URL")}} {
			w.Must(exec.Command("git", append([]string{"-C", repo}, args...)...).Run())
		}
	}
	git := func(args ...string) string {
		out, err := exec.Command("git", append([]string{"-C", repo}, args...)...).CombinedOutput()
		w.True(err == nil, "git %v: %v: %s", args, err, out)
		return string(out)
	}
	git("fetch", "-q", "origin", hash)
	out := map[string]string{}
	for _, name := range strings.Fields(git("ls-tree", "--name-only", hash)) {
		out[name] = git("show", hash+":"+name)
	}
	return out
}

func keys(m map[string]string) string {
	var out []string
	for k := range m {
		out = append(out, k)
	}
	sort.Strings(out)
	return strings.Join(out, " ")
}

func next(stage int) string {
	return w.Caos("curry", "--base:@=/cas/args/base", "--worker1:@=/cas/args/worker1",
		"--stage="+strconv.Itoa(stage), "--test-salt:@=/cas/args/test-salt",
		"--stack-sync:@=/cas/args/stack-sync", "--want:@=/cas/want")
}

// launch runs the sync stage on a stack, with what the case expects carried
// to its check as `want`.
func launch(i int, base, trunk string, layers []layer, want map[string]string) {
	w.Step(fmt.Sprintf("case %d", i))
	stack(base, layers, map[string]string{
		"00-base.source.json": `{"repository": "https://example.invalid/r.git", "commit": "` + base + `"}` + "\n",
		"01-feature.pr.md":    "Feature\n",
	})
	state := map[string]any{"repository": "https://example.invalid/r.git", "trunk": "main",
		"base": base, "trunk_head": trunk, "layers": layers}
	w.Must(os.WriteFile("/tmp/state", w.Check(json.Marshal(state)), 0o644))
	w.Put("/tmp/state", "/cas/state")
	w.Must(os.WriteFile("/tmp/empty", []byte("[]"), 0o644))
	w.Put("/tmp/empty", "/cas/empty")
	w.Must(os.WriteFile("/tmp/want", w.Check(json.Marshal(want)), 0o644))
	w.Put("/tmp/want", "/cas/want")
	request := w.Caos("prepare-request", "--base:@=/cas/args/stack-sync", "--stage=sync",
		"--state:@=/cas/state", "--root:@=/cas/stack", "--result:@=/cas/empty",
		"--request-id=stack-sync-merge-"+salt()+"-"+strconv.Itoa(i))
	w.Caos("run-request-then", request, "--then:hash="+next(i))
}

func main() {
	w.Main(func() {
		if !w.Exists("/cas/args/stage") {
			base := commit(map[string]string{"a.txt": "one\ntwo\n"}, "base")
			trunk := commit(map[string]string{"a.txt": "one\ntwo\n", "trunk.txt": "trunk\n"}, "trunk", base)
			feature := commit(map[string]string{"a.txt": "one\ntwo\n", "feature.txt": "feature\n"}, "feature", base)
			tests := commit(map[string]string{"a.txt": "one\ntwo\n", "feature.txt": "feature\n", "tests.txt": "tests\n"}, "tests", feature)
			launch(0, base, trunk, []layer{
				{Name: "01-feature", Branch: "feature", Commit: feature, Remote: feature},
				{Name: "02-tests", Commit: tests},
			}, map[string]string{"trunk": trunk, "feature": feature, "tests": tests})
			return
		}
		var want map[string]string
		w.Must(json.Unmarshal([]byte(w.Read("/cas/args/want")), &want))
		const r = "/cas/args/result"
		w.Get(r)
		out := w.Read(r + "/stdout")
		fmt.Fprintf(os.Stderr, "%s", out)
		w.True(!w.Exists(r+"/failed"), "the sync failed:\n%s", out)
		w.True(w.Exists(r+"/root"), "the sync returned no root:\n%s", out)
		w.Get(r + "/root")
		got := func(name string) string { return w.Caos("hash", r+"/root/"+name) }
		w.True(got("00-base") == want["trunk"], "00-base is %s, not the trunk's head:\n%s", got("00-base"), out)
		w.True(strings.Contains(w.Read(r+"/root/00-base.source.json"), want["trunk"]), "00-base.source.json does not name the new base")
		w.True(w.Exists(r+"/root/01-feature.pr.md"), "the description was dropped")
		switch stage := strings.TrimSpace(w.Read("/cas/args/stage")); stage {
		case "0":
			for _, phrase := range []string{"the trunk moved", "01-feature  feature: in sync, restacked onto 00-base",
				"02-tests  restacked onto 01-feature", "Next: run caos-std/stack-publish"} {
				w.True(strings.Contains(out, phrase), "case 0 does not say %q:\n%s", phrase, out)
			}
			f, t := files(got("01-feature")), files(got("02-tests"))
			w.True(keys(f) == "a.txt feature.txt trunk.txt", "01-feature after the restack holds %s", keys(f))
			w.True(keys(t) == "a.txt feature.txt tests.txt trunk.txt", "02-tests after the restack holds %s", keys(t))

			base := commit(map[string]string{"a.txt": "one\ntwo\n"}, "base 2")
			trunk := commit(map[string]string{"a.txt": "one\nTRUNK\n"}, "trunk 2", base)
			feature := commit(map[string]string{"a.txt": "one\nFEATURE\n"}, "feature 2", base)
			launch(1, base, trunk, []layer{{Name: "01-feature", Commit: feature}},
				map[string]string{"trunk": trunk, "feature": feature})
		case "1":
			for _, phrase := range []string{"the trunk moved", "FAILED: restacking 01-feature onto 00-base conflicts in a.txt",
				"caos-std/merge", `"root": "<stack>/01-feature"`, "The layers below it are synced"} {
				w.True(strings.Contains(out, phrase), "case 1 does not say %q:\n%s", phrase, out)
			}
			w.True(got("01-feature") == want["feature"], "the conflicting layer was changed")
			w.Report("stack-sync-merge: ALL PASS\n")
		default:
			w.True(false, "unknown stage %q", stage)
		}
	})
}
