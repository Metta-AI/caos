// std/flake-update: re-lock a flake by running nix itself, and return the new
// flake.lock. The tree in `in` supplies flake.nix and (usually) flake.lock;
// nothing else in it is read. Three shapes, the ones `nix flake update` has:
//
//	(no other arg)              every input moves to its tip
//	--input=<name>              that input alone moves to its tip
//	--input=<name> --rev=<sha>  that input is pinned to the commit
//
// The pin goes through `--override-input` with a flake reference built from
// the input's `original` in flake.lock (a github: or git+ reference), and the
// lock then gets that original back: the result is what `nix flake update
// <input>` would have written had the commit been the tip, not a lock whose
// original disagrees with flake.nix (which nix would re-resolve on its next
// read). A lock's `locked.rev` is always SOME commit of its original; this
// chooses which.
//
// The result is the flake.lock alone, a blob. The caller writes it where it
// belongs and keeps whatever else repeats the pin in step (a caos client
// repo's .caos-expr names the rev twice; std/flake-input-loader refuses a
// mismatch).
//
// A Go worker, but not on std/go: this image has to carry nix, so flake.nix
// compiles this file as a program of std/go's prelude module (DEEP-DEPS/prelude)
// and installs it as /worker. `caos/w` is the same prelude a script imports.
package main

import (
	"encoding/json"
	"fmt"
	"os"
	"os/exec"
	"path/filepath"
	"strings"

	"caos/w"
)

const (
	args = "/cas/args"
	ws   = "/tmp/ws"
)

func main() {
	w.Main(func() {
		// nix resolves $HOME; the worker runs as root in a minimal container
		// where a uid-0 lookup can fail, so point it at the /tmp it owns.
		w.Must(os.Setenv("HOME", "/tmp"))

		// `in` may be a tree or a commit (a source tree in the conversation):
		// resolve to the tree either way, fetch one level, then the two files.
		const root = "/cas/flake-root"
		w.Caos("resolve", w.Hash(args+"/in"), ".", root)
		w.Get(root)
		w.True(w.Check(os.Lstat(root)).IsDir(), "in is not a tree")
		w.True(w.Exists(filepath.Join(root, "flake.nix")), "no flake.nix in the tree")
		w.Must(os.MkdirAll(ws, 0o755))
		copyIn := func(name string) bool {
			if !w.Exists(filepath.Join(root, name)) {
				return false
			}
			w.Must(os.WriteFile(filepath.Join(ws, name), []byte(w.Read(filepath.Join(root, name))), 0o644))
			return true
		}
		copyIn("flake.nix")
		hasLock := copyIn("flake.lock")

		input, rev := arg("input"), arg("rev")
		w.True(rev == "" || input != "", "rev needs input: which input is pinned to %s?", rev)
		switch {
		case rev != "":
			w.True(hasLock, "pinning %s needs a flake.lock to read its reference from", input)
			pin(input, rev)
		case input != "":
			nix("flake", "update", input)
		default:
			nix("flake", "update")
		}
		w.True(w.Exists(filepath.Join(ws, "flake.lock")), "nix left no flake.lock")
		w.Put(filepath.Join(ws, "flake.lock"), "/cas/out")
	})
}

// arg is an optional string arg, "" when the caller left it unbound. Args are
// lazy placeholders: an unbound one has no entry at all.
func arg(name string) string {
	p := filepath.Join(args, name)
	if !w.Exists(p) {
		return ""
	}
	return strings.TrimSpace(w.Read(p))
}

// nix runs nix in the workspace as the single user it is here: root in a
// container with no build users and no privilege to build a sandbox. Nothing is
// built, but nix checks the settings all the same. Its output is the log.
func nix(a ...string) {
	cmd := exec.Command("nix", append([]string{
		"--extra-experimental-features", "nix-command flakes",
		"--option", "build-users-group", "",
		"--option", "sandbox", "false",
	}, a...)...)
	cmd.Dir = ws
	cmd.Stdout, cmd.Stderr = os.Stderr, os.Stderr
	if err := cmd.Run(); err != nil {
		w.True(false, "nix %s: %v", strings.Join(a, " "), err)
	}
}

// pin moves one input to a commit: nix re-locks it through an override built
// from its `original`, and the original is then restored (see the header).
func pin(input, rev string) {
	lock := readLock()
	nodes := lock["nodes"].(map[string]any)
	inputs := nodes[lock["root"].(string)].(map[string]any)["inputs"].(map[string]any)
	n, ok := inputs[input]
	w.True(ok, "no input %s in flake.lock", input)
	node, ok := n.(string)
	w.True(ok, "input %s follows %v; pin that input instead", input, n)
	orig := nodes[node].(map[string]any)["original"].(map[string]any)

	var ref string
	switch orig["type"] {
	case "github":
		ref = fmt.Sprintf("github:%s/%s/%s", orig["owner"], orig["repo"], rev)
	case "git":
		url := orig["url"].(string)
		sep := "?"
		if strings.Contains(url, "?") {
			sep = "&"
		}
		ref = "git+" + url + sep + "rev=" + rev
		if r, ok := orig["ref"]; ok {
			ref += fmt.Sprintf("&ref=%s", r)
		}
	default:
		w.True(false, "input %s is a %v reference; only github and git can be pinned to a commit", input, orig["type"])
	}
	fmt.Fprintf(os.Stderr, "flake-update: pinning %s to %s\n", input, ref)
	nix("flake", "update", input, "--override-input", input, ref)

	lock = readLock()
	entry := lock["nodes"].(map[string]any)[node].(map[string]any)
	got := entry["locked"].(map[string]any)["rev"]
	w.True(got == rev, "nix locked %s at %v, not %s", input, got, rev)
	entry["original"] = orig
	writeLock(lock)
}

// readLock parses the workspace lock. Numbers stay as written (UseNumber), so
// a lastModified goes back out byte for byte.
func readLock() map[string]any {
	d := json.NewDecoder(strings.NewReader(string(w.Check(os.ReadFile(filepath.Join(ws, "flake.lock"))))))
	d.UseNumber()
	var lock map[string]any
	w.Must(d.Decode(&lock))
	return lock
}

// writeLock writes the lock the way nix does: keys sorted, two-space indent,
// no HTML escaping (a git+ url carries `&`), a trailing newline.
func writeLock(lock map[string]any) {
	var b strings.Builder
	e := json.NewEncoder(&b)
	e.SetEscapeHTML(false)
	e.SetIndent("", "  ")
	w.Must(e.Encode(lock))
	w.Must(os.WriteFile(filepath.Join(ws, "flake.lock"), []byte(b.String()), 0o644))
}
