// The `move-sed` tool's worker. Its docs live in the sibling `.caos-expr`
// here-string, not in this header (SPEC, "CaosTools").
//
// A writer under the root contract (design/tool-contract.md). Args, under
// /cas/args:
//
//	root         the directory whose entries to rename (a source tree's
//	             commit is renamed inside its code tree)
//	pattern      a Go regular expression, matched against each entry's name
//	replacement  the new name, with $1 / ${name} for the pattern's groups
//	dry-run      optional: "true" lists the renames and changes nothing
//
// The result is {stdout, root, commit-message} or {stdout, failed}. `root` is
// the directory rebuilt: every matching entry under its new name, everything
// else linked as it was, so a layer and its sidecar files move in ONE change
// (design/publish-stack.md, "move-sed"). Nothing is checked out: the rebuilt
// folder links to the CAS entries it keeps. The harness puts it back.
//
// A refusal is a value the agent reads and corrects. Only a broken
// environment is a job error.
package main

import (
	"fmt"
	"os"
	"path/filepath"
	"regexp"
	"strings"

	"caos/w"
)

const (
	root   = "/cas/args/root"
	result = "/tmp/move-sed-result"
)

// refusal is a problem with the input, answered as a value.
type refusal struct{ msg string }

func refuse(format string, a ...any) { panic(refusal{fmt.Sprintf(format, a...)}) }

// arg reads an optional argument without its trailing line break, which a
// value written from a file carries and a name must not.
func arg(name string) (string, bool) {
	path := "/cas/args/" + name
	if !w.Exists(path) {
		return "", false
	}
	return strings.TrimRight(w.Read(path), "\r\n"), true
}

func required(name string) string {
	value, ok := arg(name)
	if !ok || value == "" {
		refuse("%s is required", name)
	}
	return value
}

// entries lists a CAS folder's names in byte order, after fetching its listing.
func entries(dir string) []string {
	w.Get(dir)
	var names []string
	for _, e := range w.Check(os.ReadDir(dir)) {
		names = append(names, e.Name())
	}
	return names
}

// directory is the root as a folder: a tree as it is, a source tree's commit
// as its code tree.
func directory() string {
	if !w.Exists(root) {
		refuse("nothing is at the root path: root is the directory whose entries to rename, such as sources/repo")
	}
	switch k := w.Caos("kind", root); k {
	case "tree":
		w.Get(root)
		return root
	case "commit":
		const code = "/cas/move-sed-code"
		w.Caos("resolve", w.Caos("hash", root), "", code)
		w.Get(code)
		return code
	default:
		refuse("the root is a %s, not a directory", k)
		return ""
	}
}

type rename struct{ from, to string }

// plan computes the renames and refuses, before anything changes, everything
// that would make the result ambiguous or lossy.
func plan(names []string, pattern, replacement string) []rename {
	if m := regexp.MustCompile(`\\[0-9]`).FindString(replacement); m != "" {
		refuse("replacement %q uses sed's %s. Go's syntax is $%s (or ${%s}); %s would be written into the name literally.",
			replacement, m, m[1:], m[1:], m)
	}
	re, err := regexp.Compile(pattern)
	if err != nil {
		refuse("pattern is not a valid Go regular expression: %v", err)
	}
	have := map[string]bool{}
	for _, name := range names {
		have[name] = true
	}
	var renames []rename
	moving := map[string]bool{}
	taken := map[string]string{}
	matched := 0
	for _, name := range names {
		if !re.MatchString(name) {
			continue
		}
		matched++
		to := re.ReplaceAllString(name, replacement)
		if to == name {
			continue
		}
		// The worker cannot tell the session's top from any other folder, and
		// a .caos anywhere is metadata: never rename or shadow one.
		if name == ".caos" || to == ".caos" {
			refuse(".caos is protocol metadata and cannot be renamed or shadowed; anchor the pattern so it does not match it")
		}
		if to == "" || to == "." || to == ".." || strings.Contains(to, "/") {
			refuse("%s would become %q, which is not one directory entry name", name, to)
		}
		if other, dup := taken[to]; dup {
			refuse("both %s and %s would become %s", other, name, to)
		}
		taken[to] = name
		moving[name] = true
		renames = append(renames, rename{name, to})
	}
	if matched == 0 {
		refuse("no entry matches %q (the entries are: %s)", pattern, strings.Join(names, ", "))
	}
	if len(renames) == 0 {
		refuse("every entry matching %q keeps its name under %q", pattern, replacement)
	}
	for _, r := range renames {
		if have[r.to] && !moving[r.to] {
			refuse("%s would become %s, which exists and is not being renamed", r.from, r.to)
		}
	}
	return renames
}

// finish writes the result and stores it at /cas/out: the new directory as
// `root`, or `failed` and nothing to apply.
func finish(newRoot, out, message string, failed bool) {
	w.Must(os.MkdirAll(result, 0o755))
	w.Must(os.WriteFile(filepath.Join(result, "stdout"), []byte(out), 0o644))
	if failed {
		w.Must(os.WriteFile(filepath.Join(result, "failed"), []byte("move-sed refused\n"), 0o644))
	} else {
		w.Must(os.Symlink(newRoot, filepath.Join(result, "root")))
		if message != "" {
			w.Must(os.WriteFile(filepath.Join(result, "commit-message"), []byte(message), 0o644))
		}
	}
	w.Put(result, "/cas/out")
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
			finish("", "move-sed: "+no.msg+"\n", "", true)
		}()

		dir := directory()
		pattern := required("pattern")
		replacement, _ := arg("replacement")
		dry, _ := arg("dry-run")
		dryRun := strings.TrimSpace(dry) == "true"

		names := entries(dir)
		renames := plan(names, pattern, replacement)

		var report strings.Builder
		if dryRun {
			fmt.Fprintf(&report, "dry run: %d rename(s), nothing changed:\n", len(renames))
		} else {
			fmt.Fprintf(&report, "renamed %d entr%s:\n", len(renames), map[bool]string{true: "y", false: "ies"}[len(renames) == 1])
		}
		for _, r := range renames {
			fmt.Fprintf(&report, "  %s -> %s\n", r.from, r.to)
		}
		if dryRun {
			// Writing back what was there changes nothing.
			finish(dir, report.String(), "", false)
			return
		}

		// The directory, rebuilt: each entry linked under its new name.
		newName := map[string]string{}
		for _, r := range renames {
			newName[r.from] = r.to
		}
		const staged = "/tmp/move-sed-dir"
		w.Must(os.MkdirAll(staged, 0o755))
		for _, name := range names {
			target := name
			if to, ok := newName[name]; ok {
				target = to
			}
			w.Must(os.Symlink(filepath.Join(dir, name), filepath.Join(staged, target)))
		}
		w.Put(staged, "/cas/renamed")
		finish("/cas/renamed", report.String(), fmt.Sprintf("move-sed: %d rename(s)\n", len(renames)), false)
	})
}
