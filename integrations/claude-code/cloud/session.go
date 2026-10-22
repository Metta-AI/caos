// The SessionStart hook for a caos cloud session, run through
// /usr/local/bin/caos-cloud-session-start.
//
// It does two things. The setup phase has already installed the client, the
// git helper and the configuration by the time Claude Code
// starts -- but it may have done so in an EARLIER session: an environment caches
// its setup and re-runs it only when the setup text changes
// (design/cloud-setup.md).
//
// THE STALENESS CHECK, which is the one thing that has to be per-session for
// that reason: is the installed caos the one a fresh setup would install NOW?
// Too late to install that one instead -- Claude Code has already started on
// the files setup left. Out of dev mode the answer is the checkout's pin, which
// the environment fetches before every session while the install stays where
// setup left it. In dev mode it is the commit the server serves: setup refuses
// a `--dev-commit` the server is not serving, but a cached setup is not asked
// again, so a stack republished since is caught only here.
//
// ONE LINE OF STDOUT naming the build. A SessionStart hook contributes its
// stdout to the session; its stderr is captured as a non-transcript event and
// DROPPED, so anything a session must be able to read goes on stdout.
package main

import (
	"bytes"
	"context"
	"encoding/json"
	"fmt"
	"io"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
	"time"
)

// `--share-dir` moves it, for tests/cloud-setup, as it does for bootstrap.go.
var shareDir = "/usr/local/share/caos"

// The `caos` wrapper refuses every call while this exists (install.go).
func staleMarker() string { return filepath.Join(shareDir, "stale-install") }

func log(format string, a ...any) {
	fmt.Fprintf(os.Stderr, "caos: "+format+"\n", a...)
}

func stamp(name string) map[string]string {
	out := map[string]string{}
	data, err := os.ReadFile(filepath.Join(shareDir, name))
	if err != nil {
		return out
	}
	for _, line := range strings.Split(string(data), "\n") {
		if key, value, ok := strings.Cut(line, "="); ok && value != "" {
			out[key] = value
		}
	}
	return out
}

func short(sha string) string {
	if len(sha) > 12 {
		return sha[:12]
	}
	return sha
}

func git(args ...string) (string, error) {
	out, err := exec.Command("git", args...).Output()
	return strings.TrimSpace(string(out)), err
}

func main() {
	for _, arg := range os.Args[1:] {
		if value, ok := strings.CutPrefix(arg, "--share-dir="); ok {
			shareDir = strings.TrimRight(value, "/")
		}
	}
	// The repo is named, not assumed from cwd: a hook's working directory is not
	// contractually the project, and a wrong one here does not error -- it
	// silently adds the remote to some other repository, and the failure shows up
	// much later as a client that cannot find a server.
	if dir := os.Getenv("CLAUDE_PROJECT_DIR"); dir != "" {
		if err := os.Chdir(dir); err != nil {
			log("cannot enter %s: %v", dir, err)
		}
	}
	payload := readPayload()
	_, err := git("rev-parse", "--absolute-git-dir")
	if err != nil {
		// A DEGRADED STATE, said where the session can read it: on stdout, because a
		// SessionStart hook drops stderr. Said only to stderr, a session in the wrong
		// directory would look healthy and every later caos hook would fail with
		// nothing to show for it.
		reason := fmt.Sprintf("%s is not a git repository (CLAUDE_PROJECT_DIR=%q)",
			mustGetwd(), os.Getenv("CLAUDE_PROJECT_DIR"))
		log("%s; caos is disabled for this session", reason)
		runDiag(payload, reason)
		reportDegraded(reason)
		return
	}

	// A marker describes one session's check, so it never outlives it.
	os.Remove(staleMarker())
	runDiag(payload, "")
	setup := stamp("setup-stamp")
	dev := stamp("dev-stamp")
	for _, key := range []string{"built", "base", "pin", "client"} {
		if value, ok := setup[key]; ok {
			log("env %s=%s", key, value)
		}
	}
	// Dev mode installs the dev commit whatever the checkout pins, so there the
	// pin says nothing about staleness. Nothing more after a failure: every call
	// would be refused.
	if dev["rev"] == "" && pinMoved(setup["pin"]) {
		return
	}

	// The one round trip to the server this hook makes, and only in dev mode: what
	// the server serves NOW as refs/caos/dev, to say whether the install is stale.
	served := make(chan devProbe, 1)
	if dev["rev"] != "" {
		go func() { served <- probeDev() }()
	}

	// STDOUT, because it is the only stream a session keeps. The stamp is the
	// authority rather than anything this hook can test: dev mode happens
	// entirely in the setup phase and every step of it there is fatal, so the
	// file existing is the whole claim.
	if rev := dev["rev"]; rev != "" {
		fmt.Printf("caos dev mode: ON -- the whole install package came from "+
			"refs/caos/dev at %s, and this session's conversation seeds from %s "+
			"(HEAD pointed at the dev server; the checkout is untouched).\n", short(rev), short(dev["seed"]))
		reportDev(rev, <-served)
	} else {
		fmt.Printf("caos dev mode: off -- this session runs the caos its repo pins (%s).\n",
			setup["client"])
	}
}

