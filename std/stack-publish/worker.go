// The `stack-publish` tool's worker. Its docs live in the sibling `.caos-expr`
// here-string, not in this header (SPEC, "CaosTools").
//
// Publish a stack directory as branches and chained pull requests
// (design/stack-publish.md), as an ordinary worker (design/tool-contract.md):
// it holds no token, and does every remote action through its neighbour
// `DEEP-DEPS/github`, the one image the token is granted to. Nothing is
// recorded before a push: a call that does not finish is answered as
// unfinished and never run again behind the agent's back, so each push's
// lease is what keeps it safe.
//
// The work is a chain of std/github batches, each answered into the next
// stage of this worker, which carries what it has learnt as `state`:
//
//	start  check the stack and plan every layer; read the repository's
//	       current name and every branch's head
//	push   push every layer, bottom to top, leased on the head just read (or,
//	       with force, on the caller's `expected`); the batch stops at the
//	       first push that does not land
//	list   read each branch's open pull request
//	prs    open the missing pull requests and update the stale ones
//	link   link them as a GitHub stack
//	done   render the receipt
//
// With dry-run, `start` reads the heads and `dry` renders the plan.
package main

import (
	"encoding/json"
	"fmt"
	"os"
	"os/exec"
	"path/filepath"
	"regexp"
	"slices"
	"strings"

	"caos/w"
)

type refusal struct{ msg string }

func refuse(format string, a ...any) { panic(refusal{fmt.Sprintf(format, a...)}) }

// arg is an optional argument, trimmed; "" when the caller left it out.
func arg(name string) string {
	path := "/cas/args/" + name
	if !w.Exists(path) {
		return ""
	}
	return strings.TrimSpace(w.Read(path))
}

func flag(name string, fallback bool) bool {
	switch strings.ToLower(arg(name)) {
	case "":
		return fallback
	case "false", "0", "no":
		return false
	case "true", "1", "yes":
		return true
	}
	refuse("%s is true or false, not %q", name, arg(name))
	return false
}

// answer is the result under the root contract: `{stdout, failed?}`. A
// reader: publishing changes nothing in the session.
func answer(text string, failed bool) {
	const dir = "/tmp/stack-publish-out"
	w.Must(os.MkdirAll(dir, 0o755))
	w.Must(os.WriteFile(filepath.Join(dir, "stdout"), []byte(text), 0o644))
	if failed {
		w.Must(os.WriteFile(filepath.Join(dir, "failed"), []byte("stack-publish failed\n"), 0o644))
	}
	w.Put(dir, "/cas/out")
}

var (
	layerName = regexp.MustCompile(`^([0-9]{2})-(.+)$`)
	hash      = regexp.MustCompile(`^[0-9a-fA-F]{40}$`)
	githubURL = regexp.MustCompile(`(?i)^https://github\.com(:443)?/([A-Za-z0-9_.-]+)/([A-Za-z0-9_.-]+?)(\.git)?/?$`)
	remoteURL = regexp.MustCompile(`^https://[^/@\s]+/\S+$`)
)

// Layer is one layer as it will be published.
type Layer struct {
	Name   string `json:"name"`
	Branch string `json:"branch"`
	Base   string `json:"base"`
	Title  string `json:"title"`
	Body   string `json:"body"`
	Commit string `json:"commit"`
}

// StackRef is a pull request's place in a GitHub stack: the stack's number,
// the PR's 1-based position from the bottom, and the stack's size.
type StackRef struct {
	Number   int64 `json:"number"`
	Position int64 `json:"position"`
	Size     int64 `json:"size"`
}

// PR is a layer's pull request as the `prs` stage leaves it. Deferred is the
// base it still needs, when its stack membership makes the link stage do it.
type PR struct {
	Number   int64     `json:"number"`
	Stack    *StackRef `json:"stack"`
	Deferred string    `json:"deferred"`
}

