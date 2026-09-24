// Stage 1 of a caos cloud session's install, run from the environment's setup
// field on every session (design/cloud-setup.md):
//
//	B=https://raw.githubusercontent.com/Metta-AI/caos/main
//	curl -fsSL "$B/integrations/claude-code/cloud/bootstrap.go" -o /tmp/caos-bootstrap.go
//	go run /tmp/caos-bootstrap.go --base="$B" --server=caos://<ticket>
//
// `--base` says where THIS FILE came from and nothing else. Which caos gets
// installed comes from the repository the environment opens -- a client repo,
// whose flake.lock pins a commit and whose root .caos-expr mounts that commit's
// std. A repo that pins nothing is a misconfigured environment and fails here,
// rather than being installed from `--base`'s branch: the step a session runs
// resolves through the pinned commit, so a client from a moving head would be a
// client from a different tree than its own tools.
//
// STAGE 2 COMES FROM THE PAYLOAD, not from `--base`. This file downloads the
// release (or, in dev mode, fetches refs/caos/dev) and then `go run`s the
// install.go it finds there. That is what lets an edit to the installer reach
// the next session with no push: in dev mode the payload is your working tree.
//
// EVERYTHING ARRIVES AS AN ARGUMENT, including the server ticket. This phase
// does not get the environment's variables -- measured: a session stamped `off`
// while the environment plainly set CAOS_DEV=1.
//
// STDLIB ONLY, and `curl`/`git` rather than `net/http`. A module fetch would
// have to reach proxy.golang.org, which is the phase that answers 503 for every
// n0 relay; and importing net/http drags in the TLS stack and doubles the cold
// `go run` compile (5.7s against 2.8s, measured) for something curl already
// does and git needs anyway.
package main

import (
	"encoding/json"
	"fmt"
	"os"
	"os/exec"
	"path/filepath"
	"regexp"
	"strings"
	"time"
)

const raw = "https://raw.githubusercontent.com"

// Where the payload, the stamps and the hook's source live. `mcp serve` reads
// the stamps from this path as a literal, so the default is the contract and
// `--share-dir` exists for a test running as an unprivileged worker.
var shareDir = "/usr/local/share/caos"

func say(format string, a ...any) {
	fmt.Fprintf(os.Stderr, "caos-setup: "+format+"\n", a...)
}

func fatal(format string, a ...any) {
	fmt.Fprintf(os.Stderr, "FATAL: "+format+"\n", a...)
	os.Exit(1)
}

// Output captured, errors reported by the caller. Stderr is inherited so a
// git or curl that explains itself is read where it happened.
func run(dir, name string, args ...string) (string, error) {
	cmd := exec.Command(name, args...)
	cmd.Dir = dir
	cmd.Stderr = os.Stderr
	out, err := cmd.Output()
	return strings.TrimSpace(string(out)), err
}

func curlTo(url, dest string) error {
	if err := os.MkdirAll(filepath.Dir(dest), 0o755); err != nil {
		return err
	}
	cmd := exec.Command("curl", "-fsSL", url, "-o", dest)
	cmd.Stderr = os.Stderr
	return cmd.Run()
}

type args struct {
	base       string
	server     string
	devServer  string
	devTree    string
	devRev     string
	enableBash bool
	repoFiles  bool
	prefix     string
	homes      string
}

func parseArgs(argv []string) args {
	a := args{prefix: "/usr/local"}
	for _, arg := range argv {
		key, value, _ := strings.Cut(arg, "=")
		switch key {
		case "--base":
			a.base = strings.TrimRight(value, "/")
		case "--server":
			a.server = value
		case "--dev-server":
			a.devServer = value
		// Passed only by this file's re-exec of the dev tree's own copy, which
		// has already paid for the fetch. Named rather than inferred from an
		// environment variable, so the second pass says what it is skipping.
		case "--dev-tree":
			a.devTree = value
		case "--dev-rev":
			a.devRev = value
		case "--enable-bash":
			a.enableBash = true
		case "--repo-files":
			a.repoFiles = true
		case "--prefix":
			a.prefix = value
		case "--share-dir":
			shareDir = strings.TrimRight(value, "/")
		// Passed straight through to stage 2, for the same reason --share-dir
		// exists here: a test asserts on what was written.
		case "--homes":
			a.homes = value
		default:
			fatal("unknown argument: %s", arg)
		}
	}
	return a
}

