// tests/symlinks — a WORKER test: no client, no repo, just this program run
// over the fixture in a std/go worker.
//
// Proves a git symlink survives the round trip into a worker: the fixture tree/
// holds a real file and a symlink to it. caos ingests the directory (reusing
// git's recorded objects, where the link is a mode-120000 blob), and
// `caos get -r` materializes it back into the worker's /cas. The worker must
// then see the link as a genuine symlink — not a regular file holding the
// target's path, and not a dereferenced copy of the file's contents.
package main

import (
	"os"
	"path/filepath"

	"caos/w"

	"github.com/bitfield/script"
)

// isSymlink uses Lstat, which does NOT follow: os.Stat would report the
// TARGET, so a link to a regular file would answer "regular file" and the
// central claim of this test would pass vacuously.
func isSymlink(path string) bool {
	return w.Check(os.Lstat(path)).Mode()&os.ModeSymlink != 0
}

func main() {
	w.Main(func() {
		const tree = "/cas/args/tree"
		file := filepath.Join(tree, "file.txt")
		link := filepath.Join(tree, "link.txt")

		w.Do(script.Exec("caos get -r " + tree))

		w.Step("the link is a real symlink")
		w.True(isSymlink(link), "%s is not a symlink", link)

		w.Step("it points at the right target")
		target := w.Check(os.Readlink(link))
		w.True(target == "file.txt", "expected target file.txt, got: %s", target)

		w.Step("the file itself is a regular file")
		w.True(w.Check(os.Lstat(file)).Mode().IsRegular(),
			"%s is not a regular file", file)

		w.Step("reading through the link yields the file's contents")
		w.True(string(w.Check(os.ReadFile(link))) == string(w.Check(os.ReadFile(file))),
			"content via the link differs from the file")

		// The regression: workers stage a result by symlinking already-fetched
		// /cas entries into a scratch tree and `caos put`ting it (this is how
		// write/edit keep untouched siblings). When a staged sibling is itself
		// a git symlink, `caos put` must reuse it AS a symlink — not follow it
		// to its target and record a regular copy.
		w.Step("a staged git symlink survives put + get")
		const stage = "/tmp/stage"
		w.Must(os.RemoveAll(stage))
		w.Must(os.MkdirAll(stage, 0o755))
		w.Must(os.Symlink(file, filepath.Join(stage, "file.txt")))
		// Staging link -> a git symlink node: the case the regression was about.
		w.Must(os.Symlink(link, filepath.Join(stage, "link.txt")))
		w.Do(script.Exec("caos put " + stage + " /cas/staged"))
		w.Do(script.Exec("caos get -r /cas/staged"))

		w.True(isSymlink("/cas/staged/link.txt"),
			"staged link.txt was flattened into a regular file")
		staged := w.Check(os.Readlink("/cas/staged/link.txt"))
		w.True(staged == "file.txt", "staged link.txt target changed: %s", staged)

		w.Report("symlinks: ALL PASS\n")
	})
}