// State is what one stage hands the next.
type State struct {
	Repository   string   `json:"repository"`
	Trunk        string   `json:"trunk"`
	Owner        string   `json:"owner"` // "" off github.com: branches only
	Repo         string   `json:"repo"`
	Force        bool     `json:"force"`
	AllowIgnored bool     `json:"allow_ignored"`
	Link         bool     `json:"link"`
	Draft        bool     `json:"draft"`
	Layers       []Layer  `json:"layers"`
	Expected     []string `json:"expected"` // per layer: the lease, or "" before it is known
	Lines        []string `json:"lines"`
	PRs          []PR     `json:"prs"`
	Creates      []int    `json:"creates"` // the layers whose PR the `prs` batch opens, in op order
	Patches      int      `json:"patches"` // how many of its ops follow the creates
	StackLine    string   `json:"stack_line"`
}

// next hands std/github a batch and names `stage` of this worker as its
// `then`, carrying the state. Batches and state go as blobs: PR bodies make
// them longer than a command line should be.
func next(stage string, ops []map[string]any, s *State) {
	w.Must(os.WriteFile("/tmp/state.json", w.Check(json.Marshal(s)), 0o644))
	w.Put("/tmp/state.json", "/cas/state")
	w.Must(os.WriteFile("/tmp/batch.json", w.Check(json.Marshal(ops)), 0o644))
	w.Put("/tmp/batch.json", "/cas/batch")
	id := arg("request-id")
	request := w.Caos("prepare-request", "--base:@=/cas/args/github", "--batch:@=/cas/batch",
		"--request-id="+id+"-"+stage)
	then := w.Caos("curry", "--base:@=/cas/args/base", "--worker1:@=/cas/args/worker1",
		"--github:@=/cas/args/github", "--stage="+stage, "--request-id="+id, "--state:@=/cas/state")
	w.Caos("run-request-then", request, "--then:hash="+then)
}

func load() *State {
	var s State
	w.Must(json.Unmarshal([]byte(w.Read("/cas/args/state")), &s))
	return &s
}

// results is std/github's answer, one result per operation of the batch.
func results(n int) []map[string]any {
	var rs []map[string]any
	text := w.Read("/cas/args/result")
	if json.Unmarshal([]byte(text), &rs) != nil || len(rs) != n {
		refuse("GitHub's answer was not %d results: %s", n, text)
	}
	return rs
}

func str(v any) string {
	s, _ := v.(string)
	return s
}

func num(v any) int64 {
	f, _ := v.(float64)
	return int64(f)
}

// why says why an operation is not ok: its error, or GitHub's answer.
func why(r map[string]any) string {
	if e := str(r["error"]); e != "" {
		return e
	}
	if r["skipped"] == true {
		return "not attempted: an operation before it failed"
	}
	message := ""
	if body, ok := r["body"].(map[string]any); ok {
		message = str(body["message"])
		if errs, ok := body["errors"].([]any); ok {
			for _, e := range errs {
				if m, ok := e.(map[string]any); ok && str(m["message"]) != "" {
					message += ": " + str(m["message"])
				}
			}
		}
	} else {
		message = str(r["body"])
	}
	return strings.TrimSpace(fmt.Sprintf("%s %s: HTTP %v %s", str(r["method"]), str(r["path"]), r["status"], message))
}

func short(commit string) string { return commit[:12] }

func kind(path string) string { return w.Caos("kind", path) }

// description is a layer's PR text from its NN-<name>.pr.md: the first
// non-empty line is the title (a leading `#` marker is dropped), the rest the
// body.
func description(name, text string) (string, string) {
	lines := strings.Split(text, "\n")
	for i, line := range lines {
		if strings.TrimSpace(line) == "" {
			continue
		}
		title := strings.TrimSpace(strings.TrimLeft(strings.TrimSpace(line), "#"))
		if title == "" {
			refuse("%s.pr.md: the first line is a bare heading marker; the PR title goes there", name)
		}
		return title, strings.TrimSpace(strings.Join(lines[i+1:], "\n"))
	}
	refuse("%s.pr.md is empty; its first line is the PR title", name)
	return "", ""
}

