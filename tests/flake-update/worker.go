// tests/flake-update — a WORKER test: no client, no repo.
//
// Runs std/flake-update twice over ./flake, a fixture whose one input is
// github:nix-systems/default (a tiny repo with no inputs of its own).
//
// Covers: locking from flake.nix alone (nix fetches the tip, and the lock names
// a commit and a narHash); pinning one input to a commit through its `original`
// (the lock then carries exactly that commit, the narHash nix computes for it is
// the one every flake.lock pinning that commit carries, and the original is
// still what flake.nix says); and the result being the lock alone, a blob.
//
// Staged: a run cannot be waited on, so each assertion is the `then` of the run
// it is about.
package main

import (
	"encoding/json"
	"os"
	"path/filepath"
	"reflect"
	"strings"

	"caos/w"
)

// The lock entry every flake pinning this commit of nix-systems/default carries.
const (
	pin    = "da67096a3b9bf56a91d16901293e51ba5b49a27e"
	pinNar = "sha256-Vy1rq5AaRuLzOxct8nz4T6wlgyUR7zLU309k9mBC768="
)

// next is this test curried for a stage, the `then` of a run. EVERYTHING A
// LATER STAGE READS IS FORWARDED: `base` is the reserved base entry, not this
// job's ArgTree.
func next(stage string) string {
	return w.Caos("curry", "--base:@=/cas/args/base", "--worker1:@=/cas/args/worker1",
		"--stage="+stage, "--test-salt:@=/cas/args/test-salt",
		"--update:@=/cas/args/update", "--flake:@=/cas/args/flake")
}

// result is the run's result, which must be the lock alone: a blob of JSON.
func result() (raw []byte, lock map[string]any) {
	const r = "/cas/args/result"
	w.Get(r)
	w.True(w.Check(os.Lstat(r)).Mode().IsRegular(), "the result is not a blob")
	raw = w.Check(os.ReadFile(r))
	w.Must(json.Unmarshal(raw, &lock))
	return raw, lock
}

// systems is the fixture input's node in a lock.
func systems(lock map[string]any) map[string]any {
	return lock["nodes"].(map[string]any)["systems"].(map[string]any)
}

func main() {
	w.Main(func() {
		stage := ""
		if w.Exists("/cas/args/stage") {
			stage = strings.TrimSpace(w.Read("/cas/args/stage"))
		}
		switch stage {
		case "":
			w.Step("lock from flake.nix alone")
			w.Caos("run-then", "/cas/args/flake",
				"--run:hash="+w.Caos("curry", "--base:@=/cas/args/update"),
				"--then:hash="+next("locked"))

		case "locked":
			raw, lock := result()
			locked := systems(lock)["locked"].(map[string]any)
			rev, _ := locked["rev"].(string)
			nar, _ := locked["narHash"].(string)
			w.True(locked["type"] == "github" && len(rev) == 40 && strings.HasPrefix(nar, "sha256-"),
				"the lock does not pin systems to a github commit: %s", raw)
			w.Step("ok: systems locked at " + rev)

			w.Step("pin one input to a commit")
			const dir = "/tmp/flake2"
			w.Must(os.MkdirAll(dir, 0o755))
			w.Get("/cas/args/flake")
			w.Must(os.WriteFile(filepath.Join(dir, "flake.nix"), []byte(w.Read("/cas/args/flake/flake.nix")), 0o644))
			w.Must(os.WriteFile(filepath.Join(dir, "flake.lock"), raw, 0o644))
			w.Put(dir, "/cas/flake2")
			w.Caos("run-then", "/cas/flake2",
				"--run:hash="+w.Caos("curry", "--base:@=/cas/args/update", "--input=systems", "--rev="+pin),
				"--then:hash="+next("pinned"))

		case "pinned":
			_, lock := result()
			node := systems(lock)
			locked := node["locked"].(map[string]any)
			w.True(locked["rev"] == pin, "systems pinned to %v, not %s", locked["rev"], pin)
			w.True(locked["narHash"] == pinNar, "narHash for %s is %v, expected %s", pin, locked["narHash"], pinNar)
			want := map[string]any{"owner": "nix-systems", "repo": "default", "type": "github"}
			w.True(reflect.DeepEqual(node["original"], want), "original changed: %v", node["original"])
			w.Step("ok: pinned, narHash agrees, original kept")
			w.Report("flake-update: ALL PASS\n")

		default:
			w.True(false, "unknown --stage: %s", stage)
		}
	})
}