// The SessionStart payload Claude Code writes on stdin. Bounded, because a
// harness that leaves stdin open must not hold the session at "starting".
func readPayload() []byte {
	done := make(chan []byte, 1)
	go func() {
		data, _ := io.ReadAll(os.Stdin)
		done <- data
	}()
	select {
	case data := <-done:
		return data
	case <-time.After(2 * time.Second):
		return nil
	}
}

// `caos mcp diag` records where this process is standing (stderr, which the hook
// events keep, and ~/.cache/caos/diag.jsonl). Best effort: it explains a
// failure, so it must not cause one.
func runDiag(payload []byte, reason string) {
	args := []string{"mcp", "diag", "--event=SessionStart"}
	if reason != "" {
		args = append(args, "--reason="+reason)
	}
	ctx, cancel := context.WithTimeout(context.Background(), 15*time.Second)
	defer cancel()
	cmd := exec.CommandContext(ctx, "caos", args...)
	if len(payload) > 0 {
		// --stdin lets the payload's own event, source and session win.
		cmd.Args = append(cmd.Args, "--stdin")
		cmd.Stdin = bytes.NewReader(payload)
	}
	cmd.Stderr = os.Stderr
	cmd.WaitDelay = 5 * time.Second
	if err := cmd.Run(); err != nil {
		log("could not record diagnostics: %v", err)
	}
}

// Tell the user (systemMessage) and the model (additionalContext) that caos is
// off and why. Exit status stays 0: a SessionStart hook cannot refuse a
// session, and failing here would only hide this message.
func reportDegraded(reason string) {
	text := "caos is DISABLED for this session: " + reason + ". " +
		"No caos tool will run, and nothing is being " +
		"recorded. Ask caos `help` with no path for the reason. Start the session in the project's checkout, or check " +
		"CLAUDE_PROJECT_DIR. Diagnostics: ~/.cache/caos/diag.jsonl."
	out, _ := json.Marshal(map[string]any{
		"systemMessage": text,
		"hookSpecificOutput": map[string]any{
			"hookEventName":     "SessionStart",
			"additionalContext": text,
		},
	})
	fmt.Println(string(out))
}

type devProbe struct {
	sha string
	err error
}

// What the server hands out as refs/caos/dev NOW. Bounded, because an
// unreachable server over iroh waits out a connect timeout and this hook holds
// the session at "starting".
func probeDev() devProbe {
	ctx, cancel := context.WithTimeout(context.Background(), 20*time.Second)
	defer cancel()
	cmd := exec.CommandContext(ctx, "git", "ls-remote", "caos", "refs/caos/dev")
	cmd.WaitDelay = 5 * time.Second
	out, err := cmd.Output()
	if err != nil {
		return devProbe{err: err}
	}
	fields := strings.Fields(string(out))
	if len(fields) == 0 {
		return devProbe{err: fmt.Errorf("the server has no refs/caos/dev")}
	}
	return devProbe{sha: fields[0]}
}