func checkBranch(what, branch string) {
	if strings.HasPrefix(branch, "refs/") || exec.Command("git", "check-ref-format", "--branch", branch).Run() != nil {
		refuse("%s %q is not a valid branch name", what, branch)
	}
}

// trunk is the branch 00-base was imported from: the requested branch, or the
// remote's default branch when none was requested. A commit or a tag names
// none.
func inferTrunk(requested, defaultBranch string) string {
	switch {
	case requested == "":
		return defaultBranch
	case hash.MatchString(requested):
		return ""
	case strings.HasPrefix(requested, "refs/heads/"):
		return strings.TrimPrefix(requested, "refs/heads/")
	case strings.HasPrefix(requested, "refs/"):
		return ""
	}
	return requested
}

// containsBelow refuses a layer whose commit does not contain the one below
// it (00-base for the first), reading the commit graph only, as std/merge.
func containsBelow(base string, s *State) {
	const repo = "/tmp/graph"
	git := func(args ...string) (string, error) {
		out, err := exec.Command("git", append([]string{"-C", repo}, args...)...).CombinedOutput()
		return strings.TrimSpace(string(out)), err
	}
	w.Must(os.MkdirAll(repo, 0o755))
	for _, args := range [][]string{
		{"init", "-q"},
		{"remote", "add", "origin", os.Getenv("CAOS_SERVER_URL")},
		{"config", "extensions.partialClone", "origin"},
		{"config", "remote.origin.promisor", "true"},
		{"config", "remote.origin.partialclonefilter", "tree:0"},
	} {
		out, err := git(args...)
		w.True(err == nil, "git %v: %v: %s", args, err, out)
	}
	fetch := []string{"fetch", "-q", "--filter=tree:0", "origin", base}
	for _, l := range s.Layers {
		fetch = append(fetch, l.Commit)
	}
	out, err := git(fetch...)
	w.True(err == nil, "git fetch: %v: %s", err, out)
	below, tip := "00-base", base
	for _, l := range s.Layers {
		cmd := exec.Command("git", "-C", repo, "merge-base", "--is-ancestor", tip, l.Commit)
		err := cmd.Run()
		if exit, ok := err.(*exec.ExitError); ok && exit.ExitCode() == 1 {
			refuse("%s does not contain %s (%s is not an ancestor of %s). Merge it first: run_tool(path=\"caos-std/merge\", arguments={\"root\": \"<stack>/%s\", \"theirs\": \"<stack>/%s\"})",
				l.Name, below, tip, l.Commit, l.Name, below)
		}
		w.True(err == nil, "git merge-base: %v", err)
		below, tip = l.Name, l.Commit
	}
}

