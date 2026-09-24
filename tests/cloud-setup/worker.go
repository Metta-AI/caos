// tests/cloud-setup — a WORKER test over the two programs a cloud container
// runs before Claude Code starts (integrations/claude-code/cloud, and
// design/cloud-setup.md).
//
// Until this existed, nothing tested them but a four-minute cloud round trip,
// which is most of why they were bash. Everything here is a fixture: a checkout
// that pins caos, an install package laid out the way a release is, and a dev
// tree laid out the way `refs/caos/dev` is. No network, no server, no client.
//
// WHAT IT IS REALLY FOR is the two failures that cost a session each and are
// invisible from inside one: a substitution that silently matches nothing (the
// session starts and every tool call dies for want of --llm-step), and a dev
// mode that installs a dev client while the tools still resolve through the
// committed pin.
package main

import (
	"encoding/json"
	"fmt"
	"os"
	"os/exec"
	"path/filepath"
	"strings"

	"caos/w"

	"github.com/bitfield/script"
)

const (
	cloud  = "/cas/args/cloud"
	shared = "/cas/args/shared"

	// Distinguishable on sight in a failure, and distinct from each other: the
	// whole point of dev mode is that the second replaces the first.
	pinRev = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
	devRev = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"

	// A stand-in for the capability to drive a caos server. It must appear in
	// the checkout (that is dev mode) and nowhere in a stamp (those are quoted
	// verbatim into a model's context).
	ticket = "caos://a-dev-ticket"

	base = "https://raw.githubusercontent.com/Metta-AI/caos/main"
)

// Run a command for its effect, failing with its own output as the diagnostic.
func must(dir string, name string, args ...string) string {
	cmd := exec.Command(name, args...)
	cmd.Dir = dir
	out, err := cmd.CombinedOutput()
	w.True(err == nil, "%s %s: %v\n%s", name, strings.Join(args, " "), err, out)
	return strings.TrimRight(string(out), "\n")
}

func git(dir string, args ...string) string { return must(dir, "git", args...) }

// Output without regard to the exit status, for the one thing here whose
// contract is to fail: `caos` with no arguments prints its usage and exits
// NON-ZERO, so asserting the status would fail for a client that works.
func output(name string, args ...string) string {
	out, _ := exec.Command(name, args...).CombinedOutput()
	return strings.TrimRight(string(out), "\n")
}

func write(path, body string, mode os.FileMode) {
	w.Must(os.MkdirAll(filepath.Dir(path), 0o755))
	w.Must(os.WriteFile(path, []byte(body), mode))
}

func read(path string) string { return string(w.Check(os.ReadFile(path))) }

func copyIn(from, to string, mode os.FileMode) { write(to, read(from), mode) }

func readJSON(path string) map[string]any {
	var out map[string]any
	w.Must(json.Unmarshal([]byte(read(path)), &out))
	return out
}

// Every key of a `key=value` stamp.
func stamp(path string) map[string]string {
	out := map[string]string{}
	for _, line := range strings.Split(read(path), "\n") {
		if key, value, ok := strings.Cut(line, "="); ok {
			out[key] = value
		}
	}
	return out
}

// An install package, laid out the way both a GitHub release and a dev tree
// arrive at stage 2. The client is a script rather than a binary because stage 2
// asserts only that what it installed RUNS and prints a usage banner.
func fixtureAssets(dir string) string {
	stub := "#!/bin/sh\necho \"caos: usage: caos <verb> [args]\"\nexit 1\n"
	write(filepath.Join(dir, "caos"), stub, 0o755)
	write(filepath.Join(dir, "git-remote-caos"), stub, 0o755)
	copyIn(filepath.Join(shared, "settings.json"), filepath.Join(dir, "settings.json"), 0o644)
	copyIn(filepath.Join(shared, "mcp.json"), filepath.Join(dir, "mcp.json"), 0o644)
	copyIn(filepath.Join(cloud, "install.go"), filepath.Join(dir, "install.go"), 0o644)
	copyIn(filepath.Join(cloud, "session.go"), filepath.Join(dir, "session.go"), 0o644)
	return dir
}

