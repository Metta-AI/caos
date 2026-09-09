// tests/exec-bit — a WORKER test: no client, no repo, just this program run
// over the fixture in a std/go worker.
//
// Proves git's executable bit round-trips through the worker CAS as METADATA,
// not as a placeholder permission: it is recorded on the placeholder (an xattr)
// and only becomes a real +x mode bit once the file is fetched.
//
// The fixture is CHECKED IN with its mode rather than chmod'd at runtime. git
// records 100755, and caos ingests git's recorded mode, so this exercises the
// real path rather than one the test arranged for itself.
package main

import (
	"os"
	"path/filepath"

	"caos/w"

	"github.com/bitfield/script"
)

// hasExecBit asks whether the file's MODE carries +x for anyone — a question
// about the file, independent of who is asking. Lstat, not Stat: the fixture
// holds no links, but following one would report the target's mode.
func hasExecBit(path string) bool {
	return w.Check(os.Lstat(path)).Mode().Perm()&0o111 != 0
}

func main() {
	w.Main(func() {
		const ws = "/cas/args/ws"
		run := filepath.Join(ws, "run.sh")
		plain := filepath.Join(ws, "plain.txt")

		// Expand ONE level: the entries appear as unfetched placeholders, not
		// loaded content — exactly the state we want to inspect.
		w.Do(script.Exec("caos get " + ws))

		w.Step("an unfetched placeholder is NOT executable, even for an exe file")
		w.Check(os.Lstat(run)) // fails here if the placeholder is missing at all
		w.True(!hasExecBit(run), "placeholder run.sh is +x before it is fetched")
		w.True(!hasExecBit(plain), "placeholder plain.txt is +x")

		w.Step("an unfetched placeholder is NOT READABLE")
		// UNLIKE THE EXEC-BIT ASSERTIONS ABOVE, this one depends on the worker
		// not being the CAS owner — and that dependency IS the mechanism. /cas
		// is root-owned and a placeholder is mode 0400 (MODE_PLACEHOLDER_FILE,
		// crates/caos/src/lib.rs), so an unprivileged worker cannot read what
		// it has not fetched. Legitimate here because this test's image is
		// std/go, which like std/bash declares no CAOS_WORKER_UID and so runs
		// as the default unprivileged uid.
		//
		// An image that grants root (std/flake-builder, dev/test-stack, the
		// host stack) IS the owner, so a placeholder is readable there and
		// reads as EMPTY — which is how dev/run-test came to describe "reads
		// as empty" as the general rule. It is not the general rule, and this
		// is the assertion that says so.
		//
		// A REAL READ, and deliberately NOT w.Check: the claim is that the
		// read FAILS, so its error is the result rather than a reason to stop.
		_, err := os.ReadFile(plain)
		w.True(err != nil,
			"an unfetched placeholder was readable — a worker can read what it has not fetched")

		w.Step("fetching restores +x on the executable only")
		w.Do(script.Exec("caos get " + run))
		w.Do(script.Exec("caos get " + plain))
		w.True(hasExecBit(run), "fetched run.sh is not +x")
		w.True(!hasExecBit(plain), "fetched plain.txt gained +x")
		// The other half of the assertion above: fetching is what makes content
		// readable (MODE_FETCHED_FILE, 0444), so a passing "not readable" check
		// above means the placeholder state, not a broken fixture.
		w.Check(os.ReadFile(plain))

		w.Step("put/get round-trips the exec bit")
		w.Do(script.Exec("caos put " + ws + " /cas/exec-roundtrip"))
		w.Do(script.Exec("caos get -r /cas/exec-roundtrip"))
		w.True(hasExecBit("/cas/exec-roundtrip/run.sh"), "run.sh lost +x through put/get")
		w.True(!hasExecBit("/cas/exec-roundtrip/plain.txt"), "plain.txt gained +x through put/get")

		w.Report("exec-bit: ALL PASS\n")
	})
}
