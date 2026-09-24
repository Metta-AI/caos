// Stage 2 of a caos cloud session's install: put a local install package in
// place and write the Claude Code configuration that points at it.
//
//	go run install.go --assets=<dir> --caos-std-path=<path> \
//	    --repo=<owner/name> --commit=<sha> --version=<build tag>
//
// IT COMES FROM THE PAYLOAD, which is the whole reason it is a second stage:
// bootstrap.go downloads the release (or fetches refs/caos/dev) and runs the
// install.go it finds THERE, so an edit here reaches the next session with no
// push and no CI. Everything it needs has already been resolved and downloaded;
// there is no release lookup here and no network access at all.
//
// The configuration is USER-level, because a cloud container serves every
// repository and a session cannot declare an MCP server for itself. Three routes
// were possible: a repo `.claude/settings.json` (works, but is a file in every
// repository), managed settings (ruled out -- an Anthropic-hosted session does
// not read a device's MDM profile), and user-level settings written here.
//
// THE HOME IS /root: the setup phase runs as root, the CLI runs as root, and
// hooks resolve $HOME to /root even though the repo sits at /home/user/repo and
// Claude's own state at /home/claude/.claude. All three are written anyway --
// it costs nothing and survives that changing.
package main

import (
	"encoding/json"
	"fmt"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
)

// What the shared assets say before this rewrites them. `std/llm-step` is caos'
// own path and is wrong for every other repository, which is what makes a
// substitution that silently matches nothing worth failing over.
const sharedStep = "--llm-step:@=std/llm-step"

func say(format string, a ...any) {
	fmt.Fprintf(os.Stderr, "caos-install: "+format+"\n", a...)
}

func fatal(format string, a ...any) {
	fmt.Fprintf(os.Stderr, "FATAL: "+format+"\n", a...)
	os.Exit(1)
}

type args struct {
	assets     string
	stdPath    string
	repo       string
	commit     string
	version    string
	seedCommit string
	prefix     string
	repoDir    string
	homes      []string
	enableBash bool
	repoFiles  bool
}

func parseArgs(argv []string) args {
	a := args{prefix: "/usr/local", homes: []string{"/root", "/home/claude", "/home/user"}}
	for _, arg := range argv {
		key, value, _ := strings.Cut(arg, "=")
		switch key {
		case "--assets":
			a.assets = strings.TrimRight(value, "/")
		case "--caos-std-path":
			a.stdPath = strings.TrimRight(value, "/")
		case "--repo":
			a.repo = value
		case "--commit":
			a.commit = value
		case "--version":
			a.version = value
		case "--seed-commit":
			a.seedCommit = value
		case "--prefix":
			a.prefix = value
		case "--repo-dir":
			a.repoDir = value
		// Defaulted to the three homes a cloud container has, and an argument
		// only so a test can assert on what was written without being root.
		case "--homes":
			a.homes = strings.Split(value, ",")
		case "--enable-bash":
			a.enableBash = true
		case "--repo-files":
			a.repoFiles = true
		default:
			fatal("unknown argument: %s", arg)
		}
	}
	return a
}

func copyFile(from, to string, mode os.FileMode) {
	data, err := os.ReadFile(from)
	if err != nil {
		fatal("%v", err)
	}
	if err := os.MkdirAll(filepath.Dir(to), 0o755); err != nil {
		fatal("could not make %s: %v", filepath.Dir(to), err)
	}
	if err := os.WriteFile(to, data, mode); err != nil {
		fatal("could not write %s: %v", to, err)
	}
}

func link(from, to string) {
	os.Remove(to)
	if err := os.Symlink(from, to); err != nil {
		fatal("could not link %s: %v", to, err)
	}
}

