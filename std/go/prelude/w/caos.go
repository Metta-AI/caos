package w

import (
	"bytes"
	"fmt"
	"os"
	"os/exec"
	"strings"
)

// The caos worker CLI, as a worker script wants it. Every Go worker was
// defining these for itself (std/actor, tests/*, anything curried onto std/go),
// each a little differently.

// Caos runs `caos <args>` and returns its trimmed stdout. Stderr is kept apart —
// a printed hash must not be mixed with a warning — and becomes the diagnostic
// when the command fails.
func Caos(args ...string) string {
	cmd := exec.Command("caos", args...)
	var stderr bytes.Buffer
	cmd.Stderr = &stderr
	out, err := cmd.Output()
	if err != nil {
		panic(failed{fmt.Sprintf("caos %s: %v: %s", strings.Join(args, " "), err, strings.TrimSpace(stderr.String()))})
	}
	return strings.TrimSpace(string(out))
}

// Get fetches the placeholder at path. For a tree that is ONE level: its
// entries appear, as placeholders (a directory for a subtree, a file for a
// blob) or, for a git symlink, as the real link. Nothing below is read.
func Get(path string) { Caos("get", path) }

// GetAll fetches path and everything under it. Prefer Get: it is the whole
// tree that this exists to avoid loading.
func GetAll(path string) { Caos("get", "-r", path) }

// Read fetches the blob at path and returns its content.
func Read(path string) string {
	Get(path)
	return string(Check(os.ReadFile(path)))
}

// Put records the file or directory src at the CAS path dst. A symlink inside
// src that points at an already-fetched CAS entry is stored as that entry, by
// its recorded hash, without being read.
func Put(src, dst string) { Caos("put", src, dst) }

// Hash is the git object id recorded on a CAS path.
func Hash(path string) string { return Caos("hash", path) }

// Exists reports whether path is present, without following a symlink.
func Exists(path string) bool {
	_, err := os.Lstat(path)
	return err == nil
}
