// The SessionStart hook for a caos cloud session, run through
// /usr/local/bin/caos-cloud-session-start.
//
// It does three things. The setup phase has already installed the client, the
// git helper, the configuration and the `caos` remote by the time Claude Code
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
// THE REGISTRY WARM, which fills the cache `mcp serve` reads. It could run in
// setup -- nothing stops it reaching the server from there -- but it is the step
// that decides whether a first turn has tools, and it is proven here.
//
// ONE LINE OF STDOUT naming the build. A SessionStart hook contributes its
// stdout to the session; its stderr is captured as a non-transcript event and
// DROPPED, so anything a session must be able to read goes on stdout.
package main

import (
	"context"
	"encoding/json"
	"fmt"
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

// Long enough for the observed resolve (~110s in a cloud session, which talks to
// the server and may run the step to list its tools), so a cap short enough to be
// "tight" would time out every warm and cache nothing.
const warmBudget = 150 * time.Second

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
	gitDir, err := git("rev-parse", "--absolute-git-dir")
	if err != nil {
		log("%s is not a git repository; nothing to warm", mustGetwd())
		return
	}

	// A marker describes one session's check, so it never outlives it.
	os.Remove(staleMarker())
	setup := stamp("setup-stamp")
	dev := stamp("dev-stamp")
	for _, key := range []string{"built", "base", "pin", "client"} {
		if value, ok := setup[key]; ok {
			log("env %s=%s", key, value)
		}
	}
	// Dev mode installs the dev commit whatever the checkout pins, so there the
	// pin says nothing about staleness. No warm after a failure: every call it
	// would make is refused.
	if dev["rev"] == "" && pinMoved(setup["pin"]) {
		return
	}

	// REPORTED, NOT REPAIRED. The setup phase adds this from `--server` before
	// Claude Code starts, and that is the only place a server is named. Adding it
	// here instead would be a remote that appears after the tool server has
	// already started without one.
	if _, err := git("remote", "get-url", "caos"); err != nil {
		log("no caos remote: the setup line named no --server=<url>, so nothing")
		log("  points this checkout at a server. Every tool call will fail.")
	}

	// Beside the warm rather than before it, so a session pays for one round
	// trip to the server, not two.
	served := make(chan devProbe, 1)
	if dev["rev"] != "" {
		go func() { served <- probeDev() }()
	}

	stepPath := setup["std_path"]
	if stepPath == "" {
		log("the setup stamp names no std path, so nothing can name the step;")
		log("  leaving the tools to mcp serve's background resolve")
		if rev := dev["rev"]; rev != "" {
			reportDev(rev, <-served)
		}
		return
	}
	locator := "--llm-step:@=" + stepPath + "/llm-step"

	// CLAIMED BEFORE THE WARM STARTS, not by the warm itself. Claude Code spawns
	// the tool server in PARALLEL with this hook and its first `tools/list` lands
	// within a second; without a claim already on disk that server resolves the
	// tools itself, duplicating the warm and colliding with it on a push of the
	// same object. `mcp serve` waits for this file instead (`warm_in_flight`).
	//
	// It carries the unix time by which the warm will have given up, so a warm
	// that is killed cannot make every later serve wait for a process that is
	// gone.
	marker := filepath.Join(gitDir, "caos-cc-warming")
	deadline := time.Now().Add(warmBudget + 30*time.Second).Unix()
	os.WriteFile(marker, []byte(fmt.Sprintf("%d\n", deadline)), 0o644)
	defer os.Remove(marker)

	// The same `--base` the hook and the tool server were given: the step resolves
	// in that commit, and the cache this leaves is keyed by it.
	warmArgs := []string{"mcp", "warm", locator}
	if seed := dev["seed"]; seed != "" {
		warmArgs = append(warmArgs, "--base="+seed)
	}
	if !warm(warmArgs, gitDir) {
		reportNoTools()
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
// A probe that failed only goes to stderr -- the warm reports an unreachable
// server on its own, and a dev check that cannot be made is not a mismatch.
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

// `mcp serve` cannot resolve the tools before it must answer the client's first
// `tools/list` -- the resolution may build an image -- so a client that reads
// that list exactly once at startup (the mounted Claude Code a cloud environment
// uses) is left with no caos tools however fast the resolve then finishes.
// Resolve them here, while this hook still blocks the session from starting, and
// leave them in the cache `mcp serve` reads when it launches.
//
// Reports whether it left a registry behind. That is the FILE, not the exit
// status: `caos mcp warm` is non-fatal by contract and exits 0 having cached
// nothing, so a status check would call a cold session warm.
func warm(args []string, gitDir string) bool {
	if _, err := exec.LookPath("caos"); err != nil {
		log("no caos on PATH; the setup phase installed no client")
		return false
	}
	// ITS OUTPUT GOES TO A FILE, and that is not tidiness: Claude Code holds the
	// session at "starting" until this hook's output stream reaches EOF, so a warm
	// that inherited that stream and left a child (a `git` the resolve forked)
	// writing to it would keep the WHOLE SESSION from starting long after the
	// warm itself returned.
	out, err := os.Create("/tmp/caos-warm.log")
	if err != nil {
		log("could not open the warm log: %v", err)
		return false
	}
	defer out.Close()

	ctx, cancel := context.WithTimeout(context.Background(), warmBudget)
	defer cancel()
	cmd := exec.CommandContext(ctx, "caos", args...)
	cmd.Stdout, cmd.Stderr = out, out
	cmd.Env = append(os.Environ(), "CLAUDE_PROJECT_DIR="+mustGetwd())
	// A resolve blocked on a network read can ignore the cancel; without this the
	// hook would wait for it anyway and hold the session past the cap.
	cmd.WaitDelay = 10 * time.Second
	log("warming the caos tool registry for the first turn")
	if err := cmd.Run(); err != nil {
		log("could not warm the tools in time; mcp serve will resolve in the background")
	}
	if data, err := os.ReadFile("/tmp/caos-warm.log"); err == nil {
		for _, line := range strings.Split(strings.TrimRight(string(data), "\n"), "\n") {
			if line != "" {
				log("warm: %s", line)
			}
		}
	}
	_, err = os.Stat(filepath.Join(gitDir, "caos-cc-registry.json"))
	return err == nil
}

// A cold registry means the session may have NO caos tools, and nothing else
// says so anywhere a reader can see: the drop happens inside Claude Code, and
// the warm's own account of it goes to this hook's STDERR, which a SessionStart
// hook has dropped. Hence stdout, the one stream a session keeps.
//
// It names what NOT to investigate, because the two things a reader reaches for
// first both look like the fault and are not it. `claude mcp list` is the trap:
// run from a shell it dials a server that by then has a warm cache and answers
// Connected, which is a different question from the one the client asked at
// startup.
func reportNoTools() {
	fmt.Print("caos: THE TOOL REGISTRY DID NOT RESOLVE IN TIME, so this session may have\n" +
		"no caos tools at all -- not even caos_status. Claude Code starts the tool\n" +
		"server in parallel with this hook, and with no cached registry that server\n" +
		"must resolve and build std/llm-step before it can answer its first\n" +
		"tools/list. A startup that overruns the client's MCP timeout drops the\n" +
		"whole server rather than leaving it empty.\n" +
		"The work is NOT lost. Building std/llm-step and running it to list its tools\n" +
		"happens on the caos SERVER, which carries on after this hook's client is\n" +
		"killed and memoizes the result, so the next session's warm is a fast memo hit\n" +
		"and starts with the tools. A NEW session is the fix. Another turn in THIS one\n" +
		"will not bring them back: the client reads its tool list when it starts, and\n" +
		"the dropped server is not asked again.\n" +
		"The configuration is not the fault: the declaration in ~/.claude.json is\n" +
		"correct, and `claude mcp list` reports caos as Connected once the cache\n" +
		"lands. Neither of those contradicts this.\n")
}

func mustGetwd() string {
	dir, err := os.Getwd()
	if err != nil {
		return "."
	}
	return dir
}