// plan reads the stack at root and decides everything before the remote is
// read: the layers to publish, their branches, bases and text.
func plan() (*State, string) {
	const root = "/cas/args/root"
	if !w.Exists(root) {
		refuse("nothing is at the root path: root is the stack directory, such as sources/repo")
	}
	if k := kind(root); k != "tree" {
		refuse("the root is a %s: stack-publish takes a stack directory, such as sources/repo", k)
	}
	w.Get(root)
	type entry struct{ name, short, commit, title, body string }
	var base string
	var layers []*entry
	descriptions := map[string]string{}
	var described []string
	for _, e := range w.Check(os.ReadDir(root)) {
		name := e.Name()
		path := filepath.Join(root, name)
		if stem, ok := strings.CutSuffix(name, ".pr.md"); ok {
			if kind(path) != "blob" {
				refuse("%s is not a file", name)
			}
			descriptions[stem] = w.Read(path)
			described = append(described, stem)
			continue
		}
		m := layerName.FindStringSubmatch(name)
		if m == nil {
			continue
		}
		if m[1] == "00" {
			if name == "00-base" && kind(path) == "commit" {
				base = w.Caos("hash", path)
			}
			continue
		}
		if kind(path) != "commit" {
			refuse("%[1]s is named like a layer but is not a gitlink. A layer is a source tree; its description is %[1]s.pr.md beside it.", name)
		}
		layers = append(layers, &entry{name: name, short: m[2], commit: w.Caos("hash", path)})
	}
	if base == "" {
		refuse("the stack has no 00-base gitlink: import the upstream tip there first (std/GIT.md)")
	}
	for _, stem := range described {
		i := slices.IndexFunc(layers, func(l *entry) bool { return l.name == stem })
		if i < 0 {
			refuse("%[1]s.pr.md describes no layer: there is no gitlink %[1]s", stem)
		}
		layers[i].title, layers[i].body = description(stem, descriptions[stem])
	}
	undescribed := ""
	for _, l := range layers {
		switch {
		case l.title == "":
			undescribed = l.name
		case undescribed != "":
			refuse("%s has a description but %s below it has none, so its PR would have no branch to target. Write %s.pr.md, or take %s out of the stack.",
				l.name, undescribed, undescribed, undescribed)
		}
	}

	var requested, defaultBranch, imported string
	if w.Exists(root + "/00-base.source.json") {
		var provenance struct {
			Repository string `json:"repository"`
			Requested  string `json:"requested_revision"`
			Default    string `json:"default_branch"`
		}
		if json.Unmarshal([]byte(w.Read(root+"/00-base.source.json")), &provenance) != nil {
			refuse("00-base.source.json is not JSON")
		}
		requested, defaultBranch, imported = provenance.Requested, provenance.Default, provenance.Repository
	}
	s := &State{
		Repository:   arg("repository"),
		Trunk:        arg("trunk"),
		Force:        flag("force", false),
		AllowIgnored: flag("allow-ignored", false),
		Link:         flag("link", true),
		Draft:        flag("draft", false),
	}
	if s.Repository == "" {
		s.Repository = imported
	}
	if s.Repository == "" {
		refuse("no repository: 00-base.source.json names none, so pass repository")
	}
	if !remoteURL.MatchString(s.Repository) {
		refuse("repository must be an HTTPS URL without credentials, not %q", s.Repository)
	}
	if m := githubURL.FindStringSubmatch(s.Repository); m != nil {
		s.Owner, s.Repo = m[2], m[3]
	}
	if s.Trunk == "" {
		s.Trunk = inferTrunk(requested, defaultBranch)
	}
	if s.Trunk == "" {
		refuse("no trunk: 00-base.source.json names no branch, so pass trunk")
	}
	checkBranch("trunk", s.Trunk)
	prefix := arg("branch-prefix")
	below := s.Trunk
	for _, l := range layers {
		if l.title == "" {
			continue
		}
		branch := prefix + l.short
		checkBranch("layer "+l.name+"'s branch", branch)
		if branch == s.Trunk {
			refuse("layer %s would publish to the trunk %s itself; rename it or pass branch-prefix", l.name, s.Trunk)
		}
		if slices.ContainsFunc(s.Layers, func(p Layer) bool { return p.Branch == branch }) {
			refuse("two layers share the branch name %s: layers differ only in their numbers, rename one", branch)
		}
		s.Layers = append(s.Layers, Layer{Name: l.name, Branch: branch, Base: below, Title: l.title, Body: l.body, Commit: l.commit})
		below = branch
	}
	if len(s.Layers) == 0 {
		refuse("no layer in the stack has a .pr.md description, so there is nothing to publish")
	}
	return s, base
}