// What a client repo declares: which caos (flake.lock) and where it mounts that
// caos' std/ in its own evaluated tree (.caos-expr's --output-path).
type pin struct {
	typ     string
	owner   string
	repo    string
	url     string
	rev     string
	stdPath string
}

func (p pin) slug() string { return p.owner + "/" + p.repo }

// WHICH PIN IS READ, and why it is not arbitrary: flake.lock. The .caos-expr
// carries the same revision, and std/flake-input-loader refuses a tree whose
// expression and lockfile disagree, so drift is loud wherever it is read from
// -- and the lockfile is the easier parse.
//
// The node walk is the loader's own (`locked_input` in its src/main.rs):
// nodes.<root>.inputs.<name> maps the input name to a node KEY, and only that
// node's `locked` section is authoritative. Guessing the key from the name
// works until someone renames an input.
func readLock(path, input string) (pin, error) {
	var p pin
	data, err := os.ReadFile(path)
	if err != nil {
		return p, err
	}
	var lock struct {
		Root  string `json:"root"`
		Nodes map[string]struct {
			// A `follows` input is an ARRAY rather than a node key, and carries
			// no lock of its own; decoding per-input is what lets it be skipped
			// rather than failing the whole file.
			Inputs map[string]json.RawMessage `json:"inputs"`
			Locked struct {
				Type  string `json:"type"`
				Owner string `json:"owner"`
				Repo  string `json:"repo"`
				URL   string `json:"url"`
				Rev   string `json:"rev"`
			} `json:"locked"`
		} `json:"nodes"`
	}
	if err := json.Unmarshal(data, &lock); err != nil {
		return p, fmt.Errorf("%s is not readable as a flake.lock: %v", path, err)
	}
	root := lock.Root
	if root == "" {
		root = "root"
	}
	var key string
	if err := json.Unmarshal(lock.Nodes[root].Inputs[input], &key); err != nil || key == "" {
		return p, fmt.Errorf("%s does not lock an input called %q", path, input)
	}
	node, ok := lock.Nodes[key]
	if !ok {
		return p, fmt.Errorf("%s names input node %q, which it does not define", path, key)
	}
	p = pin{
		typ:   node.Locked.Type,
		owner: node.Locked.Owner,
		repo:  node.Locked.Repo,
		url:   node.Locked.URL,
		rev:   node.Locked.Rev,
	}
	if p.rev == "" {
		return p, fmt.Errorf("the %q input is not pinned to a commit (no rev in %s)", input, path)
	}
	return p, nil
}

// Where caos' std lands in this repo's evaluated tree: the --output-path of the
// root expression's flake-input-loader line. Read rather than assumed, because
// it is the consumer's choice and the one place that states it -- the
// configuration's `--llm-step:@=<path>/llm-step` has to name the same directory
// or the tool server resolves nothing.
func readOutputPath(path string) (string, error) {
	data, err := os.ReadFile(path)
	if err != nil {
		return "", err
	}
	found := ""
	for _, line := range strings.Split(string(data), "\n") {
		// The loader's own rule for comments (full-line `#` only), so a
		// commented-out example line does not win over the real one.
		if t := strings.TrimSpace(line); t == "" || strings.HasPrefix(t, "#") {
			continue
		}
		for _, token := range strings.Fields(line) {
			value := strings.TrimPrefix(token, "--output-path=")
			if value == token {
				continue
			}
			if found != "" && found != value {
				return "", fmt.Errorf("%s names two --output-path values (%s and %s), so nothing says which mounts caos", path, found, value)
			}
			found = strings.TrimRight(value, "/")
		}
	}
	if found == "" {
		return "", fmt.Errorf("%s names no --output-path, so nothing says where caos' std is mounted", path)
	}
	return found, nil
}