// The published binary is the static one, without the version wrapper nix adds,
// so on its own it reports an empty rev. The wrapper puts the build back:
// telling a stale client from a current one is the only reason it prints at all.
//
// The helper goes BESIDE THE REAL BINARY rather than beside the wrapper: the
// client puts its OWN directory on PATH before shelling out to git
// (`ensure_helper_on_path`), and that directory is lib/caos. A helper only in
// bin is invisible to the git the client itself runs, so a session resolves its
// server and then dies on `git: 'remote-caos' is not a git command`.
func installClient(a args) {
	lib := filepath.Join(a.prefix, "lib/caos")
	bin := filepath.Join(a.prefix, "bin")
	if err := os.MkdirAll(bin, 0o755); err != nil {
		fatal("could not make %s: %v", bin, err)
	}
	copyFile(filepath.Join(a.assets, "caos"), filepath.Join(lib, "caos"), 0o755)
	copyFile(filepath.Join(a.assets, "git-remote-caos"), filepath.Join(lib, "git-remote-caos"), 0o755)

	// `#!/bin/bash`, NOT `#!/bin/sh`: `exec -a` is a bash builtin and /bin/sh is
	// dash on Debian and Ubuntu, where it is `exec: -a: not found` on every call.
	wrapper := "#!/bin/bash\n" +
		"export CAOS_REV=\"${CAOS_REV:-" + a.version + "}\"\n" +
		"exec -a \"$(basename \"$0\")\" " + filepath.Join(lib, "caos") + " \"$@\"\n"
	if err := os.WriteFile(filepath.Join(bin, "caos"), []byte(wrapper), 0o755); err != nil {
		fatal("could not write %s: %v", filepath.Join(bin, "caos"), err)
	}
	link(filepath.Join(bin, "caos"), filepath.Join(bin, "caos-cli"))
	link(filepath.Join(lib, "git-remote-caos"), filepath.Join(bin, "git-remote-caos"))

	// RUN IT. A wrapper is a shell script written by a program, and whether it
	// executes is not implied by having written it: a `caos` on PATH that fails
	// on every call still looks like a successful install.
	//
	// Assert the OUTPUT, not the exit code -- `caos` with no arguments prints
	// usage and exits NON-ZERO, which is its contract.
	out, _ := exec.Command(filepath.Join(bin, "caos")).CombinedOutput()
	if !strings.Contains(string(out), "usage:") {
		fatal("the installed client does not run:\n%s", firstLines(string(out), 3))
	}
	say("installed %s (%s)", filepath.Join(bin, "caos"), a.version)
}

func firstLines(s string, n int) string {
	lines := strings.Split(s, "\n")
	if len(lines) > n {
		lines = lines[:n]
	}
	return strings.Join(lines, "\n")
}

func walkStrings(value any, f func(string) string) any {
	switch typed := value.(type) {
	case string:
		return f(typed)
	case []any:
		for i := range typed {
			typed[i] = walkStrings(typed[i], f)
		}
	case map[string]any:
		for key := range typed {
			typed[key] = walkStrings(typed[key], f)
		}
	}
	return value
}

func readJSON(path string) map[string]any {
	data, err := os.ReadFile(path)
	if err != nil {
		fatal("%v", err)
	}
	var out map[string]any
	if err := json.Unmarshal(data, &out); err != nil {
		fatal("%s is not the JSON this expects: %v", path, err)
	}
	return out
}

// Written only when the bytes differ: Claude Code re-reads a settings file live,
// so rewriting identical content mid-session would needlessly swap its hooks.
func writeIfChanged(path string, data []byte) bool {
	if old, err := os.ReadFile(path); err == nil && string(old) == string(data) {
		return false
	}
	if err := os.MkdirAll(filepath.Dir(path), 0o755); err != nil {
		fatal("could not make %s: %v", filepath.Dir(path), err)
	}
	if err := os.WriteFile(path, data, 0o644); err != nil {
		fatal("could not write %s: %v", path, err)
	}
	return true
}

func marshal(value any) []byte {
	data, err := json.MarshalIndent(value, "", "  ")
	if err != nil {
		fatal("could not write the configuration as JSON: %v", err)
	}
	return append(data, '\n')
}

// A caos-client repo MOUNTS caos' std into its evaluated tree, so the step is an
// ordinary path and the client resolves it by descent -- the same walk that
// reaches `DEEP-DEPS/<x>` inside caos itself. This is also what makes
// `reader=<std>/llm-step` resolvable in a committed `.caos-secrets` entry.
//
// The locator form pins the step to another repo's tree by full sha, for a
// checkout with no std of its own.
func stepLocator(a args) string {
	if a.stdPath != "" {
		return "--llm-step:@=" + a.stdPath + "/llm-step"
	}
	if a.repo == "" || a.commit == "" {
		fatal("this build cannot say which commit it came from, so there is no\n" +
			"  tree to pin the step to and the configuration would be useless.\n" +
			"  (A caos-client repo avoids this: --caos-std-path=<path> names the\n" +
			"   step by a path in the checkout instead.)")
	}
	return "--llm-step:@@=github:" + a.repo + "?rev=" + a.commit + "&dir=std/llm-step"
}

