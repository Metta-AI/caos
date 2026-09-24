// The SessionStart hook for a caos cloud session, run through
// /usr/local/bin/caos-cloud-session-start.
//
// Almost nothing is left here. The client, the git helper, the configuration and
// the checkout's own expression are all done by the setup phase, which runs
// before Claude Code on EVERY session (design/cloud-setup.md) -- so the pin
// re-read and the client re-install that used to live here were redoing work
// done seconds earlier in the same container.
//
// TWO THINGS GENUINELY BELONG IN A SESSION.
//
// The registry warm, because of where the NETWORK is: the setup phase egresses
// through a TLS-terminating gateway that answers 503 for all seven n0 relays,
// while a session egresses through a local CONNECT proxy and reaches them fine.
// A dev ticket names a relay of one's own and so resolves in either phase, but an
// ordinary `caos://` ticket is reachable only from here.
//
// And one line of STDOUT saying which caos this session is. A SessionStart hook
// contributes its stdout to the session; its stderr is captured as a
// non-transcript event and DROPPED, which is why a dev-mode trace written with
// the rest of the logging cost exactly the debugging session it existed to
// prevent.
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

	// The setup phase adds this from its `--server` argument. The fallback is for
	// an environment that names its server in CAOS_SERVER_URL instead, which the
	// setup phase cannot read -- an existing remote is left alone, because a
	// checkout that already names a server has been set up deliberately and
	// repointing it would move someone's work to a different stack.
	if _, err := git("remote", "get-url", "caos"); err != nil {
		server := os.Getenv("CAOS_SERVER_URL")
		switch {
		case server != "":
			if _, err := git("remote", "add", "caos", server); err != nil {
				log("could not add the caos remote")
			} else {
				log("caos remote -> %s", redact(server))
			}
		default:
			log("no caos remote and no CAOS_SERVER_URL: nothing names a server.")
			log("  Pass --server=<url> on the environment's setup line.")
		}
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
	// tools itself and the warm duplicates it -- measured at 8.5s and 8.2s side
	// by side, colliding on a push of the same object. `mcp serve` waits for this
	// file instead (`warm_in_flight`).
	//
	// It carries the unix time by which the warm will have given up, so a warm
	// that is killed cannot make every later serve wait for a process that is
	// gone.
	marker := filepath.Join(gitDir, "caos-cc-warming")
	deadline := time.Now().Add(warmBudget + 30*time.Second).Unix()
	os.WriteFile(marker, []byte(fmt.Sprintf("%d\n", deadline)), 0o644)
	defer os.Remove(marker)

	warm(locator)

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
func warm(locator string) {
	if _, err := exec.LookPath("caos"); err != nil {
		log("no caos on PATH; the setup phase installed no client")
		return
	}
	// ITS OUTPUT GOES TO A FILE, and that is not tidiness: Claude Code holds the
	// session at "starting" until this hook's output stream reaches EOF, so a warm
	// that inherited that stream and left a child (a `git` the resolve forked)
	// writing to it would keep the WHOLE SESSION from starting long after the
	// warm itself returned.
	out, err := os.Create("/tmp/caos-warm.log")
	if err != nil {
		log("could not open the warm log: %v", err)
		return
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
}

func mustGetwd() string {
	dir, err := os.Getwd()
	if err != nil {
		return "."
	}
	return dir
}

// A `caos://` URL ends in the token that authorizes driving that server, and
// these lines are read back by whoever is debugging a session. The truncation is
// only right for a ticket: the same cut on `http://10.0.0.5:9090` would remove
// the address.
func redact(server string) string {
	if !strings.HasPrefix(server, "caos://") {
		return server
	}
	if cut := strings.LastIndex(server, "."); cut > 0 {
		return server[:cut] + "…"
	}
	return server
}