// The checkout is already on disk when this runs: `Cloned from seed bundle`
// precedes `Running setup script` in the environment's own log. `-e .git`
// rather than a directory test, because a worktree's .git is a FILE.
func findCheckout() string {
	candidates := []string{os.Getenv("CLAUDE_PROJECT_DIR"), "."}
	if entries, err := filepath.Glob("/home/user/*"); err == nil {
		candidates = append(candidates, entries...)
	}
	candidates = append(candidates, "/home/user")
	for _, dir := range candidates {
		if dir == "" {
			continue
		}
		if _, err := os.Stat(filepath.Join(dir, ".git")); err != nil {
			continue
		}
		if _, err := os.Stat(filepath.Join(dir, "flake.lock")); err != nil {
			continue
		}
		abs, err := filepath.Abs(dir)
		if err != nil {
			continue
		}
		return abs
	}
	return ""
}

// A build is named by its commit -- `build-<first twelve hex>` -- so resolving
// one is a lookup rather than a search.
//
// Read with `git ls-remote`, NOT api.github.com: that API is anonymous here and
// rate limited per IP, and a cloud VM shares its egress address with every other
// cloud VM, so the budget is spent by strangers and the 403 is nothing this side
// can fix.
func resolveBuild(slug, rev string) string {
	tag := "build-" + rev[:12]
	out, err := run("", "git", "ls-remote", "--tags", "https://github.com/"+slug,
		"refs/tags/"+tag, "refs/tags/"+tag+"^{}")
	if err != nil {
		fatal("could not list the tags of https://github.com/%s", slug)
	}
	target := ""
	for _, line := range strings.Split(out, "\n") {
		sha, ref, ok := strings.Cut(strings.TrimSpace(line), "\t")
		if !ok {
			continue
		}
		// Peeled first: an annotated tag's own object is not what was built.
		if strings.HasSuffix(ref, "^{}") || target == "" {
			target = sha
		}
	}
	if target == "" {
		fatal("%s has no %s, and a pinned commit does not fall back to an\n"+
			"  earlier build: the step resolves through THIS rev, so an older client\n"+
			"  would drive tools built from a different tree. The release workflow\n"+
			"  publishes build-<commit> and may still be running -- `gh run list`\n"+
			"  says when.", slug, tag)
	}
	// A tag that points somewhere else is reported, not fatal. The Releases API
	// created a missing tag at the default branch's head unless the workflow
	// passed `target_commitish`, and it did not until 2026-09-09 -- so builds
	// published before that name one commit and point at another. Nothing here
	// needs the tag's target (the assets are what is wanted, and they are named
	// by the tag), but a session driven by such a build is worth saying aloud.
	if !strings.HasPrefix(target, rev[:12]) {
		say("%s points at %s rather than at the pinned %s; this build cannot say\n"+
			"  which tree it came from", tag, target[:12], rev[:12])
	}
	return tag
}

// The four things a machine that has never seen caos needs, laid out under one
// directory so stage 2 installs from a local path whichever mode produced it.
type assets struct {
	dir     string
	version string
}

func assetsFromRelease(slug, tag, dir string) assets {
	base := "https://github.com/" + slug + "/releases/download/" + tag
	for name, asset := range map[string]string{
		"caos":            "caos-x86_64-linux",
		"git-remote-caos": "git-remote-caos-x86_64-linux",
		"settings.json":   "claude-settings.json",
		"mcp.json":        "mcp.json",
		"install.go":      "install.go",
		"session.go":      "session.go",
	} {
		if err := curlTo(base+"/"+asset, filepath.Join(dir, name)); err != nil {
			fatal("%s has no %s asset: %v", tag, asset, err)
		}
	}
	os.Chmod(filepath.Join(dir, "caos"), 0o755)
	os.Chmod(filepath.Join(dir, "git-remote-caos"), 0o755)
	return assets{dir: dir, version: tag}
}