// The root expression the fixture commits. Both its locators carry the pinned
// rev, which is what stage 1 has to repoint; it is a value rather than a literal
// because the test asserts the WORKTREE still holds exactly this afterwards.
var fixtureExpr = "# the consumer root (design/flake-inputs.md)\n" +
	"run --base:@@=github:Metta-AI/caos?rev=" + pinRev + "&dir=std/flake-input-loader" +
	" --in:@=. --expr=$CAOS_EXPR --input=caos" +
	" --input-tree:@@=github:Metta-AI/caos?rev=" + pinRev + "&dir=std" +
	" --output-path=caos-std\n"

// A caos CLIENT repo: a lockfile pinning caos by revision and a root expression
// mounting that revision's std at a path of the repo's choosing.
func fixtureRepo(dir string) string {
	expr := fixtureExpr
	lock := `{
  "nodes": {
    "root": { "inputs": { "caos": "caos", "nixpkgs": "nixpkgs" } },
    "nixpkgs": { "locked": { "type": "github", "owner": "NixOS", "repo": "nixpkgs", "rev": "` + devRev + `" } },
    "caos": {
      "locked": { "type": "github", "owner": "Metta-AI", "repo": "caos", "rev": "` + pinRev + `" }
    }
  },
  "root": "root",
  "version": 7
}
`
	write(filepath.Join(dir, ".caos-expr"), expr, 0o644)
	write(filepath.Join(dir, "flake.lock"), lock, 0o644)
	write(filepath.Join(dir, "AGENTS.md"), "a client repo\n", 0o644)
	git(dir, "init", "-q", "-b", "main")
	git(dir, "add", "-A")
	git(dir, "-c", "user.email=test@caos", "-c", "user.name=test", "commit", "-qm", "the client repo")
	return dir
}

// `refs/caos/dev` as `caosd up --iroh` publishes it: the tree, plus the
// x86_64 binaries built from it under dev-bin/.
func fixtureDevTree(dir, assets string) string {
	copyIn(filepath.Join(assets, "caos"), filepath.Join(dir, "dev-bin/caos"), 0o755)
	copyIn(filepath.Join(assets, "git-remote-caos"), filepath.Join(dir, "dev-bin/git-remote-caos"), 0o755)
	for _, name := range []string{"settings.json", "mcp.json"} {
		copyIn(filepath.Join(shared, name), filepath.Join(dir, "integrations/claude-code/shared", name), 0o644)
	}
	for _, name := range []string{"install.go", "session.go"} {
		copyIn(filepath.Join(cloud, name), filepath.Join(dir, "integrations/claude-code/cloud", name), 0o644)
	}
	return dir
}

// The hook command Claude Code will run, out of the settings this wrote. One
// string, because that is what a `command` is -- and the locator inside it has
// to be a shell WORD, quoted, where the same locator in an mcp `args` entry is
// bare argv.
func hookCommand(settings map[string]any, event string) string {
	hooks, ok := settings["hooks"].(map[string]any)
	w.True(ok, "the settings carry no hooks")
	matchers, ok := hooks[event].([]any)
	w.True(ok && len(matchers) > 0, "the settings declare no %s hook", event)
	first, ok := matchers[0].(map[string]any)
	w.True(ok, "%s's first matcher is not an object", event)
	inner, ok := first["hooks"].([]any)
	w.True(ok && len(inner) > 0, "%s's first matcher has no hooks", event)
	entry, ok := inner[0].(map[string]any)
	w.True(ok, "%s's first hook is not an object", event)
	command, _ := entry["command"].(string)
	return command
}

func mcpArgs(config map[string]any) []string {
	servers, ok := config["mcpServers"].(map[string]any)
	w.True(ok, "the user config declares no mcpServers")
	caos, ok := servers["caos"].(map[string]any)
	w.True(ok, "the user config declares no caos server")
	list, _ := caos["args"].([]any)
	var out []string
	for _, item := range list {
		out = append(out, fmt.Sprint(item))
	}
	command, _ := caos["command"].(string)
	w.True(command == "caos", "the caos server's command is %q, not the client on PATH", command)
	return out
}

