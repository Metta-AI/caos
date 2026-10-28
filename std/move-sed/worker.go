// The `move-sed` tool's worker. Its docs live in the sibling `.caos-expr`
// here-string, not in this header (SPEC, "CaosTools").
//
// A writer run on the conversation (SPEC, "Writers"). Args, under /cas/args:
//
//	in           the conversation tree it was run on
//	dir          the conversation directory whose entries to rename
//	pattern      a Go regular expression, matched against each entry's name
//	replacement  the new name, with $1 / ${name} for the pattern's groups
//	dry-run      optional: "true" lists the renames and changes nothing
//
// The result is {prop, out, message, failed?}. `prop` is the conversation with
// `dir` rebuilt: every matching entry under its new name, everything else
// linked as it was, so a layer and its sidecar files move in ONE proposal
// (design/publish-stack.md, "move-sed"). Nothing in the conversation is
// checked out: the rebuilt folders are links to the CAS entries they keep.
//
// A refusal proposes `in` unchanged and marks `failed`: the agent reads it
// and corrects the call. Only a broken environment is a job error.
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
	in     = "/cas/args/in"
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

// splitDir checks the directory's conversation path and splits it. The empty
// string and "." are the root.
func splitDir(raw string) []string {
	path := strings.Trim(strings.TrimSpace(raw), "/")
	if path == "" || path == "." {
		return nil
	}
	parts := strings.Split(path, "/")
	for _, part := range parts {
		if part == "" || part == "." || part == ".." {
			refuse("dir must be a conversation-relative path without . or .., not %q", raw)
		}
	}
	return parts
}

// walk fetches the directory level by level and returns its CAS path. A path
// through a file is a mistake, and one through a gitlink is code: this tool
// rebuilds conversation folders, never commits.
func walk(parts []string) string {
	dir := in
	for i, part := range parts {
		next := filepath.Join(dir, part)
		sofar := strings.Join(parts[:i+1], "/")
		if !w.Exists(next) {
			refuse("dir: no such path: %s", sofar)
		}
		switch k := w.Caos("kind", next); k {
		case "tree":
		case "commit":
			refuse("dir: %s is a source tree. This tool renames entries of the conversation tree; for files inside code use move.", sofar)
		default:
			refuse("dir: %s is a %s, not a directory", sofar, k)
		}
		w.Get(next)
		dir = next
	}
	return dir
}

type rename struct{ from, to string }

// plan computes the renames and refuses, before anything changes, everything
// that would make the result ambiguous or lossy.
func plan(names []string, atRoot bool, pattern, replacement string) []rename {
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
		if atRoot && (name == ".caos" || to == ".caos") {
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
	if src != "" && w.Exists(filepath.Join(src, head)) {
		child = filepath.Join(src, head)
	}
	rebuild(child, filepath.Join(dst, head), parts[1:], node)
}

// finish writes the writer result and stores it at /cas/out.
func finish(prop, out, message string, failed bool) {
	w.Must(os.MkdirAll(result, 0o755))
	w.Must(os.WriteFile(filepath.Join(result, "out"), []byte(out), 0o644))
	if message != "" {
		w.Must(os.WriteFile(filepath.Join(result, "message"), []byte(message), 0o644))
	}
	if failed {
		w.Must(os.WriteFile(filepath.Join(result, "failed"), []byte(out), 0o644))
	}
	w.Must(os.Symlink(prop, filepath.Join(result, "prop")))
	w.Put(result, "/cas/out")
}

func main() {
	w.Main(func() {
		w.Get(in)
		defer func() {
			r := recover()
			if r == nil {
				return
			}
			no, ok := r.(refusal)
			if !ok {
				panic(r)
			}
			finish(in, "FAILED: "+no.msg+"\n", "", true)
		}()

		rawDir, _ := arg("dir")
		parts := splitDir(rawDir)
		pattern := required("pattern")
		replacement, _ := arg("replacement")
		dry, _ := arg("dry-run")
		dryRun := strings.TrimSpace(dry) == "true"

		dir := walk(parts)
		names := entries(dir)
		renames := plan(names, len(parts) == 0, pattern, replacement)
		label := strings.Join(parts, "/")
		if label == "" {
			label = "the conversation root"
		}

		var report strings.Builder
		if dryRun {
			fmt.Fprintf(&report, "dry run: %d rename(s) in %s, nothing changed:\n", len(renames), label)
		} else {
			fmt.Fprintf(&report, "renamed %d entr%s in %s:\n", len(renames), map[bool]string{true: "y", false: "ies"}[len(renames) == 1], label)
		}
		for _, r := range renames {
			fmt.Fprintf(&report, "  %s -> %s\n", r.from, r.to)
		}
		if dryRun {
			finish(in, report.String(), "", false)
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
		prop := "/cas/renamed"
		if len(parts) > 0 {
			rebuild(in, "/tmp/move-sed-prop", parts, "/cas/renamed")
			w.Put("/tmp/move-sed-prop", "/cas/prop")
			prop = "/cas/prop"
		}
		finish(prop, report.String(), fmt.Sprintf("move-sed: %s: %d rename(s)\n", label, len(renames)), false)
	})
}
