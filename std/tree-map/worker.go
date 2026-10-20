// tree-map runs a worker over every directory of a tree and assembles the
// results into a SPARSE tree of the same shape.
//
// The worker is the `run` arg, a partial ArgTree. For each directory it is run
// with `--in` a tree holding just that directory's files (symlinks included, no
// subdirectories); what it leaves in /cas/out is a tree of result files for that
// directory. tree-map does the recursion: it merges each directory's result
// files with the result trees of its subdirectories, under the subdirectories'
// names, and drops a subdirectory whose result is empty. A run that finds
// nothing therefore returns the empty tree, and one that finds a file deep
// below returns only the directories on the way to it.
//
// Every other arg the caller bound is forwarded to `run` (a grep's `pattern`
// rides along), so a caller curries tree-map exactly as it would the worker.
//
// One job per directory, and nothing loads the whole tree (SPEC: "it sips from
// git"): a job expands only its own level, the key of each job is that
// directory's tree plus the args, and a child's result is linked into its
// parent's by hash, never read. Editing one file re-runs its directory and the
// directories above it, and identical subtrees share one job.
//
// An `--in` that is a file, not a tree, is handed to `run` as it is; its result
// is the result.
//
// Positions, named by `treemap-stage`:
//
//	absent    visit: split this level into files and subdirectories, run on the
//	          files, then fan out over the subdirectories
//	descend   `then` of the run: fan out, carrying the run's result
//	join      `then` of the fan-out: the run's result plus the children's
package main

import (
	"os"
	"path/filepath"
	"strings"

	"caos/w"
)

const (
	args      = "/cas/args"
	emptyTree = "4b825dc642cb6eb9a060e54bf8d69288fbee4904"
)

func main() {
	w.Main(func() {
		switch stage := stage(); stage {
		case "":
			visit()
		case "descend":
			fanOut(args+"/treemap-dirs", args+"/result")
		case "join":
			join()
		default:
			w.True(false, "unknown treemap-stage %q", stage)
		}
	})
}

func stage() string {
	if !w.Exists(args + "/treemap-stage") {
		return ""
	}
	return strings.TrimSpace(w.Read(args + "/treemap-stage"))
}

// reserved args belong to tree-map or to the server; the rest are the caller's.
func reserved(name string) bool {
	switch name {
	case "base", "worker1", "in", "run", "children", "result":
		return true
	}
	return strings.HasPrefix(name, "treemap-")
}

// forwarded is every arg the caller bound, as curry tokens, by reference.
func forwarded() []string {
	var out []string
	for _, e := range w.Check(os.ReadDir(args)) {
		if !reserved(e.Name()) {
			out = append(out, "--"+e.Name()+":@="+filepath.Join(args, e.Name()))
		}
	}
	return out
}

// self is this tree-map, rebound to what the next position needs. `base` is the
// IMAGE, not this job's ArgTree, so nothing carries over unless it is named.
func self(extra ...string) string {
	a := []string{"curry", "--base:@=" + args + "/base", "--worker1:@=" + args + "/worker1", "--run:@=" + args + "/run"}
	return w.Caos(append(append(a, forwarded()...), extra...)...)
}

// run is the caller's partial ArgTree with the forwarded args bound.
func run() string {
	return w.Caos(append([]string{"curry", "--base:@=" + args + "/run"}, forwarded()...)...)
}

func visit() {
	// `in` may be a commit (a source tree in the conversation): its tree.
	const root = "/cas/treemap-root"
	w.Caos("resolve", w.Hash(args+"/in"), ".", root)
	w.Get(root)
	if !w.Check(os.Lstat(root)).IsDir() {
		w.Caos("run-then", args+"/in", "--run:hash="+run())
		return
	}

	files, dirs := "/tmp/treemap-files", "/tmp/treemap-dirs"
	for _, d := range []string{files, dirs} {
		w.Must(os.RemoveAll(d))
		w.Must(os.MkdirAll(d, 0o755))
	}
	nfiles, ndirs := 0, 0
	for _, e := range w.Check(os.ReadDir(root)) {
		p := filepath.Join(root, e.Name())
		// A git symlink is a real link here, and is a file. A gitlink (a source
		// tree in the conversation) is a directory.
		if e.IsDir() || (e.Type()&os.ModeSymlink == 0 && w.Caos("kind", p) == "commit") {
			w.Must(os.Symlink(p, filepath.Join(dirs, e.Name())))
			ndirs++
		} else {
			w.Must(os.Symlink(p, filepath.Join(files, e.Name())))
			nfiles++
		}
	}

	switch {
	case nfiles == 0 && ndirs == 0:
		w.Put(files, "/cas/out")
	case ndirs == 0:
		w.Put(files, "/cas/treemap-files")
		w.Caos("run-then", "/cas/treemap-files", "--run:hash="+run())
	case nfiles == 0:
		w.Put(dirs, "/cas/treemap-dirs")
		fanOut("/cas/treemap-dirs", "")
	default:
		w.Put(files, "/cas/treemap-files")
		w.Put(dirs, "/cas/treemap-dirs")
		w.Caos("run-then", "/cas/treemap-files", "--run:hash="+run(),
			"--then:hash="+self("--treemap-stage=descend", "--treemap-dirs:@=/cas/treemap-dirs"))
	}
}

// fanOut maps this tree-map over the subdirectories, then joins. own is the
// run's result for this directory's files, or "" when there were none.
func fanOut(dirs, own string) {
	join := []string{"--treemap-stage=join"}
	if own != "" {
		join = append(join, "--treemap-own:@="+own)
	}
	w.Caos("map-then", dirs, "--map:hash="+self(), "--then:hash="+self(join...))
}

func join() {
	const out = "/tmp/treemap-out"
	w.Must(os.RemoveAll(out))
	w.Must(os.MkdirAll(out, 0o755))
	// Links only: `caos put` stores each by its recorded hash, unread.
	link := func(p string) { w.Must(os.Symlink(p, filepath.Join(out, filepath.Base(p)))) }

	if own := args + "/treemap-own"; w.Exists(own) {
		w.Get(own)
		w.True(w.Check(os.Lstat(own)).IsDir(), "the run arg must leave a tree of result files in /cas/out, not a file")
		for _, e := range w.Check(os.ReadDir(own)) {
			link(filepath.Join(own, e.Name()))
		}
	}
	children := args + "/children"
	w.Get(children)
	for _, e := range w.Check(os.ReadDir(children)) {
		p := filepath.Join(children, e.Name())
		if w.Hash(p) != emptyTree { // sparse: a directory that found nothing is absent
			link(p)
		}
	}
	w.Put(out, "/cas/out")
}