// expected reads `expected`, the leases of a force push: one line per layer,
// `<branch> <commit>` or `<branch> absent`, as a dry run prints them.
func expected(s *State) {
	text := arg("expected")
	if text == "" {
		refuse("a force push needs expected: one line `<branch> <head>` per layer, the heads you looked at (a full commit hash, or absent for a new branch). A dry run prints them.")
	}
	leases := map[string]string{}
	for _, line := range strings.Split(text, "\n") {
		fields := strings.Fields(line)
		if len(fields) == 0 {
			continue
		}
		if len(fields) != 2 || (fields[1] != "absent" && !hash.MatchString(fields[1])) {
			refuse("expected: %q is not `<branch> <full commit hash>` or `<branch> absent`", line)
		}
		leases[fields[0]] = strings.ToLower(fields[1])
	}
	s.Expected = make([]string, len(s.Layers))
	for i, l := range s.Layers {
		lease, ok := leases[l.Branch]
		if !ok {
			refuse("expected names no head for branch %s: give every layer's branch", l.Branch)
		}
		s.Expected[i] = lease
		delete(leases, l.Branch)
	}
	for branch := range leases {
		refuse("expected names %s, which is no layer's branch", branch)
	}
}

// heads is a batch that reads the repository's current name (a renamed
// repository answers at its old name, but a `head=<owner>:` filter matches
// literally) and every layer's branch head.
func heads(s *State) []map[string]any {
	var ops []map[string]any
	if s.Owner != "" {
		ops = append(ops, map[string]any{"op": "api", "method": "GET", "path": "/repos/" + s.Owner + "/" + s.Repo})
	}
	for _, l := range s.Layers {
		ops = append(ops, map[string]any{"op": "head", "repository": s.Repository, "branch": l.Branch})
	}
	return ops
}

// observe reads the answer to `heads`: the repository's name, then the heads.
// It returns the first problem instead, if any.
func observe(s *State) ([]string, string) {
	n := len(s.Layers)
	skip := 0
	if s.Owner != "" {
		skip = 1
	}
	rs := results(n + skip)
	if skip == 1 {
		if rs[0]["ok"] != true {
			return nil, "reading the repository: " + why(rs[0])
		}
		if body, ok := rs[0]["body"].(map[string]any); ok {
			owner, _ := body["owner"].(map[string]any)
			if login, name := str(owner["login"]), str(body["name"]); login != "" && name != "" {
				s.Owner, s.Repo = login, name
			}
		}
	}
	var out []string
	for i, r := range rs[skip:] {
		if r["ok"] != true {
			return nil, why(r) + " for branch " + s.Layers[i].Branch
		}
		head := str(r["head"])
		if head == "" {
			head = "absent"
		}
		out = append(out, head)
	}
	return out, ""
}

func start() {
	s, base := plan()
	if !flag("dry-run", false) && s.Force {
		expected(s)
	}
	containsBelow(base, s)
	switch {
	case flag("dry-run", false):
		next("dry", heads(s), s)
	default:
		next("push", heads(s), s)
	}
}

func header(s *State) string {
	return fmt.Sprintf("repository %s  trunk %s\n", s.Repository, s.Trunk)
}

func dry(s *State) {
	observed, problem := observe(s)
	var out strings.Builder
	out.WriteString("dry run: nothing pushed or changed.\n" + header(s))
	for _, l := range s.Layers {
		fmt.Fprintf(&out, "%s  %s  %s  base %s  PR title: %s\n", l.Name, l.Branch, short(l.Commit), l.Base, l.Title)
	}
	if problem != "" {
		fmt.Fprintf(&out, "the remote heads were not read: %s\n", problem)
	} else {
		out.WriteString("remote heads (expected, for a force push):\n")
		for i, l := range s.Layers {
			fmt.Fprintf(&out, "%s %s\n", l.Branch, observed[i])
		}
	}
	answer(out.String(), false)
}

func push(s *State) {
	observed, problem := observe(s)
	if problem != "" {
		answer("stack-publish: "+problem+"; nothing was pushed\n", true)
		return
	}
	if !s.Force {
		s.Expected = observed
	}
	var ops []map[string]any
	for i, l := range s.Layers {
		ops = append(ops, map[string]any{
			"op": "push", "repository": s.Repository, "branch": l.Branch, "commit": l.Commit,
			"expected": s.Expected[i], "force": s.Force, "allow_ignored": s.AllowIgnored,
		})
	}
	next("list", ops, s)
}