// `caosd up --iroh` publishes the working checkout to refs/caos/dev: one commit
// carrying the tree and the x86_64 binaries built from it, which is exactly what
// the release workflow copies into its assets.
func fetchDevTree(server string) (string, string) {
	sha, err := run("", "git", "ls-remote", server, "refs/caos/dev")
	if err != nil || sha == "" {
		// SAY WHICH FAILURE. An empty result has two very different causes and
		// they look identical from here: "no such ref" means the stack is up but
		// published nothing, a transport error means it was never reached -- and
		// the usual reason for that is the RELAY being down, which has already
		// been mistaken for the other three times.
		helper, _ := exec.LookPath("git-remote-caos")
		if helper == "" {
			helper = "NOT ON PATH"
		}
		fatal("--dev-server named a server this phase could not use.\n"+
			"  git-remote-caos: %s\n"+
			"  Empty with no error means the stack is up but published no\n"+
			"  refs/caos/dev: run `caosd up --iroh`. An error instead usually means\n"+
			"  the RELAY is down -- this phase cannot reach n0's, so it depends on\n"+
			"  the one CAOS_IROH_RELAY names.", helper)
	}
	sha = strings.Fields(sha)[0]

	gitDir := filepath.Join(shareDir, "dev.git")
	tree := filepath.Join(shareDir, "dev-tree")
	os.RemoveAll(gitDir)
	os.RemoveAll(tree)
	if err := os.MkdirAll(tree, 0o755); err != nil {
		fatal("could not make %s: %v", tree, err)
	}
	if _, err := run("", "git", "init", "-q", "--bare", gitDir); err != nil {
		fatal("could not create %s", gitDir)
	}
	if _, err := run("", "git", "--git-dir="+gitDir, "fetch", "-q", "--depth=1", server, sha); err != nil {
		fatal("could not fetch %s from the dev server", sha)
	}
	archive := exec.Command("git", "--git-dir="+gitDir, "archive", sha)
	untar := exec.Command("tar", "-x", "-C", tree)
	archive.Stderr, untar.Stderr = os.Stderr, os.Stderr
	pipe, err := archive.StdoutPipe()
	if err != nil {
		fatal("could not unpack the dev commit: %v", err)
	}
	untar.Stdin = pipe
	if err := archive.Start(); err != nil {
		fatal("could not read %s out of %s: %v", sha, gitDir, err)
	}
	if err := untar.Run(); err != nil {
		fatal("could not unpack %s into %s: %v", sha, tree, err)
	}
	if err := archive.Wait(); err != nil {
		fatal("could not read %s out of %s: %v", sha, gitDir, err)
	}
	say("dev package: %s from the dev server", sha)
	return tree, sha
}

func assetsFromDev(tree, sha, dir string) assets {
	copyFile := func(from, to string, mode os.FileMode) {
		data, err := os.ReadFile(from)
		if err != nil {
			fatal("the dev package has no %s: %v", from, err)
		}
		if err := os.WriteFile(to, data, mode); err != nil {
			fatal("could not write %s: %v", to, err)
		}
	}
	if err := os.MkdirAll(dir, 0o755); err != nil {
		fatal("could not make %s: %v", dir, err)
	}
	copyFile(filepath.Join(tree, "dev-bin/caos"), filepath.Join(dir, "caos"), 0o755)
	copyFile(filepath.Join(tree, "dev-bin/git-remote-caos"), filepath.Join(dir, "git-remote-caos"), 0o755)
	shared := filepath.Join(tree, "integrations/claude-code/shared")
	copyFile(filepath.Join(shared, "settings.json"), filepath.Join(dir, "settings.json"), 0o644)
	copyFile(filepath.Join(shared, "mcp.json"), filepath.Join(dir, "mcp.json"), 0o644)
	cloud := filepath.Join(tree, "integrations/claude-code/cloud")
	copyFile(filepath.Join(cloud, "install.go"), filepath.Join(dir, "install.go"), 0o644)
	copyFile(filepath.Join(cloud, "session.go"), filepath.Join(dir, "session.go"), 0o644)
	return assets{dir: dir, version: "dev-" + sha[:12]}
}

// The git remote helper, alone, from the pinned release. A `caos://` fetch is an
// ordinary git fetch through this binary, so in dev mode it is the ONE thing
// that still has to come from GitHub -- it is the transport the real package
// arrives over. Installed where the client looks for it (`ensure_helper_on_path`
// reads the client's OWN directory, which is lib/caos), with the symlink in bin
// for a person typing it and for a git that inherits an ordinary PATH.
func installHelper(prefix, slug, tag string) {
	lib := filepath.Join(prefix, "lib/caos")
	bin := filepath.Join(prefix, "bin")
	for _, dir := range []string{lib, bin} {
		if err := os.MkdirAll(dir, 0o755); err != nil {
			fatal("could not make %s: %v", dir, err)
		}
	}
	dest := filepath.Join(lib, "git-remote-caos")
	url := "https://github.com/" + slug + "/releases/download/" + tag + "/git-remote-caos-x86_64-linux"
	if err := curlTo(url, dest); err != nil {
		fatal("could not download the git remote helper from %s:\n"+
			"  without it nothing can reach a caos:// server at all.", url)
	}
	if err := os.Chmod(dest, 0o755); err != nil {
		fatal("could not make %s executable: %v", dest, err)
	}
	link := filepath.Join(bin, "git-remote-caos")
	os.Remove(link)
	if err := os.Symlink(dest, link); err != nil {
		fatal("could not link %s: %v", link, err)
	}
}