// A mismatch FAILS THE SESSION: it is the state in which everything else
// looks right and the tree under test is not the one running.
//
// A probe that failed only goes to stderr: a dev check that cannot be made is
// not a mismatch.
func reportDev(installed string, p devProbe) {
	if p.err != nil {
		log("could not read the server's refs/caos/dev to check the dev install: %v", p.err)
		return
	}
	if p.sha == installed {
		log("dev check: the server still serves %s", short(installed))
		return
	}
	msg := fmt.Sprintf("caos: STALE DEV INSTALL -- this session runs dev commit %s, but the server\n"+
		"now serves %s at refs/caos/dev. The environment's setup is cached from an\n"+
		"earlier session and re-runs only when its setup text changes, so it is still\n"+
		"on the old commit. Every caos call in this session is refused. Put the\n"+
		"--dev-commit=<sha> that the latest `caosd up --iroh` printed on the\n"+
		"environment's setup line, then start a new session.\n", short(installed), short(p.sha))
	fail(msg)
}

// Whether the checkout now pins a different caos than the one setup installed,
// failing the session if so. A cached setup cannot notice: the environment
// fetches the repo before every session but re-runs setup only when the setup
// TEXT changes, so moving the pin moves the checkout and leaves the install
// behind -- a session that runs the old tools with nothing saying so.
func pinMoved(installed string) bool {
	if installed == "" {
		log("the setup stamp names no pin, so the install cannot be checked against the checkout")
		return false
	}
	top, err := git("rev-parse", "--show-toplevel")
	if err != nil {
		log("could not find the checkout's top level: %v", err)
		return false
	}
	// A lock that no longer reads is a checkout a fresh setup would refuse, which
	// is as stale as one that moved.
	now, err := lockedCaos(filepath.Join(top, "flake.lock"))
	if err != nil {
		now = "nothing readable (" + err.Error() + ")"
	}
	if now == installed {
		log("pin check: the checkout still pins %s", installed)
		return false
	}
	fail(fmt.Sprintf("caos: STALE INSTALL -- this session runs caos\n  %s\nbut the checkout now pins\n  %s\n"+
		"The environment's setup is cached from an earlier session and\n"+
		"re-runs only when its setup text changes, so it is still on the old pin.\n"+
		"Every caos call in this session is refused. Change the environment's setup\n"+
		"line (the date on its first line will do), then start a new session.\n", installed, now))
	return true
}

// The `caos` input as bootstrap.go stamps it, `owner/repo@rev`, by readLock's
// node walk: the input name maps to a node KEY, and only that node's `locked`
// section is authoritative.
func lockedCaos(path string) (string, error) {
	data, err := os.ReadFile(path)
	if err != nil {
		return "", err
	}
	var lock struct {
		Root  string `json:"root"`
		Nodes map[string]struct {
			Inputs map[string]json.RawMessage `json:"inputs"`
			Locked struct {
				Owner string `json:"owner"`
				Repo  string `json:"repo"`
				Rev   string `json:"rev"`
			} `json:"locked"`
		} `json:"nodes"`
	}
	if err := json.Unmarshal(data, &lock); err != nil {
		return "", err
	}
	root := lock.Root
	if root == "" {
		root = "root"
	}
	var key string
	if err := json.Unmarshal(lock.Nodes[root].Inputs["caos"], &key); err != nil || key == "" {
		return "", fmt.Errorf("no caos input")
	}
	locked := lock.Nodes[key].Locked
	return locked.Owner + "/" + locked.Repo + "@" + locked.Rev, nil
}

// A SessionStart hook cannot refuse a session, so this writes the marker the
// `caos` wrapper (install.go) turns into exit 2 on every call -- blocking each
// prompt and each caos tool call with this text. Also on STDOUT, the one
// stream a session keeps from this hook.
func fail(msg string) {
	if err := os.WriteFile(staleMarker(), []byte(msg), 0o644); err != nil {
		log("could not write %s, so this session is NOT blocked: %v", staleMarker(), err)
	}
	fmt.Print(msg)
}

func mustGetwd() string {
	dir, err := os.Getwd()
	if err != nil {
		return "."
	}
	return dir
}