func main() {
	w.Main(func() {
		w.Do(script.Exec("caos get -r " + cloud))
		w.Do(script.Exec("caos get -r " + shared))

		assets := fixtureAssets("/tmp/assets")
		const step = "--llm-step:@=caos-std/llm-step"
		// A home that does not exist is skipped rather than invented (/home/claude
		// is not on every container), so a fixture home has to be made first.
		w.Must(os.MkdirAll("/tmp/home", 0o755))
		w.Must(os.MkdirAll("/tmp/home1", 0o755))

		// -------------------------------------------------------------------
		w.Step("stage 2 installs the package and writes a user-level config")
		// -------------------------------------------------------------------
		seed := "cccccccccccccccccccccccccccccccccccccccc"
		must("", "go", "run", filepath.Join(assets, "install.go"),
			"--assets="+assets,
			"--caos-std-path=caos-std",
			"--repo=Metta-AI/caos",
			"--commit="+pinRev,
			"--version=build-"+pinRev[:12],
			"--seed-commit="+seed,
			"--prefix=/tmp/prefix",
			"--homes=/tmp/home",
			"--enable-bash")

		w.True(strings.Contains(output("/tmp/prefix/bin/caos-cli"), "usage:"),
			"the installed client does not run through its own symlink")
		wrapper := read("/tmp/prefix/bin/caos")
		w.True(strings.Contains(wrapper, "CAOS_REV:-build-"+pinRev[:12]),
			"the wrapper does not stamp the build it installed:\n%s", wrapper)
		// The client finds the helper in its OWN directory, which is lib/caos --
		// a helper only in bin is invisible to the git the client shells out to.
		w.Check(os.Stat("/tmp/prefix/lib/caos/git-remote-caos"))
		w.True(stamp("/tmp/prefix/share/caos/build")["commit"] == pinRev,
			"the build record does not name the commit that was installed")

		settings := readJSON("/tmp/home/.claude/settings.json")
		prompt := hookCommand(settings, "UserPromptSubmit")
		// THE HOOK CREATES THE CONVERSATION, so it is the hook that must carry
		// the seed. Putting --base only on `mcp serve` left every conversation
		// seeded from HEAD while everything else about dev mode worked.
		w.True(strings.Contains(prompt, "'"+step+"'"),
			"the prompt hook does not name the step as a quoted shell word: %s", prompt)
		w.True(strings.Contains(prompt, "--base="+seed),
			"the prompt hook does not seed the conversation: %s", prompt)
		w.True(!strings.Contains(prompt, "${CAOS_BIN"),
			"the shared asset's ${CAOS_BIN} placeholder survived into the config: %s", prompt)
		w.True(hookCommand(settings, "SessionStart") == "caos-cloud-session-start",
			"nothing runs the session hook")

		permissions, _ := settings["permissions"].(map[string]any)
		deny := fmt.Sprint(permissions["deny"])
		allow := fmt.Sprint(permissions["allow"])
		w.True(!strings.Contains(deny, "Bash") && strings.Contains(allow, "Bash"),
			"--enable-bash did not move Bash from deny to allow (deny=%s allow=%s)", deny, allow)
		w.True(strings.Contains(deny, "Write"),
			"--enable-bash let Write out of the deny list (deny=%s)", deny)

		args := mcpArgs(readJSON("/tmp/home/.claude.json"))
		w.True(len(args) > 0, "the caos server takes no arguments")
		w.True(contains(args, step), "the tool server does not name the step: %v", args)
		// Its own argv element. Appended to the locator it became one argument,
		// which the client reads as a flag with a nonsense value.
		w.True(contains(args, "--base="+seed),
			"the tool server does not seed the conversation as its own argument: %v", args)

		// -------------------------------------------------------------------
		w.Step("stage 1 in dev mode seeds a conversation and leaves the checkout clean")
		// -------------------------------------------------------------------
		repo := fixtureRepo("/tmp/repo")
		devTree := fixtureDevTree("/tmp/dev-tree", assets)
		head := git(repo, "rev-parse", "HEAD")
		bootstrap := exec.Command("go", "run", filepath.Join(cloud, "bootstrap.go"),
			"--base="+base,
			"--server="+ticket,
			"--dev-server="+ticket,
			"--dev-tree="+devTree,
			"--dev-rev="+devRev,
			"--prefix=/tmp/prefix1",
			"--share-dir=/tmp/share",
			"--homes=/tmp/home1")
		bootstrap.Env = append(os.Environ(), "CLAUDE_PROJECT_DIR="+repo)
		bootstrap.Dir = "/tmp"
		out, err := bootstrap.CombinedOutput()
		w.True(err == nil, "stage 1 failed: %v\n%s", err, out)

		// THE CHECKOUT IS UNTOUCHED. The dev pin lives only in the seed commit
		// below, so nothing leaves a `caos://` ticket -- a credential -- in a file
		// an agent can be asked to commit. A dirty worktree here is the whole
		// regression this arrangement exists to prevent.
		w.True(git(repo, "status", "--short") == "",
			"stage 1 dirtied the checkout:\n%s", git(repo, "status", "--short"))
		w.True(read(filepath.Join(repo, ".caos-expr")) == fixtureExpr,
			"the worktree's expression was rewritten")

		dev := stamp("/tmp/share/dev-stamp")
		w.True(dev["rev"] == devRev, "the dev stamp names %q, not the fetched revision", dev["rev"])
		w.True(dev["std_path"] == "caos-std" && dev["repo"] == repo,
			"the dev stamp does not describe the checkout it rewrote: %v", dev)
		// A stamp is quoted verbatim into a model's context by `caos_status`.
		for _, name := range []string{"dev-stamp", "setup-stamp"} {
			body := read("/tmp/share/" + name)
			w.True(!strings.Contains(body, "caos://"),
				"%s carries the ticket, which is the capability to drive the server:\n%s", name, body)
		}
		w.True(stamp("/tmp/share/setup-stamp")["client"] == "dev-"+devRev[:12],
			"the setup stamp does not say the client came from the dev server")

		w.Step("the conversation's seed commit is the rewritten tree, and unreferenced")
		seedCommit := dev["seed"]
		w.True(len(seedCommit) == 40, "the dev stamp carries no seed commit: %q", seedCommit)
		w.True(git(repo, "rev-parse", seedCommit+"^") == head,
			"the seed commit is not a child of HEAD")
		// THE REWRITE IS HERE AND ONLY HERE. `resolve_cli_image_arg_in_tree`
		// resolves the session's `--llm-step:@=caos-std/llm-step` in this tree, so
		// this is what decides which tools the session gets.
		seedExpr := git(repo, "show", seedCommit+":.caos-expr")
		w.True(!strings.Contains(seedExpr, pinRev),
			"the seed still resolves caos through the committed pin:\n%s", seedExpr)
		for _, dir := range []string{"std/flake-input-loader", "std"} {
			want := ":@@=git+" + ticket + "?rev=" + devRev + "&dir=" + dir
			w.True(strings.Contains(seedExpr, want),
				"the seed does not reach the dev server for %s:\n%s", dir, seedExpr)
		}
		// BOTH FILES, or the loader refuses the tree for naming two revisions.
		var seedLock map[string]any
		w.Must(json.Unmarshal([]byte(git(repo, "show", seedCommit+":flake.lock")), &seedLock))
		locked := seedLock["nodes"].(map[string]any)["caos"].(map[string]any)["locked"].(map[string]any)
		w.True(locked["rev"] == devRev && locked["url"] == ticket,
			"the seed's lockfile disagrees with its expression: %v", locked)
		// Nothing points at it, so `git push` cannot carry the ticket it holds.
		w.True(!strings.Contains(git(repo, "for-each-ref", "--format=%(objectname)"), seedCommit),
			"a ref points at the seed commit, so a push could carry the ticket")
		w.True(git(repo, "rev-parse", "HEAD") == head, "stage 1 moved the branch")

		w.Step("stage 1 leaves a session ready to start")
		w.True(git(repo, "remote", "get-url", "caos") == ticket,
			"the checkout has no caos remote, which is what the client finds the server through")
		launcher := read("/tmp/prefix1/bin/caos-cloud-session-start")
		w.True(strings.Contains(launcher, "/tmp/share/session.go"),
			"the session hook launcher does not run the hook: %s", launcher)
		w.True(read("/tmp/share/session.go") == read(filepath.Join(cloud, "session.go")),
			"the installed session hook is not the one in the package")
		// Stage 1 ran stage 2 out of the PAYLOAD, which is the whole reason there
		// are two stages: in dev mode that is the working tree.
		devPrompt := hookCommand(readJSON("/tmp/home1/.claude/settings.json"), "UserPromptSubmit")
		w.True(strings.Contains(devPrompt, "--base="+seedCommit),
			"the session would seed from HEAD rather than from the dev commit: %s", devPrompt)

		w.Report(fmt.Sprintf("cloud-setup: stage 2 installed and configured; stage 1 repointed a\n"+
			"checkout at %s and seeded %s\n", devRev[:12], seedCommit[:12]))
	})
}

func contains(haystack []string, needle string) bool {
	for _, item := range haystack {
		if item == needle {
			return true
		}
	}
	return false
}