// The conversation's seed: the checkout as it stands, with `.caos-expr` and
// `flake.lock` repointed at the dev server, as ONE unreferenced commit.
//
// NOTHING IS WRITTEN TO THE WORKTREE. The rewrite is built in a throwaway index
// instead, so the checkout stays clean and the `caos://` ticket -- a credential
// -- never enters a file an agent can be asked to commit. It used to be written
// to disk because the tool server resolved `--llm-step:@=<std>/llm-step` by
// ingesting ".", which meant the worktree decided which tools a session got;
// `resolve_cli_image_arg_in_tree` now resolves it in the commit the conversation
// seeds from, so this commit is the only place the dev pin has to exist.
//
// UNREFERENCED: nothing points at it, so `git push` cannot carry it. The branch
// and the working tree are left exactly as they were found.
//
// BOTH FILES, or neither works: std/flake-input-loader refuses a tree whose
// expression and flake.lock name different revisions.
func seedWithDevPin(repoDir, server, sha string) string {
	index, err := os.CreateTemp("", "caos-seed-index")
	if err != nil {
		fatal("could not make a seed index: %v", err)
	}
	index.Close()
	os.Remove(index.Name())
	defer os.Remove(index.Name())

	git := func(stdin string, args ...string) (string, error) {
		cmd := exec.Command("git", args...)
		cmd.Dir = repoDir
		// An identity of its own, because `commit-tree` refuses without one and
		// the container's git may have none. Whose commit this is carries no
		// meaning: nothing ever pushes it, and its only reader is the hook that
		// seeds a conversation from it.
		cmd.Env = append(os.Environ(),
			"GIT_INDEX_FILE="+index.Name(),
			"GIT_AUTHOR_NAME=caos setup", "GIT_AUTHOR_EMAIL=caos@localhost",
			"GIT_COMMITTER_NAME=caos setup", "GIT_COMMITTER_EMAIL=caos@localhost")
		if stdin != "" {
			cmd.Stdin = strings.NewReader(stdin)
		}
		cmd.Stderr = os.Stderr
		out, err := cmd.Output()
		return strings.TrimSpace(string(out)), err
	}
	// The worktree's content, not HEAD's: a session that opens a checkout with
	// local edits should record those too.
	if _, err := git("", "read-tree", "HEAD"); err != nil {
		fatal("could not read HEAD into a seed index: %v", err)
	}
	if _, err := git("", "add", "-A"); err != nil {
		fatal("could not stage the checkout into a seed index: %v", err)
	}
	for path, content := range map[string]string{
		".caos-expr": repointExpr(repoDir, server, sha),
		"flake.lock": repointLock(repoDir, server, sha),
	} {
		blob, err := git(content, "hash-object", "-w", "--stdin")
		if err != nil || blob == "" {
			fatal("could not store the repointed %s: %v", path, err)
		}
		if _, err := git("", "update-index", "--add", "--cacheinfo", "100644,"+blob+","+path); err != nil {
			fatal("could not stage the repointed %s: %v", path, err)
		}
	}
	tree, err := git("", "write-tree")
	if err != nil || tree == "" {
		fatal("could not write the seed tree: %v", err)
	}
	head, err := git("", "rev-parse", "HEAD")
	if err != nil {
		fatal("could not read HEAD: %v", err)
	}
	commit, err := git("", "commit-tree", tree, "-p", head,
		"-m", "caos dev mode: the checkout, repointed at the dev server")
	if err != nil || commit == "" {
		fatal("could not mint a conversation seed commit: %v\n"+
			"  Without it the session evaluates the COMMITTED pin while running a\n"+
			"  dev client -- the half-update dev mode prevents.", err)
	}
	say("dev tools: %s seeds from %s, which resolves caos from the dev server at %s",
		repoDir, commit[:12], sha[:12])
	return commit
}

