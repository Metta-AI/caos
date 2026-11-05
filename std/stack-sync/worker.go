// The `stack-sync` tool's worker. Its docs live in the sibling `.caos-expr`
// here-string, not in this header (SPEC, "CaosTools").
//
// Bring a stack directory up to date with its remote, the way `gt sync` does
// (design/stack-publish.md, "stack-sync"), as an ordinary WRITER
// (design/tool-contract.md): its root is the stack directory, and it returns
// that directory with the layers' gitlinks moved, which the harness applies
// as one change. It holds no token: reads and imports go through its
// neighbour `DEEP-DEPS/github`. Merges are git's three-way `merge-tree`,
// done here, as std/merge does them, and minted into caos.
//
// Stages, by --stage:
//
//	start   read the stack; read the repository's current name, the trunk's
//	        head and every published layer's branch head
//	import  read which layers' PRs were merged, and import every remote head
//	        the stack does not hold, so the sync can reach it
//	sync    adopt, merge and restack bottom to top; answer with the new
//	        directory
package main

import (
	"bytes"
	"encoding/json"
	"fmt"
	"os"
	"os/exec"
	"path/filepath"
	"regexp"
	"slices"
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

const root = "/cas/args/root"

var (
	layerName = regexp.MustCompile(`^([0-9]{2})-(.+)$`)
	hash      = regexp.MustCompile(`^[0-9a-fA-F]{40}$`)
	githubURL = regexp.MustCompile(`(?i)^https://github\.com(:443)?/([A-Za-z0-9_.-]+)/([A-Za-z0-9_.-]+?)(\.git)?/?$`)
	remoteURL = regexp.MustCompile(`^https://[^/@\s]+/\S+$`)
)

// Layer is one gitlink of the stack. Branch is "" for an undescribed layer,
// which is never published and so has no remote to sync with.
type Layer struct {
	Name   string `json:"name"`
	Branch string `json:"branch"`
	Commit string `json:"commit"`
	Remote string `json:"remote"` // the branch's head, "" when it does not exist
	Merged int64  `json:"merged"` // the number of its merged PR, 0 for none
}

// State is what one stage hands the next.
type State struct {
	Repository   string   `json:"repository"`
	Trunk        string   `json:"trunk"`
	Owner        string   `json:"owner"`
	Repo         string   `json:"repo"`
	Base         string   `json:"base"`
	TrunkHead    string   `json:"trunk_head"`
	Layers       []Layer  `json:"layers"`
	Notes        []string `json:"notes"` // per layer, what reading the remote found
	Checked      []int    `json:"checked"` // the layers whose closed PRs the import batch lists, in op order
	Imports      int      `json:"imports"`
	RemoveMerged bool     `json:"remove_merged"`
	DryRun       bool     `json:"dry_run"`
}

func header(s *State) string {
	return fmt.Sprintf("repository %s  trunk %s\n", s.Repository, s.Trunk)
}

// answer is the writer's result: `stdout`, and the new directory as `root`
// (the old one when nothing changed), or `failed` and nothing to apply.
func answer(text, newRoot string, failed bool) {
	const dir = "/tmp/stack-sync-out"
	w.Must(os.RemoveAll(dir))
	w.Must(os.MkdirAll(dir, 0o755))
	w.Must(os.WriteFile(filepath.Join(dir, "stdout"), []byte(text), 0o644))
	if failed {
		w.Must(os.WriteFile(filepath.Join(dir, "failed"), []byte("stack-sync failed\n"), 0o644))
	} else {
		w.Must(os.Symlink(newRoot, filepath.Join(dir, "root")))
		w.Must(os.WriteFile(filepath.Join(dir, "commit-message"), []byte("stack-sync\n"), 0o644))
	}
	w.Put(dir, "/cas/out")
}

// next hands std/github a batch and names `stage` as its `then`. The root
// rides along: the sync stage builds the new directory from it.
func next(stage string, ops []map[string]any, s *State) {
	w.Must(os.WriteFile("/tmp/state.json", w.Check(json.Marshal(s)), 0o644))
	w.Put("/tmp/state.json", "/cas/state")
	w.Must(os.WriteFile("/tmp/batch.json", w.Check(json.Marshal(ops)), 0o644))
	w.Put("/tmp/batch.json", "/cas/batch")
	id := arg("request-id")
	request := w.Caos("prepare-request", "--base:@=/cas/args/github", "--batch:@=/cas/batch",
		"--request-id="+id+"-"+stage)
	then := w.Caos("curry", "--base:@=/cas/args/base", "--worker1:@=/cas/args/worker1",
		"--github:@=/cas/args/github", "--stage="+stage, "--request-id="+id,
		"--state:@=/cas/state", "--root:@="+root)
	w.Caos("run-request-then", request, "--then:hash="+then)
}

func load() *State {
	var s State
	w.Must(json.Unmarshal([]byte(w.Read("/cas/args/state")), &s))
	return &s
}

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

func why(r map[string]any) string {
	if e := str(r["error"]); e != "" {
		return e
	}
	if r["skipped"] == true {
		return "not attempted: an operation before it failed"
	}
	message := str(r["body"])
	if body, ok := r["body"].(map[string]any); ok {
		message = str(body["message"])
	}
	return strings.TrimSpace(fmt.Sprintf("%s %s: HTTP %v %s", str(r["method"]), str(r["path"]), r["status"], message))
}

func short(commit string) string { return commit[:12] }

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

// provenance is 00-base.source.json, parsed; nil when there is none.
func provenance() map[string]any {
	path := root + "/00-base.source.json"
	if !w.Exists(path) {
		return nil
	}
	var value map[string]any
	if json.Unmarshal([]byte(w.Read(path)), &value) != nil {
		refuse("00-base.source.json is not JSON")
	}
	return value
}

// start reads the stack, as stack-publish does: 00-base, every layer, and
// which layers are described (and so published).
func start() {
	if !w.Exists(root) {
		refuse("nothing is at the root path: root is the stack directory, such as sources/repo")
	}
	if k := kind(root); k != "tree" {
		refuse("the root is a %s: stack-sync takes a stack directory, such as sources/repo", k)
	}
	w.Get(root)
	s := &State{RemoveMerged: flag("remove-merged"), DryRun: flag("dry-run")}
	described := map[string]bool{}
	for _, e := range w.Check(os.ReadDir(root)) {
		name := e.Name()
		path := filepath.Join(root, name)
		if stem, ok := strings.CutSuffix(name, ".pr.md"); ok {
			described[stem] = true
			continue
		}
		m := layerName.FindStringSubmatch(name)
		if m == nil {
			continue
		}
		if m[1] == "00" {
			if name == "00-base" && kind(path) == "commit" {
				s.Base = w.Caos("hash", path)
			}
			continue
		}
		if kind(path) != "commit" {
			refuse("%[1]s is named like a layer but is not a gitlink. A layer is a source tree; its description is %[1]s.pr.md beside it.", name)
		}
		s.Layers = append(s.Layers, Layer{Name: name, Branch: m[2], Commit: w.Caos("hash", path)})
	}
	if s.Base == "" {
		refuse("the stack has no 00-base gitlink: import the upstream tip there first (std/GIT.md)")
	}
	prefix := arg("branch-prefix")
	for i := range s.Layers {
		l := &s.Layers[i]
		if !described[l.Name] {
			l.Branch = ""
			continue
		}
		delete(described, l.Name)
		l.Branch = prefix + l.Branch
		checkBranch("layer "+l.Name+"'s branch", l.Branch)
	}
	for stem := range described {
		refuse("%[1]s.pr.md describes no layer: there is no gitlink %[1]s", stem)
	}
	p := provenance()
	s.Repository, s.Trunk = arg("repository"), arg("trunk")
	if s.Repository == "" {
		s.Repository = str(p["repository"])
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
		s.Trunk = inferTrunk(str(p["requested_revision"]), str(p["default_branch"]))
	}
	if s.Trunk == "" {
		refuse("no trunk: 00-base.source.json names no branch, so pass trunk")
	}
	checkBranch("trunk", s.Trunk)

	var ops []map[string]any
	if s.Owner != "" {
		ops = append(ops, map[string]any{"op": "api", "method": "GET", "path": "/repos/" + s.Owner + "/" + s.Repo})
	}
	ops = append(ops, map[string]any{"op": "head", "repository": s.Repository, "branch": s.Trunk})
	for _, l := range s.Layers {
		if l.Branch != "" {
			ops = append(ops, map[string]any{"op": "head", "repository": s.Repository, "branch": l.Branch})
		}
	}
	next("import", ops, s)
}

// importStage reads the heads, then lists each published layer's closed PRs
// and imports every remote head the stack does not already hold.
func importStage(s *State) {
	published := 0
	for _, l := range s.Layers {
		if l.Branch != "" {
			published++
		}
	}
	skip := 0
	if s.Owner != "" {
		skip = 1
	}
	rs := results(skip + 1 + published)
	for _, r := range rs {
		if r["ok"] != true {
			answer("stack-sync: reading the remote failed; nothing changed: "+why(r)+"\n", "", true)
			return
		}
	}
	if skip == 1 {
		if body, ok := rs[0]["body"].(map[string]any); ok {
			owner, _ := body["owner"].(map[string]any)
			if login, name := str(owner["login"]), str(body["name"]); login != "" && name != "" {
				s.Owner, s.Repo = login, name
			}
		}
	}
	s.TrunkHead = str(rs[skip]["head"])
	if s.TrunkHead == "" {
		answer(fmt.Sprintf("stack-sync refused; nothing changed: the trunk %s does not exist on %s\n", s.Trunk, s.Repository), "", true)
		return
	}
	k := skip + 1
	for i := range s.Layers {
		if s.Layers[i].Branch != "" {
			s.Layers[i].Remote = str(rs[k]["head"])
			k++
		}
	}
	var ops []map[string]any
	if s.Owner != "" {
		for i, l := range s.Layers {
			if l.Branch != "" {
				s.Checked = append(s.Checked, i)
				ops = append(ops, map[string]any{"op": "api", "method": "GET",
					"path": fmt.Sprintf("/repos/%s/%s/pulls?head=%s:%s&state=closed", s.Owner, s.Repo, s.Owner, l.Branch)})
			}
		}
	}
	var wanted []string
	if s.TrunkHead != s.Base {
		wanted = append(wanted, s.TrunkHead)
	}
	for _, l := range s.Layers {
		if l.Remote != "" && l.Remote != l.Commit && !slices.Contains(wanted, l.Remote) {
			wanted = append(wanted, l.Remote)
		}
	}
	for _, commit := range wanted {
		ops = append(ops, map[string]any{"op": "import", "repository": s.Repository, "revision": commit})
	}
	s.Imports = len(wanted)
	if len(ops) == 0 {
		sync(s)
		return
	}
	next("sync", ops, s)
}

// graph is a scratch repository holding the commit graph of everything the
// sync reads; merge-tree faults in the trees it needs from caos.
const graph = "/tmp/graph"

func git(args ...string) (string, int) {
	cmd := exec.Command("git", append([]string{"-C", graph}, args...)...)
	var out bytes.Buffer
	cmd.Stdout, cmd.Stderr = &out, &out
	err := cmd.Run()
	code := 0
	if exit, ok := err.(*exec.ExitError); ok {
		code = exit.ExitCode()
	} else if err != nil {
		w.True(false, "git %v: %v", args, err)
	}
	return strings.TrimSpace(out.String()), code
}

func mustGit(args ...string) string {
	out, code := git(args...)
	w.True(code == 0, "git %v: exit %d: %s", args, code, out)
	return out
}

func fetch(commits []string) {
	w.Must(os.MkdirAll(graph, 0o755))
	mustGit("init", "-q")
	mustGit("remote", "add", "origin", os.Getenv("CAOS_SERVER_URL"))
	mustGit("config", "extensions.partialClone", "origin")
	mustGit("config", "remote.origin.promisor", "true")
	mustGit("config", "remote.origin.partialclonefilter", "tree:0")
	mustGit(append([]string{"fetch", "-q", "--filter=tree:0", "origin"}, commits...)...)
}

func isAncestor(a, b string) bool {
	out, code := git("merge-base", "--is-ancestor", a, b)
	w.True(code == 0 || code == 1, "git merge-base --is-ancestor: exit %d: %s", code, out)
	return code == 0
}

func mergeBase(a, b string) string {
	out, code := git("merge-base", a, b)
	if code != 0 {
		return ""
	}
	return out
}

// rewriteOf is the nearest first-parent ancestor of tip, tip itself first,
// whose tree is `tree`: a remote head with the tree of a commit we hold is a
// rewrite of it, which is what GitHub rebasing the branches above a merged
// stacked PR produces.
func rewriteOf(tip, tree string) string {
	out := mustGit("log", "--first-parent", "-n", "500", "--format=%H %T", tip)
	for _, line := range strings.Split(out, "\n") {
		if c, t, ok := strings.Cut(line, " "); ok && t == tree {
			return c
		}
	}
	return ""
}

var minted int

// merge three-way merges theirs into ours from base, and mints the merge
// commit (parents [ours, theirs], ours' time, as std/merge) into caos. On a
// conflict it returns the conflicting paths instead.
func merge(base, ours, theirs, message string) (string, []string) {
	out, code := git("merge-tree", "--write-tree", "--name-only", "--merge-base="+base, ours, theirs)
	w.True(code == 0 || code == 1, "git merge-tree: exit %d: %s", code, out)
	lines := strings.Split(out, "\n")
	if code == 1 {
		var paths []string
		for _, line := range lines[1:] {
			if line == "" {
				break
			}
			paths = append(paths, line)
		}
		return "", paths
	}
	minted++
	work := fmt.Sprintf("/tmp/merged-%d", minted)
	w.Must(os.MkdirAll(work, 0o755))
	archive := exec.Command("sh", "-c", `git -C "$1" archive "$2" | tar -x -C "$3"`, "sh", graph, lines[0], work)
	w.Must(archive.Run())
	w.Caos("put", work, fmt.Sprintf("/cas/tree-%d", minted))
	tree := w.Caos("hash", fmt.Sprintf("/cas/tree-%d", minted))
	when := mustGit("show", "-s", "--format=%ct", ours)
	body := fmt.Sprintf("tree %s\nparent %s\nparent %s\nauthor caos <caos@caos> %s +0000\ncommitter caos <caos@caos> %s +0000\n\n%s\n",
		tree, ours, theirs, when, when, message)
	file := fmt.Sprintf("/tmp/commit-%d", minted)
	w.Must(os.WriteFile(file, []byte(body), 0o644))
	commit := w.Caos("put-commit", file, fmt.Sprintf("/cas/commit-%d", minted))
	// The new commit must be reachable from the graph for the layers above.
	mustGit("fetch", "-q", "--filter=tree:0", "origin", commit)
	return commit, nil
}

func hint(layer, theirs string) string {
	return fmt.Sprintf("Run run_tool(path=\"caos-std/merge\", arguments={\"root\": \"<stack>/%s\", \"theirs\": \"%s\"}) to get a conflict ledger, resolve it, and run stack-sync again.", layer, theirs)
}

// sync does the work, bottom to top, and answers with the new directory. A
// layer that cannot be synced stops it there: the layers below it are synced
// and returned, the ones above are not touched.
func sync(s *State) {
	if arg("stage") == "sync" {
		rs := results(len(s.Checked) + s.Imports)
		for k, r := range rs {
			if r["ok"] != true {
				answer("stack-sync: reading the remote failed; nothing changed: "+why(r)+"\n", "", true)
				return
			}
			if k < len(s.Checked) {
				prs, _ := r["body"].([]any)
				for _, raw := range prs {
					pr, _ := raw.(map[string]any)
					if pr["merged_at"] != nil {
						n, _ := pr["number"].(float64)
						s.Layers[s.Checked[k]].Merged = int64(n)
						break
					}
				}
			}
		}
	}
	commits := []string{s.Base, s.TrunkHead}
	for _, l := range s.Layers {
		commits = append(commits, l.Commit)
		if l.Remote != "" {
			commits = append(commits, l.Remote)
		}
	}
	fetch(commits)

	var out strings.Builder
	if s.DryRun {
		out.WriteString("dry run: nothing changed.\n")
	}
	out.WriteString(header(s))
	changed := map[string]string{} // entry name -> new commit, "" to remove
	below, belowName := s.Base, "00-base"
	if s.TrunkHead == s.Base {
		fmt.Fprintf(&out, "00-base  %s  %s  at the trunk's head\n", s.Trunk, short(s.Base))
	} else {
		fmt.Fprintf(&out, "00-base  %s  %s -> %s  the trunk moved\n", s.Trunk, short(s.Base), short(s.TrunkHead))
		changed["00-base"] = s.TrunkHead
		below = s.TrunkHead
	}
	stopped := ""
	for _, l := range s.Layers {
		tip := l.Commit
		var notes []string
		removed := false
		if l.Branch != "" {
			if l.Merged != 0 {
				notes = append(notes, fmt.Sprintf("PR #%d is merged", l.Merged))
				if s.RemoveMerged {
					changed[l.Name], changed[l.Name+".pr.md"] = "", ""
					notes = append(notes, "removed with its .pr.md")
					removed = true
				}
			}
			switch {
			case removed:
			case l.Remote == "":
				notes = append(notes, l.Branch+": not published")
			case l.Remote == tip:
				notes = append(notes, l.Branch+": in sync")
			case isAncestor(l.Remote, tip):
				notes = append(notes, l.Branch+": local edits ahead of the remote")
			default:
				remoteTree := mustGit("rev-parse", l.Remote+"^{tree}")
				same := rewriteOf(tip, remoteTree)
				switch {
				case same == tip:
					tip = l.Remote
					notes = append(notes, fmt.Sprintf("%s: rewritten remotely with the same content, adopted %s", l.Branch, short(l.Remote)))
				case same != "":
					commit, conflicts := merge(same, tip, l.Remote, "stack-sync: adopt the rewritten "+l.Branch)
					if conflicts != nil {
						stopped = fmt.Sprintf("%s: adopting the rewritten remote %s conflicts in %s. %s", l.Branch, l.Remote, strings.Join(conflicts, ", "), hint(l.Name, l.Remote))
					} else {
						tip = commit
						notes = append(notes, fmt.Sprintf("%s: rewritten remotely, adopted %s under the local edits", l.Branch, short(l.Remote)))
					}
				default:
					base := mergeBase(tip, l.Remote)
					if base == "" {
						stopped = fmt.Sprintf("%s: the remote head %s shares no history with the layer. %s", l.Branch, l.Remote, hint(l.Name, l.Remote))
						break
					}
					commit, conflicts := merge(base, tip, l.Remote, "stack-sync: merge the remote "+l.Branch)
					if conflicts != nil {
						stopped = fmt.Sprintf("%s: merging the remote %s conflicts in %s. %s", l.Branch, l.Remote, strings.Join(conflicts, ", "), hint(l.Name, l.Remote))
					} else {
						tip = commit
						notes = append(notes, fmt.Sprintf("%s: merged the remote's changes (%s)", l.Branch, short(l.Remote)))
					}
				}
			}
		}
		if stopped == "" && !removed && !isAncestor(below, tip) {
			base := mergeBase(below, tip)
			if base == "" {
				stopped = fmt.Sprintf("%s: shares no history with %s. %s", l.Name, belowName, hint(l.Name, below))
			} else if commit, conflicts := merge(base, tip, below, fmt.Sprintf("stack-sync: restack %s onto %s", l.Name, belowName)); conflicts != nil {
				stopped = fmt.Sprintf("restacking %s onto %s conflicts in %s. %s", l.Name, belowName, strings.Join(conflicts, ", "), hint(l.Name, below))
			} else {
				tip = commit
				notes = append(notes, "restacked onto "+belowName)
			}
		}
		if stopped == "" && !removed {
			if tip == l.Commit {
				notes = append(notes, "unchanged")
			} else {
				changed[l.Name] = tip
			}
			below, belowName = tip, l.Name
		}
		fmt.Fprintf(&out, "%s  %s\n", l.Name, strings.Join(notes, ", "))
		if stopped != "" {
			break
		}
	}
	switch {
	case stopped != "":
		out.WriteString("FAILED: " + stopped + "\nThe layers below it are synced; the ones above were not touched.\n")
	case len(changed) == 0:
		out.WriteString("Nothing to do: the stack is in sync.\n")
	case !s.DryRun:
		out.WriteString("Next: run caos-std/stack-publish to push.\n")
	}
	if s.DryRun || len(changed) == 0 {
		answer(out.String(), root, false)
		return
	}
	answer(out.String(), rebuild(s, changed), false)
}

// rebuild stages the new stack directory: links to every entry of the old one
// but those that changed, each moved gitlink as its commit, and
// 00-base.source.json saying where 00-base now is.
func rebuild(s *State, changed map[string]string) string {
	const dir = "/tmp/stack"
	w.Must(os.MkdirAll(dir, 0o755))
	for _, e := range w.Check(os.ReadDir(root)) {
		name := e.Name()
		commit, moved := changed[name]
		switch {
		case !moved && !(name == "00-base.source.json" && changed["00-base"] != ""):
			w.Must(os.Symlink(filepath.Join(root, name), filepath.Join(dir, name)))
		case name == "00-base.source.json":
			p := provenance()
			p["commit"] = changed["00-base"]
			p["observed_at"] = time.Now().Unix()
			w.Must(os.WriteFile(filepath.Join(dir, name), append(w.Check(json.MarshalIndent(p, "", "  ")), '\n'), 0o644))
		case commit == "":
			// removed
		default:
			at := "/cas/link-" + name
			w.Caos("get-hash", commit, at)
			w.Must(os.Symlink(at, filepath.Join(dir, name)))
		}
	}
	return dir
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
			answer("stack-sync refused; nothing changed: "+no.msg+"\n", "", true)
		}()
		switch stage := arg("stage"); stage {
		case "":
			start()
		case "import":
			w.Get(root)
			importStage(load())
		case "sync":
			w.Get(root)
			sync(load())
		default:
			refuse("unknown stage %q", stage)
		}
	})
}