// THE HOOK NEEDS `--base` TOO: the HOOK creates the conversation
// (`on_user_prompt`), not the tool server, so a seed put only on `mcp serve` --
// where it looks like it belongs -- leaves every conversation seeded from HEAD.
func settingsJSON(a args, locator string) []byte {
	settings := readJSON(filepath.Join(a.assets, "settings.json"))
	// In a hook command the locator is a SHELL WORD, so its `&` and `?` are
	// quoted; in an mcp `args` entry it is bare argv. The two are not
	// interchangeable and this is the only place that difference is expressed.
	shellStep := "'" + locator + "'"
	if a.seedCommit != "" {
		shellStep += " --base=" + a.seedCommit
	}
	walkStrings(settings, func(s string) string {
		s = strings.ReplaceAll(s, `"${CAOS_BIN:-caos}"`, "caos")
		s = strings.ReplaceAll(s, "${CAOS_BIN:-caos}", "caos")
		return strings.ReplaceAll(s, sharedStep, shellStep)
	})

	hooks, _ := settings["hooks"].(map[string]any)
	if hooks == nil {
		hooks = map[string]any{}
		settings["hooks"] = hooks
	}
	hooks["SessionStart"] = []any{map[string]any{
		"hooks": []any{map[string]any{"type": "command", "command": "caos-cloud-session-start"}},
	}}

	if a.enableBash {
		// Lift the read/inspect tools out of deny and into allow, so a session
		// can be driven to dump the container. Edit/Write/NotebookEdit/Monitor
		// stay denied -- this is for looking, not for the model rewriting the
		// checkout.
		inspect := []string{"Bash", "Read", "Grep", "Glob"}
		permissions, _ := settings["permissions"].(map[string]any)
		if permissions == nil {
			permissions = map[string]any{}
			settings["permissions"] = permissions
		}
		permissions["deny"] = without(permissions["deny"], inspect)
		permissions["allow"] = with(permissions["allow"], inspect)
		say("--enable-bash: Bash/Read/Grep/Glob will be allowed this session")
	}

	data := marshal(settings)
	// A no-op substitution is the failure worth catching: the session starts and
	// every tool call dies for want of --llm-step, and the reason is a literal
	// nobody looked at.
	if !strings.Contains(string(data), locator) {
		fatal("the settings asset does not name --llm-step, so nothing points at\n" +
			"  the step. Is this build older than the client?")
	}
	return data
}

// The tool server is named directly rather than through a generated shim: the
// setup phase runs every session, so the binary it installed seconds earlier is
// the current one and there is nothing for a shim to refresh.
func mcpServers(a args, locator string) map[string]any {
	mcp := readJSON(filepath.Join(a.assets, "mcp.json"))
	walkStrings(mcp, func(s string) string {
		s = strings.ReplaceAll(s, `"${CAOS_BIN:-caos}"`, "caos")
		s = strings.ReplaceAll(s, "${CAOS_BIN:-caos}", "caos")
		return strings.ReplaceAll(s, sharedStep, locator)
	})
	servers, _ := mcp["mcpServers"].(map[string]any)
	if servers == nil {
		fatal("the mcp asset declares no mcpServers")
	}
	caos, _ := servers["caos"].(map[string]any)
	if caos == nil {
		fatal("the mcp asset declares no `caos` server")
	}
	// ITS OWN ARGV ELEMENT. Appended to the locator's string it is a single
	// argument (`--llm-step:@=… --base=…`), which the client reads as one flag
	// with a nonsense value.
	if a.seedCommit != "" {
		list, _ := caos["args"].([]any)
		caos["args"] = append(list, "--base="+a.seedCommit)
	}
	if !strings.Contains(string(marshal(servers)), locator) {
		fatal("the mcp asset does not name --llm-step, so nothing points at the step")
	}
	return servers
}

