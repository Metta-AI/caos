// The `stack-merge-to-master` tool's worker. Its docs live in the sibling
// `.caos-expr` here-string, not in this header (SPEC, "CaosTools").
//
// Merge a stack's pull requests, bottom to top, each squashed with its
// `.pr.md` as the commit message (design/stack-publish.md,
// "stack-merge-to-master"), as an ordinary worker (design/tool-contract.md):
// it holds no token, and makes every GitHub call through its neighbour
// `DEEP-DEPS/github`. Everything is read before anything is merged: each
// layer to merge must have an open pull request whose head is the layer's
// commit, and CI on each of those heads must be passing unless `force`. Then
// each PR is merged with GitHub's asynchronous merge, one at a time, the next
// once GitHub has pointed it at the trunk. A merge that fails stops there.
//
// The work is a chain of std/github batches, each answered into the next
// stage of this worker, which carries the state:
//
//	start    check the stack, choose the layers; read the repository's name
//	list     read each layer's open and closed pull requests
//	ci       check the PRs; read CI on their heads
//	verdict  decide, or read the pending heads again (wait-for-ci-min)
//	based    read the next PR to merge until it targets the trunk
//	merging  follow its asynchronous merge to the end
package main

import (
	"encoding/json"
	"fmt"
	"os"
	"os/exec"
	"path/filepath"
	"regexp"
	"sort"
	"strconv"
	"strings"
	"time"

	"caos/w"
)

type refusal struct{ msg string }

func refuse(format string, a ...any) { panic(refusal{fmt.Sprintf(format, a...)}) }

func arg(name string) string {
	path := "/cas/args/" + name
	if !w.Exists(path) {
		return ""
	}
	return strings.TrimSpace(w.Read(path))
}

func flag(name string) bool {
	switch strings.ToLower(arg(name)) {
	case "", "false", "0", "no":
		return false
	case "true", "1", "yes":
		return true
	}
	refuse("%s is true or false, not %q", name, arg(name))
	return false
}

// answer is the result under the root contract: `{stdout, failed?}`.
func answer(text string, failed bool) {
	const dir = "/tmp/merge-out"
	w.Must(os.MkdirAll(dir, 0o755))
	w.Must(os.WriteFile(filepath.Join(dir, "stdout"), []byte(text), 0o644))
	if failed {
		w.Must(os.WriteFile(filepath.Join(dir, "failed"), []byte("stack-merge-to-master failed\n"), 0o644))
	}
	w.Put(dir, "/cas/out")
}

var (
	layerName = regexp.MustCompile(`^([0-9]{2})-(.+)$`)
	hash      = regexp.MustCompile(`^[0-9a-fA-F]{40}$`)
	githubURL = regexp.MustCompile(`(?i)^https://github\.com(:443)?/([A-Za-z0-9_.-]+)/([A-Za-z0-9_.-]+?)(\.git)?/?$`)
	remoteURL = regexp.MustCompile(`^https://[^/@\s]+/\S+$`)
)

// Check is one check run or commit status, as ci-status prints it.
type Check struct {
	Name     string `json:"name"`
	State    string `json:"state"`
	Standing string `json:"standing"` // passing, failing or pending
	URL      string `json:"url"`
}

// Target is one layer to merge.
type Target struct {
	Name    string  `json:"name"`
	Branch  string  `json:"branch"`
	Title   string  `json:"title"`
	Body    string  `json:"body"`
	Commit  string  `json:"commit"`
	Number  int64   `json:"number"`  // its open PR, 0 when it is already merged
	Already int64   `json:"already"` // its merged PR, when it has no open one
	Checks  []Check `json:"checks"`
	Read    bool    `json:"read"` // its checks have been read
	Outcome string  `json:"outcome"`
}

// State is what one stage hands the next.
type State struct {
	Repository string   `json:"repository"`
	Trunk      string   `json:"trunk"`
	Owner      string   `json:"owner"`
	Repo       string   `json:"repo"`
	Force      bool     `json:"force"`
	DryRun     bool     `json:"dry_run"`
	Minutes    int      `json:"minutes"`
	Deadline   int64    `json:"deadline"` // of the CI wait
	Targets    []Target `json:"targets"`
	Reading    []int    `json:"reading"` // the targets the CI batch reads, in op order
	Step       int      `json:"step"`    // counts batches, for their request ids
	K          int      `json:"k"`       // the target being merged
	Until      int64    `json:"until"`   // the deadline of the current wait for GitHub
	Merged     int      `json:"merged"`
}