// The root expression with every `:@@=<locator>?rev=…&dir=…` pointed at the dev
// server. Read from the WORKTREE rather than from HEAD, so a local edit to the
// expression survives into the session.
func repointExpr(repoDir, server, sha string) string {
	path := filepath.Join(repoDir, ".caos-expr")
	expr, err := os.ReadFile(path)
	if err != nil {
		fatal("cannot read %s: %v\n"+
			"  Without it this session runs your client against the pinned tools.", path, err)
	}
	locator := regexp.MustCompile(`:@@=[^ ?]*\?rev=[0-9a-f]*&dir=([^ ]*)`)
	rewritten := locator.ReplaceAllString(string(expr), ":@@=git+"+server+"?rev="+sha+"&dir=$1")
	if rewritten == string(expr) {
		fatal("%s carries no `:@@=…?rev=…&dir=…` locator to repoint, so the\n"+
			"  session would evaluate the committed tools with a dev client.", path)
	}
	return rewritten
}

// The lockfile with the `caos` input's locked node naming the dev server.
func repointLock(repoDir, server, sha string) string {
	path := filepath.Join(repoDir, "flake.lock")
	data, err := os.ReadFile(path)
	if err != nil {
		fatal("could not read %s: %v", path, err)
	}
	var lock map[string]any
	if err := json.Unmarshal(data, &lock); err != nil {
		fatal("could not read %s as JSON: %v", path, err)
	}
	root, _ := lock["root"].(string)
	if root == "" {
		root = "root"
	}
	nodes, _ := lock["nodes"].(map[string]any)
	rootNode, _ := nodes[root].(map[string]any)
	inputs, _ := rootNode["inputs"].(map[string]any)
	key, _ := inputs["caos"].(string)
	node, _ := nodes[key].(map[string]any)
	if node == nil {
		fatal("%s has no 'caos' input node to repoint; the loader would refuse the drift", path)
	}
	node["locked"] = map[string]string{"type": "git", "url": server, "rev": sha}
	out, err := json.MarshalIndent(lock, "", "  ")
	if err != nil {
		fatal("could not rewrite %s: %v", path, err)
	}
	return string(out) + "\n"
}

func writeStamp(path string, lines map[string]string) {
	keys := []string{"built", "base", "rev", "seed", "repo", "std_path", "pin", "client"}
	var b strings.Builder
	for _, key := range keys {
		if value, ok := lines[key]; ok {
			fmt.Fprintf(&b, "%s=%s\n", key, value)
		}
	}
	if err := os.MkdirAll(filepath.Dir(path), 0o755); err != nil {
		fatal("could not make %s: %v", filepath.Dir(path), err)
	}
	if err := os.WriteFile(path, []byte(b.String()), 0o644); err != nil {
		fatal("could not write %s: %v", path, err)
	}
}