// list reads the pushes, and for a fully pushed stack on github.com reads each
// branch's open pull request.
func list(s *State) {
	rs := results(len(s.Layers))
	var problem string
	for i, l := range s.Layers {
		r := rs[i]
		line := fmt.Sprintf("%s  %s  %s  ", l.Name, l.Branch, short(l.Commit))
		status := str(r["status"])
		switch {
		case r["skipped"] == true:
			line += "not attempted"
		case r["ok"] == true && s.Expected[i] == strings.ToLower(l.Commit):
			line += "unchanged"
		case r["ok"] == true && s.Expected[i] == "absent":
			line += "created"
		case r["ok"] == true && s.Force:
			line += "pushed (leased on " + short(s.Expected[i]) + ")"
		case r["ok"] == true:
			line += "pushed (fast-forward)"
		default:
			what := "REJECTED"
			if status == "uncertain" {
				what = "UNCERTAIN"
			}
			reason := str(r["diagnostic"])
			if reason == "" {
				reason = str(r["kind"])
			}
			if reason == "" {
				reason = why(r)
			}
			line += what + " (" + reason + ")"
			problem = fmt.Sprintf("the push of %s to %s was %s: %s. The layers above it were not pushed.",
				l.Name, l.Branch, strings.ToLower(what), reason)
			if status == "uncertain" {
				problem += " It may still have landed: read the branch before acting again."
			}
		}
		s.Lines = append(s.Lines, line)
	}
	if problem != "" {
		done(s, problem)
		return
	}
	if s.Owner == "" {
		s.StackLine = s.Repository + " is not on github.com: branches pushed, no pull requests opened"
		done(s, "")
		return
	}
	var ops []map[string]any
	for _, l := range s.Layers {
		ops = append(ops, map[string]any{"op": "api", "method": "GET",
			"path": fmt.Sprintf("/repos/%s/%s/pulls?head=%s:%s&state=open", s.Owner, s.Repo, s.Owner, l.Branch)})
	}
	next("prs", ops, s)
}

func normalize(text string) string {
	return strings.TrimSpace(strings.ReplaceAll(text, "\r\n", "\n"))
}

func stackOf(pr map[string]any) *StackRef {
	stack, ok := pr["stack"].(map[string]any)
	if !ok {
		return nil
	}
	return &StackRef{num(stack["number"]), num(stack["position"]), num(stack["size"])}
}

// prs reads each branch's open pull request and plans the changes: open the
// missing ones on the branch below, bring the text and base of the rest in
// line with the plan. A base change for a PR in a GitHub stack waits for the
// link stage, which has to unstack it first.
func prs(s *State) {
	rs := results(len(s.Layers))
	repo := fmt.Sprintf("/repos/%s/%s", s.Owner, s.Repo)
	var creates, patches []map[string]any
	s.PRs = make([]PR, len(s.Layers))
	for i, l := range s.Layers {
		if rs[i]["ok"] != true {
			done(s, "the branches are pushed, but GitHub refused while handling the pull requests: "+why(rs[i]))
			return
		}
		found, _ := rs[i]["body"].([]any)
		if len(found) == 0 {
			s.Creates = append(s.Creates, i)
			creates = append(creates, map[string]any{"op": "api", "method": "POST", "path": repo + "/pulls",
				"body": map[string]any{"title": l.Title, "head": l.Branch, "base": l.Base, "body": l.Body, "draft": s.Draft}})
			continue
		}
		pr, _ := found[0].(map[string]any)
		number := num(pr["number"])
		baseRef := ""
		if b, ok := pr["base"].(map[string]any); ok {
			baseRef = str(b["ref"])
		}
		s.PRs[i] = PR{Number: number, Stack: stackOf(pr)}
		var notes []string
		patch := map[string]any{}
		if strings.TrimSpace(str(pr["title"])) != strings.TrimSpace(l.Title) || normalize(str(pr["body"])) != normalize(l.Body) {
			patch["title"], patch["body"] = l.Title, l.Body
			notes = append(notes, "description updated from the .pr.md")
		}
		if baseRef != l.Base {
			if s.PRs[i].Stack != nil {
				s.PRs[i].Deferred = l.Base
				notes = append(notes, fmt.Sprintf("base %s -> %s waits for the stack step", baseRef, l.Base))
			} else {
				patch["base"] = l.Base
				notes = append(notes, fmt.Sprintf("base %s -> %s", baseRef, l.Base))
			}
		}
		if len(patch) > 0 {
			patches = append(patches, map[string]any{"op": "api", "method": "PATCH",
				"path": fmt.Sprintf("%s/pulls/%d", repo, number), "body": patch})
		}
		if len(notes) == 0 {
			s.Lines[i] += fmt.Sprintf("  PR #%d unchanged", number)
		} else {
			s.Lines[i] += fmt.Sprintf("  PR #%d: %s", number, strings.Join(notes, ", "))
		}
	}
	s.Patches = len(patches)
	ops := append(creates, patches...)
	if len(ops) == 0 {
		link(s)
		return
	}
	next("link", ops, s)
}