func next(stage string, ops []map[string]any, s *State) {
	s.Step++
	w.Must(os.WriteFile("/tmp/state.json", w.Check(json.Marshal(s)), 0o644))
	w.Put("/tmp/state.json", "/cas/state")
	w.Must(os.WriteFile("/tmp/batch.json", w.Check(json.Marshal(ops)), 0o644))
	w.Put("/tmp/batch.json", "/cas/batch")
	id := arg("request-id")
	request := w.Caos("prepare-request", "--base:@=/cas/args/github", "--batch:@=/cas/batch",
		fmt.Sprintf("--request-id=%s-%s-%d", id, stage, s.Step))
	then := w.Caos("curry", "--base:@=/cas/args/base", "--worker1:@=/cas/args/worker1",
		"--github:@=/cas/args/github", "--stage="+stage, "--request-id="+id, "--state:@=/cas/state")
	w.Caos("run-request-then", request, "--then:hash="+then)
}

func load() *State {
	var s State
	w.Must(json.Unmarshal([]byte(w.Read("/cas/args/state")), &s))
	return &s
}

// results is std/github's answer; an operation that is not ok refuses, with
// what GitHub said, unless it is the last and `lastMayFail`.
func results(n int) []map[string]any {
	var rs []map[string]any
	text := w.Read("/cas/args/result")
	if json.Unmarshal([]byte(text), &rs) != nil || (n >= 0 && len(rs) != n) {
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

func obj(v any) map[string]any {
	m, _ := v.(map[string]any)
	return m
}

func why(r map[string]any) string {
	if e := str(r["error"]); e != "" {
		return e
	}
	if r["skipped"] == true {
		return "not attempted: an operation before it failed"
	}
	message := str(r["body"])
	if body := obj(r["body"]); body != nil {
		message = str(body["message"])
	}
	return strings.TrimSpace(fmt.Sprintf("%s %s: HTTP %v %s", str(r["method"]), str(r["path"]), r["status"], message))
}

// ok refuses (or, once merging has begun, fails) on an operation that is not
// ok.
func ok(s *State, r map[string]any) {
	if r["ok"] == true {
		return
	}
	if s != nil {
		fail(s, why(r))
	}
	refuse("%s", why(r))
}

func short(commit string) string {
	if len(commit) > 12 {
		return commit[:12]
	}
	return commit
}

func kind(path string) string { return w.Caos("kind", path) }

func checkBranch(what, branch string) {
	if strings.HasPrefix(branch, "refs/") || exec.Command("git", "check-ref-format", "--branch", branch).Run() != nil {
		refuse("%s %q is not a valid branch name", what, branch)
	}
}

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

// start reads the stack at root and chooses the layers to merge: the
// described ones, up to and including up-to.
func start() {
	const root = "/cas/args/root"
	if !w.Exists(root) {
		refuse("nothing is at the root path: root is the stack directory, such as sources/repo")
	}
	if k := kind(root); k != "tree" {
		refuse("the root is a %s: stack-merge-to-master takes a stack directory, such as sources/repo", k)
	}
	w.Get(root)
	s := &State{Force: flag("force"), DryRun: flag("dry-run")}
	if text := arg("wait-for-ci-min"); text != "" {
		m, err := strconv.Atoi(text)
		if err != nil || m < 0 {
			refuse("wait-for-ci-min is a whole number of minutes, such as 20, not %q", text)
		}
		s.Minutes = m
	}
	type layer struct{ name, short, commit, title, body string }
	var layers []*layer
	descriptions := map[string]string{}
	for _, e := range w.Check(os.ReadDir(root)) {
		name := e.Name()
		path := filepath.Join(root, name)
		if stem, ok := strings.CutSuffix(name, ".pr.md"); ok {
			descriptions[stem] = w.Read(path)
			continue
		}
		m := layerName.FindStringSubmatch(name)
		if m == nil || m[1] == "00" {
			continue
		}
		if kind(path) != "commit" {
			refuse("%[1]s is named like a layer but is not a gitlink. A layer is a source tree; its description is %[1]s.pr.md beside it.", name)
		}
		layers = append(layers, &layer{name: name, short: m[2], commit: w.Caos("hash", path)})
	}
	for stem, text := range descriptions {
		found := false
		for _, l := range layers {
			if l.name == stem {
				l.title, l.body = description(stem, text)
				found = true
			}
		}
		if !found {
			refuse("%[1]s.pr.md describes no layer: there is no gitlink %[1]s", stem)
		}
	}
	top := len(layers)
	if upTo := strings.Trim(arg("up-to"), "/"); upTo != "" {
		top = -1
		for i, l := range layers {
			if l.name == upTo || l.short == upTo || l.name[:2] == upTo {
				top = i + 1
				if l.title == "" {
					refuse("up-to %s: it has no .pr.md, so it has no pull request to merge", l.name)
				}
				break
			}
		}
		if top < 0 {
			refuse("up-to %q names no layer of the stack", upTo)
		}
	}
	prefix := arg("branch-prefix")
	for _, l := range layers[:top] {
		if l.title == "" {
			continue
		}
		branch := prefix + l.short
		checkBranch("layer "+l.name+"'s branch", branch)
		s.Targets = append(s.Targets, Target{Name: l.name, Branch: branch, Title: l.title, Body: l.body, Commit: l.commit})
	}
	if len(s.Targets) == 0 {
		refuse("no layer has a .pr.md description, so none has a pull request to merge")
	}

	var requested, defaultBranch string
	if w.Exists(root + "/00-base.source.json") {
		var p struct {
			Repository string `json:"repository"`
			Requested  string `json:"requested_revision"`
			Default    string `json:"default_branch"`
		}
		if json.Unmarshal([]byte(w.Read(root+"/00-base.source.json")), &p) != nil {
			refuse("00-base.source.json is not JSON")
		}
		s.Repository, requested, defaultBranch = p.Repository, p.Requested, p.Default
	}
	if r := arg("repository"); r != "" {
		s.Repository = r
	}
	if s.Repository == "" {
		refuse("no repository: 00-base.source.json names none, so pass repository")
	}
	if !remoteURL.MatchString(s.Repository) {
		refuse("repository must be an HTTPS URL without credentials, not %q", s.Repository)
	}
	s.Trunk = arg("trunk")
	if s.Trunk == "" {
		s.Trunk = inferTrunk(requested, defaultBranch)
	}
	if s.Trunk == "" {
		refuse("no trunk: 00-base.source.json names no branch, so pass trunk")
	}
	checkBranch("trunk", s.Trunk)
	m := githubURL.FindStringSubmatch(s.Repository)
	if m == nil {
		refuse("%s is not on github.com, and merging pull requests needs GitHub", s.Repository)
	}
	s.Owner, s.Repo = m[2], m[3]
	next("list", []map[string]any{{"op": "api", "method": "GET", "path": "/repos/" + s.Owner + "/" + s.Repo}}, s)
}

func repoPath(s *State, rest string) string {
	return fmt.Sprintf("/repos/%s/%s%s", s.Owner, s.Repo, rest)
}

// list takes the repository's current name (a renamed repository answers at
// its old name, but a `head=<owner>:` filter matches literally) and reads
// each layer's pull requests.
func list(s *State) {
	r := results(1)[0]
	ok(nil, r)
	body := obj(r["body"])
	if login, name := str(obj(body["owner"])["login"]), str(body["name"]); login != "" && name != "" {
		s.Owner, s.Repo = login, name
	}
	var ops []map[string]any
	for _, t := range s.Targets {
		for _, state := range []string{"open", "closed"} {
			ops = append(ops, map[string]any{"op": "api", "method": "GET",
				"path": repoPath(s, fmt.Sprintf("/pulls?head=%s:%s&state=%s", s.Owner, t.Branch, state))})
		}
	}
	next("ci", ops, s)
}

// readCI is a batch reading the checks of every PR head still pending (all of
// them the first time).
func readCI(s *State) {
	s.Reading = nil
	var ops []map[string]any
	for i, t := range s.Targets {
		if t.Number == 0 || (t.Read && verdict(t.Checks) != "pending") {
			continue
		}
		s.Reading = append(s.Reading, i)
		base := repoPath(s, "/commits/"+t.Commit)
		ops = append(ops,
			map[string]any{"op": "api", "method": "GET", "path": base + "/check-runs?per_page=100"},
			map[string]any{"op": "api", "method": "GET", "path": base + "/status"})
	}
	if len(ops) == 0 {
		decide(s)
		return
	}
	next("verdict", ops, s)
}

// ciStage checks each layer's pull request: open, not a draft, and holding
// the layer's commit; or already merged.
func ciStage(s *State) {
	rs := results(2 * len(s.Targets))
	for _, r := range rs {
		ok(nil, r)
	}
	for i := range s.Targets {
		t := &s.Targets[i]
		open, _ := rs[2*i]["body"].([]any)
		closed, _ := rs[2*i+1]["body"].([]any)
		if len(open) == 0 {
			for _, raw := range closed {
				if pr := obj(raw); pr["merged_at"] != nil {
					t.Already = num(pr["number"])
					break
				}
			}
			if t.Already == 0 {
				refuse("%s has no open pull request on %s: run caos-std/stack-publish first", t.Name, t.Branch)
			}
			continue
		}
		pr := obj(open[0])
		t.Number = num(pr["number"])
		if pr["draft"] == true {
			refuse("PR #%d (%s) is a draft: mark it ready for review first", t.Number, t.Name)
		}
		if head := str(obj(pr["head"])["sha"]); head != t.Commit {
			refuse("PR #%d holds %s but %s is %s: run caos-std/stack-sync (to adopt what GitHub holds) and caos-std/stack-publish (to push the layer) so the pull request merges what the stack holds",
				t.Number, short(head), t.Name, short(t.Commit))
		}
	}
	s.Deadline = time.Now().Add(time.Duration(s.Minutes) * time.Minute).Unix()
	readCI(s)
}

func runStanding(status, conclusion string) string {
	if status != "completed" {
		return "pending"
	}
	switch conclusion {
	case "success", "neutral", "skipped":
		return "passing"
	}
	return "failing"
}

func statusStanding(state string) string {
	switch state {
	case "success":
		return "passing"
	case "pending":
		return "pending"
	}
	return "failing"
}

// verdict is a layer's CI as one word: failing, pending, passing, or "" when
// it has no checks.
func verdict(checks []Check) string {
	v := ""
	for _, c := range checks {
		switch {
		case c.Standing == "failing":
			return "failing"
		case c.Standing == "pending":
			v = "pending"
		case v == "":
			v = "passing"
		}
	}
	return v
}

func summary(checks []Check) string {
	named := map[string][]string{}
	for _, c := range checks {
		named[c.Standing] = append(named[c.Standing], c.Name)
	}
	switch {
	case len(checks) == 0:
		return "no checks"
	case len(named["failing"]) > 0:
		text := "FAILING: " + strings.Join(named["failing"], ", ")
		if len(named["pending"]) > 0 {
			text += "; pending: " + strings.Join(named["pending"], ", ")
		}
		return text
	case len(named["pending"]) > 0:
		return fmt.Sprintf("pending: %s (%d passing)", strings.Join(named["pending"], ", "), len(named["passing"]))
	}
	return fmt.Sprintf("%d passing", len(named["passing"]))
}

// verdictStage reads CI, then waits and reads again, or decides.
func verdictStage(s *State) {
	rs := results(2 * len(s.Reading))
	for _, r := range rs {
		ok(nil, r)
	}
	for k, i := range s.Reading {
		var checks []Check
		runs, _ := obj(rs[2*k]["body"])["check_runs"].([]any)
		for _, raw := range runs {
			run := obj(raw)
			state := str(run["conclusion"])
			if state == "" {
				state = str(run["status"])
			}
			url := str(run["html_url"])
			if url == "" {
				url = str(run["details_url"])
			}
			checks = append(checks, Check{str(run["name"]), state, runStanding(str(run["status"]), str(run["conclusion"])), url})
		}
		statuses, _ := obj(rs[2*k+1]["body"])["statuses"].([]any)
		for _, raw := range statuses {
			st := obj(raw)
			checks = append(checks, Check{str(st["context"]), str(st["state"]), statusStanding(str(st["state"])), str(st["target_url"])})
		}
		s.Targets[i].Checks, s.Targets[i].Read = checks, true
	}
	// Settled, as ci-status's wait-min: a check failed, or none is pending.
	failing, pending := false, false
	for _, t := range s.Targets {
		switch verdict(t.Checks) {
		case "failing":
			failing = true
		case "pending":
			pending = true
		}
	}
	settled := failing || !pending
	if !settled && time.Now().Unix() < s.Deadline {
		time.Sleep(min(15*time.Second, time.Until(time.Unix(s.Deadline, 0))))
		readCI(s)
		return
	}
	decide(s)
}

func header(s *State) string {
	return fmt.Sprintf("repository %s  trunk %s\n", s.Repository, s.Trunk)
}

func line(t Target) string {
	text := t.Name + "  " + t.Branch
	switch {
	case t.Number != 0:
		text += fmt.Sprintf("  PR #%d  checks: %s", t.Number, summary(t.Checks))
	case t.Already != 0:
		text += fmt.Sprintf("  PR #%d  already merged", t.Already)
	}
	if t.Outcome != "" {
		text += "  " + t.Outcome
	}
	return text + "\n"
}

// detail is every check of each layer whose CI is not passing, as ci-status
// prints them.
func detail(s *State) string {
	var out strings.Builder
	for _, t := range s.Targets {
		if v := verdict(t.Checks); t.Number == 0 || (v != "failing" && v != "pending") {
			continue
		}
		fmt.Fprintf(&out, "\n%s  PR #%d\nchecks: %s\nrepository %s  branch %s  head %s\n", t.Name, t.Number, summary(t.Checks), s.Repository, t.Branch, short(t.Commit))
		checks := append([]Check(nil), t.Checks...)
		order := map[string]int{"failing": 0, "pending": 1, "passing": 2}
		sort.SliceStable(checks, func(i, j int) bool { return order[checks[i].Standing] < order[checks[j].Standing] })
		for _, c := range checks {
			fmt.Fprintf(&out, "%-16s %s", c.State, c.Name)
			if c.URL != "" {
				out.WriteString("  " + c.URL)
			}
			out.WriteString("\n")
		}
	}
	return out.String()
}

func title(t Target) string { return fmt.Sprintf("%s (#%d)", t.Title, t.Number) }

// decide refuses on CI that is not passing (unless force), answers a dry run,
// or starts the merges.
func decide(s *State) {
	var blockers []string
	for _, t := range s.Targets {
		if v := verdict(t.Checks); t.Number != 0 && (v == "failing" || v == "pending") {
			blockers = append(blockers, fmt.Sprintf("%s (PR #%d): %s", t.Name, t.Number, summary(t.Checks)))
		}
	}
	if len(blockers) > 0 && !s.Force {
		var out strings.Builder
		out.WriteString(header(s))
		for _, t := range s.Targets {
			out.WriteString(line(t))
		}
		waited := ""
		if s.Minutes > 0 && time.Now().Unix() >= s.Deadline {
			waited = fmt.Sprintf(" after waiting %d min", s.Minutes)
		}
		fmt.Fprintf(&out, "FAILED: stack-merge-to-master refused; nothing was merged: CI is not passing on %s%s. Fix it, or wait for it (wait-for-ci-min=<minutes>), or pass force=true to merge anyway.\n",
			strings.Join(blockers, "; "), waited)
		out.WriteString(detail(s))
		answer(out.String(), true)
		return
	}
	if s.DryRun {
		var out strings.Builder
		out.WriteString("dry run: nothing merged.\n" + header(s))
		for _, t := range s.Targets {
			text := strings.TrimSuffix(line(t), "\n")
			if t.Number != 0 {
				text += fmt.Sprintf("  would merge as %q", title(t))
			}
			out.WriteString(text + "\n")
		}
		if len(blockers) > 0 {
			fmt.Fprintf(&out, "force=true: CI is not passing on %s.\n", strings.Join(blockers, "; "))
		}
		answer(out.String(), false)
		return
	}
	s.K = -1
	advance(s)
}

// advance moves to the next layer with an open PR and reads it, or finishes.
func advance(s *State) {
	for s.K++; s.K < len(s.Targets) && s.Targets[s.K].Number == 0; s.K++ {
	}
	if s.K >= len(s.Targets) {
		finish(s, "")
		return
	}
	s.Until = time.Now().Add(2 * time.Minute).Unix()
	next("based", []map[string]any{readPR(s)}, s)
}

func readPR(s *State) map[string]any {
	return map[string]any{"op": "api", "method": "GET", "path": repoPath(s, fmt.Sprintf("/pulls/%d", s.Targets[s.K].Number))}
}

// based waits for GitHub to point the PR at the trunk, which it does itself
// for a PR in a GitHub stack once the PR below merges, and does it for one
// that is not; then starts the merge of the head it read.
func based(s *State) {
	rs := results(-1)
	r := rs[len(rs)-1]
	ok(s, rs[0])
	ok(s, r)
	t := &s.Targets[s.K]
	pr := obj(r["body"])
	if pr["merged_at"] != nil || pr["merged"] == true {
		t.Outcome = "already merged"
		advance(s)
		return
	}
	base := str(obj(pr["base"])["ref"])
	if base == s.Trunk {
		body := map[string]any{"sha": str(obj(pr["head"])["sha"]), "merge_action": "direct_merge",
			"merge_method": "squash", "commit_title": title(*t)}
		if t.Body != "" {
			body["commit_message"] = t.Body
		}
		s.Until = time.Now().Add(5 * time.Minute).Unix()
		next("merging", []map[string]any{{"op": "api", "method": "PUT",
			"path": repoPath(s, fmt.Sprintf("/pulls/%d/merge-async", t.Number)), "body": body, "ok_status": []int{409}}}, s)
		return
	}
	if time.Now().Unix() >= s.Until {
		t.Outcome = "not merged"
		fail(s, fmt.Sprintf("PR #%d still targets %s two minutes after the layer below merged: GitHub has not retargeted it at %s yet. Run stack-merge-to-master again in a minute; the layers below are merged.",
			t.Number, base, s.Trunk))
		return
	}
	if pr["stack"] != nil {
		time.Sleep(3 * time.Second)
		next("based", []map[string]any{readPR(s)}, s)
		return
	}
	next("based", []map[string]any{{"op": "api", "method": "PATCH",
		"path": repoPath(s, fmt.Sprintf("/pulls/%d", t.Number)), "body": map[string]any{"base": s.Trunk}}, readPR(s)}, s)
}

// merging follows an asynchronous merge to its end.
func merging(s *State) {
	r := results(1)[0]
	t := &s.Targets[s.K]
	if r["ok"] != true {
		t.Outcome = "NOT MERGED"
		fail(s, why(r))
		return
	}
	body := obj(r["body"])
	details := obj(body["details"])
	switch str(body["status"]) {
	case "merged":
		t.Outcome = fmt.Sprintf("merged %s  %q", short(str(details["sha"])), title(*t))
		if str(details["sha"]) == "" {
			t.Outcome = fmt.Sprintf("merged  %q", title(*t))
		}
		s.Merged++
		advance(s)
		return
	case "failed":
		t.Outcome = "NOT MERGED"
		reason := str(details["message"])
		if reason == "" {
			reason = "no reason given"
		}
		fail(s, fmt.Sprintf("GitHub could not merge PR #%d: %s", t.Number, reason))
		return
	case "enqueued":
		t.Outcome = "NOT MERGED"
		fail(s, fmt.Sprintf("PR #%d was added to the merge queue, which merges it on its own schedule with the repository's default commit message. Run stack-merge-to-master again once it has merged, for the layers above.", t.Number))
		return
	}
	uuid := str(details["uuid"])
	if uuid == "" {
		t.Outcome = "NOT MERGED"
		fail(s, fmt.Sprintf("GitHub accepted the merge of PR #%d but gave no id to poll; check the PR before trying again", t.Number))
		return
	}
	if time.Now().Unix() >= s.Until {
		t.Outcome = "NOT MERGED"
		fail(s, fmt.Sprintf("the merge of PR #%d is still pending after five minutes (merge %s); check the PR before trying again", t.Number, uuid))
		return
	}
	time.Sleep(3 * time.Second)
	next("merging", []map[string]any{{"op": "api", "method": "GET",
		"path": repoPath(s, fmt.Sprintf("/pulls/%d/merge-async/%s", t.Number, uuid))}}, s)
}

// fail stops the merges: the layers below are merged, the ones above not.
func fail(s *State, problem string) {
	finish(s, problem)
	panic(stopped{})
}

type stopped struct{}

func finish(s *State, problem string) {
	var out strings.Builder
	out.WriteString(header(s))
	for _, t := range s.Targets {
		out.WriteString(line(t))
	}
	if problem != "" {
		out.WriteString("FAILED: " + problem + "\nThe layers above it were not merged.\n")
		out.WriteString(detail(s))
	}
	switch {
	case s.Merged > 0:
		out.WriteString("Next: run caos-std/stack-sync with remove-merged=true to drop the merged layers and adopt GitHub's rebase of the ones above, then caos-std/stack-publish.\n")
	case problem == "":
		out.WriteString("Nothing to merge: every selected layer's pull request is already merged.\n")
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
			if _, ok := r.(stopped); ok {
				return
			}
			no, ok := r.(refusal)
			if !ok {
				panic(r)
			}
			answer("stack-merge-to-master refused; nothing was merged: "+no.msg+"\n", true)
		}()
		switch stage := arg("stage"); stage {
		case "":
			start()
		case "list":
			list(load())
		case "ci":
			ciStage(load())
		case "verdict":
			verdictStage(load())
		case "based":
			based(load())
		case "merging":
			merging(load())
		default:
			refuse("unknown stage %q", stage)
		}
	})
}
