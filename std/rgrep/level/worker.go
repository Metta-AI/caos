// rgrep's per-directory worker: grep the files of ONE directory. tree-map runs
// it with `--in` a tree of just the files, and supplies the recursion.
//
// The result is a tree with one file per MATCHING input file, holding its
// matches as `linenum:line` lines; a file with none is absent. An `--in` that is
// a single file is the file-scoped grep: the result is the matches blob itself.
// Binary files (a NUL in the first 8KB) and symlinks are skipped, like grep -I.
package main

import (
	"bytes"
	"fmt"
	"os"
	"path/filepath"
	"regexp"
	"strings"

	"caos/w"
)

func main() {
	w.Main(func() {
		pattern := w.Read("/cas/args/pattern")
		re, err := regexp.Compile(pattern)
		w.True(err == nil, "invalid pattern %q: %v (the caller validates)", pattern, err)

		const in = "/cas/args/in"
		w.GetAll(in) // one directory's files, which is all of it
		if !w.Check(os.Lstat(in)).IsDir() {
			found, _ := matches(re, in)
			w.Must(os.WriteFile("/tmp/matches", []byte(found), 0o644))
			w.Put("/tmp/matches", "/cas/out")
			return
		}

		const out = "/tmp/rgrep-out"
		w.Must(os.RemoveAll(out))
		w.Must(os.MkdirAll(out, 0o755))
		for _, e := range w.Check(os.ReadDir(in)) {
			if e.Type()&os.ModeSymlink != 0 || e.IsDir() {
				continue
			}
			if found, ok := matches(re, filepath.Join(in, e.Name())); ok {
				w.Must(os.WriteFile(filepath.Join(out, e.Name()), []byte(found), 0o644))
			}
		}
		w.Put(out, "/cas/out")
	})
}

// matches is the file's matching lines as `linenum:line`, and whether there
// were any; a binary file has none.
func matches(re *regexp.Regexp, path string) (string, bool) {
	data := w.Check(os.ReadFile(path))
	if bytes.IndexByte(data[:min(len(data), 8192)], 0) >= 0 {
		return "", false
	}
	lines := strings.Split(string(data), "\n")
	if n := len(lines); lines[n-1] == "" {
		lines = lines[:n-1] // text ending in a newline has no further line
	}
	var found strings.Builder
	for i, line := range lines {
		if line = strings.TrimSuffix(line, "\r"); re.MatchString(line) {
			fmt.Fprintf(&found, "%d:%s\n", i+1, line)
		}
	}
	return found.String(), found.Len() > 0
}