// StackPlan is what the link stage does, from where each PR is now.
type StackPlan struct {
	Kind    string // unchanged, create, extend, recreate
	Stack   int64
	Add     []int64
	Unstack []int64
}

func planStack(prs []PR) StackPlan {
	inOrder := func(first *StackRef, slice []PR) bool {
		for i, pr := range slice {
			if pr.Stack == nil || pr.Stack.Number != first.Number || pr.Stack.Position != int64(i+1) {
				return false
			}
		}
		return true
	}
	none := func(slice []PR) bool {
		for _, pr := range slice {
			if pr.Stack != nil {
				return false
			}
		}
		return true
	}
	if len(prs) > 0 && prs[0].Stack != nil {
		first := prs[0].Stack
		if first.Size == int64(len(prs)) && inOrder(first, prs) {
			return StackPlan{Kind: "unchanged", Stack: first.Number}
		}
		k := int(first.Size)
		if k < len(prs) && inOrder(first, prs[:k]) && none(prs[k:]) {
			p := StackPlan{Kind: "extend", Stack: first.Number}
			for _, pr := range prs[k:] {
				p.Add = append(p.Add, pr.Number)
			}
			return p
		}
	} else if none(prs) {
		return StackPlan{Kind: "create"}
	}
	return StackPlan{Kind: "recreate", Unstack: stacks(prs)}
}

func stacks(prs []PR) []int64 {
	var out []int64
	for _, pr := range prs {
		if pr.Stack != nil && !slices.Contains(out, pr.Stack.Number) {
			out = append(out, pr.Stack.Number)
		}
	}
	slices.Sort(out)
	return out
}