func main() {
	a := parseArgs(os.Args[1:])
	if a.base == "" {
		fatal("--base is required: it says where these scripts come from.\n" +
			"  A program fetched by URL cannot see the URL it came from, so it has\n" +
			"  to be told once.")
	}
	if !strings.HasPrefix(a.base, raw+"/") || len(strings.Split(strings.TrimPrefix(a.base, raw+"/"), "/")) < 3 {
		fatal("--base must look like\n  %s/<owner>/<repo>/<ref>\n  got: %s", raw, a.base)
	}
	// In dev mode the two are the same server, and saying it twice in a settings
	// form is a way to get them out of step.
	if a.server == "" {
		a.server = a.devServer
	}

	repoDir := findCheckout()
	if repoDir == "" {
		fatal("no checkout with a flake.lock was found under /home/user.\n" +
			"  A caos session starts from a CLIENT repo: flake.nix + flake.lock\n" +
			"  pinning a 'caos' input by revision, a root .caos-expr mounting that\n" +
			"  input's std, the AGENTS.md the agent is given, and .caos-secrets\n" +
			"  declaring what it may use. Point this environment at one, or fork\n" +
			"  Metta-AI/caos-session.")
	}
	p, err := readLock(filepath.Join(repoDir, "flake.lock"), "caos")
	if err != nil {
		fatal("%v\n"+
			"  (a 'follows' input carries no lock of its own, and is not one either)\n"+
			"  This environment's repository must pin caos: it is the version knob,\n"+
			"  and `--base` names a branch, so falling back to it would install a\n"+
			"  client from a different tree than the tools it drives.", err)
	}
	// A pin that is not a GitHub one is a different failure from no pin at all,
	// and naming it separately is the difference between "point this at a client
	// repo" and "this client repo is already pointed at your machine" -- which is
	// what a checkout carrying a lock a previous dev session rewrote looks like.
	if p.typ == "git" && strings.HasPrefix(p.url, "https://github.com/") {
		trimmed := strings.TrimSuffix(strings.TrimSuffix(p.url, "/"), ".git")
		trimmed = strings.TrimPrefix(trimmed, "https://github.com/")
		p.owner, p.repo, _ = strings.Cut(trimmed, "/")
	}
	if p.owner == "" || p.repo == "" {
		fatal("this repository pins caos at %s, which is not a GitHub release this\n"+
			"  phase can install from. Dev mode still starts from the committed\n"+
			"  GitHub pin: it is the bootstrap that supplies git-remote-caos, which\n"+
			"  is how anything reaches a caos:// server at all. Commit a github:\n"+
			"  pin, or clone a checkout whose flake.lock has not been rewritten.",
			firstNonEmpty(p.url, p.typ))
	}
	if len(p.rev) != 40 || strings.Trim(p.rev, "0123456789abcdef") != "" {
		fatal("the caos input is pinned to %q, which is not a full commit sha", p.rev)
	}
	p.stdPath, err = readOutputPath(filepath.Join(repoDir, ".caos-expr"))
	if err != nil {
		fatal("%v\n"+
			"  A caos-client repo's root expression loads the pinned input:\n"+
			"  run --base:@@=...&dir=std/flake-input-loader --in:@=. --expr=$CAOS_EXPR \\\n"+
			"      --input=caos --input-tree:@@=...&dir=std --output-path=caos-std", err)
	}
	say("%s pins caos %s at %s, and mounts its std at %s", repoDir, p.slug(), p.rev[:12], p.stdPath)

	assetDir := filepath.Join(shareDir, "assets")
	os.RemoveAll(assetDir)
	os.MkdirAll(assetDir, 0o755)

	// A stamp that exists is the claim that every step above it succeeded, so it
	// is removed before any of them and written after all of them.
	os.Remove(filepath.Join(shareDir, "dev-stamp"))

	// THE RELEASE IS ONLY LOOKED UP WHEN SOMETHING IS TAKEN FROM IT. The second
	// pass of a dev session is handed the tree the first pass fetched, so it
	// needs neither the tag nor the network -- which is also what lets a test
	// drive this stage with no GitHub at all.
	var pkg assets
	devTree, devRev := a.devTree, a.devRev
	switch {
	case a.devServer != "" && devTree == "":
		installHelper(a.prefix, p.slug(), resolveBuild(p.slug(), p.rev))
		devTree, devRev = fetchDevTree(a.devServer)
		// THIS FILE, from the dev tree, once. An edit to stage 1 is part of the
		// package and would otherwise be the one file still needing a push. The
		// fetched tree is handed over rather than re-fetched, which is also what
		// tells the second pass it is the second pass.
		self := filepath.Join(devTree, "integrations/claude-code/cloud/bootstrap.go")
		if _, err := os.Stat(self); err == nil {
			say("re-running the dev tree's own bootstrap.go")
			argv := append([]string{"run", self}, os.Args[1:]...)
			argv = append(argv, "--dev-tree="+devTree, "--dev-rev="+devRev)
			cmd := exec.Command("go", argv...)
			cmd.Stdout, cmd.Stderr = os.Stdout, os.Stderr
			if err := cmd.Run(); err != nil {
				os.Exit(1)
			}
			return
		}
		pkg = assetsFromDev(devTree, devRev, assetDir)
	case devTree != "":
		pkg = assetsFromDev(devTree, devRev, assetDir)
	default:
		tag := resolveBuild(p.slug(), p.rev)
		installHelper(a.prefix, p.slug(), tag)
		pkg = assetsFromRelease(p.slug(), tag, assetDir)
	}

	seed := ""
	if devRev != "" {
		seed = seedWithDevPin(repoDir, a.devServer, devRev)
	}

	// STAGE 2, from the payload. `go run` rather than an exec of a built binary:
	// stage 1 has just compiled the same stdlib packages, so this costs the link
	// and not the compile.
	install := []string{"run", filepath.Join(pkg.dir, "install.go"),
		"--assets=" + pkg.dir,
		"--caos-std-path=" + p.stdPath,
		"--repo=" + p.slug(),
		"--commit=" + p.rev,
		"--version=" + pkg.version,
		"--prefix=" + a.prefix,
	}
	if seed != "" {
		install = append(install, "--seed-commit="+seed)
	}
	if a.enableBash {
		install = append(install, "--enable-bash")
	}
	if a.homes != "" {
		install = append(install, "--homes="+a.homes)
	}
	if a.repoFiles {
		install = append(install, "--repo-files", "--repo-dir="+repoDir)
	}
	cmd := exec.Command("go", install...)
	cmd.Stdout, cmd.Stderr = os.Stdout, os.Stderr
	if err := cmd.Run(); err != nil {
		fatal("the install package would not install; this session has no client.")
	}

	// The hook, which is the only caos code that runs per session now that the
	// install is not repeated: it warms the tool registry and says which caos
	// this session is. Kept as source beside the assets so its `go run` reuses
	// the build cache the two stages above just filled.
	hook := filepath.Join(shareDir, "session.go")
	if data, err := os.ReadFile(filepath.Join(pkg.dir, "session.go")); err == nil {
		os.WriteFile(hook, data, 0o644)
	} else {
		fatal("the install package has no session.go: %v", err)
	}
	launcher := "#!/bin/sh\nexec go run " + hook + " \"$@\"\n"
	launcherPath := filepath.Join(a.prefix, "bin/caos-cloud-session-start")
	if err := os.WriteFile(launcherPath, []byte(launcher), 0o755); err != nil {
		fatal("could not write %s: %v", launcherPath, err)
	}

	// The `caos` remote, from the argument. The client finds caos through it and
	// an arbitrary checkout has none, so this is what makes the arrangement
	// repo-independent -- nothing has to be committed to a session repo.
	if a.server != "" {
		// Absent is this probe's expected answer, so its stderr is not inherited:
		// git's `No such remote 'caos'` on the ordinary path reads as a failure.
		probe := exec.Command("git", "remote", "get-url", "caos")
		probe.Dir = repoDir
		if err := probe.Run(); err != nil {
			if _, err := run(repoDir, "git", "remote", "add", "caos", a.server); err != nil {
				say("could not add the caos remote; the session will have no server")
			}
		}
	} else {
		say("no --server, so this checkout has no caos remote. The hook falls back\n" +
			"  to $CAOS_SERVER_URL, which this phase cannot read -- pass the ticket\n" +
			"  as --server=<url> on the setup line to have it set before the session.")
	}

	// Unshallowed here rather than in the hook: caos pushes the workspace commit
	// and a push packs its whole reachable graph, so the history has to be
	// present before the first prompt. claude.ai/code clones shallow. Non-fatal
	// -- a big repo pays a one-time fetch here rather than failing later.
	if shallow, _ := run(repoDir, "git", "rev-parse", "--is-shallow-repository"); shallow == "true" {
		if _, err := run(repoDir, "git", "fetch", "--unshallow", "--quiet"); err != nil {
			say("could not unshallow; a repo the server has not seen may fail to resolve")
		}
	}

	stamp := map[string]string{
		"built":    time.Now().Format(time.RFC3339),
		"base":     a.base,
		"pin":      p.slug() + "@" + p.rev,
		"std_path": p.stdPath,
		"client":   pkg.version,
	}
	writeStamp(filepath.Join(shareDir, "setup-stamp"), stamp)
	// NOT THE TICKET. A `caos://` URL is the capability to drive that server, and
	// these files are quoted verbatim into a model's context by `caos_status`.
	if devRev != "" {
		writeStamp(filepath.Join(shareDir, "dev-stamp"), map[string]string{
			"rev":      devRev,
			"seed":     seed,
			"std_path": p.stdPath,
			"repo":     repoDir,
		})
	}
}

func firstNonEmpty(values ...string) string {
	for _, v := range values {
		if v != "" {
			return v
		}
	}
	return "?"
}
