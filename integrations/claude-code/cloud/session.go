// The SessionStart hook for a caos cloud session, run through
// /usr/local/bin/caos-cloud-session-start.
//
// It does two things, because the setup phase runs on EVERY session
// (design/cloud-setup.md) and has already installed the client, the git helper,
// the configuration and the `caos` remote by the time Claude Code starts.
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
	"fmt"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
	"time"
)

const shareDir = "/usr/local/share/caos"

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

	setup := stamp("setup-stamp")
	dev := stamp("dev-stamp")
	for _, key := range []string{"built", "base", "pin", "client"} {
		if value, ok := setup[key]; ok {
			log("env %s=%s", key, value)
		}
	}

	// REPORTED, NOT REPAIRED. The setup phase adds this from `--server` before
	// Claude Code starts, and that is the only place a server is named. Adding it
	// here instead would be a remote that appears after the tool server has
	// already started without one.
	if _, err := git("remote", "get-url", "caos"); err != nil {
		log("no caos remote: the setup line named no --server=<url>, so nothing")
		log("  points this checkout at a server. Every tool call will fail.")
	}

	stepPath := setup["std_path"]
	if stepPath == "" {
		log("the setup stamp names no std path, so nothing can name the step;")
		log("  leaving the tools to mcp serve's background resolve")
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

	if !warm(locator, gitDir) {
		reportNoTools()
	}

	// STDOUT, because it is the only stream a session keeps. The stamp is the
	// authority rather than anything this hook can test: dev mode happens
	// entirely in the setup phase and every step of it there is fatal, so the
	// file existing is the whole claim.
	if rev := dev["rev"]; rev != "" {
		fmt.Printf("caos dev mode: ON -- the whole install package came from "+
			"refs/caos/dev at %s, and this session's conversation seeds from %s "+
			"(the checkout as setup left it).\n", short(rev), short(dev["seed"]))
	} else {
		fmt.Printf("caos dev mode: off -- this session runs the caos its repo pins (%s).\n",
			setup["client"])
	}
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
func warm(locator, gitDir string) bool {
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
	cmd := exec.CommandContext(ctx, "caos", "mcp", "warm", locator)
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