// link reads the PR changes, when there were any, and links the PRs as a
// GitHub stack, or brings the one they are in up to date.
func link(s *State) {
	if arg("stage") == "link" {
		rs := results(len(s.Creates) + s.Patches)
		for k, r := range rs {
			if r["ok"] != true {
				done(s, "the branches are pushed, but GitHub refused while handling the pull requests: "+why(r))
				return
			}
			if k < len(s.Creates) {
				i := s.Creates[k]
				body, _ := r["body"].(map[string]any)
				s.PRs[i] = PR{Number: num(body["number"])}
				s.Lines[i] += fmt.Sprintf("  PR #%d created, base %s", s.PRs[i].Number, s.Layers[i].Base)
			}
		}
	}
	var numbers []int64
	var deferred []int
	for i, pr := range s.PRs {
		numbers = append(numbers, pr.Number)
		if pr.Deferred != "" {
			deferred = append(deferred, i)
		}
	}
	if !s.Link {
		if len(deferred) == 0 {
			s.StackLine = "stack: not linked (link=false)"
			done(s, "")
			return
		}
		done(s, fmt.Sprintf("the base of %d PR(s) was not changed: they are in GitHub stack(s) %v, which refuse base changes. Publish with link=true to restack them.",
			len(deferred), stacks(s.PRs)))
		return
	}
	p := planStack(s.PRs)
	if len(deferred) > 0 && p.Kind != "recreate" {
		p = StackPlan{Kind: "recreate", Unstack: stacks(s.PRs)}
	}
	if len(numbers) < 2 && p.Kind != "recreate" {
		s.StackLine = "stack: one layer, nothing to link"
		done(s, "")
		return
	}
	repo := fmt.Sprintf("/repos/%s/%s", s.Owner, s.Repo)
	create := map[string]any{"op": "api", "method": "POST", "path": repo + "/stacks",
		"body": map[string]any{"pull_requests": numbers}}
	switch p.Kind {
	case "unchanged":
		s.StackLine = fmt.Sprintf("stack %d: unchanged", p.Stack)
		done(s, "")
	case "create":
		s.StackLine = fmt.Sprintf("linked %v", numbers)
		next("done", []map[string]any{create}, s)
	case "extend":
		s.StackLine = fmt.Sprintf("stack %d: added %v", p.Stack, p.Add)
		next("done", []map[string]any{{"op": "api", "method": "POST",
			"path": fmt.Sprintf("%s/stacks/%d/add", repo, p.Stack), "body": map[string]any{"pull_requests": p.Add}}}, s)
	case "recreate":
		var ops []map[string]any
		for _, stack := range p.Unstack {
			ops = append(ops, map[string]any{"op": "api", "method": "POST",
				"path": fmt.Sprintf("%s/stacks/%d/unstack", repo, stack)})
		}
		for _, i := range deferred {
			ops = append(ops, map[string]any{"op": "api", "method": "PATCH",
				"path": fmt.Sprintf("%s/pulls/%d", repo, s.PRs[i].Number), "body": map[string]any{"base": s.PRs[i].Deferred}})
		}
		if len(numbers) < 2 {
			s.StackLine = fmt.Sprintf("stack: unstacked %v; one layer, nothing to link", p.Unstack)
		} else {
			s.StackLine = fmt.Sprintf("recreated with %v (unstacked %v)", numbers, p.Unstack)
			ops = append(ops, create)
		}
		if len(ops) == 0 {
			done(s, "")
			return
		}
		next("done", ops, s)
	}
}

// linked reads the stack step's answer: the last operation is the one that
// names the stack.
func linked(s *State) {
	var rs []map[string]any
	w.Must(json.Unmarshal([]byte(w.Read("/cas/args/result")), &rs))
	for _, r := range rs {
		if r["ok"] != true {
			done(s, "the pull requests are up to date, but linking them as a stack failed: "+why(r))
			return
		}
	}
	if body, ok := rs[len(rs)-1]["body"].(map[string]any); ok && body["number"] != nil &&
		(strings.HasPrefix(s.StackLine, "linked") || strings.HasPrefix(s.StackLine, "recreated")) {
		s.StackLine = fmt.Sprintf("stack %d: %s", num(body["number"]), s.StackLine)
	}
	done(s, "")
}

// done renders the receipt: one line per layer, the stack, and the problem
// that stopped the call, if one did.
func done(s *State, problem string) {
	var out strings.Builder
	out.WriteString(header(s))
	for _, line := range s.Lines {
		out.WriteString(line + "\n")
	}
	if s.StackLine != "" && problem == "" {
		out.WriteString(s.StackLine + "\n")
	}
	if problem != "" {
		out.WriteString("FAILED: " + problem + "\n")
	}
	answer(out.String(), problem != "")
}

func main() {
	w.Main(func() {
		defer func() {
			r := recover()
			if r == nil {
				return
			}
			no, ok := r.(refusal)
			if !ok {
				panic(r)
			}
			answer("stack-publish refused; nothing was pushed: "+no.msg+"\n", true)
		}()
		switch stage := arg("stage"); stage {
		case "":
			start()
		case "dry":
			dry(load())
		case "push":
			push(load())
		case "list":
			list(load())
		case "prs":
			prs(load())
		case "link":
			link(load())
		case "done":
			linked(load())
		default:
			refuse("unknown stage %q", stage)
		}
	})
}