func without(value any, remove []string) []any {
	var out []any
	for _, item := range toList(value) {
		if s, ok := item.(string); ok && contains(remove, s) {
			continue
		}
		out = append(out, item)
	}
	return out
}

func with(value any, add []string) []any {
	out := toList(value)
	for _, candidate := range add {
		found := false
		for _, item := range out {
			if s, ok := item.(string); ok && s == candidate {
				found = true
			}
		}
		if !found {
			out = append(out, candidate)
		}
	}
	return out
}

func toList(value any) []any {
	list, _ := value.([]any)
	return list
}

func contains(haystack []string, needle string) bool {
	for _, item := range haystack {
		if item == needle {
			return true
		}
	}
	return false
}

func writeUserConfig(a args, locator string) {
	settings := settingsJSON(a, locator)
	servers := mcpServers(a, locator)
	changed, seen := false, 0
	for _, home := range a.homes {
		// A home that is not there is skipped rather than invented: /home/claude
		// exists only on some containers, and all three are written only because
		// which one a hook resolves $HOME to has moved before.
		if info, err := os.Stat(home); err != nil || !info.IsDir() {
			continue
		}
		seen++
		if writeIfChanged(filepath.Join(home, ".claude/settings.json"), settings) {
			changed = true
		}
		// settings.json cannot declare an MCP server -- that lives in the user
		// config beside it, MERGED, because it also holds account state a
		// session put there.
		path := filepath.Join(home, ".claude.json")
		config := map[string]any{}
		if data, err := os.ReadFile(path); err == nil && len(data) > 0 {
			if err := json.Unmarshal(data, &config); err != nil {
				config = map[string]any{}
			}
		}
		existing, _ := config["mcpServers"].(map[string]any)
		if existing == nil {
			existing = map[string]any{}
		}
		for name, server := range servers {
			existing[name] = server
		}
		config["mcpServers"] = existing
		if writeIfChanged(path, marshal(config)) {
			changed = true
		}
	}
	// None of them existing means a session with no hooks and no tool server,
	// which otherwise presents as a client that installed perfectly well and a
	// model that has no caos at all.
	if seen == 0 {
		fatal("none of %s exists, so there is nowhere to write the user-level\n"+
			"  configuration and the session would start with no caos tools.",
			strings.Join(a.homes, ", "))
	}
	if changed {
		say("wrote the user-level configuration naming %s", locator)
	}
}

// The repository files, for a checkout that wants its own configuration rather
// than the container's. NOT overwritten: a checkout that already has
// `.claude/settings.json` has someone's configuration in it.
func writeRepoFiles(a args, locator string) {
	root := a.repoDir
	if root == "" {
		root = "."
	}
	files := map[string][]byte{
		".claude/settings.json": settingsJSON(a, locator),
		".mcp.json":             marshal(map[string]any{"mcpServers": mcpServers(a, locator)}),
	}
	for name, data := range files {
		path := filepath.Join(root, name)
		if _, err := os.Stat(path); err == nil {
			say("keeping the existing %s", name)
			continue
		}
		writeIfChanged(path, data)
		say("wrote %s", name)
	}
}

func main() {
	a := parseArgs(os.Args[1:])
	if a.assets == "" {
		fatal("--assets=<dir> is required: this stage installs from a local\n" +
			"  package that bootstrap.go has already downloaded.")
	}
	installClient(a)

	// What this client is, for whoever has to name the step it drives: the
	// twelve digits in the wrapper cannot be expanded without it, and
	// `caos_status` quotes it into a session's own diagnostic.
	record := fmt.Sprintf("repo=%s\ncommit=%s\nversion=%s\n", a.repo, a.commit, a.version)
	if err := os.MkdirAll(filepath.Join(a.prefix, "share/caos"), 0o755); err != nil {
		fatal("could not make %s: %v", filepath.Join(a.prefix, "share/caos"), err)
	}
	if err := os.WriteFile(filepath.Join(a.prefix, "share/caos/build"), []byte(record), 0o644); err != nil {
		fatal("could not write the build record: %v", err)
	}

	locator := stepLocator(a)
	say("the tools come from %s", locator)
	writeUserConfig(a, locator)
	if a.repoFiles {
		writeRepoFiles(a, locator)
	}
}
